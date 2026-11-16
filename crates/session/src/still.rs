//! Stills of the files next to the current one, which a frontend draws ahead on threads of its
//! own.

use std::sync::Arc;

use nsxiv_ng_core::viewport::Viewport;
use nsxiv_ng_decode::Pyramid;
use nsxiv_ng_render::image_view::{self, Turn};
use nsxiv_ng_render::{Canvas, Colors};

use crate::Frame;

/// A neighbor's image with the view it gets once the viewer moves to it, which draws on any
/// thread what [`crate::Session::draw`] would draw then, with no part of the session.
// shortcut: an animation's first frame draws through the pyramid's levels, where the viewer
// once there draws the animation's frame without them, so a scaled-down one may differ a
// little; skip frames ahead of animations if it shows
pub struct Still {
    pub(crate) pyramid: Arc<Pyramid>,
    pub(crate) view: Viewport,
    pub(crate) colors: Option<Colors>,
    pub(crate) background: u32,
}

impl Still {
    /// The view the image gets: its zoom and where it lies in the canvas named.
    pub fn view(&self) -> &Viewport {
        &self.view
    }

    /// Draws the frame as [`crate::Session::draw`] would once the viewer showed the file,
    /// onto a canvas of the area above the bar, at `scale` pixels a logical unit. True when
    /// every pixel came out opaque.
    pub fn draw(&self, canvas: &mut Canvas, scale: f64, frame: Frame) -> bool {
        canvas
            .pixels_mut()
            .fill(if frame.clear { 0 } else { self.background });
        let turn = Turn::default();
        let colors = self.colors.as_ref();
        let pyramid = &self.pyramid;
        let covered = draw(
            canvas,
            pyramid,
            &self.view,
            turn,
            colors,
            scale,
            frame.whole,
        );
        covered || !frame.clear
    }
}

/// Draws a pyramid's image as the view places it, and gives whether opaque pixels cover the
/// canvas.
pub(crate) fn draw(
    canvas: &mut Canvas,
    pyramid: &Pyramid,
    view: &Viewport,
    turn: Turn,
    colors: Option<&Colors>,
    scale: f64,
    whole: bool,
) -> bool {
    let levels: Vec<&[u8]> = pyramid.levels.iter().map(|l| &l.rgba[..]).collect();
    let image = image_view::Image {
        width: pyramid.levels[0].width,
        height: pyramid.levels[0].height,
        levels: &levels,
        opaque: pyramid.opaque,
    };
    image_view::draw(canvas, &image, view, turn, colors, scale, whole)
}

// a terminal sends a still to a thread of its own
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Still>();
};
