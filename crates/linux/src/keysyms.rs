//! The key name the bindings use for a keysym. X11 and xkbcommon share keysym values and
//! names, so both window systems name their keys here.

use xkbcommon::xkb::{self, Keysym};

/// The key's name for the bindings: printable ASCII as typed, any other key by its keysym's
/// name, and None for a modifier alone, for no key, or for a keysym without a name.
pub fn key_name(keysym: Keysym) -> Option<String> {
    match keysym.raw() {
        raw @ 0x20..=0x7e => Some(char::from(raw as u8).to_string()),
        // no symbol, and every modifier: Shift_L to Hyper_R, the ISO locks, shifts and
        // latches, Mode_switch and Num_Lock
        0 => None,
        _ if keysym.is_modifier_key() => None,
        _ => Some(xkb::keysym_get_name(keysym)).filter(|name| !name.is_empty()),
    }
}

/// The character a key types, as the session's text line takes it: None for a key that types
/// none, or a control character, as Return and BackSpace do, which go by their names.
// shortcut: a key at a time, with no input method: a dead key types nothing and the letter
// after it types alone, and Compose sequences are not composed; go through xkbcommon's compose
// tables, or an input method, if text of accented letters is typed so
pub fn typed(keysym: Keysym) -> Option<String> {
    let c = char::from_u32(xkb::keysym_to_utf32(keysym))?;
    (c != '\0' && !c.is_control()).then(|| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(raw: u32) -> Option<String> {
        key_name(Keysym::new(raw))
    }

    #[test]
    fn keys_are_named_as_typed_or_by_their_keysym() {
        assert_eq!(name(0x6a).as_deref(), Some("j"));
        assert_eq!(name(0x4a).as_deref(), Some("J"));
        assert_eq!(name(0x20).as_deref(), Some(" "));
        assert_eq!(name(0x7e).as_deref(), Some("~"));
        assert_eq!(name(0xff0d).as_deref(), Some("Return"));
        assert_eq!(name(0xff1b).as_deref(), Some("Escape"));
        assert_eq!(name(0xff08).as_deref(), Some("BackSpace"));
        assert_eq!(name(0xff51).as_deref(), Some("Left"));
        assert_eq!(name(0xff55).as_deref(), Some("Prior"));
        assert_eq!(name(0xff8d).as_deref(), Some("KP_Enter"));
        assert_eq!(name(0xe9).as_deref(), Some("eacute"), "past ASCII, by name");
    }

    #[test]
    fn a_key_types_its_character_and_one_of_control_none() {
        let typed = |raw| typed(Keysym::new(raw));
        assert_eq!(typed(0x6a).as_deref(), Some("j"));
        assert_eq!(typed(0xe9).as_deref(), Some("é"), "eacute");
        assert_eq!(typed(0x20ac).as_deref(), Some("€"), "EuroSign");
        assert_eq!(typed(0xff0d), None, "Return");
        assert_eq!(typed(0xff08), None, "BackSpace");
        assert_eq!(typed(0xff51), None, "Left");
    }

    #[test]
    fn a_modifier_alone_is_no_key() {
        for modifier in [
            0xffe1, 0xffe3, 0xffe9, 0xffeb, 0xffee, 0xff7e, 0xff7f, 0xfe03,
            // ISO_Level3_Latch and ISO_Level5_Shift
            0xfe04, 0xfe11,
        ] {
            assert_eq!(name(modifier), None, "{modifier:#x}");
        }
        assert_eq!(name(0), None, "no symbol");
    }
}
