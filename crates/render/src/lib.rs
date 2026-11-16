//! Rendering for nsxiv-ng: what turns decoded pixels into a frame.

#![forbid(unsafe_code)]

pub mod bar;
pub mod canvas;
mod colors;
pub mod grid_view;
pub mod image_view;
pub mod text;

pub use canvas::Canvas;
pub use colors::Colors;
