//! The Linux frontends of nsxiv-ng; empty on any other system.
#![cfg(target_os = "linux")]

pub use nsxiv_ng_core::printable;

pub mod driver;
pub mod keysyms;
pub mod terminal;
pub mod wayland;
pub mod x11;
