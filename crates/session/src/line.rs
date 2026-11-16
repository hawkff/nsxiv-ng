//! The session's own text line, for a frontend without a text field of its own: an annotation's
//! words as they are typed, drawn over the canvas where the field would be.

use nsxiv_ng_core::Rect;
use nsxiv_ng_core::viewer::Keystroke;
use nsxiv_ng_render::Canvas;
use nsxiv_ng_render::text::Text;

/// The words being typed and the cursor in them.
pub(crate) struct Line {
    /// Where the line shows, in logical units of the canvas, as the edit places a field.
    rect: Rect,
    text: String,
    /// The cursor, a byte offset at a character's boundary.
    cursor: usize,
    /// The annotation's color, which the words are typed in.
    color: [u8; 3],
}

/// What a key did to the line.
#[derive(Debug, PartialEq)]
pub(crate) enum Typed {
    /// The line changed, or its cursor moved.
    Changed,
    /// Return or Escape ended it, with its words.
    Ended(String),
    /// Nothing, as for a key with Control or one the line has no use for.
    Nothing,
}

impl Line {
    /// A line over `rect` holding `text`, the cursor at its end.
    pub(crate) fn new(rect: Rect, text: String, color: [u8; 3]) -> Line {
        let cursor = text.len();
        Line {
            rect,
            text,
            cursor,
            color,
        }
    }

    /// Its words, as they are.
    pub(crate) fn into_text(self) -> String {
        self.text
    }

