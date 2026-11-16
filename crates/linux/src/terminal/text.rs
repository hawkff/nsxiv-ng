//! Text for the terminal: the status line.

use nsxiv_ng_core::viewer::fitted_status;

use crate::printable;

/// The status bar as a line of `columns` cells, to show in reverse video: `left`, the name or
/// what went wrong, cut from its head, then the fields of `right` that fit, a cell of padding at
/// each end. A line that would still be longer, as the last field in a narrow terminal, keeps
/// its last `columns` chars, since the index at the end matters most: on the bottom row, a
/// longer line would wrap and scroll the screen.
// shortcut: counts chars, so a name with wide or combining ones may end the line off its last
// cell; a width table would fix it
pub fn status_line(left: &str, right: &str, columns: usize) -> String {
    let room = columns.saturating_sub(2);
    let count = |s: &str| s.chars().count();
    let right = fitted_status(right, room as f64, |s| count(s) as f64);
    let left = truncated_head(&printable(left), room as isize - count(&right) as isize - 1);
    let gap = room.saturating_sub(count(&left) + count(&right));
    let line = format!(" {left}{}{right} ", " ".repeat(gap));
    let over = count(&line).saturating_sub(columns);
    line.chars().skip(over).collect()
}

// s in width chars, cut from its head with "…" when it is longer, and empty when no char of it fits
fn truncated_head(s: &str, width: isize) -> String {
    let length = s.chars().count() as isize;
    if length <= width.max(0) {
        return s.to_owned();
    }
    if width < 2 {
        return String::new();
    }
    let kept: String = s.chars().skip((length - width + 1) as usize).collect();
    format!("…{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_lose_their_start() {
        assert_eq!(truncated_head("abcdefgh", 5), "…efgh");
        assert_eq!(truncated_head("abc", 5), "abc");
        assert_eq!(truncated_head("abc", 1), "");
        assert_eq!(truncated_head("", -3), "");
    }

    #[test]
    fn the_status_line_fills_its_columns_and_keeps_the_index() {
        assert_eq!(status_line("abcdefgh", "1/2", 12), " …defgh 1/2 ");
        assert_eq!(status_line("a.png", "1/2", 12), " a.png  1/2 ");
        // whole fields drop from the front, and then the name has no room
        assert_eq!(
            status_line("abcdefgh", "5s | 50% | 1/2", 12),
            "  50% | 1/2 "
        );
        assert_eq!(status_line("a\x1bb", "1/1", 10), " a?b  1/1 ");
        // never wider than the terminal, the index's end kept longest
        assert_eq!(status_line("a", "1/1", 0), "");
        assert_eq!(status_line("a", "1/1", 4), "1/1 ");
        assert_eq!(status_line("a", "12/345", 5), "/345 ");
        for columns in [0, 1, 2, 4, 5, 9, 10, 40] {
            let line = status_line("name.png", "50% | 12/345", columns);
            assert_eq!(line.chars().count(), columns, "{line:?}");
        }
    }
}
