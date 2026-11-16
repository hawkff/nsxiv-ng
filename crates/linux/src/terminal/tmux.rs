//! tmux: what it passes on to the terminal around it, and the placeholders it shows images
//! through.

/// What tmux passes unread to the terminal around it, its escapes doubled.
pub fn passthrough(sequence: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
}

/// The format for `tmux display -p` whose answer `tmux_support` reads.
pub const TMUX_FORMAT: &str = "#{allow-passthrough}|#{client_utf8}|#{sixel_support}|\
    #{client_termfeatures}|#{client_termname}";

/// What tmux passes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TmuxSupport {
    /// Kitty graphics, with allow-passthrough, to a client with UTF-8, which the placeholders are.
    pub passthrough: bool,
    /// Sixels, where tmux draws them, from tmux 3.6, and its client shows them; tmux claims
    /// sixels whatever is around it.
    pub sixel: bool,
}

/// What tmux passes on, from its answer to `TMUX_FORMAT`. A client that is another tmux, or
/// screen, gets neither.
pub fn tmux_support(answer: &str) -> TmuxSupport {
    let fields: Vec<&str> = answer.trim().split('|').collect();
    let none = TmuxSupport {
        passthrough: false,
        sixel: false,
    };
    let [passthrough, utf8, sixel, features, term] = fields[..] else {
        return none;
    };
    if term.starts_with("screen") || term.starts_with("tmux") {
        return none;
    }
    TmuxSupport {
        passthrough: matches!(passthrough, "on" | "all") && utf8 == "1",
        sixel: sixel == "1" && features.split(',').any(|f| f == "sixel"),
    }
}

/// The cells of `columns × rows` that show an image through placeholders, whose id's high byte
/// is `high`: U+10EEEE in each cell, and the row, column 0 and `high` in diacritics on the
/// first cell of each row, from which the cells after it count on. The low byte of the id goes
/// in the 256-color foreground around them. Diacritics on every cell would nearly triple the
/// text, which tmux redraws a cell at a time as they come: past 8 bytes a cell, tmux drops what
/// it has yet to send and redraws the window, and with allow-passthrough on it drops what
/// passes through until it has. Rows past the diacritics' count are left out.
// shortcut: a tmux menu over part of a row leaves the cells right of it blank until it closes
pub fn placeholder_cells(columns: usize, rows: usize, high: u8) -> String {
    let mut text = String::new();
    if columns == 0 {
        return text;
    }
    for (row, &mark) in DIACRITICS.iter().take(rows).enumerate() {
        text += &format!(
            "\x1b[{};1H\u{10EEEE}{mark}{}{}",
            row + 1,
            DIACRITICS[0],
            DIACRITICS[usize::from(high)]
        );
        text.extend(std::iter::repeat_n('\u{10EEEE}', columns - 1));
    }
    text
}

