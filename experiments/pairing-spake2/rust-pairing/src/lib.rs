//! Experimental BoringSSL SPAKE2 adapter; not a production pairing protocol.
//!
//! FFI signatures are pinned to revision
//! 3769c110eab4ddbc0c159fadb4f1d0d98b2cf0cc, see ../source-lock.json.
//! The caller supplies a fresh Noise XX transcript and actual transcript public
//! keys, an eight-digit code, deadlines, cancellation, and online-guess limits.
//! Send both confirmation tags before receiving/verifying the peer tag. Never
//! persist trust on cancellation, timeout, or failed confirmation. This module
//! does not store trust and never exports the intermediate PAKE key.

use anyhow::{Result, ensure};
use std::ffi::{c_int, c_uint, c_void};
use std::ptr::NonNull;
use zeroize::Zeroizing;

pub const MESSAGE_BYTES: usize = 32;
pub const CONFIRMATION_BYTES: usize = 32;
pub const CODE_BYTES: usize = 8;
const KEY_BYTES: usize = 64;
const DOMAIN: &[u8] = b"Crossflow/experimental-pairing/v1/Noise_XX_25519_ChaChaPoly_BLAKE2s/SPAKE2";

/// Ordered public channel binding, derived from one completed Noise XX session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub handshake_hash: [u8; 32],
    pub initiator_static: [u8; 32],
    pub responder_static: [u8; 32],
}

impl Binding {
    fn validate(&self) -> Result<()> {
        ensure!(self.handshake_hash != [0; 32], "Empty handshake binding");
        ensure!(
            self.initiator_static != [0; 32] && self.responder_static != [0; 32],
            "Empty public identity binding"
        );
        ensure!(
            self.initiator_static != self.responder_static,
            "Pairing identities must differ"
        );
        Ok(())
    }

    fn append(&self, output: &mut Vec<u8>) {
        output.extend_from_slice(&self.handshake_hash);
        output.extend_from_slice(&self.initiator_static);
        output.extend_from_slice(&self.responder_static);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Responder,
}

impl Role {
    fn peer(self) -> Self {
        match self {
            Self::Initiator => Self::Responder,
            Self::Responder => Self::Initiator,
        }
    }

    fn byte(self) -> u8 {
        match self {
            Self::Initiator => 0,
            Self::Responder => 1,
        }
    }
}

// Opaque upstream C allocations; layouts are never dereferenced by Rust.
#[repr(C)]
struct SpakeContext {
    _private: [u8; 0],
}
#[repr(C)]
struct Digest {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn SPAKE2_CTX_new(
        role: c_int,
        own: *const u8,
        own_len: usize,
        peer: *const u8,
        peer_len: usize,
    ) -> *mut SpakeContext;
    fn SPAKE2_CTX_free(ctx: *mut SpakeContext);
    fn SPAKE2_generate_msg(
        ctx: *mut SpakeContext,
        out: *mut u8,
        out_len: *mut usize,
        max_len: usize,
        password: *const u8,
        password_len: usize,
    ) -> c_int;
    fn SPAKE2_process_msg(
        ctx: *mut SpakeContext,
        key: *mut u8,
        key_len: *mut usize,
        max_key_len: usize,
        message: *const u8,
        message_len: usize,
    ) -> c_int;
    fn EVP_sha256() -> *const Digest;
    fn HMAC(
        digest: *const Digest,
        key: *const c_void,
        key_len: usize,
        data: *const u8,
        data_len: usize,
        out: *mut u8,
        out_len: *mut c_uint,
    ) -> *mut u8;
    fn CRYPTO_memcmp(left: *const c_void, right: *const c_void, len: usize) -> c_int;
}

struct Context(NonNull<SpakeContext>);

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: Context exclusively owns the allocation returned by new.
        // The pinned implementation frees via OPENSSL_free, which cleanses
        // allocations in the default allocator used by this build experiment.
        unsafe { SPAKE2_CTX_free(self.0.as_ptr()) };
    }
}

