//! Windows app-owned storage for previously authenticated and authorized identities.
//!
//! Importing an existing bundle is a migration, NOT short-code pairing. Network
//! discovery and the saved endpoint never establish trust. The pinned identity
//! remains authoritative and is revalidated before a record is returned.
//! This does not isolate data from other processes running as the same user.

use crate::security::Identity;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};

pub const MAX_PEERS: usize = 64;
const MAX_METADATA_BYTES: u64 = 4096;
const COMMIT_FILE: &str = "peer.json";
const PENDING_FILE: &str = "peer.pending.json";
const IDENTITY_FILE: &str = "identity.dpapi";

/// Public display and routing hints only. No private key or storage path.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PeerSummary {
    pub schema: u32,
    pub id: String,
    pub friendly_name: String,
    pub last_endpoint: SocketAddr,
}

#[derive(Debug, Serialize)]
pub struct PeerList {
    pub peers: Vec<PeerSummary>,
    /// Includes corrupt, unsupported, mismatched, or partially written entries.
    /// A zero-length peers list with nonzero invalid_records is not an empty store.
    pub invalid_records: usize,
}

pub struct PeerStore {
    root: PathBuf,
}

impl PeerStore {
    pub fn default_store() -> Result<Self> {
        supported()?;
        let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?;
        let store = Self::at(PathBuf::from(base).join("Crossflow").join("peers"));
        store.check_root(false)?;
        Ok(store)
    }

    /// Explicit root for diagnostics/tests. Operations require an absolute path.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Migrate an already-authorized Windows sender bundle exactly once.
    /// The source is retained; no new trust or network pairing is performed.
    pub fn remember_existing(
        &self,
        bundle: &Path,
        friendly_name: &str,
        last_endpoint: SocketAddr,
    ) -> Result<PeerSummary> {
        supported()?;
        validate_name(friendly_name)?;
        validate_endpoint(last_endpoint)?;
        // Identity::load fails closed when a protected file exists but fails.
        let identity = Identity::load(bundle).context("Cannot import existing identity")?;
        self.remember_authorized(identity, friendly_name, last_endpoint)
    }

