//! Experimental invitation lifecycle around the actual SPAKE2 exchange.
//!
//! Reserve BEFORE accepting any Noise XX handshake work. Keep the manager alive
//! across connections; reconnecting never renews an invitation's attempt budget.
//! Each invitation lasts 120 seconds and permits three attempts, with one live
//! attempt in this process. All public managers share five reservations per
//! rolling 60 seconds. Cancel/replacement/failure/drop never refund them.
//! Restarting the process resets the gate but invalidates ALL old invitations;
//! never persist codes or restart per connection to retain an old invitation.
//!
//! Every phase checks the same manager, generation, attempt and invitation end.
//! The caller must additionally bound network IO and observe cancellation while
//! waiting: a returned message/tag is public data, not a revocable network send.
//! Only explicit local approval produces ApprovedBinding. It is evidence of local
//! approval at that moment, NOT mutual delivery, durable consent, or saved trust.
//! Later cancellation does not retroactively invalidate an already returned
//! ApprovedBinding; a future persistence workflow needs its own commit policy.

use crate::{
    AuthenticatedBinding, Binding, CODE_BYTES, CONFIRMATION_BYTES, Confirmation, Pairing, Role,
};
use anyhow::{Context, Result, ensure};
use std::collections::VecDeque;
use std::ffi::c_int;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const INVITATION_LIFETIME: Duration = Duration::from_secs(120);
const INVITATION_ATTEMPTS: u8 = 3;
const GLOBAL_WINDOW: Duration = Duration::from_secs(60);
const GLOBAL_ATTEMPTS: usize = 5;

// Exact signature from the fixed revision's include/openssl/rand.h.
unsafe extern "C" {
    fn RAND_bytes(buffer: *mut u8, len: usize) -> c_int;
}

/// Explicit local-display access only. Never Debug, Display, Clone or Serialize.
/// A displayed copy cannot authorize attempts after cancel/replacement/expiry.
pub struct DisplayCode(Zeroizing<[u8; CODE_BYTES]>);

impl DisplayCode {
    pub fn as_bytes(&self) -> &[u8; CODE_BYTES] {
        &self.0
    }
}

#[derive(Default)]
struct Budget {
    reservations: VecDeque<Instant>,
    busy: Option<u64>,
    serial: u64,
}

impl Budget {
    fn admit(&mut self, now: Instant) -> Result<u64> {
        self.reservations
            .retain(|at| now.saturating_duration_since(*at) < GLOBAL_WINDOW);
        ensure!(self.busy.is_none(), "Attempt already in progress");
        ensure!(
            self.reservations.len() < GLOBAL_ATTEMPTS,
            "Global attempt limit reached"
        );
        let serial = self
            .serial
            .checked_add(1)
            .context("Global attempt counter exhausted")?;
        self.reservations.push_back(now);
        self.serial = serial;
        self.busy = Some(serial);
        Ok(serial)
    }
}

static PROCESS_BUDGET: OnceLock<Arc<Mutex<Budget>>> = OnceLock::new();

struct Invitation {
    generation: u64,
    deadline: Instant,
    code: Zeroizing<[u8; CODE_BYTES]>,
    attempts: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct AttemptId {
    generation: u64,
    serial: u64,
}

struct State {
    alive: bool,
    generation: u64,
    serial: u64,
    active: Option<Invitation>,
    busy: Option<AttemptId>,
}

impl State {
    fn expire(&mut self, now: Instant) {
        if self
            .active
            .as_ref()
            .is_some_and(|invite| now >= invite.deadline)
        {
            self.active = None;
        }
    }

    fn revoke(&mut self) {
        self.active = None;
        // Also supersede a replacement which is currently generating randomness.
        if let Some(generation) = self.generation.checked_add(1) {
            self.generation = generation;
        } else {
            self.alive = false;
        }
        // Busy is deliberately retained until the old ticket is dropped.
    }

