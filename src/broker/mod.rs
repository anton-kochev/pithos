//! Host broker building blocks with unconditional CLI activation refusal.
//! Explicit status approval, offline loopback handling and read-only host
//! observations do not grant containers Docker authority or prove admission.

// Offline plan only; no production launch path consumes it yet.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod api;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod app;
pub mod bootstrap;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod browser;
pub mod compose;
pub mod credential;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod extension;
pub mod grant;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod host;
pub mod journal;
pub mod resources;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod runtime;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod state;
pub mod status;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod transport;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod volumes;