/// One SPAKE2 attempt. Dropping it cancels and cleans up native secret state.
/// `process` consumes this value even on failure; there is no retry or Clone.
pub struct Pairing {
    context: Context,
    role: Role,
    binding: Binding,
    message: [u8; MESSAGE_BYTES],
}

impl Pairing {
    pub fn start(role: Role, code: &[u8], binding: Binding) -> Result<Self> {
        ensure!(
            code.len() == CODE_BYTES && code.iter().all(u8::is_ascii_digit),
            "Pairing code must contain exactly eight ASCII digits"
        );
        binding.validate()?;
        let own = identity_name(role, binding);
        let peer = identity_name(role.peer(), binding);
        // SAFETY: All slices are valid for their lengths. Upstream copies the
        // opaque names and recognizes enum values alice=0 and bob=1.
        let ptr = unsafe {
            SPAKE2_CTX_new(
                role.byte().into(),
                own.as_ptr(),
                own.len(),
                peer.as_ptr(),
                peer.len(),
            )
        };
        let context = Context(
            NonNull::new(ptr).ok_or_else(|| anyhow::anyhow!("SPAKE2 context creation failed"))?,
        );
        let password = Zeroizing::new(code.to_vec());
        let mut message = [0; MESSAGE_BYTES];
        let mut len = 0;
        // SAFETY: This fresh context has not generated a message. The output
        // allocation has the documented max size and input lives for the call.
        let ok = unsafe {
            SPAKE2_generate_msg(
                context.0.as_ptr(),
                message.as_mut_ptr(),
                &mut len,
                message.len(),
                password.as_ptr(),
                password.len(),
            )
        };
        ensure!(
            ok == 1 && len == MESSAGE_BYTES,
            "SPAKE2 message generation failed"
        );
        Ok(Self {
            context,
            role,
            binding,
            message,
        })
    }

    pub fn message(&self) -> &[u8] {
        &self.message
    }

    /// Consumes this state, so invalid messages cannot be retried on it.
    ///
    /// ```compile_fail
    /// use crossflow_pairing_spike::Pairing;
    /// fn cannot_retry(attempt: Pairing, message: &[u8]) {
    ///     let _ = attempt.process(message);
    ///     let _ = attempt.process(message);
    /// }
    /// ```
    pub fn process(self, peer_message: &[u8]) -> Result<Confirmation> {
        ensure!(
            peer_message.len() == MESSAGE_BYTES,
            "Invalid SPAKE2 message length"
        );
        ensure!(peer_message != self.message, "Reflected SPAKE2 message");
        let mut key = Zeroizing::new([0; KEY_BYTES]);
        let mut len = 0;
        // SAFETY: Owned generated context, bounded peer input, and a full-size
        // key buffer. Consuming self guarantees no second process call.
        let ok = unsafe {
            SPAKE2_process_msg(
                self.context.0.as_ptr(),
                key.as_mut_ptr(),
                &mut len,
                key.len(),
                peer_message.as_ptr(),
                peer_message.len(),
            )
        };
        ensure!(ok == 1 && len == KEY_BYTES, "SPAKE2 peer processing failed");
        let own_tag = confirmation(&key[..], self.role, self.binding)?;
        let expected_peer_tag =
            Zeroizing::new(confirmation(&key[..], self.role.peer(), self.binding)?);
        // Both key and native context are destroyed on return, including errors.
        Ok(Confirmation {
            own_tag,
            expected_peer_tag,
            role: self.role,
            binding: self.binding,
        })
    }
}

fn identity_name(role: Role, binding: Binding) -> Vec<u8> {
    let mut output = DOMAIN.to_vec();
    output.extend_from_slice(b"\0identity\0");
    output.push(role.byte());
    binding.append(&mut output);
    output
}