    fn check(&mut self, id: AttemptId, now: Instant) -> Result<()> {
        self.expire(now);
        ensure!(self.alive, "Invitation manager is closed");
        let active = self
            .active
            .as_ref()
            .context("Invitation is inactive or expired")?;
        ensure!(
            active.generation == id.generation && self.busy == Some(id),
            "Invitation attempt was revoked"
        );
        // The last admitted attempt remains valid even when attempts == 3.
        Ok(())
    }
}

pub struct InvitationManager {
    state: Arc<Mutex<State>>,
    budget: Arc<Mutex<Budget>>,
}

impl Default for InvitationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl InvitationManager {
    pub fn new() -> Self {
        Self::with_budget(
            PROCESS_BUDGET
                .get_or_init(|| Arc::new(Mutex::new(Budget::default())))
                .clone(),
        )
    }

    fn with_budget(budget: Arc<Mutex<Budget>>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                alive: true,
                generation: 0,
                serial: 0,
                active: None,
                busy: None,
            })),
            budget,
        }
    }

    /// Revoke the current invitation and create a new random one. This never
    /// releases a live old ticket or resets the shared rolling attempt gate.
    pub fn replace(&self) -> Result<DisplayCode> {
        self.replace_with_clock(Instant::now)
    }

    fn replace_with_clock(&self, clock: impl Fn() -> Instant) -> Result<DisplayCode> {
        let (generation, deadline, previous_code) = {
            let mut state = lock(&self.state)?;
            ensure!(state.alive, "Invitation manager is closed");
            let deadline = clock()
                .checked_add(INVITATION_LIFETIME)
                .context("Invitation deadline overflow")?;
            let previous_code = state.active.take().map(|invite| invite.code);
            state.revoke();
            ensure!(state.alive, "Invitation generation exhausted");
            (state.generation, deadline, previous_code)
        };
        let mut code = random_code()?;
        // Avoid immediately reissuing the same displayed code during replacement.
        // The generation, not the code's digits, identifies its attempt budget.
        for _ in 0..32 {
            if previous_code
                .as_ref()
                .is_none_or(|old| old.as_slice() != code.as_slice())
            {
                break;
            }
            code = random_code()?;
        }
        ensure!(
            previous_code
                .as_ref()
                .is_none_or(|old| old.as_slice() != code.as_slice()),
            "Could not replace invitation code"
        );
        let mut state = lock(&self.state)?;
        ensure!(
            state.alive && state.generation == generation,
            "Invitation replacement was revoked"
        );
        ensure!(clock() < deadline, "Invitation expired during creation");
        let display = DisplayCode(Zeroizing::new(*code));
        state.active = Some(Invitation {
            generation,
            deadline,
            code,
            attempts: 0,
        });
        Ok(display)
    }

    pub fn cancel(&self) {
        // Cancellation must remain fail-closed even following a poisoned lock.
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .revoke();
    }

    /// Charge before network/Noise work. Failed or abandoned attempts still count.
    pub fn reserve(&self) -> Result<AttemptPermit> {
        self.reserve_with_clock(Instant::now)
    }

    #[cfg(test)]
    fn reserve_at(&self, now: Instant) -> Result<AttemptPermit> {
        self.reserve_with_clock(|| now)
    }

    fn reserve_with_clock(&self, clock: impl Fn() -> Instant) -> Result<AttemptPermit> {
        // Like Ticket Drop, lock manager state -> process budget. No path
        // holds budget while acquiring state, including Drop and approval.
        let mut state = lock(&self.state)?;
        let mut budget = lock(&self.budget)?;
        // Read the decision time AFTER acquiring both locks. Waiting for either
        // mutex must not extend expiry or move a reservation outside its window.
        let now = clock();
        state.expire(now);
        ensure!(state.alive, "Invitation manager is closed");
        let active = state
            .active
            .as_ref()
            .context("Invitation is inactive or expired")?;
        ensure!(state.busy.is_none(), "Attempt already in progress");
        ensure!(
            active.attempts < INVITATION_ATTEMPTS,
            "Invitation attempt limit reached"
        );
        let serial = state
            .serial
            .checked_add(1)
            .context("Invitation attempt counter exhausted")?;
        let id = AttemptId {
            generation: active.generation,
            serial,
        };
        let global_token = budget.admit(now)?;
        // No fallible operations after the global reservation is charged.
        state.serial = serial;
        state
            .active
            .as_mut()
            .expect("checked active invitation")
            .attempts += 1;
        state.busy = Some(id);
        Ok(AttemptPermit {
            ticket: Ticket {
                state: self.state.clone(),
                id,
                budget: self.budget.clone(),
                global_token,
            },
        })
    }
}

