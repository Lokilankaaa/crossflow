//! Native capture lifecycle used by the shared duplex session host.
//! Implementations never open sockets or grant control authority.
use anyhow::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    Toggle,
    Stop,
}

pub trait Capture {
    /// Called by the session owner, including during bounded startup handshakes.
    /// Renew suppression only here; a timer cannot keep a dead session active.
    fn poll(&mut self, now_ns: u64) -> Result<()>;
    fn command(&mut self) -> Option<Shortcut>;
    /// Fixed, redacted failure category. A safety stop must remain distinct
    /// from the user's normal Stop even after its command has been consumed.
    fn failure(&self) -> Option<&'static str> {
        None
    }
    /// Stop capture and local suppression; remote owned-input release belongs
    /// to the endpoint and is checked separately before process shutdown.
    fn stop(&mut self) -> Result<()>;
}

/// Platform hooks used by the common app runtime. Native implementations live
/// with their own platform; connection, layout and authority stay shared.
pub struct Platform {
    pub inventory: fn() -> Result<Vec<crate::pair_wire::Display>>,
    pub capture: fn(
        crate::pair_source::SharedSource,
        crate::metrics::Log,
    ) -> Result<Box<dyn Capture + Send>>,
}

#[cfg(windows)]
#[path = "platform/windows_pair.rs"]
pub mod windows;

#[cfg(target_os = "macos")]
#[path = "platform/macos_pair.rs"]
pub mod macos;