/// The kitty graphics protocol's diacritics for numbers in placeholders, the nth for n.
#[rustfmt::skip]
pub(super) const DIACRITICS: [char; 297] = [
    '\u{305}', '\u{30d}', '\u{30e}', '\u{310}', '\u{312}', '\u{33d}', '\u{33e}', '\u{33f}',
    '\u{346}', '\u{34a}', '\u{34b}', '\u{34c}', '\u{350}', '\u{351}', '\u{352}', '\u{357}',
    '\u{35b}', '\u{363}', '\u{364}', '\u{365}', '\u{366}', '\u{367}', '\u{368}', '\u{369}',
    '\u{36a}', '\u{36b}', '\u{36c}', '\u{36d}', '\u{36e}', '\u{36f}', '\u{483}', '\u{484}',
    '\u{485}', '\u{486}', '\u{487}', '\u{592}', '\u{593}', '\u{594}', '\u{595}', '\u{597}',
    '\u{598}', '\u{599}', '\u{59c}', '\u{59d}', '\u{59e}', '\u{59f}', '\u{5a0}', '\u{5a1}',
    '\u{5a8}', '\u{5a9}', '\u{5ab}', '\u{5ac}', '\u{5af}', '\u{5c4}', '\u{610}', '\u{611}',
    '\u{612}', '\u{613}', '\u{614}', '\u{615}', '\u{616}', '\u{617}', '\u{657}', '\u{658}',
    '\u{659}', '\u{65a}', '\u{65b}', '\u{65d}', '\u{65e}', '\u{6d6}', '\u{6d7}', '\u{6d8}',
    '\u{6d9}', '\u{6da}', '\u{6db}', '\u{6dc}', '\u{6df}', '\u{6e0}', '\u{6e1}', '\u{6e2}',
    '\u{6e4}', '\u{6e7}', '\u{6e8}', '\u{6eb}', '\u{6ec}', '\u{730}', '\u{732}', '\u{733}',
    '\u{735}', '\u{736}', '\u{73a}', '\u{73d}', '\u{73f}', '\u{740}', '\u{741}', '\u{743}',
    '\u{745}', '\u{747}', '\u{749}', '\u{74a}', '\u{7eb}', '\u{7ec}', '\u{7ed}', '\u{7ee}',
    '\u{7ef}', '\u{7f0}', '\u{7f1}', '\u{7f3}', '\u{816}', '\u{817}', '\u{818}', '\u{819}',
    '\u{81b}', '\u{81c}', '\u{81d}', '\u{81e}', '\u{81f}', '\u{820}', '\u{821}', '\u{822}',
    '\u{823}', '\u{825}', '\u{826}', '\u{827}', '\u{829}', '\u{82a}', '\u{82b}', '\u{82c}',
    '\u{82d}', '\u{951}', '\u{953}', '\u{954}', '\u{f82}', '\u{f83}', '\u{f86}', '\u{f87}',
    '\u{135d}', '\u{135e}', '\u{135f}', '\u{17dd}', '\u{193a}', '\u{1a17}', '\u{1a75}', '\u{1a76}',
    '\u{1a77}', '\u{1a78}', '\u{1a79}', '\u{1a7a}', '\u{1a7b}', '\u{1a7c}', '\u{1b6b}', '\u{1b6d}',
    '\u{1b6e}', '\u{1b6f}', '\u{1b70}', '\u{1b71}', '\u{1b72}', '\u{1b73}', '\u{1cd0}', '\u{1cd1}',
    '\u{1cd2}', '\u{1cda}', '\u{1cdb}', '\u{1ce0}', '\u{1dc0}', '\u{1dc1}', '\u{1dc3}', '\u{1dc4}',
    '\u{1dc5}', '\u{1dc6}', '\u{1dc7}', '\u{1dc8}', '\u{1dc9}', '\u{1dcb}', '\u{1dcc}', '\u{1dd1}',
    '\u{1dd2}', '\u{1dd3}', '\u{1dd4}', '\u{1dd5}', '\u{1dd6}', '\u{1dd7}', '\u{1dd8}', '\u{1dd9}',
    '\u{1dda}', '\u{1ddb}', '\u{1ddc}', '\u{1ddd}', '\u{1dde}', '\u{1ddf}', '\u{1de0}', '\u{1de1}',
    '\u{1de2}', '\u{1de3}', '\u{1de4}', '\u{1de5}', '\u{1de6}', '\u{1dfe}', '\u{20d0}', '\u{20d1}',
    '\u{20d4}', '\u{20d5}', '\u{20d6}', '\u{20d7}', '\u{20db}', '\u{20dc}', '\u{20e1}', '\u{20e7}',
    '\u{20e9}', '\u{20f0}', '\u{2cef}', '\u{2cf0}', '\u{2cf1}', '\u{2de0}', '\u{2de1}', '\u{2de2}',
    '\u{2de3}', '\u{2de4}', '\u{2de5}', '\u{2de6}', '\u{2de7}', '\u{2de8}', '\u{2de9}', '\u{2dea}',
    '\u{2deb}', '\u{2dec}', '\u{2ded}', '\u{2dee}', '\u{2def}', '\u{2df0}', '\u{2df1}', '\u{2df2}',
    '\u{2df3}', '\u{2df4}', '\u{2df5}', '\u{2df6}', '\u{2df7}', '\u{2df8}', '\u{2df9}', '\u{2dfa}',
    '\u{2dfb}', '\u{2dfc}', '\u{2dfd}', '\u{2dfe}', '\u{2dff}', '\u{a66f}', '\u{a67c}', '\u{a67d}',
    '\u{a6f0}', '\u{a6f1}', '\u{a8e0}', '\u{a8e1}', '\u{a8e2}', '\u{a8e3}', '\u{a8e4}', '\u{a8e5}',
    '\u{a8e6}', '\u{a8e7}', '\u{a8e8}', '\u{a8e9}', '\u{a8ea}', '\u{a8eb}', '\u{a8ec}', '\u{a8ed}',
    '\u{a8ee}', '\u{a8ef}', '\u{a8f0}', '\u{a8f1}', '\u{aab0}', '\u{aab2}', '\u{aab3}', '\u{aab7}',
    '\u{aab8}', '\u{aabe}', '\u{aabf}', '\u{aac1}', '\u{fe20}', '\u{fe21}', '\u{fe22}', '\u{fe23}',
    '\u{fe24}', '\u{fe25}', '\u{fe26}', '\u{10a0f}', '\u{10a38}', '\u{1d185}', '\u{1d186}',
    '\u{1d187}', '\u{1d188}', '\u{1d189}', '\u{1d1aa}', '\u{1d1ab}', '\u{1d1ac}', '\u{1d1ad}',
    '\u{1d242}', '\u{1d243}', '\u{1d244}',
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_for_the_terminal_around_tmux_goes_in_its_passthrough_its_escapes_doubled() {
        assert_eq!(
            passthrough("\x1b_Ga=q\x1b\\"),
            "\x1bPtmux;\x1b\x1b_Ga=q\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn the_format_asks_for_the_five_fields_in_order() {
        assert_eq!(
            TMUX_FORMAT.split('|').collect::<Vec<_>>(),
            [
                "#{allow-passthrough}",
                "#{client_utf8}",
                "#{sixel_support}",
                "#{client_termfeatures}",
                "#{client_termname}"
            ]
        );
    }

    #[test]
    fn tmux_passes_kitty_graphics_and_sixels_where_it_and_its_client_have_them() {
        let support = |answer: &str| {
            let s = tmux_support(answer);
            (s.passthrough, s.sixel)
        };
        assert_eq!(
            support("on|1|1|bpaste,clipboard,sixel,sync|xterm-kitty\n"),
            (true, true)
        );
        assert_eq!(support("all|1|1|RGB|xterm-ghostty\n"), (true, false));
        assert_eq!(support("on|0|1|sixel|foot\n"), (false, true));
        assert_eq!(support("off|1||sixel|xterm-256color\n"), (false, false));
        assert_eq!(support("on|1|1|sixel|tmux-256color\n"), (false, false));
        assert_eq!(support(""), (false, false));
    }

    #[test]
    fn placeholders_name_their_rows_and_the_ids_high_byte_on_the_first_cell_of_each_row() {
        // two rows of three cells of an image whose id has the high byte 3: the diacritics for 0,
        // 1 and 3 are U+0305, U+030D and U+0310, and the cells after the first count on
        assert_eq!(
            placeholder_cells(3, 2, 3),
            "\x1b[1;1H\u{10EEEE}\u{305}\u{305}\u{310}\u{10EEEE}\u{10EEEE}\
             \x1b[2;1H\u{10EEEE}\u{30D}\u{305}\u{310}\u{10EEEE}\u{10EEEE}"
        );
        assert!(placeholder_cells(0, 2, 3).is_empty());
        // rows past the diacritics' count are left out
        assert_eq!(
            placeholder_cells(1, 400, 255).matches('\x1b').count(),
            DIACRITICS.len()
        );
    }
}