fn confirmation(key: &[u8], role: Role, binding: Binding) -> Result<[u8; CONFIRMATION_BYTES]> {
    let mut data = DOMAIN.to_vec();
    data.extend_from_slice(b"\0confirmation\0");
    data.push(role.byte());
    binding.append(&mut data);
    let mut tag = Zeroizing::new([0; CONFIRMATION_BYTES]);
    let mut len = 0;
    // SAFETY: SHA-256 has a 32-byte output, all inputs remain live, and the
    // output is caller-owned (never use the API's implicit static buffer).
    let result = unsafe {
        HMAC(
            EVP_sha256(),
            key.as_ptr().cast(),
            key.len(),
            data.as_ptr(),
            data.len(),
            tag.as_mut_ptr(),
            &mut len,
        )
    };
    ensure!(
        !result.is_null() && len as usize == CONFIRMATION_BYTES,
        "SPAKE2 confirmation failed"
    );
    Ok(*tag)
}

/// Contains confirmation tags only; the PAKE key has already been erased.
pub struct Confirmation {
    own_tag: [u8; CONFIRMATION_BYTES],
    expected_peer_tag: Zeroizing<[u8; CONFIRMATION_BYTES]>,
    role: Role,
    binding: Binding,
}

impl Confirmation {
    /// Send this before waiting for the other endpoint's tag.
    pub fn tag(&self) -> &[u8; CONFIRMATION_BYTES] {
        &self.own_tag
    }

    /// One verification attempt; consumes the state on success or failure.
    /// A local success does not prove that the peer received our own tag.
    ///
    /// ```compile_fail
    /// use crossflow_pairing_spike::Confirmation;
    /// fn cannot_retry(confirmation: Confirmation, tag: &[u8]) {
    ///     let _ = confirmation.verify_peer(tag);
    ///     let _ = confirmation.verify_peer(tag);
    /// }
    /// ```
    pub fn verify_peer(self, peer_tag: &[u8]) -> Result<AuthenticatedBinding> {
        ensure!(
            peer_tag.len() == CONFIRMATION_BYTES,
            "Invalid confirmation length"
        );
        // SAFETY: Both buffers contain exactly CONFIRMATION_BYTES readable bytes.
        let matches = unsafe {
            CRYPTO_memcmp(
                peer_tag.as_ptr().cast(),
                self.expected_peer_tag.as_ptr().cast(),
                CONFIRMATION_BYTES,
            )
        } == 0;
        ensure!(matches, "Peer confirmation rejected");
        Ok(AuthenticatedBinding {
            role: self.role,
            binding: self.binding,
        })
    }
}

/// Evidence of local peer confirmation only. No secret or session key is exposed.
/// Applications must still complete their own send/ack and persistence policy.
#[derive(Debug)]
pub struct AuthenticatedBinding {
    role: Role,
    binding: Binding,
}

impl AuthenticatedBinding {
    pub fn role(&self) -> Role {
        self.role
    }
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> Binding {
        Binding {
            handshake_hash: [1; 32],
            initiator_static: [2; 32],
            responder_static: [3; 32],
        }
    }

    fn exchange(
        code_b: &[u8],
        bind_b: Binding,
        role_b: Role,
    ) -> (Result<Confirmation>, Result<Confirmation>) {
        let a = Pairing::start(Role::Initiator, b"01234567", binding()).unwrap();
        let b = Pairing::start(role_b, code_b, bind_b).unwrap();
        let a_message = a.message().to_vec();
        let b_message = b.message().to_vec();
        (a.process(&b_message), b.process(&a_message))
    }

    #[test]
    fn same_code_requires_both_direction_confirmations() {
        for _ in 0..8 {
            let (a, b) = exchange(b"01234567", binding(), Role::Responder);
            let (a, b) = (a.unwrap(), b.unwrap());
            let (a_tag, b_tag) = (*a.tag(), *b.tag());
            assert!(a_tag != b_tag);
            assert_eq!(a.verify_peer(&b_tag).unwrap().binding(), &binding());
            assert_eq!(b.verify_peer(&a_tag).unwrap().role(), Role::Responder);
        }
    }