impl Drop for InvitationManager {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.alive = false;
        state.revoke();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| anyhow::anyhow!("Invitation state unavailable"))
}

struct Ticket {
    state: Arc<Mutex<State>>,
    id: AttemptId,
    budget: Arc<Mutex<Budget>>,
    global_token: u64,
}

impl Ticket {
    fn check_with_clock(&self, clock: &impl Fn() -> Instant) -> Result<()> {
        let mut state = lock(&self.state)?;
        state.check(self.id, clock())
    }

    fn code_with_clock(&self, clock: &impl Fn() -> Instant) -> Result<Zeroizing<[u8; CODE_BYTES]>> {
        let mut state = lock(&self.state)?;
        state.check(self.id, clock())?;
        Ok(Zeroizing::new(
            *state
                .active
                .as_ref()
                .expect("checked active invitation")
                .code,
        ))
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.busy == Some(self.id) {
            state.busy = None;
        }
        let mut budget = self
            .budget
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if budget.busy == Some(self.global_token) {
            budget.busy = None;
        }
        // Invitation attempts and process budget are never refunded.
    }
}

pub struct AttemptPermit {
    ticket: Ticket,
}

impl AttemptPermit {
    pub(crate) fn attempt_check(&self) -> AttemptCheck {
        AttemptCheck {
            state: self.ticket.state.clone(),
            id: self.ticket.id,
        }
    }

    pub fn start(self, binding: Binding) -> Result<InvitationPairing> {
        self.start_with_clock(binding, Instant::now)
    }

    fn start_with_clock(
        self,
        binding: Binding,
        clock: impl Fn() -> Instant,
    ) -> Result<InvitationPairing> {
        let code = self.ticket.code_with_clock(&clock)?;
        let pairing = Pairing::start(Role::Responder, code.as_slice(), binding)?;
        self.ticket.check_with_clock(&clock)?;
        Ok(InvitationPairing {
            ticket: self.ticket,
            pairing,
        })
    }
}

/// Read-only validity observer for network waits. It never owns a Ticket and
/// cannot reserve, release, or refund either the local or process-wide slot.
pub(crate) struct AttemptCheck {
    state: Arc<Mutex<State>>,
    id: AttemptId,
}

impl AttemptCheck {
    pub(crate) fn check(&self) -> Result<()> {
        let state = lock(&self.state)?;
        ensure!(state.alive, "Invitation manager is closed");
        let active = state
            .active
            .as_ref()
            .context("Invitation is inactive or expired")?;
        ensure!(
            active.generation == self.id.generation && state.busy == Some(self.id),
            "Invitation attempt was revoked"
        );
        ensure!(Instant::now() < active.deadline, "Invitation expired");
        Ok(())
    }
}

pub struct InvitationPairing {
    ticket: Ticket,
    pairing: Pairing,
}

impl InvitationPairing {
    pub fn message(&self) -> Result<&[u8]> {
        self.message_with_clock(Instant::now)
    }

    #[cfg(test)]
    fn message_at(&self, now: Instant) -> Result<&[u8]> {
        self.message_with_clock(|| now)
    }

    fn message_with_clock(&self, clock: impl Fn() -> Instant) -> Result<&[u8]> {
        self.ticket.check_with_clock(&clock)?;
        Ok(self.pairing.message())
    }

