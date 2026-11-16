//! Keys from the bytes a terminal sends for them.

use nsxiv_ng_core::viewer::Keystroke;

/// The longest escape sequence, ESC [ or ESC O to the final byte, in bytes. Keys take a few tens
/// at most, and the longest device attributes some 40.
pub const SEQUENCE_MAX: usize = 128;

/// The longest kitty graphics reply, ESC _ G to ESC \, in bytes. The replies the viewer asks for
/// take a few hundred at most.
pub const REPLY_MAX: usize = 1024;

/// The keys in a burst of terminal input, named as on X11: printable text as typed, control
/// bytes as C-letter, the usual escape sequences by keysym name, None for a key with no name.
/// An escape sequence the burst ends inside comes back as the rest, to join the next burst; a
/// rest of a lone ESC that no more bytes follow is the Escape key, and ESC _ is Alt and `_`.
/// Kitty graphics replies, from ESC _ G to ESC \, are no keys. A sequence or a reply longer than
/// `SEQUENCE_MAX` or `REPLY_MAX` is malformed: it ends there as one key without a name, and the
/// bytes after it go on as keys, so the rest is never longer than `REPLY_MAX`.
// shortcut: C-m and C-[ send the bytes of Return and Escape, so those two bindings need the
// kitty keyboard protocol, which the viewer does not turn on
pub fn keys(bytes: &[u8]) -> (Vec<Option<Keystroke>>, &[u8]) {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            // Escape, or a sequence's start
            let Some(&next) = bytes.get(i + 1) else {
                return (keys, &bytes[i..]);
            };
            // ESC _ G starts a kitty graphics reply, up to ESC \; ESC _ alone is Alt and _, so a
            // burst that ends after ESC _ leaves it to the next one
            if next == 0x5f {
                let Some(&third) = bytes.get(i + 2) else {
                    return (keys, &bytes[i..]);
                };
                if third == 0x47 {
                    let window = bytes.len().min(i + REPLY_MAX);
                    match (i + 2..window).find(|&j| bytes[j] == 0x5c && bytes[j - 1] == 0x1b) {
                        Some(end) => i = end + 1,
                        None if window < i + REPLY_MAX => return (keys, &bytes[i..]),
                        None => {
                            keys.push(None);
                            i = window;
                        }
                    }
                    continue;
                }
            }
            if next != 0x5b && next != 0x4f {
                // not ESC [ or ESC O
                if (0x20..=0x7e).contains(&next) {
                    // Alt and a key: the key, as on X11
                    keys.push(Some(Keystroke::new(&char::from(next).to_string())));
                    i += 2;
                } else {
                    keys.push(Some(Keystroke::new("Escape")));
                    i += 1;
                }
                continue;
            }
            // parameters, then the final byte; any other byte cuts the sequence short, as does a
            // parameter where the longest sequence must have its final byte
            let mut end = i + 2;
            while end < bytes.len()
                && end - i < SEQUENCE_MAX - 1
                && (0x20..=0x3f).contains(&bytes[end])
            {
                end += 1;
            }
            let Some(&last) = bytes.get(end) else {
                return (keys, &bytes[i..]);
            };
            if !(0x40..=0x7e).contains(&last) {
                keys.push(None); // and the byte that cut it starts the next key
                i = end;
                continue;
            }
            // parameter bytes are ASCII
            let params = std::str::from_utf8(&bytes[i + 2..end]).unwrap_or_default();
            let mut params = params.split(';').filter(|p| !p.is_empty());
            let first = params.next();
            // the second parameter is 1 plus the modifier bits, 4 being Ctrl: ESC [1;5A is C-Up
            let ctrl = params
                .next()
                .and_then(|p| p.parse::<i64>().ok())
                .is_some_and(|m| m > 1 && (m - 1) & 4 != 0);
            keys.push(
                sequence_name(last, first, next == 0x4f).map(|key| Keystroke {
                    key: key.to_owned(),
                    ctrl,
                }),
            );
            i = end + 1;
            continue;
        }
        i += 1;
        match b {
            0x0d => keys.push(Some(Keystroke::new("Return"))),
            0x09 => keys.push(Some(Keystroke::new("Tab"))),
            0x7f => keys.push(Some(Keystroke::new("BackSpace"))),
            0x00 => keys.push(Some(Keystroke::ctrl(" "))),
            0x1d => keys.push(Some(Keystroke::ctrl("]"))),
            0x1e => keys.push(Some(Keystroke::ctrl("6"))),
            0x01..=0x1a => keys.push(Some(Keystroke::ctrl(&char::from(b + 0x60).to_string()))),
            0x20..=0x7e => keys.push(Some(Keystroke::new(&char::from(b).to_string()))),
            0x80..=0xbf => {}     // the rest of a character that is not ASCII
            _ => keys.push(None), // another control byte, or a character that is not ASCII
        }
    }
    (keys, &[])
}

