//! Persistent tmux sessions exposed through the shared authenticated socket runtime.
#![forbid(unsafe_code)]
pub mod pty;
mod tmux;
pub use tmux::TmuxConfig;
mod backend;
mod session;
pub use backend::{PtyBackend, PtyDomainConfig};
#[cfg(test)]
mod tests;
