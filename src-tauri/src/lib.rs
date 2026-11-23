//! Sotto application library.
//!
//! The binary in `main.rs` is a thin shell: it builds the Tauri app, wires the
//! tray, and registers commands. Everything with behaviour worth testing lives
//! here, so the integration tests can drive the real pipeline without opening a
//! window or touching the desktop.

pub mod overlay;
pub mod pipeline;
pub mod playback_host;
pub mod shortcuts;