// the keysym name of an escape sequence by its final byte and first parameter; ss3 for ESC O
fn sequence_name(last: u8, first: Option<&str>, ss3: bool) -> Option<String> {
    let name = match last {
        0x41 => "Up",
        0x42 => "Down",
        0x43 => "Right",
        0x44 => "Left",
        0x46 => "End",
        0x48 => "Home",
        0x5a => "ISO_Left_Tab", // Shift-Tab
        0x50..=0x53 if ss3 || first == Some("1") => return Some(format!("F{}", last - 0x4f)),
        0x7e => match first? {
            "1" | "7" => "Home",
            "2" => "Insert",
            "3" => "Delete",
            "4" | "8" => "End",
            "5" => "Prior",
            "6" => "Next",
            "11" => "F1",
            "12" => "F2",
            "13" => "F3",
            "14" => "F4",
            "15" => "F5",
            "17" => "F6",
            "18" => "F7",
            "19" => "F8",
            "20" => "F9",
            "21" => "F10",
            "23" => "F11",
            "24" => "F12",
            _ => return None,
        },
        _ => return None,
    };
    Some(name.to_owned())
}

/// A key held over slow frames repeats faster than they draw: a run of three or more of one
/// key in one burst counts once, while a double tap still counts twice.
pub fn collapsing_repeats(keys: &[Option<Keystroke>]) -> Vec<Option<Keystroke>> {
    let mut out = Vec::new();
    for run in keys.chunk_by(|a, b| a == b) {
        out.extend_from_slice(if run.len() >= 3 { &run[..1] } else { run });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: &str) -> Option<Keystroke> {
        Some(Keystroke::new(k))
    }

    fn ctrl(k: &str) -> Option<Keystroke> {
        Some(Keystroke::ctrl(k))
    }

    #[test]
    fn terminal_bytes_arrive_as_the_keys_x11_names() {
        let (keys, rest) = keys("j\x1b[A\x1b[1;5D\x18\r\x7f\x1b[3~\x1bOP\0\u{e9}\x1bj".as_bytes());
        let named = [
            key("j"),
            key("Up"),
            ctrl("Left"),
            ctrl("x"),
            key("Return"),
            key("BackSpace"),
            key("Delete"),
            key("F1"),
            ctrl(" "),
            None,
            key("j"), // Alt-j is j, as on X11
        ];
        assert_eq!(keys, named);
        assert!(rest.is_empty());
    }

    #[test]
    fn broken_sequences_end_as_one_key_without_a_name() {
        // a sequence another escape cuts short is one key without a name, so its end is no D;
        // a modifier as low as i64::MIN cannot overflow
        let (keys, rest) = keys(b"\x1b[\x1b[D\x1b[1;-9223372036854775808A");
        assert_eq!(keys, [None, key("Left"), key("Up")]);
        assert!(rest.is_empty());
    }

    #[test]
    fn a_sequence_a_read_cuts_off_joins_the_next_read() {
        // Left cut in two between reads, where its last byte alone would be D, which drops the
        // image
        let (first, rest) = keys(b"k\x1b[");
        let joined = [rest, b"D"].concat();
        let (second, rest) = keys(&joined);
        assert_eq!(first, [key("k")]);
        assert_eq!(second, [key("Left")]);
        assert!(rest.is_empty());
        // a lone escape waits to tell Escape from a sequence
        assert_eq!(keys(b"\x1b").1, b"\x1b");
    }

    #[test]
    fn kitty_graphics_replies_never_arrive_as_keys() {
        // an answer to the probe that came late is no keys, as its G and = would be
        let (late, rest) = keys(b"j\x1b_Gi=31;OK\x1b\\k\x1b_Gi=3");
        assert_eq!(late, [key("j"), key("k")]);
        assert_eq!(rest, b"\x1b_Gi=3");
        // a read that ends after ESC _ may have cut a reply in two, whose G and the rest are no
        // keys; Alt and _ is still _, as on X11
        let (half, rest) = keys(b"\x1b_");
        assert!(half.is_empty());
        assert_eq!(rest, b"\x1b_");
        let joined = [rest, b"Gi=77,p=1;ENOENT:No such image\x1b\\"].concat();
        assert!(keys(&joined).0.is_empty());
        assert_eq!(keys(b"\x1b_j").0, [key("_"), key("j")]);
    }

    #[test]
    fn keys_typed_during_the_probe_count() {
        // what the probe read: herdr's answers, a j typed meanwhile, and the start of a Left whose
        // D comes in the next read, which must not count alone, as D would drop the image; the
        // device attributes end only a C-x prefix
        let (probed, rest) = keys(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22cj\x1b[");
        assert_eq!(probed, [None, key("j")]);
        assert_eq!(keys(&[rest, b"D"].concat()).0, [key("Left")]);
    }

    #[test]
    fn sequences_name_their_keys() {
        let (named, _) = keys(
            b"\x1b[5~\x1b[6~\x1b[1~\x1b[4~\x1b[2~\x1b[24~\x1b[Z\x1bOQ\
              \x1b[1;5S\x1b[S\x1b[99~\x1b[;5A",
        );
        assert_eq!(
            named,
            [
                key("Prior"),
                key("Next"),
                key("Home"),
                key("End"),
                key("Insert"),
                key("F12"),
                key("ISO_Left_Tab"),
                key("F2"),
                ctrl("F4"),
                None,
                None,
                // an empty parameter is skipped, so 5 is the first and no modifier
                key("Up"),
            ]
        );
        assert_eq!(
            keys(b"\t\x1d\x1e\x1c\x01 ").0,
            [key("Tab"), ctrl("]"), ctrl("6"), None, ctrl("a"), key(" ")]
        );
    }

    // keys of bytes sent one read at a time, prepending each read's rest to the next, and the
    // longest rest seen
    fn read_by_read<'a>(
        start: &[u8],
        reads: impl IntoIterator<Item = &'a [u8]>,
    ) -> (Vec<Option<Keystroke>>, usize) {
        let (mut all, mut longest, mut rest) = (Vec::new(), 0, start.to_vec());
        for read in reads {
            let joined = [&rest[..], read].concat();
            let (keys, left) = keys(&joined);
            all.extend(keys);
            longest = longest.max(left.len());
            rest = left.to_vec();
        }
        (all, longest)
    }

    #[test]
    fn a_sequence_that_never_ends_ends_at_its_longest_and_keys_after_it_arrive() {
        let reads = std::iter::repeat_n(&b"1"[..], 3 * SEQUENCE_MAX).chain([&b"j"[..]]);
        let (got, rest) = read_by_read(b"\x1b[", reads);
        assert!(rest < SEQUENCE_MAX, "a rest of {rest} bytes");
        assert_eq!(got.first(), Some(&None));
        assert_eq!(got.last(), Some(&key("j")));
        // the longest sequence still names its key, and one a byte longer is malformed
        let padded = |zeros| [&b"\x1b["[..], &vec![b'0'; zeros], b"1;5A"].concat();
        let longest = padded(SEQUENCE_MAX - 6);
        assert_eq!(longest.len(), SEQUENCE_MAX);
        assert_eq!(keys(&longest).0, [ctrl("Up")]);
        let one_over = padded(SEQUENCE_MAX - 5);
        let (over, rest) = keys(&one_over);
        assert_eq!((over.first(), over.last()), (Some(&None), Some(&key("A"))));
        assert!(rest.is_empty());
    }

    #[test]
    fn a_reply_that_never_ends_ends_at_its_longest_and_keys_after_it_arrive() {
        let reads = std::iter::repeat_n(&b"x"[..], 3 * REPLY_MAX).chain([&b"j"[..]]);
        let (got, rest) = read_by_read(b"\x1b_G", reads);
        assert!(rest < REPLY_MAX, "a rest of {rest} bytes");
        assert_eq!(got.first(), Some(&None));
        assert_eq!(got.last(), Some(&key("j")));
        assert_eq!(got.iter().filter(|k| k.is_none()).count(), 1);
        // the longest reply is still no keys
        let reply = [&b"\x1b_G"[..], &vec![b'x'; REPLY_MAX - 5], b"\x1b\\"].concat();
        assert_eq!(reply.len(), REPLY_MAX);
        assert_eq!(
            keys(&[&reply[..], b"j"].concat()),
            (vec![key("j")], &[][..])
        );
    }

    #[test]
    fn a_held_keys_repeats_count_once_and_a_double_tap_twice() {
        let mut held = vec![key("j"); 5];
        held.extend([key("k"), key("k")]);
        assert_eq!(collapsing_repeats(&held), [key("j"), key("k"), key("k")]);
        assert_eq!(
            collapsing_repeats(&[None, None, None, key("j")]),
            [None, key("j")]
        );
    }
}
