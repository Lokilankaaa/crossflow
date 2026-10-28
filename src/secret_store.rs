//! Short-secret protection with Windows current-user DPAPI.
//!
//! This adapter does not store files, migrate identities, or establish peer trust.
//! The caller owns persistence and must validate the decrypted record's format.
//! Current-user scope is not an application sandbox: another process running as
//! that user can use DPAPI, and Windows roaming/recovery policies still apply.
//! No prompt or machine-wide protection is permitted. Other platforms fail closed.

use anyhow::Result;
use zeroize::Zeroizing;

pub const MAX_SECRET_BYTES: usize = 4_096;
pub const MAX_PROTECTED_BYTES: usize = 65_536;

/// Decrypted bytes are erased on drop. Deliberately not Clone or Serialize.
/// The caller must also erase any copies it makes through `as_slice`.
pub struct SecretBytes(Zeroizing<Vec<u8>>);

impl SecretBytes {
    pub fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBytes([REDACTED])")
    }
}

/// Protect a nonempty secret using the current Windows user's DPAPI credentials.
/// The caller retains ownership of (and responsibility for erasing) `secret`.
pub fn protect(secret: &[u8]) -> Result<Vec<u8>> {
    platform::protect(secret)
}

/// Unprotect a blob produced by this adapter, without showing any UI.
/// Success does not replace application-level record validation.
pub fn unprotect(blob: &[u8]) -> Result<SecretBytes> {
    platform::unprotect(blob)
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn protect(_secret: &[u8]) -> Result<Vec<u8>> {
        anyhow::bail!("Windows current-user DPAPI is unsupported on this platform")
    }

    pub fn unprotect(_blob: &[u8]) -> Result<SecretBytes> {
        anyhow::bail!("Windows current-user DPAPI is unsupported on this platform")
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use anyhow::{Context, ensure};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use zeroize::Zeroize;

    // Public application-domain separation, not a password or additional secret.
    const ENTROPY: &[u8] = b"Crossflow/current-user-secret/v1";

    /// Owns only memory returned by DPAPI. This also covers early-return paths.
    struct NativeOutput(CRYPT_INTEGER_BLOB);

    impl NativeOutput {
        fn empty() -> Self {
            Self(CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: null_mut(),
            })
        }

        fn copy(&self, limit: usize) -> Result<Vec<u8>> {
            let len = self.0.cbData as usize;
            ensure!(
                len > 0 && len <= limit,
                "DPAPI output exceeds permitted size"
            );
            ensure!(!self.0.pbData.is_null(), "DPAPI returned an invalid buffer");
            // SAFETY: Successful DPAPI output owns cbData readable bytes. Length
            // and nonnull checks happen before making a slice or allocating.
            Ok(unsafe { std::slice::from_raw_parts(self.0.pbData, len) }.to_vec())
        }
    }

    impl Drop for NativeOutput {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                // SAFETY: DPAPI allocates this buffer with LocalAlloc, with its
                // actual length in cbData. Erase the entire allocation, including
                // output rejected by our size policy, before exactly one free.
                unsafe {
                    std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize).zeroize();
                    LocalFree(self.0.pbData.cast());
                }
            }
        }
    }

    fn transform(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>> {
        let input_limit = if encrypt {
            MAX_SECRET_BYTES
        } else {
            MAX_PROTECTED_BYTES
        };
        ensure!(!bytes.is_empty(), "DPAPI input must not be empty");
        ensure!(
            bytes.len() <= input_limit,
            "DPAPI input exceeds permitted size"
        );
        // DPAPI accepts a mutable byte pointer. Keep an owned, erasable copy so
        // the caller's borrowed bytes never become mutable through the FFI.
        let mut input = Zeroizing::new(bytes.to_vec());
        let input_blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_mut_ptr(),
        };
        let mut entropy = ENTROPY.to_vec();
        let entropy_blob = CRYPT_INTEGER_BLOB {
            cbData: entropy.len() as u32,
            pbData: entropy.as_mut_ptr(),
        };
        let mut output = NativeOutput::empty();
        // SAFETY: All input buffers live across this synchronous call. Output
        // is initialized. No description allocation is requested, all reserved
        // and prompt arguments are null. LOCAL_MACHINE is intentionally absent.
        let ok = unsafe {
            if encrypt {
                CryptProtectData(
                    &input_blob,
                    null(),
                    &entropy_blob,
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output.0,
                )
            } else {
                CryptUnprotectData(
                    &input_blob,
                    null_mut(),
                    &entropy_blob,
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output.0,
                )
            }
        };
        if ok == 0 {
            // Capture the OS error before any cleanup calls can change it. No
            // input, plaintext, or blob bytes are included in the error chain.
            return Err(std::io::Error::last_os_error()).context(if encrypt {
                "Current-user DPAPI protection failed"
            } else {
                "Current-user DPAPI unprotection failed"
            });
        }
        output.copy(if encrypt {
            MAX_PROTECTED_BYTES
        } else {
            MAX_SECRET_BYTES
        })
    }

    pub fn protect(secret: &[u8]) -> Result<Vec<u8>> {
        transform(secret, true)
    }

    pub fn unprotect(blob: &[u8]) -> Result<SecretBytes> {
        Ok(SecretBytes(Zeroizing::new(transform(blob, false)?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_secret_and_length() {
        let secret = SecretBytes(Zeroizing::new(vec![123; 32]));
        assert_eq!(format!("{secret:?}"), "SecretBytes([REDACTED])");
    }

    #[cfg(windows)]
    #[test]
    fn real_current_user_roundtrip_at_boundaries() {
        for len in [1, 32, MAX_SECRET_BYTES] {
            let original = Zeroizing::new(vec![0x5a; len]);
            let protected = protect(&original).expect("DPAPI protect");
            assert!(protected.len() <= MAX_PROTECTED_BYTES);
            let recovered = unprotect(&protected).expect("DPAPI unprotect");
            // Do not print either operand on assertion failure.
            assert!(
                recovered.as_slice() == original.as_slice(),
                "Secret mismatch"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn real_dpapi_rejects_corrupted_authenticator_and_truncation() {
        let mut protected = protect(&[0xa5; 32]).expect("DPAPI protect");
        let last = protected.len() - 1;
        protected[last] ^= 0x80;
        assert!(unprotect(&protected).is_err(), "Corruption was accepted");
        protected.truncate(protected.len() / 2);
        assert!(unprotect(&protected).is_err(), "Truncation was accepted");
        // These cases verify observed DPAPI rejection, not a promise that every
        // metadata bit mutation is rejected. The OS documents that limitation.
    }

    #[cfg(windows)]
    #[test]
    fn rejects_empty_and_oversized_inputs_before_dpapi() {
        assert!(protect(&[]).is_err());
        assert!(unprotect(&[]).is_err());
        assert!(protect(&vec![0; MAX_SECRET_BYTES + 1]).is_err());
        assert!(unprotect(&vec![0; MAX_PROTECTED_BYTES + 1]).is_err());
        assert!(unprotect(&[0; 16]).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn other_platforms_are_explicitly_unsupported() {
        assert!(
            protect(&[1])
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
        assert!(
            unprotect(&[1])
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
    }
}