    pub fn process(self, peer_message: &[u8]) -> Result<InvitationConfirmation> {
        self.process_with_clock(peer_message, Instant::now)
    }

    fn process_with_clock(
        self,
        peer_message: &[u8],
        clock: impl Fn() -> Instant,
    ) -> Result<InvitationConfirmation> {
        self.ticket.check_with_clock(&clock)?;
        let confirmation = self.pairing.process(peer_message)?;
        self.ticket.check_with_clock(&clock)?;
        Ok(InvitationConfirmation {
            ticket: self.ticket,
            confirmation,
        })
    }
}

pub struct InvitationConfirmation {
    ticket: Ticket,
    confirmation: Confirmation,
}

impl InvitationConfirmation {
    pub fn tag(&self) -> Result<&[u8; CONFIRMATION_BYTES]> {
        self.tag_with_clock(Instant::now)
    }

    #[cfg(test)]
    fn tag_at(&self, now: Instant) -> Result<&[u8; CONFIRMATION_BYTES]> {
        self.tag_with_clock(|| now)
    }

    fn tag_with_clock(&self, clock: impl Fn() -> Instant) -> Result<&[u8; CONFIRMATION_BYTES]> {
        self.ticket.check_with_clock(&clock)?;
        Ok(self.confirmation.tag())
    }

    pub fn verify_peer(self, peer_tag: &[u8]) -> Result<PendingApproval> {
        self.verify_with_clock(peer_tag, Instant::now)
    }

    fn verify_with_clock(
        self,
        peer_tag: &[u8],
        clock: impl Fn() -> Instant,
    ) -> Result<PendingApproval> {
        self.ticket.check_with_clock(&clock)?;
        let authenticated = self.confirmation.verify_peer(peer_tag)?;
        self.ticket.check_with_clock(&clock)?;
        Ok(PendingApproval {
            ticket: self.ticket,
            authenticated,
        })
    }
}

/// Authentication is complete locally, but local consent is still pending.
/// Dropping this value rejects the attempt without refunding its budget.
pub struct PendingApproval {
    ticket: Ticket,
    authenticated: AuthenticatedBinding,
}

impl PendingApproval {
    pub fn binding(&self) -> Result<&Binding> {
        self.ticket.check_with_clock(&Instant::now)?;
        Ok(self.authenticated.binding())
    }

    pub fn approve(self) -> Result<ApprovedBinding> {
        self.approve_with_clock(Instant::now)
    }

    #[cfg(test)]
    fn approve_at(self, now: Instant) -> Result<ApprovedBinding> {
        self.approve_with_clock(|| now)
    }

    fn approve_with_clock(self, clock: impl Fn() -> Instant) -> Result<ApprovedBinding> {
        let (approved, hold) = self.approve_held_with_clock(clock)?;
        drop(hold);
        Ok(approved)
    }

    pub(crate) fn approve_held(self) -> Result<(ApprovedBinding, ApprovalHold)> {
        self.approve_held_with_clock(Instant::now)
    }

    fn approve_held_with_clock(
        self,
        clock: impl Fn() -> Instant,
    ) -> Result<(ApprovedBinding, ApprovalHold)> {
        let mut state = lock(&self.ticket.state)?;
        // Validation and consumption are one linearizable operation against
        // cancel/replace/manager Drop. Never use a cached verification result.
        state.check(self.ticket.id, clock())?;
        let deadline = state.active.as_ref().expect("checked invitation").deadline;
        state.active = None;
        let approved = ApprovedBinding {
            binding: *self.authenticated.binding(),
            role: self.authenticated.role(),
        };
        drop(state); // Ticket Drop acquires the same mutex; do not destroy it here.
        Ok((
            approved,
            ApprovalHold {
                ticket: self.ticket,
                deadline,
            },
        ))
    }
}

/// Keep the process-wide in-flight slot until the final transport exchange ends.
/// Unlike ApprovedBinding, this internal lease remains revocable after local
/// approval. It authorizes no input, persistence, or additional pairing attempt.
pub(crate) struct ApprovalHold {
    ticket: Ticket,
    deadline: Instant,
}

