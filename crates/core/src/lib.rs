//! The platform-independent core of nsxiv-ng.
#![forbid(unsafe_code)]

pub mod config;
pub mod file_list;
pub mod grid;
pub mod options;
pub mod viewer;
pub mod viewport;

/// A point in logical units.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// A width and height in logical units.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

/// A rectangle in logical units; which corner `x` and `y` name depends on the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn max_x(&self) -> f64 {
        self.x + self.width
    }

    pub fn max_y(&self) -> f64 {
        self.y + self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// `s` with its control characters as `?`: a file name, or other text from outside, may hold
/// some, which a terminal showing it, on screen or on stderr, would run as commands.
pub fn printable(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\0'..='\x1f' | '\x7f'..='\u{9f}' => '?',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_in_a_name_never_reach_the_terminal() {
        assert_eq!(
            printable("a\x1b]52;c;eA==\x07b\u{9b}.png"),
            "a?]52;c;eA==?b?.png"
        );
        assert_eq!(printable("é ✓"), "é ✓");
    }
}