    /// A key, which the line takes whatever it is: a key named by one character, which is not
    /// a control character, types it at the cursor; BackSpace and Delete remove the character
    /// before the cursor and the one after it; Left, Right, Home and End move the cursor;
    /// Return and Escape end the line, Escape too keeping its words, as the macOS window's
    /// field does.
    // shortcut: the cursor moves and deletes by characters, so a letter with a combining mark
    // or an emoji of several characters takes as many steps; step by grapheme clusters if
    // such text gets typed
    pub(crate) fn key(&mut self, key: &Keystroke) -> Typed {
        if key.ctrl {
            return Typed::Nothing;
        }
        let before = |text: &str, at: usize| text[..at].chars().next_back().map(char::len_utf8);
        let after = |text: &str, at: usize| text[at..].chars().next().map(char::len_utf8);
        match key.key.as_str() {
            "Return" | "KP_Enter" | "Escape" => {
                return Typed::Ended(std::mem::take(&mut self.text));
            }
            "BackSpace" => match before(&self.text, self.cursor) {
                Some(width) => {
                    self.cursor -= width;
                    self.text.remove(self.cursor);
                }
                None => return Typed::Nothing,
            },
            "Delete" | "KP_Delete" => match after(&self.text, self.cursor) {
                Some(_) => {
                    self.text.remove(self.cursor);
                }
                None => return Typed::Nothing,
            },
            "Left" | "KP_Left" => match before(&self.text, self.cursor) {
                Some(width) => self.cursor -= width,
                None => return Typed::Nothing,
            },
            "Right" | "KP_Right" => match after(&self.text, self.cursor) {
                Some(width) => self.cursor += width,
                None => return Typed::Nothing,
            },
            "Home" | "KP_Home" => self.cursor = 0,
            "End" | "KP_End" => self.cursor = self.text.len(),
            typed => {
                let mut chars = typed.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if !c.is_control() => {
                        self.text.insert(self.cursor, c);
                        self.cursor += c.len_utf8();
                    }
                    _ => return Typed::Nothing,
                }
            }
        }
        Typed::Changed
    }

    /// Draws the line onto `canvas` at `scale`: a box in the bar's colors where the edit places
    /// a field, within the canvas, at least a line of the bar font high, and in it the words in
    /// the annotation's color with the cursor, the words moved left where they would hide the
    /// cursor.
    pub(crate) fn draw(&self, canvas: &mut Canvas, text: &mut Text, scale: f64, bar: (u32, u32)) {
        let (background, foreground) = bar;
        let [r, g, b] = self.color;
        let color = 0xff00_0000 | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b);
        let pixels = |v: f64| (v * scale).round() as i32;
        let pad = pixels(4.0).max(1);
        let Some(mut font) = text.font(scale) else {
            return;
        };
        let line = font.height();
        let (canvas_width, canvas_height) = (canvas.width() as i32, canvas.height() as i32);
        // its bottom where the field's is, which the edit puts by the text's baseline, moved
        // left and down into the canvas where it would leave it, no wider than the canvas
        let width = pixels(self.rect.width).min(canvas_width);
        let height = pixels(self.rect.height).max(line + 2 * pad);
        let x = pixels(self.rect.x).min(canvas_width - width).max(0);
        let y = (pixels(self.rect.max_y()) - height)
            .min(canvas_height - height)
            .max(0);
        canvas.fill(x, y, width, height, foreground);
        canvas.fill(x + 1, y + 1, width - 2, height - 2, background);
        // the words' room, as much of it as the canvas shows, and none in a box too small
        let (left, right) = ((x + pad).max(0), (x + width - pad).min(canvas_width));
        let (upper, lower) = ((y + 1).max(0), (y + height - 1).min(canvas_height));
        if right <= left || lower <= upper {
            return;
        }
        let inside = [left, upper, right - left, lower - upper];
        let cursor = font.width(&self.text[..self.cursor]);
        let shift = (x + pad + cursor - (right - 1)).max(0);
        let top = y + (height - line) / 2;
        font.draw(canvas, &self.text, x + pad - shift, top, color, inside);
        let at = x + pad + cursor - shift;
        canvas.fill(
            at,
            top.max(upper),
            pixels(1.0).max(1),
            line.min(lower - top.max(upper)),
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> Line {
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 20.0,
        };
        Line::new(rect, text.to_owned(), [255, 0, 0])
    }

    fn keys(line: &mut Line, names: &[&str]) -> Vec<Typed> {
        names
            .iter()
            .map(|name| line.key(&Keystroke::new(name)))
            .collect()
    }

    #[test]
    fn characters_go_in_at_the_cursor_which_the_keys_move() {
        let mut typing = line("ac");
        keys(&mut typing, &["Left", "b", "End", "d", "Home", "é", " "]);
        assert_eq!((typing.text.as_str(), typing.cursor), ("é abcd", 3));
        // BackSpace takes the character before the cursor, Delete the one after it
        keys(&mut typing, &["BackSpace", "Delete"]);
        assert_eq!((typing.text.as_str(), typing.cursor), ("ébcd", 2));
        // at the ends they do nothing, nor do keys of more than one character or with Control
        let nothing = keys(&mut typing, &["Home", "BackSpace", "Left", "Tab", "F1"]);
        assert_eq!(
            nothing[1..],
            [
                Typed::Nothing,
                Typed::Nothing,
                Typed::Nothing,
                Typed::Nothing
            ]
        );
        assert_eq!(typing.key(&Keystroke::ctrl("a")), Typed::Nothing);
        assert_eq!(
            typing.key(&Keystroke::new("\u{7}")),
            Typed::Nothing,
            "a control character"
        );
        assert_eq!(typing.text, "ébcd");
    }

    const RED: u32 = 0xffff_0000;

    /// The line of `words` over `rect` on a canvas 60 x 40 of black, the cursor at their end,
    /// and the height of a line of the bar font.
    fn drawn(rect: Rect, words: &str) -> (Vec<u32>, usize) {
        let (sender, ready) = std::sync::mpsc::channel();
        let mut text = Text::new("monospace", 13.0, move || sender.send(()).unwrap());
        ready.recv().unwrap();
        let height = text.font(1.0).unwrap().height() as usize;
        let mut pixels = vec![0; 60 * 40];
        let mut canvas = Canvas::new(&mut pixels, 60, 40).unwrap();
        let line = Line::new(rect, words.to_owned(), [255, 0, 0]);
        line.draw(&mut canvas, &mut text, 1.0, (0xff20_2020, 0xffe0_e0e0));
        (pixels, height)
    }

    #[test]
    fn the_words_and_the_cursor_stay_in_the_box_and_the_canvas() {
        // a box too narrow for any words shows neither them nor the cursor
        let tiny = Rect {
            x: 20.0,
            y: 10.0,
            width: 1.0,
            height: 20.0,
        };
        assert!(!drawn(tiny, "....").0.contains(&RED));
        // past the canvas's right edge, the words move left so the cursor at their end shows:
        // a column of red a line high, which dots have none of
        let past = Rect {
            x: 10.0,
            y: 10.0,
            width: 200.0,
            height: 20.0,
        };
        assert!(cursor(drawn(past, "..............")), "no cursor shows");
    }

    /// Whether a column of red a line high shows: the cursor, which dots have none of.
    fn cursor((pixels, height): (Vec<u32>, usize)) -> bool {
        let column = |x: usize| (0..40).filter(|y| pixels[y * 60 + x] == RED).count();
        (0..60).any(|x| column(x) >= height)
    }

    #[test]
    fn a_box_by_an_edge_moves_into_the_canvas() {
        // its bottom near the top: it would go above, and comes down
        let top = Rect {
            x: 5.0,
            y: -20.0,
            width: 50.0,
            height: 22.0,
        };
        assert!(cursor(drawn(top, "..")), "near the top");
        // a few pixels from the right edge: it would go past, and moves left
        let right = Rect {
            x: 57.0,
            y: 10.0,
            width: 50.0,
            height: 20.0,
        };
        assert!(cursor(drawn(right, "..")), "near the right edge");
    }

    #[test]
    fn return_and_escape_end_the_line_with_its_words() {
        for end in ["Return", "KP_Enter", "Escape"] {
            let mut typing = line("hi");
            assert_eq!(
                typing.key(&Keystroke::new(end)),
                Typed::Ended("hi".to_owned())
            );
        }
    }
}
