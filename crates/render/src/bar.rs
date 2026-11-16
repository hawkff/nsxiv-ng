//! The status bar, the title bar of a window the compositor leaves undecorated, and the text
//! of what went wrong. A box is `[x, y, width, height]` in pixels; colors are canvas pixels.

use std::cell::RefCell;

use nsxiv_ng_core::viewer::fitted_status;

use crate::Canvas;
use crate::text::Text;

/// The bar's height in logical units: the line of `Ag` at scale 1, plus 8. None while the
/// fonts load and when none is installed.
pub fn height(text: &mut Text) -> Option<f64> {
    Some(f64::from(text.font(1.0)?.height() + 8))
}

/// The status bar in `area`: its background, the right part of the status fitted from the
/// front with [`fitted_status`] at the right end, and the left part cut from its head with
/// `…` in the room left, with 8 × `scale` pixels of padding. `status` is the left and the
/// right part, as `Viewer::status_text` gives them; `colors` are the background and the text.
pub fn status(
    canvas: &mut Canvas,
    text: &mut Text,
    area: [i32; 4],
    scale: f64,
    status: (&str, &str),
    colors: (u32, u32),
) {
    let [x, y, width, height] = area;
    canvas.fill(x, y, width, height, colors.0);
    let Some(mut font) = text.font(scale) else {
        return;
    };
    let pad = (8.0 * scale).round() as i32;
    let room = f64::from(width - pad * 2);
    // fitted_status measures through a shared closure, and measuring fills the font's caches
    let measured = RefCell::new(&mut font);
    let right = fitted_status(status.1, room, |s| {
        f64::from(measured.borrow_mut().width(s))
    });
    let right_width = font.width(&right);
    let top = y + (height - font.height()) / 2;
    font.draw(
        canvas,
        &right,
        x + width - right_width - pad,
        top,
        colors.1,
        area,
    );
    let left = truncated_head(status.0, width - right_width - pad * 3, |s| font.width(s));
    font.draw(canvas, &left, x + pad, top, colors.1, area);
}

/// The title bar filling `canvas`: the title cut from its head with `…`, and the `×` of the
/// close button centered in the square at the right end.
pub fn title(canvas: &mut Canvas, text: &mut Text, title: &str, scale: f64, colors: (u32, u32)) {
    let (width, height) = (canvas.width() as i32, canvas.height() as i32);
    let area = [0, 0, width, height];
    canvas.fill(0, 0, width, height, colors.0);
    let Some(mut font) = text.font(scale) else {
        return;
    };
    let top = (height - font.height()) / 2;
    let close = font.width("×");
    font.draw(
        canvas,
        "×",
        width - (height + close) / 2,
        top,
        colors.1,
        area,
    );
    let pad = (8.0 * scale).round() as i32;
    let name = truncated_head(title, width - height - pad * 2, |s| font.width(s));
    font.draw(canvas, &name, pad, top, colors.1, area);
}

/// The text of what went wrong, centered in `area` and clipped to it, over what is there.
pub fn error(
    canvas: &mut Canvas,
    text: &mut Text,
    message: &str,
    area: [i32; 4],
    scale: f64,
    color: u32,
) {
    let Some(mut font) = text.font(scale) else {
        return;
    };
    let [x, y, width, height] = area;
    let left = x + (width - font.width(message)) / 2;
    let top = y + (height - font.height()) / 2;
    font.draw(canvas, message, left, top, color, area);
}

/// `…name`, the way nsxiv cuts a long name: characters drop from the front until the rest
/// with `…` fits `width`, and nothing fits no room.
// shortcut: cuts between chars rather than grapheme clusters, so a cut can
// split a grapheme, leaving a combining mark, a flag's half or part of an emoji sequence after
// the `…`; cut on grapheme boundaries if names with those matter
fn truncated_head(s: &str, width: i32, mut measure: impl FnMut(&str) -> i32) -> String {
    if width <= 0 {
        return String::new();
    }
    if measure(s) <= width {
        return s.to_owned();
    }
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        let cut = format!("…{rest}");
        if !rest.is_empty() && measure(&cut) <= width {
            return cut;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::truncated_head;

    fn cut(s: &str, width: i32) -> String {
        truncated_head(s, width, |s| s.chars().count() as i32)
    }

    #[test]
    fn a_long_name_loses_its_head() {
        assert_eq!(cut("photo.jpg", 9), "photo.jpg");
        assert_eq!(cut("photo.jpg", 8), "…oto.jpg");
        assert_eq!(cut("photo.jpg", 2), "…g");
        // no room for the ellipsis and a character, or none at all
        assert_eq!(cut("photo.jpg", 1), "");
        assert_eq!(cut("photo.jpg", 0), "");
        assert_eq!(cut("", 5), "");
        // characters, not bytes
        assert_eq!(cut("äöüß.png", 6), "…ß.png");
    }
}