    /// Save an identity after the caller has authenticated the peer and obtained
    /// explicit local authorization. This does not perform pairing or infer
    /// trust from a name/endpoint. No plaintext identity file is created.
    pub fn remember_authorized(
        &self,
        identity: Identity,
        friendly_name: &str,
        last_endpoint: SocketAddr,
    ) -> Result<PeerSummary> {
        supported()?;
        validate_name(friendly_name)?;
        validate_endpoint(last_endpoint)?;
        identity.validate()?;
        ensure!(
            identity.role == "windows",
            "Peer store requires a Windows sender identity"
        );
        let summary = PeerSummary {
            schema: 1,
            id: identity.peer_public_key_hex(),
            friendly_name: friendly_name.to_owned(),
            last_endpoint,
        };
        validate_summary(&summary)?;
        self.check_root(true)?;
        ensure!(
            self.entries()?.len() < MAX_PEERS,
            "Peer store record limit reached"
        );
        let output = self.root.join(&summary.id);
        // save_protected creates a NEW directory. Existing records and even
        // partial records are never overwritten or reused by an import.
        identity
            .save_protected(&output)
            .context("Cannot store protected peer identity")?;
        // Recheck the bound after directory creation, including concurrent
        // imports. Failure leaves an uncommitted, unusable partial directory.
        self.entries()?;
        let bytes = serde_json::to_vec(&summary).context("Cannot encode peer metadata")?;
        ensure!(
            bytes.len() as u64 <= MAX_METADATA_BYTES,
            "Peer metadata exceeds size limit"
        );
        let mut pending = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join(PENDING_FILE))
            .context("Cannot create pending peer metadata")?;
        pending
            .write_all(&bytes)
            .context("Cannot write peer metadata")?;
        pending.sync_all().context("Cannot flush peer metadata")?;
        drop(pending);
        // Final commit marker: partial JSON can never become a connectable
        // record. Both files are already flushed; rename stays in one directory.
        fs::rename(output.join(PENDING_FILE), output.join(COMMIT_FILE))
            .context("Cannot commit peer metadata")?;
        Ok(summary)
    }

    pub fn list(&self) -> Result<PeerList> {
        supported()?;
        if !self.check_root(false)? {
            return Ok(PeerList {
                peers: Vec::new(),
                invalid_records: 0,
            });
        }
        let mut peers = Vec::new();
        let mut invalid_records = 0;
        for entry in self.entries()? {
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                invalid_records += 1;
                continue;
            };
            match self.resolve_record(&id) {
                Ok((summary, _)) => peers.push(summary),
                Err(_) => invalid_records += 1,
            }
        }
        peers.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        Ok(PeerList {
            peers,
            invalid_records,
        })
    }

    /// Resolve a safe peer ID to a verified public summary and internal bundle.
    /// The returned path is for the local input runtime, never discovery/UI data.
    pub fn resolve(&self, id: &str) -> Result<(PeerSummary, PathBuf)> {
        supported()?;
        validate_id(id)?;
        ensure!(self.check_root(false)?, "Peer store does not exist");
        self.entries()?;
        self.resolve_record(id)
    }

    fn resolve_record(&self, id: &str) -> Result<(PeerSummary, PathBuf)> {
        validate_id(id)?;
        let bundle = self.root.join(id);
        require_directory(&bundle)?;
        let metadata_path = bundle.join(COMMIT_FILE);
        require_file(&metadata_path)?;
        let file = File::open(metadata_path).context("Cannot read peer metadata")?;
        ensure!(
            file.metadata()?.is_file(),
            "Peer metadata is not a regular file"
        );
        let mut bytes = Vec::new();
        file.take(MAX_METADATA_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("Cannot read peer metadata")?;
        ensure!(
            bytes.len() as u64 <= MAX_METADATA_BYTES,
            "Peer metadata exceeds size limit"
        );
        let summary: PeerSummary =
            serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid peer metadata"))?;
        validate_summary(&summary)?;
        ensure!(
            summary.id == id,
            "Peer metadata does not match its directory"
        );
        // The OS-protected file is mandatory. A plaintext file beside it cannot
        // act as a fallback, including when the protected file was removed.
        require_file(&bundle.join(IDENTITY_FILE))?;
        let identity = Identity::load(&bundle).context("Cannot open protected peer identity")?;
        ensure!(
            identity.role == "windows",
            "Peer store requires a Windows sender identity"
        );
        ensure!(
            identity.peer_public_key_hex() == id,
            "Protected identity does not match peer ID"
        );
        Ok((summary, bundle))
    }

    fn check_root(&self, create: bool) -> Result<bool> {
        ensure!(self.root.is_absolute(), "Peer store root must be absolute");
        ensure!(
            !self
                .root
                .components()
                .any(|part| matches!(part, Component::ParentDir)),
            "Peer store root must not contain parent traversal"
        );
        // Reject existing reparse/symlink ancestors before creating directories.
        for path in self.root.ancestors() {
            match fs::symlink_metadata(path) {
                Ok(metadata) => ensure!(
                    metadata.is_dir() && !is_link(&metadata),
                    "Peer store path is not a plain directory"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Cannot inspect peer store path"),
            }
        }
        match fs::symlink_metadata(&self.root) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&self.root).context("Cannot create peer store")?;
                require_directory(&self.root)?;
                Ok(true)
            }
            Err(error) => Err(error).context("Cannot inspect peer store"),
        }
    }

    fn entries(&self) -> Result<Vec<fs::DirEntry>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.root).context("Cannot enumerate peer store")? {
            entries.push(entry.context("Cannot enumerate peer store entry")?);
            ensure!(
                entries.len() <= MAX_PEERS,
                "Peer store exceeds the record limit"
            );
        }
        Ok(entries)
    }
}

