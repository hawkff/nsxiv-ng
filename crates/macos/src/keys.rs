//! A key's name for the viewer, from what AppKit says of it.

/// The virtual key code of Escape, which AppKit names by no special key.
const ESCAPE: u16 = 53;

/// The name of the key with virtual code `code` that typed `characters` without modifiers but
/// Shift, as the bindings take it: Escape, Return, the arrows and BackSpace by name,
/// a special key else (Tab, keypad Enter, F1 and the like) none, and any other key its text.
/// A special key is one AppKit names as an `NSEvent.SpecialKey`: the function keys'
/// private range, and a few control characters.
pub(crate) fn key_name(code: u16, characters: &str) -> Option<String> {
    if code == ESCAPE {
        return Some("Escape".to_owned());
    }
    let first = u32::from(characters.chars().next()?);
    let name = match first {
        0x0d => "Return",
        0xf702 => "Left",
        0xf703 => "Right",
        0xf700 => "Up",
        0xf701 => "Down",
        0x08 | 0x7f => "BackSpace",
        // enter, tab, newline, form feed, back tab and the line and paragraph separators
        0x03 | 0x09 | 0x0a | 0x0c | 0x19 | 0x2028 | 0x2029 | 0xf700..=0xf8ff => return None,
        _ => return Some(characters.to_owned()),
    };
    Some(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_named_as_the_bindings_name_them() {
        assert_eq!(key_name(53, "\u{1b}").as_deref(), Some("Escape"));
        assert_eq!(key_name(36, "\r").as_deref(), Some("Return"));
        assert_eq!(key_name(123, "\u{f702}").as_deref(), Some("Left"));
        assert_eq!(key_name(124, "\u{f703}").as_deref(), Some("Right"));
        assert_eq!(key_name(126, "\u{f700}").as_deref(), Some("Up"));
        assert_eq!(key_name(125, "\u{f701}").as_deref(), Some("Down"));
        assert_eq!(key_name(51, "\u{7f}").as_deref(), Some("BackSpace"));
        assert_eq!(key_name(117, "\u{8}").as_deref(), Some("BackSpace"));
        // a letter, with Shift as typed; Control keeps the letter, which the flag carries
        assert_eq!(key_name(0, "a").as_deref(), Some("a"));
        assert_eq!(key_name(0, "A").as_deref(), Some("A"));
        assert_eq!(key_name(24, "+").as_deref(), Some("+"));
        // Tab, keypad Enter and F1 have no name, and nor has a dead key's empty text
        assert_eq!(key_name(48, "\t"), None);
        assert_eq!(key_name(76, "\u{3}"), None);
        assert_eq!(key_name(122, "\u{f704}"), None);
        assert_eq!(key_name(32, ""), None);
    }
}
