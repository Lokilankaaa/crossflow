//! Pure authority for one authenticated two-device connection.
//!
//! Exactly one elected coordinator owns this state machine. Assign A/B by the
//! ordering of the authenticated public identities; either side may own input.
//! The driver authenticates every event's side and scopes events to this exact
//! connection. Discovery, UI routing status and input packets cannot grant
//! authority. A replacement connection always constructs a fresh, disabled
//! instance rather than restoring this state.
//!
//! Revoke is a barrier broadcast to BOTH sides: stop sending, undo local input
//! suppression, discard queued old input, and release owned injected input.
//! Each side sends Released only after its own adapter release has completed.
//! No timer or missing acknowledgement may synthesize Released. Adapter return
//! is the existing release evidence, not proof of OS/display consumption.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    A,
    B,
}

impl Side {
    fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Explicit user enable, never inferred from network connection or focus.
    Enable {
        source: Side,
    },
    /// A genuine local physical action, never a forwarded/injected event.
    RequestControl {
        source: Side,
        observed_epoch: u64,
    },
    Released {
        side: Side,
        barrier_epoch: u64,
    },
    Pause,
    Disconnect,
    /// Applies to the connection's current authority, not a stale timer callback.
    LeaseExpired,
    /// Any uncertain owned-input cleanup ends this connection; never retry input.
    ReleaseFailed {
        side: Side,
    },
    /// Revision acceptance is deferred until both sides have released input.
    ChangeLayout {
        revision: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevokeReason {
    Enable,
    Takeover,
    Pause,
    LayoutChange,
    Disconnect,
    LeaseExpired,
    ReleaseFailed { side: Side },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Broadcast; this is NOT an acknowledgement that cleanup has completed.
    Revoke {
        barrier_epoch: u64,
        reason: RevokeReason,
    },
    /// Only this effect creates input authority. Both release acknowledgements
    /// have completed; fresh per-target input leases/Arm are still required.
    Grant {
        source: Side,
        owner_epoch: u64,
        layout_revision: u64,
    },
    /// Explicit pause/layout-change cleanup has completed on both devices.
    Paused {
        epoch: u64,
    },
    LayoutAccepted {
        revision: u64,
    },
    /// Close the connection and finish local cleanup. This does not assert that
    /// either side's releases succeeded, and must never trigger automatic enable.
    Ended {
        reason: RevokeReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlError {
    InvalidLayoutRevision,
    Disconnected,
    AlreadyEnabled,
    SharingDisabled,
    BarrierInProgress,
    NoBarrier,
    StaleEpoch {
        expected: u64,
        received: u64,
    },
    /// Terminal: caller must close transport and finish local cleanup.
    EpochExhausted,
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pair control rejected event: {self:?}")
    }
}
impl std::error::Error for ControlError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    Acquire,
    Pause,
    Layout(u64),
}

#[derive(Clone, Debug)]
struct Barrier {
    epoch: u64,
    // The last observed epoch when the barrier began. Requests already in
    // flight for that epoch may join this SAME barrier, never create a grant.
    request_epoch: u64,
    released: [bool; 2],
    candidates: [bool; 2],
    purpose: Purpose,
}

#[derive(Debug)]
pub struct PairControl {
    connected: bool,
    enabled: bool,
    epoch: u64,
    layout_revision: u64,
    owner: Option<Side>,
    barrier: Option<Barrier>,
}

impl PairControl {
    pub fn new(layout_revision: u64) -> Result<Self, ControlError> {
        if layout_revision == 0 {
            return Err(ControlError::InvalidLayoutRevision);
        }
        Ok(Self {
            connected: true,
            enabled: false,
            epoch: 0,
            layout_revision,
            owner: None,
            barrier: None,
        })
    }

    pub fn connected(&self) -> bool {
        self.connected
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn owner(&self) -> Option<Side> {
        self.owner
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn layout_revision(&self) -> u64 {
        self.layout_revision
    }
    pub fn pending_barrier(&self) -> Option<u64> {
        self.barrier.as_ref().map(|b| b.epoch)
    }

    /// Additional gate before existing receiver lease/sequence/Arm validation.
    /// A grant alone never authorizes stale input, a wrong target layout, or the
    /// other physical input source. During every release barrier both fail.
    pub fn authorize_input(&self, source: Side, owner_epoch: u64, layout_revision: u64) -> bool {
        self.connected
            && self.enabled
            && self.barrier.is_none()
            && self.owner == Some(source)
            && self.epoch == owner_epoch
            && self.layout_revision == layout_revision
    }

    pub fn handle(&mut self, event: Event) -> Result<Vec<Effect>, ControlError> {
        if !self.connected {
            return if event == Event::Disconnect {
                Ok(vec![])
            } else {
                Err(ControlError::Disconnected)
            };
        }
        match event {
            Event::Enable { source } => {
                if self.enabled {
                    return Err(ControlError::AlreadyEnabled);
                }
                if self.barrier.is_some() {
                    return Err(ControlError::BarrierInProgress);
                }
                let effect =
                    self.begin_barrier(Purpose::Acquire, Some(source), RevokeReason::Enable)?;
                self.enabled = true;
                Ok(vec![effect])
            }
            Event::RequestControl {
                source,
                observed_epoch,
            } => {
                if !self.enabled {
                    return Err(ControlError::SharingDisabled);
                }
                if let Some(barrier) = &mut self.barrier {
                    if observed_epoch != barrier.request_epoch {
                        return Err(ControlError::StaleEpoch {
                            expected: barrier.request_epoch,
                            received: observed_epoch,
                        });
                    }
                    // A is the deterministic winner among requests received
                    // before the second Released. This is a finite, explicit
                    // arbitration window, not a claim to detect wall-clock
                    // simultaneity over a network. A later request needs the
                    // newly granted epoch and constitutes a new takeover.
                    barrier.candidates[source.index()] = true;
                    Ok(vec![])
                } else {
                    if observed_epoch != self.epoch {
                        return Err(ControlError::StaleEpoch {
                            expected: self.epoch,
                            received: observed_epoch,
                        });
                    }
                    if self.owner == Some(source) {
                        return Ok(vec![]);
                    }
                    Ok(vec![self.begin_barrier(
                        Purpose::Acquire,
                        Some(source),
                        RevokeReason::Takeover,
                    )?])
                }
            }
            Event::Released {
                side,
                barrier_epoch,
            } => {
                let barrier = self.barrier.as_mut().ok_or(ControlError::NoBarrier)?;
                if barrier_epoch != barrier.epoch {
                    return Err(ControlError::StaleEpoch {
                        expected: barrier.epoch,
                        received: barrier_epoch,
                    });
                }
                barrier.released[side.index()] = true;
                if !barrier.released.iter().all(|released| *released) {
                    return Ok(vec![]);
                }
                let barrier = self.barrier.take().expect("checked barrier");
                match barrier.purpose {
                    Purpose::Acquire => {
                        let source = if barrier.candidates[0] {
                            Side::A
                        } else {
                            Side::B
                        };
                        self.owner = Some(source);
                        Ok(vec![Effect::Grant {
                            source,
                            owner_epoch: self.epoch,
                            layout_revision: self.layout_revision,
                        }])
                    }
                    Purpose::Pause => Ok(vec![Effect::Paused { epoch: self.epoch }]),
                    Purpose::Layout(revision) => {
                        self.layout_revision = revision;
                        Ok(vec![
                            Effect::LayoutAccepted { revision },
                            Effect::Paused { epoch: self.epoch },
                        ])
                    }
                }
            }
            Event::Pause => {
                // A repeated pause cannot perpetually invalidate pending
                // releases or accidentally cancel a pending layout update.
                if !self.enabled && self.owner.is_none() {
                    return Ok(vec![]);
                }
                let effect = self.begin_barrier(Purpose::Pause, None, RevokeReason::Pause)?;
                self.enabled = false;
                Ok(vec![effect])
            }
            Event::ChangeLayout { revision } => {
                let newest = match self.barrier.as_ref().map(|b| b.purpose) {
                    Some(Purpose::Layout(pending)) => pending,
                    _ => self.layout_revision,
                };
                if revision <= newest {
                    return Err(ControlError::InvalidLayoutRevision);
                }
                // A replacement proposal is explicit: it gets a fresh barrier
                // so acknowledgements for an older proposal cannot accept it.
                let effect = self.begin_barrier(
                    Purpose::Layout(revision),
                    None,
                    RevokeReason::LayoutChange,
                )?;
                self.enabled = false;
                Ok(vec![effect])
            }
            Event::Disconnect => self.end(RevokeReason::Disconnect),
            Event::LeaseExpired => self.end(RevokeReason::LeaseExpired),
            Event::ReleaseFailed { side } => self.end(RevokeReason::ReleaseFailed { side }),
        }
    }

    fn next_epoch(&mut self) -> Result<u64, ControlError> {
        let Some(epoch) = self.epoch.checked_add(1) else {
            self.connected = false;
            self.enabled = false;
            self.owner = None;
            self.barrier = None;
            return Err(ControlError::EpochExhausted);
        };
        self.epoch = epoch;
        Ok(epoch)
    }

    fn begin_barrier(
        &mut self,
        purpose: Purpose,
        candidate: Option<Side>,
        reason: RevokeReason,
    ) -> Result<Effect, ControlError> {
        let request_epoch = self.epoch;
        let epoch = self.next_epoch()?;
        self.owner = None;
        let mut candidates = [false; 2];
        if let Some(side) = candidate {
            candidates[side.index()] = true;
        }
        self.barrier = Some(Barrier {
            epoch,
            request_epoch,
            released: [false; 2],
            candidates,
            purpose,
        });
        Ok(Effect::Revoke {
            barrier_epoch: epoch,
            reason,
        })
    }

    fn end(&mut self, reason: RevokeReason) -> Result<Vec<Effect>, ControlError> {
        let epoch = self.next_epoch()?;
        self.connected = false;
        self.enabled = false;
        self.owner = None;
        self.barrier = None;
        Ok(vec![
            Effect::Revoke {
                barrier_epoch: epoch,
                reason,
            },
            Effect::Ended { reason },
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair() -> PairControl {
        PairControl::new(7).unwrap()
    }
    fn release(pair: &mut PairControl, side: Side) -> Vec<Effect> {
        pair.handle(Event::Released {
            side,
            barrier_epoch: pair.pending_barrier().unwrap(),
        })
        .unwrap()
    }
    fn enable(pair: &mut PairControl, source: Side) -> u64 {
        let effects = pair.handle(Event::Enable { source }).unwrap();
        assert!(matches!(
            effects.as_slice(),
            [Effect::Revoke {
                reason: RevokeReason::Enable,
                ..
            }]
        ));
        assert!(release(pair, Side::A).is_empty());
        assert_eq!(
            release(pair, Side::B),
            vec![Effect::Grant {
                source,
                owner_epoch: pair.epoch(),
                layout_revision: pair.layout_revision()
            }]
        );
        pair.epoch()
    }
    fn neither(pair: &PairControl) {
        for side in [Side::A, Side::B] {
            assert!(!pair.authorize_input(side, pair.epoch(), pair.layout_revision()));
        }
    }

    #[test]
    fn fresh_connection_is_disabled_and_requires_both_releases_before_first_grant() {
        for source in [Side::A, Side::B] {
            let mut p = pair();
            assert!(!p.enabled());
            assert_eq!(p.owner(), None);
            neither(&p);
            assert_eq!(
                p.handle(Event::RequestControl {
                    source,
                    observed_epoch: 0
                }),
                Err(ControlError::SharingDisabled)
            );
            p.handle(Event::Enable { source }).unwrap();
            assert!(p.enabled());
            neither(&p);
            assert!(release(&mut p, Side::B).is_empty());
            neither(&p);
            assert!(release(&mut p, Side::B).is_empty());
            neither(&p);
            let epoch = p.epoch();
            assert_eq!(
                release(&mut p, Side::A),
                vec![Effect::Grant {
                    source,
                    owner_epoch: epoch,
                    layout_revision: 7
                }]
            );
            assert!(p.authorize_input(source, epoch, 7));
        }
    }
    #[test]
    fn inputs_require_exact_source_epoch_and_layout_revision() {
        let mut p = pair();
        let epoch = enable(&mut p, Side::B);
        assert!(p.authorize_input(Side::B, epoch, 7));
        assert!(!p.authorize_input(Side::A, epoch, 7));
        assert!(!p.authorize_input(Side::B, epoch - 1, 7));
        assert!(!p.authorize_input(Side::B, epoch + 1, 7));
        assert!(!p.authorize_input(Side::B, epoch, 8));
    }
    #[test]
    fn either_side_can_take_control_and_old_input_stops_at_revoke() {
        for first in [Side::A, Side::B] {
            let second = if first == Side::A { Side::B } else { Side::A };
            let mut p = pair();
            let old = enable(&mut p, first);
            assert!(
                p.handle(Event::RequestControl {
                    source: first,
                    observed_epoch: old
                })
                .unwrap()
                .is_empty()
            );
            p.handle(Event::RequestControl {
                source: second,
                observed_epoch: old,
            })
            .unwrap();
            neither(&p);
            assert!(!p.authorize_input(first, old, 7));
            assert!(release(&mut p, first).is_empty());
            neither(&p);
            release(&mut p, second);
            assert!(p.authorize_input(second, p.epoch(), 7));
            assert_eq!(
                p.handle(Event::RequestControl {
                    source: first,
                    observed_epoch: old
                }),
                Err(ControlError::StaleEpoch {
                    expected: p.epoch(),
                    received: old
                })
            );
        }
    }
    #[test]
    fn same_barrier_contenders_choose_a_independently_of_request_and_release_order() {
        for first in [Side::A, Side::B] {
            for first_release in [Side::A, Side::B] {
                let other = if first == Side::A { Side::B } else { Side::A };
                let second_release = if first_release == Side::A {
                    Side::B
                } else {
                    Side::A
                };
                let mut p = pair();
                p.handle(Event::Enable { source: first }).unwrap();
                release(&mut p, first_release);
                p.handle(Event::RequestControl {
                    source: other,
                    observed_epoch: 0,
                })
                .unwrap();
                release(&mut p, second_release);
                assert_eq!(p.owner(), Some(Side::A));
                assert!(p.authorize_input(Side::A, p.epoch(), 7));
            }
        }
    }
    #[test]
    fn no_wait_or_single_release_can_grant_and_stale_release_cannot_complete_replacement() {
        let mut p = pair();
        let old = enable(&mut p, Side::A);
        p.handle(Event::RequestControl {
            source: Side::B,
            observed_epoch: old,
        })
        .unwrap();
        let stale = p.pending_barrier().unwrap();
        release(&mut p, Side::A);
        p.handle(Event::Pause).unwrap();
        let current = p.pending_barrier().unwrap();
        neither(&p);
        assert_eq!(
            p.handle(Event::Released {
                side: Side::B,
                barrier_epoch: stale
            }),
            Err(ControlError::StaleEpoch {
                expected: current,
                received: stale
            })
        );
        release(&mut p, Side::B);
        neither(&p);
        assert_eq!(
            release(&mut p, Side::A),
            vec![Effect::Paused { epoch: current }]
        );
        neither(&p);
    }
    #[test]
    fn pause_cancels_pending_takeover_and_requires_explicit_enable_after_cleanup() {
        let mut p = pair();
        let old = enable(&mut p, Side::A);
        p.handle(Event::RequestControl {
            source: Side::B,
            observed_epoch: old,
        })
        .unwrap();
        p.handle(Event::Pause).unwrap();
        assert!(!p.enabled());
        neither(&p);
        assert_eq!(
            p.handle(Event::Enable { source: Side::B }),
            Err(ControlError::BarrierInProgress)
        );
        assert!(p.handle(Event::Pause).unwrap().is_empty());
        release(&mut p, Side::A);
        release(&mut p, Side::B);
        assert_eq!(p.owner(), None);
        enable(&mut p, Side::B);
        assert_eq!(p.owner(), Some(Side::B));
    }
    #[test]
    fn layout_is_accepted_only_after_both_releases_and_never_reenables() {
        let mut p = pair();
        let old = enable(&mut p, Side::B);
        assert_eq!(
            p.handle(Event::ChangeLayout { revision: 8 }).unwrap(),
            vec![Effect::Revoke {
                barrier_epoch: old + 1,
                reason: RevokeReason::LayoutChange
            }]
        );
        assert!(!p.enabled());
        assert_eq!(p.layout_revision(), 7);
        neither(&p);
        release(&mut p, Side::B);
        assert_eq!(p.layout_revision(), 7);
        assert_eq!(
            release(&mut p, Side::A),
            vec![
                Effect::LayoutAccepted { revision: 8 },
                Effect::Paused { epoch: old + 1 }
            ]
        );
        assert_eq!(p.layout_revision(), 8);
        neither(&p);
        let new = enable(&mut p, Side::A);
        assert!(p.authorize_input(Side::A, new, 8));
        assert!(!p.authorize_input(Side::A, new, 7));
    }
    #[test]
    fn replacement_layout_ignores_old_acknowledgements_and_pause_preserves_pending_proposal() {
        let mut p = pair();
        p.handle(Event::ChangeLayout { revision: 8 }).unwrap();
        let old = p.epoch();
        release(&mut p, Side::A);
        p.handle(Event::ChangeLayout { revision: 9 }).unwrap();
        let new = p.epoch();
        assert_eq!(
            p.handle(Event::ChangeLayout { revision: 8 }),
            Err(ControlError::InvalidLayoutRevision)
        );
        assert_eq!(
            p.handle(Event::ChangeLayout { revision: 9 }),
            Err(ControlError::InvalidLayoutRevision)
        );
        assert!(p.handle(Event::Pause).unwrap().is_empty());
        assert_eq!(
            p.handle(Event::Released {
                side: Side::B,
                barrier_epoch: old
            }),
            Err(ControlError::StaleEpoch {
                expected: new,
                received: old
            })
        );
        release(&mut p, Side::B);
        assert_eq!(p.layout_revision(), 7);
        release(&mut p, Side::A);
        assert_eq!(p.layout_revision(), 9);
    }
    #[test]
    fn disconnect_lease_and_release_failure_are_terminal_without_synthetic_grant_or_cleanup_ack() {
        for event in [
            Event::Disconnect,
            Event::LeaseExpired,
            Event::ReleaseFailed { side: Side::A },
            Event::ReleaseFailed { side: Side::B },
        ] {
            for during_barrier in [false, true] {
                let mut p = pair();
                let old = enable(&mut p, Side::B);
                if during_barrier {
                    p.handle(Event::RequestControl {
                        source: Side::A,
                        observed_epoch: old,
                    })
                    .unwrap();
                    release(&mut p, Side::A);
                }
                let effects = p.handle(event).unwrap();
                assert!(matches!(
                    effects.as_slice(),
                    [Effect::Revoke { .. }, Effect::Ended { .. }]
                ));
                assert!(!p.connected());
                assert!(!p.enabled());
                neither(&p);
                assert_eq!(p.pending_barrier(), None);
                assert_eq!(
                    p.handle(Event::Enable { source: Side::A }),
                    Err(ControlError::Disconnected)
                );
                assert_eq!(
                    p.handle(Event::Released {
                        side: Side::B,
                        barrier_epoch: old
                    }),
                    Err(ControlError::Disconnected)
                );
                assert!(p.handle(Event::Disconnect).unwrap().is_empty());
                let fresh = pair();
                assert!(!fresh.enabled());
                assert_eq!(fresh.owner(), None);
                neither(&fresh);
            }
        }
    }
    #[test]
    fn invalid_revision_and_overflow_cannot_create_authority() {
        assert!(matches!(
            PairControl::new(0),
            Err(ControlError::InvalidLayoutRevision)
        ));
        let mut p = pair();
        for revision in [0, 7, 6] {
            assert_eq!(
                p.handle(Event::ChangeLayout { revision }),
                Err(ControlError::InvalidLayoutRevision)
            );
        }
        p.epoch = u64::MAX;
        assert_eq!(
            p.handle(Event::Enable { source: Side::A }),
            Err(ControlError::EpochExhausted)
        );
        assert!(!p.connected());
        neither(&p);
    }
}
