//! The macOS frontend of nsxiv-ng, AppKit through objc2; empty on any other system.
#![cfg(target_os = "macos")]

mod app;
mod fonts;
mod frame;
mod geometry;
mod keys;
mod title;
mod vision;
mod watch;

pub use app::{Files, run};
#[doc(hidden)]
pub use app::{KEYS_PASSED, LAST_KEY_PASSED, MOVED_FRAMES};