fn supported() -> Result<()> {
    ensure!(
        cfg!(windows),
        "App-owned peer storage is unsupported on this platform"
    );
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 64
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "Invalid peer ID"
    );
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 256 && name.chars().count() <= 64,
        "Invalid device name length"
    );
    ensure!(
        name.trim() == name,
        "Device name must not have surrounding whitespace"
    );
    ensure!(!name.chars().any(|ch| ch.is_control() || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{206f}' | '\u{feff}')), "Device name contains forbidden control or directional characters");
    Ok(())
}

fn validate_endpoint(endpoint: SocketAddr) -> Result<()> {
    let ip = match endpoint.ip() {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    };
    ensure!(
        endpoint.port() != 0 && !ip.is_unspecified() && !ip.is_multicast(),
        "Invalid peer routing hint"
    );
    if let IpAddr::V4(ip) = ip {
        ensure!(!ip.is_broadcast(), "Invalid peer routing hint");
    }
    // Loopback is intentionally accepted for controlled local diagnostics.
    Ok(())
}

fn validate_summary(summary: &PeerSummary) -> Result<()> {
    ensure!(summary.schema == 1, "Unsupported peer metadata schema");
    validate_id(&summary.id)?;
    validate_name(&summary.friendly_name)?;
    validate_endpoint(summary.last_endpoint)
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT includes junctions, not just symlinks.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn require_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("Cannot inspect peer directory")?;
    ensure!(
        metadata.is_dir() && !is_link(&metadata),
        "Peer entry is not a plain directory"
    );
    Ok(())
}

fn require_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("Cannot inspect peer file")?;
    ensure!(
        metadata.is_file() && !is_link(&metadata),
        "Peer entry is not a regular file"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_names_and_endpoints_are_strict() {
        for bad in [
            "../peer",
            "..\\peer",
            "C:\\peer",
            "",
            &"A".repeat(64),
            &"g".repeat(64),
        ] {
            assert!(validate_id(bad).is_err());
        }
        for bad in [
            "",
            " name",
            "name ",
            "line\nname",
            "name\u{202e}",
            "name\u{2066}",
            &"名".repeat(65),
        ] {
            assert!(validate_name(bad).is_err());
        }
        assert!(validate_name("工作用 MacBook Air").is_ok());
        for bad in [
            "0.0.0.0:1",
            "[::]:1",
            "224.0.0.1:1",
            "[ff02::1]:1",
            "255.255.255.255:1",
            "127.0.0.1:0",
            "[::ffff:255.255.255.255]:1",
            "[::ffff:224.0.0.1]:1",
            "[::ffff:0.0.0.0]:1",
        ] {
            assert!(validate_endpoint(bad.parse().unwrap()).is_err());
        }
        assert!(validate_endpoint("127.0.0.1:54321".parse().unwrap()).is_ok());
    }

    #[cfg(not(windows))]
    #[test]
    fn native_store_is_explicitly_unsupported() {
        assert!(PeerStore::default_store().is_err());
        assert!(PeerStore::at(std::env::temp_dir()).list().is_err());
    }

    #[cfg(windows)]
    mod windows {
        use super::*;
        use crate::security::{PairInfo, create_pair};
        use zeroize::Zeroizing;

        struct MemoryFixture(PathBuf);
        impl MemoryFixture {
            fn new() -> Self {
                let root = std::env::temp_dir().join(format!(
                    "crossflow-authorized-store-{}",
                    crate::discovery::new_public_id().unwrap()
                ));
                fs::create_dir(&root).unwrap();
                Self(root)
            }
        }
        impl Drop for MemoryFixture {
            fn drop(&mut self) {
                assert!(self.0.starts_with(std::env::temp_dir()));
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        fn authorized_identity(role: &str) -> (Identity, Zeroizing<[u8; 32]>) {
            let builder = snow::Builder::new(crate::security::PATTERN.parse().unwrap());
            let local = builder.generate_keypair().unwrap();
            let remote = builder.generate_keypair().unwrap();
            let local_private = Zeroizing::new(local.private);
            let _remote_private = Zeroizing::new(remote.private);
            let private: Zeroizing<[u8; 32]> =
                Zeroizing::new(local_private.as_slice().try_into().unwrap());
            let identity = Identity::from_authorized_pairing(
                role,
                "0123456789abcdef0123456789abcdef".into(),
                Zeroizing::new(*private),
                remote.public.as_slice().try_into().unwrap(),
            )
            .unwrap();
            (identity, private)
        }

        #[test]
        fn authorized_identity_saves_directly_without_plaintext_and_matches_migration() {
            let f = MemoryFixture::new();
            let store = PeerStore::at(f.0.join("peers"));
            let (identity, private) = authorized_identity("windows");
            let public = identity.public_key_hex();
            let peer = identity.peer_public_key_hex();
            let endpoint = "127.0.0.1:5555".parse().unwrap();
            let saved = store
                .remember_authorized(identity, "Authorized Mac", endpoint)
                .unwrap();
            assert_eq!(saved.id, peer);
            let reopened = PeerStore::at(&store.root);
            assert_eq!(reopened.list().unwrap().peers, vec![saved.clone()]);
            let (_, bundle) = reopened.resolve(&saved.id).unwrap();
            let loaded = Identity::load(&bundle).unwrap();
            assert_eq!(loaded.public_key_hex(), public);
            assert_eq!(loaded.peer_public_key_hex(), peer);
            let mut files = fs::read_dir(&bundle)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            files.sort();
            assert_eq!(
                files,
                vec![
                    std::ffi::OsString::from(IDENTITY_FILE),
                    std::ffi::OsString::from(COMMIT_FILE)
                ]
            );
            let sealed = fs::read(bundle.join(IDENTITY_FILE)).unwrap();
            assert!(
                !sealed
                    .windows(private.len())
                    .any(|bytes| bytes == &private[..])
            );
            let metadata = fs::read(bundle.join(COMMIT_FILE)).unwrap();
            let listed = serde_json::to_string(&reopened.list().unwrap()).unwrap();
            assert!(
                !listed.contains("private")
                    && !listed.contains("bundle")
                    && !listed.contains("path")
            );

            // A diagnostic migration of the SAME protected identity is equivalent,
            // but the direct path above did not need a source identity directory.
            let diagnostic = f.0.join("diagnostic-protected-source");
            loaded.save_protected(&diagnostic).unwrap();
            let migrated = PeerStore::at(f.0.join("migrated"))
                .remember_existing(&diagnostic, "Authorized Mac", endpoint)
                .unwrap();
            assert_eq!(migrated, saved);
            assert!(!diagnostic.join("identity.json").exists());
            assert!(
                store
                    .remember_authorized(loaded, "Changed", "127.0.0.1:5556".parse().unwrap())
                    .is_err()
            );
            assert!(sealed == fs::read(bundle.join(IDENTITY_FILE)).unwrap());
            assert!(metadata == fs::read(bundle.join(COMMIT_FILE)).unwrap());
        }

        #[test]
        fn invalid_authorized_data_is_rejected_before_creating_store() {
            let f = MemoryFixture::new();
            let store = PeerStore::at(f.0.join("not-created"));
            for case in 0..4 {
                let (mut identity, _) = authorized_identity("windows");
                let mut name = "Authorized Mac";
                let mut endpoint = "127.0.0.1:5555".parse().unwrap();
                match case {
                    0 => name = " name",
                    1 => endpoint = "0.0.0.0:5555".parse().unwrap(),
                    2 => identity.role = "mac".into(),
                    _ => identity.pair_id = "A".repeat(32),
                }
                assert!(store.remember_authorized(identity, name, endpoint).is_err());
                assert!(!store.root.exists());
            }
            let (_, private) = authorized_identity("windows");
            for peer in [[0; 32], {
                let mut key = [0; 32];
                key[0] = 1;
                key
            }] {
                let result = Identity::from_authorized_pairing(
                    "windows",
                    "0123456789abcdef0123456789abcdef".into(),
                    Zeroizing::new(*private),
                    peer,
                )
                .and_then(|identity| {
                    store.remember_authorized(
                        identity,
                        "Invalid peer",
                        "127.0.0.1:5555".parse().unwrap(),
                    )
                });
                assert!(result.is_err());
                assert!(!store.root.exists());
            }
        }

        struct Fixture {
            root: PathBuf,
            pair: PairInfo,
            store: PeerStore,
        }
        impl Fixture {
            fn new() -> Self {
                let root = std::env::temp_dir().join(format!(
                    "crossflow-peer-store-{}",
                    crate::discovery::new_public_id().unwrap()
                ));
                fs::create_dir(&root).unwrap();
                let pair = create_pair(&root.join("source")).unwrap();
                let store = PeerStore::at(root.join("appdata/Crossflow/peers"));
                Self { root, pair, store }
            }
            fn remember(&self) -> PeerSummary {
                self.store
                    .remember_existing(
                        &self.pair.windows_bundle,
                        "Test Mac",
                        "127.0.0.1:5555".parse().unwrap(),
                    )
                    .unwrap()
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                // Only this test's newly created, absolute temp directory.
                assert!(self.root.starts_with(std::env::temp_dir()));
                let _ = fs::remove_dir_all(&self.root);
            }
        }

        #[test]
        fn real_dpapi_store_survives_reopen_and_never_overwrites() {
            let f = Fixture::new();
            assert!(f.store.list().unwrap().peers.is_empty());
            let saved = f.remember();
            let reopened = PeerStore::at(&f.store.root);
            let list = reopened.list().unwrap();
            assert_eq!(list.invalid_records, 0);
            assert_eq!(list.peers, vec![saved.clone()]);
            let (resolved, bundle) = reopened.resolve(&saved.id).unwrap();
            assert_eq!(resolved, saved);
            assert!(bundle.join(IDENTITY_FILE).is_file());
            assert!(!bundle.join("identity.json").exists());
            let before = fs::read(bundle.join(IDENTITY_FILE)).unwrap();
            assert!(
                f.store
                    .remember_existing(
                        &f.pair.windows_bundle,
                        "Changed",
                        "127.0.0.1:5556".parse().unwrap()
                    )
                    .is_err()
            );
            assert!(before == fs::read(bundle.join(IDENTITY_FILE)).unwrap());
            let json = serde_json::to_string(&list).unwrap();
            assert!(
                !json.contains("private") && !json.contains("bundle") && !json.contains("path")
            );
        }

        #[test]
        fn corrupt_protection_never_falls_back_or_appears_connectable() {
            let f = Fixture::new();
            let saved = f.remember();
            let bundle = f.store.root.join(&saved.id);
            fs::copy(
                f.pair.windows_bundle.join("identity.json"),
                bundle.join("identity.json"),
            )
            .unwrap();
            fs::write(bundle.join(IDENTITY_FILE), b"corrupt").unwrap();
            assert!(f.store.resolve(&saved.id).is_err());
            let list = f.store.list().unwrap();
            assert!(list.peers.is_empty());
            assert_eq!(list.invalid_records, 1);
            fs::remove_file(bundle.join(IDENTITY_FILE)).unwrap();
            assert!(f.store.resolve(&saved.id).is_err());
            // The importer also cannot bypass a damaged protected source.
            fs::write(f.pair.windows_bundle.join(IDENTITY_FILE), b"corrupt").unwrap();
            let other = PeerStore::at(f.root.join("other"));
            assert!(
                other
                    .remember_existing(
                        &f.pair.windows_bundle,
                        "Test",
                        "127.0.0.1:5".parse().unwrap()
                    )
                    .is_err()
            );
            assert!(!other.root.exists());
        }

        #[test]
        fn uncommitted_and_mismatched_records_are_counted() {
            let f = Fixture::new();
            let saved = f.remember();
            let bundle = f.store.root.join(&saved.id);
            fs::rename(bundle.join(COMMIT_FILE), bundle.join(PENDING_FILE)).unwrap();
            assert!(f.store.resolve(&saved.id).is_err());
            assert_eq!(f.store.list().unwrap().invalid_records, 1);
            assert!(
                f.store
                    .remember_existing(
                        &f.pair.windows_bundle,
                        "Test",
                        "127.0.0.1:5".parse().unwrap()
                    )
                    .is_err()
            );
            fs::rename(bundle.join(PENDING_FILE), bundle.join(COMMIT_FILE)).unwrap();
            let other_pair = create_pair(&f.root.join("other-source")).unwrap();
            let mut changed = saved.clone();
            changed.id = Identity::load(&other_pair.windows_bundle)
                .unwrap()
                .peer_public_key_hex();
            fs::write(
                bundle.join(COMMIT_FILE),
                serde_json::to_vec(&changed).unwrap(),
            )
            .unwrap();
            assert!(f.store.resolve(&saved.id).is_err());
            // Metadata now matches its new directory, but the pinned key does not.
            fs::rename(&bundle, f.store.root.join(&changed.id)).unwrap();
            assert!(f.store.resolve(&changed.id).is_err());
            assert_eq!(f.store.list().unwrap().invalid_records, 1);
        }

        #[test]
        fn malformed_metadata_traversal_and_store_limit_fail_explicitly() {
            let f = Fixture::new();
            let saved = f.remember();
            for id in ["../source/windows", "..\\source\\windows", "C:\\Windows"] {
                assert!(f.store.resolve(id).is_err());
            }
            fs::write(f.store.root.join(&saved.id).join(COMMIT_FILE), b"{").unwrap();
            assert_eq!(f.store.list().unwrap().invalid_records, 1);
            for n in 0..MAX_PEERS {
                fs::create_dir(f.store.root.join(format!("partial-{n}"))).unwrap();
            }
            assert!(f.store.list().is_err());
            assert!(f.store.resolve(&saved.id).is_err());
        }

        #[test]
        fn rejects_wrong_local_role_before_creating_store() {
            let f = Fixture::new();
            assert!(
                f.store
                    .remember_existing(
                        &f.pair.mac_bundle,
                        "Wrong role",
                        "127.0.0.1:5".parse().unwrap()
                    )
                    .is_err()
            );
            assert!(!f.store.root.exists());
        }
    }
}