    fn assert_not_authenticated(a: Result<Confirmation>, b: Result<Confirmation>) {
        if let (Ok(a), Ok(b)) = (a, b) {
            let (a_tag, b_tag) = (*a.tag(), *b.tag());
            assert!(a.verify_peer(&b_tag).is_err());
            assert!(b.verify_peer(&a_tag).is_err());
        }
    }

    #[test]
    fn wrong_code_is_rejected_by_confirmation() {
        let (a, b) = exchange(b"76543210", binding(), Role::Responder);
        // PAKE processing alone must not be treated as authentication.
        assert!(a.is_ok() && b.is_ok());
        assert_not_authenticated(a, b);
    }

    #[test]
    fn role_and_each_binding_field_are_bound() {
        let (a, b) = exchange(b"01234567", binding(), Role::Initiator);
        assert_not_authenticated(a, b);
        for field in 0..3 {
            let mut changed = binding();
            match field {
                0 => changed.handshake_hash[0] ^= 1,
                1 => changed.initiator_static[0] ^= 1,
                _ => changed.responder_static[0] ^= 1,
            }
            let (a, b) = exchange(b"01234567", changed, Role::Responder);
            assert_not_authenticated(a, b);
        }
    }

    #[test]
    fn reflected_messages_and_tags_are_rejected() {
        let a = Pairing::start(Role::Initiator, b"01234567", binding()).unwrap();
        let reflected = a.message().to_vec();
        assert!(a.process(&reflected).is_err());
        let (a, _) = exchange(b"01234567", binding(), Role::Responder);
        let a = a.unwrap();
        let reflected = *a.tag();
        assert!(a.verify_peer(&reflected).is_err());
    }

    #[test]
    fn malformed_and_tampered_inputs_are_rejected() {
        for code in [b"1234567".as_slice(), b"123456789", b"1234567x"] {
            assert!(Pairing::start(Role::Initiator, code, binding()).is_err());
        }
        for message in [&[][..], &[0; 31][..], &[0; 33][..]] {
            let a = Pairing::start(Role::Initiator, b"01234567", binding()).unwrap();
            assert!(a.process(message).is_err());
        }
        // A 32-byte string may decode as a valid curve point. Processing such
        // input is not authentication; an invalid peer tag must still fail.
        let a = Pairing::start(Role::Initiator, b"01234567", binding()).unwrap();
        if let Ok(confirmation) = a.process(&[0; 32]) {
            assert!(confirmation.verify_peer(&[0; CONFIRMATION_BYTES]).is_err());
        }
        let (a, b) = exchange(b"01234567", binding(), Role::Responder);
        let mut tag = *b.unwrap().tag();
        tag[0] ^= 1;
        assert!(a.unwrap().verify_peer(&tag).is_err());
        let (a, _) = exchange(b"01234567", binding(), Role::Responder);
        assert!(a.unwrap().verify_peer(&[0; 31]).is_err());
    }

    #[test]
    fn empty_and_self_identity_bindings_are_rejected() {
        let mut bad = binding();
        bad.handshake_hash = [0; 32];
        assert!(Pairing::start(Role::Initiator, b"01234567", bad).is_err());
        bad = binding();
        bad.initiator_static = bad.responder_static;
        assert!(Pairing::start(Role::Initiator, b"01234567", bad).is_err());
    }

    #[test]
    fn fresh_attempts_do_not_accept_old_confirmation_tags() {
        let (old, _) = exchange(b"01234567", binding(), Role::Responder);
        let tag = *old.unwrap().tag();
        let (_, fresh) = exchange(b"01234567", binding(), Role::Responder);
        assert!(fresh.unwrap().verify_peer(&tag).is_err());
    }
}
