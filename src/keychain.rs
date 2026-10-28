//! App-owned, non-synchronizing macOS file-based Keychain records.
//!
//! This module never stores files or establishes trust. Callers must validate the
//! recovered record. Public operations use a dedicated helper process. Only that
//! helper disables its process-wide Keychain interaction, and direct operations
//! refuse to run unless interaction is disabled. The caller's process is never
//! reconfigured. There is no Data Protection Keychain or plaintext fallback.
//! Calls are synchronous, so keep them off UI threads.
//!
//! This explicitly selected backend does not require Data Protection Keychain
//! access-group provisioning. The default file-based Keychain's lock and ACL
//! policy still applies. There is no promise of device-only/WhenUnlocked data
//! protection semantics, or of access after changing the helper's signature.
//! The adapter never unlocks, creates or changes the default Keychain or its ACL.
//!
//! References are public locators, not authorization capabilities. They can only
//! select a random account in this module's fixed service and item class. The OS
//! enforces access control. Do not accept arbitrary locators from network peers.
//!
//! Apple primary references checked 2026-10-29:
//! https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains
//! https://developer.apple.com/documentation/security/seckeychainsetuserinteractionallowed(_:)
//! https://github.com/apple-oss-distributions/Security/blob/main/OSX/libsecurity_keychain/lib/SecItem.cpp

use anyhow::{Result, ensure};
use zeroize::Zeroizing;

pub const MAX_SECRET_BYTES: usize = 4096;
const PREFIX: &[u8] = b"Crossflow/file-keychain-reference/v1/";
pub const REFERENCE_BYTES: usize = PREFIX.len() + 64;

/// Public opaque locator. No service, access group or arbitrary query is accepted.
pub struct KeychainReference {
    account: [u8; 64],
}

impl KeychainReference {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == REFERENCE_BYTES && bytes.starts_with(PREFIX),
            "Invalid Keychain reference format"
        );
        let account: [u8; 64] = bytes[PREFIX.len()..].try_into()?;
        ensure!(
            account
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
                && account.iter().any(|b| *b != b'0'),
            "Invalid Keychain reference account"
        );
        Ok(Self { account })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(REFERENCE_BYTES);
        bytes.extend_from_slice(PREFIX);
        bytes.extend_from_slice(&self.account);
        bytes
    }
}

/// Add a fresh random account. Never updates or replaces an existing item.
pub fn store(secret: &[u8]) -> Result<KeychainReference> {
    validate_secret(secret)?;
    #[cfg(target_os = "macos")]
    {
        crate::keychain_process::store(secret)
    }
    #[cfg(not(target_os = "macos"))]
    {
        platform::store(secret)
    }
}

/// Recover bounded bytes. This does not validate the caller's identity format.
/// Rust-owned returned bytes erase on drop; OS/framework copies are OS-managed.
pub fn load(reference: &KeychainReference) -> Result<Zeroizing<Vec<u8>>> {
    #[cfg(target_os = "macos")]
    {
        crate::keychain_process::load(reference)
    }
    #[cfg(not(target_os = "macos"))]
    {
        platform::load(reference)
    }
}

/// Delete only the exact fixed-service account. Missing items count as success.
/// A valid reference to another Crossflow record would select that record: callers
/// must retain the reference from their own successful store for failure cleanup.
pub fn remove(reference: &KeychainReference) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        crate::keychain_process::remove(reference)
    }
    #[cfg(not(target_os = "macos"))]
    {
        platform::remove(reference)
    }
}

fn validate_secret(secret: &[u8]) -> Result<()> {
    ensure!(
        !secret.is_empty() && secret.len() <= MAX_SECRET_BYTES,
        "Keychain secret size is invalid"
    );
    Ok(())
}

/// Only the dedicated, single-purpose helper may call this process-global setup.
/// Do not call it in a UI, enrollment worker, or other general-purpose process.
#[cfg(target_os = "macos")]
pub(crate) fn worker_initialize() -> Result<()> {
    platform::initialize()
}