impl ApprovalHold {
    pub(crate) fn check(&self) -> Result<()> {
        self.check_with_clock(Instant::now)
    }

    fn check_with_clock(&self, clock: impl Fn() -> Instant) -> Result<()> {
        let state = lock(&self.ticket.state)?;
        let budget = lock(&self.ticket.budget)?;
        ensure!(
            state.alive && state.generation == self.ticket.id.generation,
            "Approval was revoked"
        );
        ensure!(
            state.busy == Some(self.ticket.id) && budget.busy == Some(self.ticket.global_token),
            "Approval attempt is no longer held"
        );
        ensure!(clock() < self.deadline, "Approval invitation expired");
        Ok(())
    }
}

/// Unforgeable through this public API; only PendingApproval::approve creates it.
/// Public binding only. No private material, persistence, or peer-delivery claim.
pub struct ApprovedBinding {
    binding: Binding,
    role: Role,
}

impl ApprovedBinding {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn role(&self) -> Role {
        self.role
    }
}

fn random_code() -> Result<Zeroizing<[u8; CODE_BYTES]>> {
    sample_code(|buffer| {
        // SAFETY: The mutable byte slice is valid for buffer.len() bytes and is
        // exclusively owned. BoringSSL supplies the operating-system CSPRNG.
        let ok = unsafe { RAND_bytes(buffer.as_mut_ptr(), buffer.len()) };
        ensure!(ok == 1, "Invitation randomness unavailable");
        Ok(())
    })
}

fn sample_code(
    mut fill: impl FnMut(&mut [u8]) -> Result<()>,
) -> Result<Zeroizing<[u8; CODE_BYTES]>> {
    let mut output = Zeroizing::new([0; CODE_BYTES]);
    let mut buffer = Zeroizing::new([0; 32]);
    let mut written = 0;
    for _ in 0..32 {
        fill(buffer.as_mut_slice())?;
        for byte in buffer.iter().copied() {
            // 250 is divisible by ten: values 250..255 must be discarded.
            if byte < 250 {
                output[written] = b'0' + byte % 10;
                written += 1;
                if written == CODE_BYTES {
                    return Ok(output);
                }
            }
        }
    }
    anyhow::bail!("Invitation random sampling failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> InvitationManager {
        InvitationManager::with_budget(Arc::new(Mutex::new(Budget::default())))
    }
    fn binding() -> Binding {
        Binding {
            handshake_hash: [1; 32],
            initiator_static: [2; 32],
            responder_static: [3; 32],
        }
    }

    fn confirm(manager: &InvitationManager, code: &DisplayCode, now: Instant) -> PendingApproval {
        let client = Pairing::start(Role::Initiator, code.as_bytes(), binding()).unwrap();
        let server = manager
            .reserve_at(now)
            .unwrap()
            .start_with_clock(binding(), || now)
            .unwrap();
        let client_message = client.message().to_vec();
        let server_message = server.message_at(now).unwrap().to_vec();
        let client = client.process(&server_message).unwrap();
        let server = server.process_with_clock(&client_message, || now).unwrap();
        let client_tag = *client.tag();
        let server_tag = *server.tag_at(now).unwrap();
        assert!(client.verify_peer(&server_tag).is_ok());
        server.verify_with_clock(&client_tag, || now).unwrap()
    }

    #[test]
    fn actual_exchange_requires_approval_and_consumes_invitation() {
        let manager = manager();
        let now = Instant::now();
        let code = manager.replace_with_clock(|| now).unwrap();
        let pending = confirm(&manager, &code, now);
        assert!(manager.reserve_at(now).is_err());
        let approved = pending.approve_at(now).unwrap();
        assert_eq!(approved.binding(), &binding());
        assert_eq!(approved.role(), Role::Responder);
        assert!(manager.reserve_at(now).is_err());
        manager.cancel();
        // Approved evidence describes a past local decision; cancel is not a
        // retroactive revocation mechanism for an external trust database.
        assert_eq!(approved.binding(), &binding());
    }

    #[test]
    fn dropped_and_invalid_attempts_are_charged_and_last_attempt_can_finish() {
        let manager = manager();
        let now = Instant::now();
        let code = manager.replace_with_clock(|| now).unwrap();
        drop(manager.reserve_at(now).unwrap());
        let mut invalid = binding();
        invalid.handshake_hash = [0; 32];
        assert!(
            manager
                .reserve_at(now)
                .unwrap()
                .start_with_clock(invalid, || now)
                .is_err()
        );
        let pending = confirm(&manager, &code, now);
        assert!(pending.approve_at(now).is_ok());
        assert!(manager.reserve_at(now).is_err());
    }

    #[test]
    fn wrong_code_confirmation_consumes_attempt_without_authenticating() {
        let manager = manager();
        let now = Instant::now();
        let code = manager.replace_with_clock(|| now).unwrap();
        let mut wrong = Zeroizing::new(*code.as_bytes());
        wrong[0] = if wrong[0] == b'9' { b'0' } else { wrong[0] + 1 };
        for _ in 0..INVITATION_ATTEMPTS {
            let client = Pairing::start(Role::Initiator, wrong.as_slice(), binding()).unwrap();
            let server = manager
                .reserve_at(now)
                .unwrap()
                .start_with_clock(binding(), || now)
                .unwrap();
            let client_message = client.message().to_vec();
            let server_message = server.message_at(now).unwrap().to_vec();
            let client = client.process(&server_message).unwrap();
            let server = server.process_with_clock(&client_message, || now).unwrap();
            assert!(server.verify_with_clock(client.tag(), || now).is_err());
        }
        assert!(manager.reserve_at(now).is_err());
        assert_eq!(lock(&manager.budget).unwrap().reservations.len(), 3);
    }

    #[test]
    fn cancellation_and_replacement_revoke_but_keep_busy_until_drop() {
        let manager = manager();
        let now = Instant::now();
        manager.replace_with_clock(|| now).unwrap();
        let old = manager.reserve_at(now).unwrap();
        manager.cancel();
        manager.replace_with_clock(|| now).unwrap();
        assert!(manager.reserve_at(now).is_err());
        assert!(old.start_with_clock(binding(), || now).is_err());
        let next = manager.reserve_at(now).unwrap();
        assert!(next.start_with_clock(binding(), || now).is_ok());
        assert_eq!(lock(&manager.budget).unwrap().reservations.len(), 2);
    }

    #[test]
    fn cancellation_is_checked_at_message_process_tag_verify_and_approve() {
        for phase in 0..5 {
            let manager = manager();
            let now = Instant::now();
            let code = manager.replace_with_clock(|| now).unwrap();
            let client = Pairing::start(Role::Initiator, code.as_bytes(), binding()).unwrap();
            let server = manager
                .reserve_at(now)
                .unwrap()
                .start_with_clock(binding(), || now)
                .unwrap();
            if phase == 0 {
                manager.cancel();
                assert!(server.message_at(now).is_err());
                continue;
            }
            let client_message = client.message().to_vec();
            let server_message = server.message_at(now).unwrap().to_vec();
            if phase == 1 {
                manager.cancel();
                assert!(server.process_with_clock(&client_message, || now).is_err());
                continue;
            }
            let client = client.process(&server_message).unwrap();
            let server = server.process_with_clock(&client_message, || now).unwrap();
            if phase == 2 {
                manager.cancel();
                assert!(server.tag_at(now).is_err());
            } else if phase == 3 {
                manager.cancel();
                assert!(server.verify_with_clock(client.tag(), || now).is_err());
            } else {
                let pending = server.verify_with_clock(client.tag(), || now).unwrap();
                manager.cancel();
                assert!(pending.approve_at(now).is_err());
            }
        }
    }

    #[test]
    fn expiry_is_total_and_cannot_be_extended_by_starting_late() {
        let manager = manager();
        let now = Instant::now();
        let code = manager.replace_with_clock(|| now).unwrap();
        let pending = confirm(&manager, &code, now + Duration::from_secs(119));
        assert!(pending.approve_at(now + INVITATION_LIFETIME).is_err());
        assert!(manager.reserve_at(now + INVITATION_LIFETIME).is_err());
        manager.replace_with_clock(|| now).unwrap();
        let permit = manager.reserve_at(now).unwrap();
        assert!(
            permit
                .start_with_clock(binding(), || now + INVITATION_LIFETIME)
                .is_err()
        );
    }

    #[test]
    fn manager_drop_and_other_manager_cannot_resurrect_ticket() {
        let now = Instant::now();
        let first = manager();
        let code = first.replace_with_clock(|| now).unwrap();
        let pending = confirm(&first, &code, now);
        drop(first);
        let second = manager();
        second.replace_with_clock(|| now).unwrap();
        assert!(pending.approve_at(now).is_err());
        assert!(second.reserve_at(now).is_ok());
    }

    #[test]
    fn shared_rolling_budget_survives_cancel_replace_and_manager_drop() {
        let budget = Arc::new(Mutex::new(Budget::default()));
        let a = InvitationManager::with_budget(budget.clone());
        let b = InvitationManager::with_budget(budget.clone());
        let now = Instant::now();
        for manager in [&a, &a, &a, &b, &b] {
            manager.replace_with_clock(|| now).unwrap();
            drop(manager.reserve_at(now).unwrap());
            manager.cancel();
        }
        b.replace_with_clock(|| now).unwrap();
        assert!(b.reserve_at(now + Duration::from_secs(59)).is_err());
        drop(a);
        let c = InvitationManager::with_budget(budget);
        c.replace_with_clock(|| now).unwrap();
        assert!(c.reserve_at(now + Duration::from_secs(59)).is_err());
        assert!(c.reserve_at(now + GLOBAL_WINDOW).is_ok());
    }

    #[test]
    fn shared_busy_slot_survives_cancel_replace_and_owner_drop_until_ticket_drop() {
        let budget = Arc::new(Mutex::new(Budget::default()));
        let a = InvitationManager::with_budget(budget.clone());
        let b = InvitationManager::with_budget(budget.clone());
        let now = Instant::now();
        a.replace_with_clock(|| now).unwrap();
        b.replace_with_clock(|| now).unwrap();
        let old = a.reserve_at(now).unwrap();
        a.cancel();
        a.replace_with_clock(|| now).unwrap();
        assert!(b.reserve_at(now).is_err());
        drop(a);
        assert!(b.reserve_at(now).is_err());
        drop(old);
        assert!(b.reserve_at(now).is_ok());
        assert_eq!(lock(&budget).unwrap().reservations.len(), 2);
    }

    #[test]
    fn admission_and_approval_sample_decision_time_inside_required_locks() {
        let manager = manager();
        let now = Instant::now();
        let code = manager.replace_with_clock(|| now).unwrap();
        let permit = manager
            .reserve_with_clock(|| {
                assert!(matches!(
                    manager.state.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                assert!(matches!(
                    manager.budget.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                now
            })
            .unwrap();
        drop(permit);
        let pending = confirm(&manager, &code, now);
        assert!(
            pending
                .approve_with_clock(|| {
                    assert!(matches!(
                        manager.state.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ));
                    now + INVITATION_LIFETIME
                })
                .is_err()
        );
    }

    #[test]
    fn expiry_during_crypto_is_rechecked_before_returning_next_state() {
        let manager = manager();
        let now = Instant::now();
        manager.replace_with_clock(|| now).unwrap();
        let permit = manager.reserve_at(now).unwrap();
        let checks = std::cell::Cell::new(0);
        assert!(
            permit
                .start_with_clock(binding(), || {
                    let count = checks.get();
                    checks.set(count + 1);
                    if count == 0 {
                        now
                    } else {
                        now + INVITATION_LIFETIME
                    }
                })
                .is_err()
        );
        assert_eq!(checks.get(), 2);
        assert!(lock(&manager.budget).unwrap().busy.is_none());
        assert_eq!(lock(&manager.budget).unwrap().reservations.len(), 1);
    }

    #[test]
    fn expiry_and_replacement_after_verification_prevent_approval() {
        let manager = manager();
        let now = Instant::now();
        let code = manager.replace_with_clock(|| now).unwrap();
        let pending = confirm(&manager, &code, now);
        manager.replace_with_clock(|| now).unwrap();
        assert!(pending.approve_at(now).is_err());
        assert!(manager.reserve_at(now).is_ok());
    }

    #[test]
    fn random_sampling_is_uniform_by_rejection_and_bounded_on_failure() {
        let code = sample_code(|buffer| {
            buffer.fill(255);
            buffer[..10].copy_from_slice(&[0, 9, 10, 249, 250, 255, 42, 11, 12, 13]);
            Ok(())
        })
        .unwrap();
        assert!(code.as_slice() == b"09092123");
        assert!(
            sample_code(|buffer| {
                buffer.fill(255);
                Ok(())
            })
            .is_err()
        );
        let code = random_code().unwrap();
        assert!(code.len() == CODE_BYTES && code.iter().all(u8::is_ascii_digit));
    }

    #[test]
    fn tickets_are_send_and_manager_is_shared_without_native_context_send() {
        fn send<T: Send>() {}
        fn shared<T: Send + Sync>() {}
        send::<AttemptPermit>();
        send::<PendingApproval>();
        shared::<InvitationManager>();
    }

    #[test]
    fn approved_transport_hold_retains_global_slot_and_remains_revocable() {
        let manager = manager();
        let now = Instant::now();
        let code = manager.replace_with_clock(|| now).unwrap();
        let pending = confirm(&manager, &code, now);
        let (approved, hold) = pending.approve_held_with_clock(|| now).unwrap();
        let other = InvitationManager::with_budget(manager.budget.clone());
        other.replace_with_clock(|| now).unwrap();
        assert!(hold.check_with_clock(|| now).is_ok());
        assert!(other.reserve_at(now).is_err());
        manager.cancel();
        assert!(hold.check_with_clock(|| now).is_err());
        assert!(other.reserve_at(now).is_err());
        drop(hold);
        assert!(other.reserve_at(now).is_ok());
        assert_eq!(approved.binding(), &binding());
    }

    #[test]
    fn approved_transport_hold_expires_at_original_invitation_deadline() {
        let manager = manager();
        let now = Instant::now();
        let code = manager.replace_with_clock(|| now).unwrap();
        let pending = confirm(&manager, &code, now + Duration::from_secs(119));
        let (_, hold) = pending
            .approve_held_with_clock(|| now + Duration::from_secs(119))
            .unwrap();
        assert!(hold.check_with_clock(|| now + INVITATION_LIFETIME).is_err());
        drop(manager);
        assert!(hold.check_with_clock(|| now).is_err());
    }

    #[test]
    fn attempt_observer_cannot_release_or_revive_a_ticket() {
        let manager = manager();
        let now = Instant::now();
        manager.replace_with_clock(|| now).unwrap();
        let permit = manager.reserve_at(now).unwrap();
        let observer = permit.attempt_check();
        assert!(observer.check().is_ok());
        drop(observer);
        assert!(manager.reserve_at(now).is_err());
        let observer = permit.attempt_check();
        manager.cancel();
        manager.replace_with_clock(|| now).unwrap();
        assert!(observer.check().is_err());
        assert!(manager.reserve_at(now).is_err());
        drop(permit);
        let new_permit = manager.reserve_at(now).unwrap();
        assert!(observer.check().is_err());
        assert!(new_permit.attempt_check().check().is_ok());
        assert_eq!(lock(&manager.budget).unwrap().reservations.len(), 2);
    }
}