#[cfg(target_os = "macos")]
pub(crate) fn worker_store(secret: &[u8]) -> Result<KeychainReference> {
    validate_secret(secret)?;
    platform::store(secret)
}

#[cfg(target_os = "macos")]
pub(crate) fn worker_load(reference: &KeychainReference) -> Result<Zeroizing<Vec<u8>>> {
    platform::load(reference)
}

#[cfg(target_os = "macos")]
pub(crate) fn worker_remove(reference: &KeychainReference) -> Result<()> {
    platform::remove(reference)
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;
    pub fn store(_: &[u8]) -> Result<KeychainReference> {
        anyhow::bail!("macOS Keychain is unsupported on this platform")
    }
    pub fn load(_: &KeychainReference) -> Result<Zeroizing<Vec<u8>>> {
        anyhow::bail!("macOS Keychain is unsupported on this platform")
    }
    pub fn remove(_: &KeychainReference) -> Result<()> {
        anyhow::bail!("macOS Keychain is unsupported on this platform")
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::{
        ffi::c_void,
        ptr::{NonNull, null, null_mut},
    };

    type Cf = *const c_void;
    const SERVICE: &[u8] = b"Crossflow identity v1";
    const NOT_FOUND: i32 = -25300;
    const UTF8: u32 = 0x08000100;

    // These symbols are only passed by address to CoreFoundation; opaque types
    // avoid duplicating the callbacks' C function signatures/layout in Rust.
    #[repr(C)]
    struct Callbacks {
        _opaque: [u8; 0],
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFTypeDictionaryKeyCallBacks: Callbacks;
        static kCFTypeDictionaryValueCallBacks: Callbacks;
        static kCFTypeArrayCallBacks: Callbacks;
        static kCFBooleanTrue: Cf;
        static kCFBooleanFalse: Cf;
        static kCFAllocatorNull: Cf;
        fn CFRelease(value: Cf);
        fn CFArrayCreate(
            allocator: Cf,
            values: *const Cf,
            count: isize,
            callbacks: *const Callbacks,
        ) -> Cf;
        fn CFGetTypeID(value: Cf) -> usize;
        fn CFDataGetTypeID() -> usize;
        fn CFDataGetLength(value: Cf) -> isize;
        fn CFDataGetBytePtr(value: Cf) -> *const u8;
        fn CFDataCreateWithBytesNoCopy(
            allocator: Cf,
            bytes: *const u8,
            length: isize,
            deallocator: Cf,
        ) -> Cf;
        fn CFStringCreateWithBytes(
            allocator: Cf,
            bytes: *const u8,
            length: isize,
            encoding: u32,
            external: u8,
        ) -> Cf;
        fn CFDictionaryCreate(
            allocator: Cf,
            keys: *const Cf,
            values: *const Cf,
            count: isize,
            key_callbacks: *const Callbacks,
            value_callbacks: *const Callbacks,
        ) -> Cf;
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecClass: Cf;
        static kSecClassGenericPassword: Cf;
        static kSecAttrService: Cf;
        static kSecAttrAccount: Cf;
        static kSecAttrSynchronizable: Cf;
        static kSecUseKeychain: Cf;
        static kSecMatchSearchList: Cf;
        static kSecValueData: Cf;
        static kSecReturnData: Cf;
        static kSecMatchLimit: Cf;
        static kSecMatchLimitOne: Cf;
        fn SecItemAdd(attributes: Cf, result: *mut Cf) -> i32;
        fn SecItemCopyMatching(query: Cf, result: *mut Cf) -> i32;
        fn SecItemDelete(query: Cf) -> i32;
        fn SecRandomCopyBytes(random: Cf, count: usize, bytes: *mut u8) -> i32;
        fn SecKeychainCopyDefault(keychain: *mut Cf) -> i32;
        fn SecKeychainSetUserInteractionAllowed(allowed: u8) -> i32;
        fn SecKeychainGetUserInteractionAllowed(allowed: *mut u8) -> i32;
    }

    struct Owned(NonNull<c_void>);
    impl Owned {
        fn new(value: Cf) -> Result<Self> {
            Ok(Self(NonNull::new(value.cast_mut()).ok_or_else(|| {
                anyhow::anyhow!("Keychain returned no object")
            })?))
        }
        fn raw(&self) -> Cf {
            self.0.as_ptr().cast_const()
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: Every Owned holds one Create/Copy-rule reference, released once.
            unsafe {
                CFRelease(self.raw());
            }
        }
    }

    fn status(code: i32, operation: &str) -> Result<()> {
        ensure!(code == 0, "Keychain {operation} failed (OSStatus {code})");
        Ok(())
    }

    pub fn initialize() -> Result<()> {
        // SAFETY: Dedicated helper startup only. This alters only its own
        // process's optional Keychain interaction, never the parent's or OS ACLs.
        status(
            unsafe { SecKeychainSetUserInteractionAllowed(0) },
            "disable helper interaction",
        )?;
        ensure_interaction_disabled()
    }

    fn ensure_interaction_disabled() -> Result<()> {
        let mut allowed = 1u8;
        // SAFETY: Valid writable Boolean pointer. A failed call is not treated as
        // disabled. The helper must not run other threads which change this flag.
        status(
            unsafe { SecKeychainGetUserInteractionAllowed(&mut allowed) },
            "check helper interaction",
        )?;
        ensure!(allowed == 0, "Keychain helper interaction must be disabled");
        Ok(())
    }

    fn default_keychain() -> Result<Owned> {
        ensure_interaction_disabled()?;
        let mut value = null();
        // SAFETY: Valid initialized output pointer. CopyDefault neither changes
        // the configured default nor creates/unlocks a Keychain on our behalf.
        let code = unsafe { SecKeychainCopyDefault(&mut value) };
        let owned = if value.is_null() {
            None
        } else {
            Some(Owned::new(value)?)
        };
        status(code, "open default")?;
        owned.ok_or_else(|| anyhow::anyhow!("Default Keychain is unavailable"))
    }
    fn string(bytes: &[u8]) -> Result<Owned> {
        // SAFETY: All strings are bounded ASCII constants or validated accounts;
        // CoreFoundation copies the input during this synchronous call.
        Owned::new(unsafe {
            CFStringCreateWithBytes(null(), bytes.as_ptr(), bytes.len() as isize, UTF8, 0)
        })
    }

    fn query(reference: &KeychainReference, add: bool, extra: &[(Cf, Cf)]) -> Result<Owned> {
        let keychain = default_keychain()?;
        let values = [keychain.raw()];
        // SAFETY: Single valid retained Keychain; standard callbacks retain it.
        // An explicit search list prevents queries over other configured stores.
        let search = Owned::new(unsafe {
            CFArrayCreate(null(), values.as_ptr(), 1, &kCFTypeArrayCallBacks)
        })?;
        let service = string(SERVICE)?;
        let account = string(&reference.account)?;
        // SAFETY: Framework constant objects are valid for process lifetime.
        let mut entries = unsafe {
            vec![
                (kSecClass, kSecClassGenericPassword),
                (kSecAttrService, service.raw()),
                (kSecAttrAccount, account.raw()),
                (kSecAttrSynchronizable, kCFBooleanFalse),
                if add {
                    (kSecUseKeychain, keychain.raw())
                } else {
                    (kSecMatchSearchList, search.raw())
                },
            ]
        };
        entries.extend_from_slice(extra);
        let keys: Vec<Cf> = entries.iter().map(|(key, _)| *key).collect();
        let values: Vec<Cf> = entries.iter().map(|(_, value)| *value).collect();
        // SAFETY: Arrays live throughout the call; standard CF callbacks retain
        // all keys/values, so local string objects may safely be released after.
        Owned::new(unsafe {
            CFDictionaryCreate(
                null(),
                keys.as_ptr(),
                values.as_ptr(),
                entries.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        })
    }

    pub fn store(secret: &[u8]) -> Result<KeychainReference> {
        ensure_interaction_disabled()?;
        let mut random = [0u8; 32];
        // SAFETY: A null SecRandomRef selects kSecRandomDefault. The destination
        // has exactly count writable bytes; the account is public, not key material.
        status(
            unsafe { SecRandomCopyBytes(null(), random.len(), random.as_mut_ptr()) },
            "random account generation",
        )?;
        ensure!(
            random.iter().any(|b| *b != 0),
            "Keychain random account is invalid"
        );
        let mut account = [0; 64];
        for (index, byte) in random.iter().enumerate() {
            account[index * 2] = b"0123456789abcdef"[(byte >> 4) as usize];
            account[index * 2 + 1] = b"0123456789abcdef"[(byte & 15) as usize];
        }
        let reference = KeychainReference { account };
        // The immutable CFData borrows this zeroizing buffer. Drop order is
        // attributes, data, input; neither CF nor the caller can free input early.
        let input = Zeroizing::new(secret.to_vec());
        // SAFETY: input outlives both data and the retaining attributes dictionary.
        // kCFAllocatorNull keeps CF from freeing Rust memory. No mutation occurs.
        let data = Owned::new(unsafe {
            CFDataCreateWithBytesNoCopy(
                null(),
                input.as_ptr(),
                input.len() as isize,
                kCFAllocatorNull,
            )
        })?;
        let attributes = query(&reference, true, &unsafe { [(kSecValueData, data.raw())] })?;
        // SAFETY: Fully retained dictionary is valid during synchronous SecItemAdd;
        // no output object is requested. Duplicate account is an error, not update.
        status(unsafe { SecItemAdd(attributes.raw(), null_mut()) }, "add")?;
        Ok(reference)
    }

    pub fn load(reference: &KeychainReference) -> Result<Zeroizing<Vec<u8>>> {
        let query = query(reference, false, &unsafe {
            [
                (kSecReturnData, kCFBooleanTrue),
                (kSecMatchLimit, kSecMatchLimitOne),
            ]
        })?;
        let mut result = null();
        // SAFETY: query is retained, output pointer is writable and initialized.
        let code = unsafe { SecItemCopyMatching(query.raw(), &mut result) };
        let result = if result.is_null() {
            None
        } else {
            Some(Owned::new(result)?)
        };
        status(code, "load")?;
        let data = result.ok_or_else(|| anyhow::anyhow!("Keychain returned no data"))?;
        // SAFETY: data is a valid CF object. Check its concrete type before using
        // CFData accessors, then validate length before constructing any slice.
        ensure!(
            unsafe { CFGetTypeID(data.raw()) == CFDataGetTypeID() },
            "Keychain returned invalid data type"
        );
        let len = unsafe { CFDataGetLength(data.raw()) };
        ensure!(
            len > 0 && len <= MAX_SECRET_BYTES as isize,
            "Keychain data size is invalid"
        );
        let bytes = unsafe { CFDataGetBytePtr(data.raw()) };
        ensure!(!bytes.is_null(), "Keychain returned invalid data buffer");
        // SAFETY: Valid CFData owns len readable bytes until data is dropped.
        // Its immutable OS-owned buffer must not be mutated to attempt wiping.
        Ok(Zeroizing::new(
            unsafe { std::slice::from_raw_parts(bytes, len as usize) }.to_vec(),
        ))
    }

    pub fn remove(reference: &KeychainReference) -> Result<()> {
        let query = query(reference, false, &unsafe {
            [(kSecMatchLimit, kSecMatchLimitOne)]
        })?;
        // SAFETY: Retained dictionary scopes deletion to the exact account,
        // fixed service/class and exactly one explicit file-based Keychain.
        let code = unsafe { SecItemDelete(query.raw()) };
        if code == NOT_FOUND {
            return Ok(());
        }
        status(code, "remove")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_bytes() -> Vec<u8> {
        let mut bytes = PREFIX.to_vec();
        bytes.extend_from_slice(&[b'a'; 64]);
        bytes
    }

    #[test]
    fn reference_roundtrip_has_one_canonical_fixed_format() {
        let bytes = reference_bytes();
        assert_eq!(
            KeychainReference::from_bytes(&bytes).unwrap().to_bytes(),
            bytes
        );
        assert_eq!(bytes.len(), REFERENCE_BYTES);
    }

    #[test]
    fn reference_rejects_corruption_traversal_noncanonical_and_extra_fields() {
        let valid = reference_bytes();
        for len in [0, PREFIX.len(), valid.len() - 1] {
            assert!(KeychainReference::from_bytes(&valid[..len]).is_err());
        }
        let mut extra = valid.clone();
        extra.push(b'\n');
        assert!(KeychainReference::from_bytes(&extra).is_err());
        for (index, byte) in [
            (0, b'c'),
            (PREFIX.len() - 2, b'2'),
            (PREFIX.len(), b'A'),
            (PREFIX.len(), b'/'),
            (PREFIX.len(), 0),
            (PREFIX.len(), 0xff),
        ] {
            let mut altered = valid.clone();
            altered[index] = byte;
            assert!(KeychainReference::from_bytes(&altered).is_err());
        }
        let mut zero = PREFIX.to_vec();
        zero.extend_from_slice(&[b'0'; 64]);
        assert!(KeychainReference::from_bytes(&zero).is_err());
        assert!(KeychainReference::from_bytes(b"../../another-service").is_err());
        let mut old_backend = b"Crossflow/keychain-reference/v1/".to_vec();
        old_backend.extend_from_slice(&[b'a'; 64]);
        assert!(KeychainReference::from_bytes(&old_backend).is_err());
    }

    #[test]
    fn empty_and_oversized_secrets_rejected_before_native_calls() {
        assert!(store(&[]).is_err());
        assert!(store(&vec![0; MAX_SECRET_BYTES + 1]).is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_platform_has_no_storage_fallback() {
        let reference = KeychainReference::from_bytes(&reference_bytes()).unwrap();
        assert!(store(&[1]).is_err());
        assert!(load(&reference).is_err());
        assert!(remove(&reference).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "Explicit opt-in only: writes current user's Keychain through packaged dedicated helper; requires unlocked Keychain and matching helper ACL"]
    fn macos_keychain_roundtrip_missing_corrupt_reference_and_exact_cleanup() {
        struct Cleanup(KeychainReference);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = remove(&self.0);
            }
        }
        let original = Zeroizing::new(vec![0x5a; MAX_SECRET_BYTES]);
        let first = Cleanup(store(&original).expect("Keychain add first test record"));
        let second = Cleanup(store(&[0xa5]).expect("Keychain add second test record"));
        let restored = KeychainReference::from_bytes(&first.0.to_bytes()).unwrap();
        assert!(
            load(&restored).expect("Keychain load") == original,
            "Secret mismatch"
        );
        let mut corrupt = first.0.to_bytes();
        corrupt[0] ^= 1;
        assert!(KeychainReference::from_bytes(&corrupt).is_err());
        remove(&first.0).expect("Keychain exact cleanup");
        assert!(load(&first.0).is_err(), "Removed item still accessible");
        assert!(
            load(&second.0)
                .expect("Other test record must survive")
                .as_slice()
                == [0xa5]
        );
        remove(&first.0).expect("Missing cleanup is idempotent");
        remove(&second.0).expect("Keychain second cleanup");
        assert!(load(&second.0).is_err());
    }
}
