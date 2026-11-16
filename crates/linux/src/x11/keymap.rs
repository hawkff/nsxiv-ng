//! Keysyms from the server's core keyboard mapping and an event's state, as Xlib's
//! `XLookupString` finds them: the second group, Shift and Lock, the keypad with Num Lock, and
//! levels 3 and 4, which AltGr reaches.
//!
//! For a key with more than two levels the server lays the core mapping out as group 1 levels
//! 1 and 2, group 2 levels 1 and 2, group 1 levels 3 and 4, then group 2 levels 3 and 4.

use xkbcommon::xkb::Keysym;

const SHIFT: u16 = 1 << 0;
const LOCK: u16 = 1 << 1;
const CAPS_LOCK: u32 = 0xffe5;
const SHIFT_LOCK: u32 = 0xffe6;
const ISO_LOCK: u32 = 0xfe01;
const MODE_SWITCH: u32 = 0xff7e;
const NUM_LOCK: u32 = 0xff7f;
const LEVEL3_SHIFT: u32 = 0xfe03;
const VOID_SYMBOL: u32 = 0xff_ffff;

/// What the Lock modifier means, from the keys bound to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lock {
    None,
    Caps,
    Shift,
}

/// The core keyboard mapping and what its modifier mapping makes of Lock, Mode_switch,
/// Num_Lock and ISO_Level3_Shift.
#[derive(Debug)]
pub(crate) struct Keymap {
    min_keycode: u8,
    per_keycode: usize,
    keysyms: Vec<u32>,
    lock: Lock,
    /// The modifiers bound to Mode_switch, which select the second group.
    mode_switch: u16,
    num_lock: u16,
    /// The modifiers bound to ISO_Level3_Shift, which select levels 3 and 4.
    level3: u16,
}

impl Keymap {
    /// The mapping `GetKeyboardMapping` gives from `min_keycode`, with the keycodes
    /// `GetModifierMapping` gives, eight rows of them.
    pub(crate) fn new(
        min_keycode: u8,
        per_keycode: u8,
        keysyms: Vec<u32>,
        modifiers: &[u8],
    ) -> Keymap {
        let keymap = Keymap {
            min_keycode,
            per_keycode: usize::from(per_keycode),
            keysyms,
            lock: Lock::None,
            mode_switch: 0,
            num_lock: 0,
            level3: 0,
        };
        let (mut lock, mut mode_switch, mut num_lock, mut level3) = (Lock::None, 0, 0, 0);
        let row = (modifiers.len() / 8).max(1);
        for (i, &keycode) in modifiers.iter().enumerate() {
            let modifier = i / row;
            for &keysym in keymap.keysyms(keycode) {
                match (modifier, keysym) {
                    (1, CAPS_LOCK | ISO_LOCK) => lock = Lock::Caps,
                    (1, SHIFT_LOCK) if lock != Lock::Caps => lock = Lock::Shift,
                    (3..=7, MODE_SWITCH) => mode_switch |= 1 << modifier,
                    (3..=7, NUM_LOCK) => num_lock |= 1 << modifier,
                    (3..=7, LEVEL3_SHIFT) => level3 |= 1 << modifier,
                    _ => {}
                }
            }
        }
        Keymap {
            lock,
            mode_switch,
            num_lock,
            level3,
            ..keymap
        }
    }

    fn keysyms(&self, keycode: u8) -> &[u32] {
        let Some(index) = keycode.checked_sub(self.min_keycode) else {
            return &[];
        };
        let start = usize::from(index) * self.per_keycode;
        self.keysyms
            .get(start..start + self.per_keycode)
            .unwrap_or(&[])
    }

    /// The keysym of `keycode` under the modifiers and group of an event's `state`, 0 for none.
    /// The group comes from XKB's bits of the state, or from the modifier Mode_switch is bound
    /// to, for a server that gives no XKB state.
    pub(crate) fn keysym(&self, keycode: u8, state: u16) -> u32 {
        let row = self.keysyms(keycode);
        let mut syms = row;
        while syms.len() > 2 && syms[syms.len() - 1] == 0 {
            syms = &syms[..syms.len() - 1];
        }
        // shortcut: a third or fourth group reads the first, as the core mapping has only two
        // groups in a known place; an XKB keymap would read them all
        let second_group = (state >> 13) & 3 == 1 || state & self.mode_switch != 0;
        let (shift, lock) = (state & SHIFT != 0, state & LOCK != 0);
        // levels 3 and 4 of the group, where the key has them, Shift choosing
        let third = if second_group { 6 } else { 4 };
        let level = |i: usize| row.get(third + i).copied().unwrap_or(0);
        let level3 = state & self.level3 != 0 && level(0) != 0;
        if syms.len() > 2 && second_group {
            syms = &syms[2..];
        }
        let sym = |i: usize| syms.get(i).copied().unwrap_or(0);
        // one keysym, or none in the second column, stands for both cases
        let single = syms.len() < 2 || sym(1) == 0;
        let keysym = if level3 {
            if shift && level(1) != 0 {
                level(1)
            } else {
                level(0)
            }
        } else if state & self.num_lock != 0 && is_keypad(sym(1)) {
            if shift || (lock && self.lock == Lock::Shift) {
                sym(0)
            } else {
                sym(1)
            }
        } else if !shift && (!lock || self.lock == Lock::None) {
            if single { case(sym(0)).0 } else { sym(0) }
        } else if !lock || self.lock != Lock::Caps {
            if single { case(sym(0)).1 } else { sym(1) }
        } else {
            // Caps Lock gives the upper case of what Shift would, but leaves a key's second
            // keysym that is no capital to Shift, as 1 and !
            let chosen = if single { sym(0) } else { sym(1) };
            let (lower, upper) = case(chosen);
            if !shift && chosen != sym(0) && (chosen != upper || lower == upper) {
                case(sym(0)).1
            } else {
                upper
            }
        };
        if keysym == VOID_SYMBOL { 0 } else { keysym }
    }
}

fn is_keypad(keysym: u32) -> bool {
    let keysym = Keysym::new(keysym);
    keysym.is_keypad_key() || keysym.is_private_keypad_key()
}

/// The lower and upper case of a keysym, which is both where its character has no other case.
fn case(keysym: u32) -> (u32, u32) {
    let Some(c) = Keysym::new(keysym).key_char() else {
        return (keysym, keysym);
    };
    // only a change of case changes the keysym, so a keypad digit stays on the keypad
    let other = |mapped: &mut dyn Iterator<Item = char>| match (mapped.next(), mapped.next()) {
        (Some(d), None) if d != c => Keysym::from_char(d).raw(),
        _ => keysym,
    };
    (other(&mut c.to_lowercase()), other(&mut c.to_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NUM: u16 = 1 << 4; // Mod2, as Num_Lock is bound below
    const MODE: u16 = 1 << 7; // Mod5, Mode_switch's
    const GROUP_2: u16 = 1 << 13;

    // four keysyms a key from keycode 10: two groups of two cases
    fn keymap() -> Keymap {
        let keys: [[u32; 4]; 7] = [
            [0x71, 0x51, 0x6ca, 0x6ea], // 10: q Q, Cyrillic_shorti _SHORTI
            [0x31, 0x21, 0, 0],         // 11: 1 exclam
            [0xff9c, 0xffb1, 0, 0],     // 12: KP_End KP_1
            [CAPS_LOCK, 0, 0, 0],       // 13
            [NUM_LOCK, 0, 0, 0],        // 14
            [MODE_SWITCH, 0, 0, 0],     // 15
            [0xe9, 0, 0, 0],            // 16: eacute alone
        ];
        // Shift, Lock, Control, Mod1 to Mod5, one keycode each
        let modifiers = [0, 13, 0, 0, 14, 0, 0, 15];
        Keymap::new(10, 4, keys.concat(), &modifiers)
    }

    #[test]
    fn shift_and_caps_lock_choose_the_case() {
        let keymap = keymap();
        assert_eq!(keymap.keysym(10, 0), 0x71);
        assert_eq!(keymap.keysym(10, SHIFT), 0x51);
        assert_eq!(keymap.keysym(10, LOCK), 0x51, "Caps Lock");
        assert_eq!(keymap.keysym(10, SHIFT | LOCK), 0x51);
        assert_eq!(keymap.keysym(11, SHIFT), 0x21);
        assert_eq!(keymap.keysym(11, LOCK), 0x31, "Caps Lock leaves digits");
        // one keysym gives its other case
        assert_eq!(keymap.keysym(16, 0), 0xe9);
        assert_eq!(keymap.keysym(16, SHIFT), 0xc9, "Eacute");
        assert_eq!(keymap.keysym(16, LOCK), 0xc9);
    }

    #[test]
    fn shift_lock_shifts_every_key() {
        let mut keymap = keymap();
        keymap.lock = Lock::Shift;
        assert_eq!(keymap.keysym(11, LOCK), 0x21);
        keymap.lock = Lock::None;
        assert_eq!(keymap.keysym(10, LOCK), 0x71, "a Lock bound to nothing");
        let shift_lock = Keymap::new(10, 1, vec![SHIFT_LOCK], &[0, 10, 0, 0, 0, 0, 0, 0]);
        assert_eq!(shift_lock.lock, Lock::Shift);
    }

    #[test]
    fn the_second_group_comes_from_xkb_or_mode_switch() {
        let keymap = keymap();
        assert_eq!(keymap.mode_switch, MODE);
        assert_eq!(keymap.keysym(10, GROUP_2), 0x6ca);
        assert_eq!(keymap.keysym(10, GROUP_2 | SHIFT), 0x6ea);
        assert_eq!(keymap.keysym(10, MODE), 0x6ca);
        assert_eq!(
            keymap.keysym(11, GROUP_2 | SHIFT),
            0x21,
            "a key with one group keeps it"
        );
        assert_eq!(
            keymap.keysym(10, 2 << 13),
            0x71,
            "a third group reads the first"
        );
    }

    #[test]
    fn num_lock_turns_the_keypad_to_digits_and_shift_back() {
        let keymap = keymap();
        assert_eq!(keymap.num_lock, NUM);
        assert_eq!(keymap.keysym(12, 0), 0xff9c);
        assert_eq!(keymap.keysym(12, NUM), 0xffb1);
        assert_eq!(keymap.keysym(12, NUM | SHIFT), 0xff9c);
        assert_eq!(keymap.keysym(10, NUM), 0x71, "no keypad key");
    }

    #[test]
    fn a_keycode_outside_the_mapping_has_no_keysym() {
        let keymap = keymap();
        assert_eq!(keymap.keysym(9, 0), 0);
        assert_eq!(keymap.keysym(17, 0), 0);
        assert_eq!(keymap.keysym(255, SHIFT), 0);
    }

    // a German layout's rows, eight keysyms a key from keycode 10, with ISO_Level3_Shift bound
    // to the modifier `level3` names, of Mod1 to Mod5 (3 to 7)
    fn german(level3: usize) -> Keymap {
        let keys: [[u32; 8]; 4] = [
            [0x37, 0x2f, 0x37, 0x2f, 0x7b, 0x5c, 0, 0], // 10: 7 slash, braceleft backslash
            [0x71, 0x51, 0x71, 0x51, 0x40, 0, 0, 0],    // 11: q Q, at alone
            [0x61, 0x41, 0x61, 0x41, 0, 0, 0, 0],       // 12: a A, no third level
            // 13: x X, Cyrillic_che _CHE; guillemotright greater, Cyrillic_ie _IE
            [0x78, 0x58, 0x6de, 0x6fe, 0xbb, 0x3e, 0x6c5, 0x6e5],
        ];
        let mut modifiers = [0; 8];
        modifiers[level3] = 14;
        let mut keysyms = keys.concat();
        keysyms.extend([LEVEL3_SHIFT, 0, 0, 0, 0, 0, 0, 0]); // 14
        Keymap::new(10, 8, keysyms, &modifiers)
    }

    #[test]
    fn altgr_reaches_levels_3_and_4_and_shift_chooses_between_them() {
        let mod5 = 1 << 7;
        let keymap = german(7);
        assert_eq!(keymap.level3, mod5);
        assert_eq!(keymap.keysym(10, 0), 0x37);
        assert_eq!(keymap.keysym(10, SHIFT), 0x2f);
        assert_eq!(keymap.keysym(10, mod5), 0x7b, "braceleft");
        assert_eq!(keymap.keysym(10, mod5 | SHIFT), 0x5c, "backslash");
        assert_eq!(keymap.keysym(11, mod5 | SHIFT), 0x40, "no fourth level");
        assert_eq!(keymap.keysym(12, mod5), 0x61, "no third level");
        assert_eq!(keymap.keysym(12, mod5 | SHIFT), 0x41);
        // the second group's levels 3 and 4 come after the first's
        assert_eq!(keymap.keysym(13, GROUP_2), 0x6de);
        assert_eq!(keymap.keysym(13, GROUP_2 | mod5), 0x6c5);
        assert_eq!(keymap.keysym(13, GROUP_2 | mod5 | SHIFT), 0x6e5);
        assert_eq!(keymap.keysym(13, mod5 | SHIFT), 0x3e);
    }

    #[test]
    fn the_level_3_modifier_is_whichever_holds_iso_level3_shift() {
        for modifier in 3..=7 {
            let keymap = german(modifier);
            let held = 1 << modifier;
            assert_eq!(keymap.level3, held, "Mod{}", modifier - 2);
            assert_eq!(keymap.keysym(10, held), 0x7b);
            let other = if modifier == 7 { 1 << 3 } else { 1 << 7 };
            assert_eq!(keymap.keysym(10, other), 0x37, "another modifier");
        }
        // bound to Shift, Lock or Control, it selects nothing, as for Mode_switch
        assert_eq!(german(2).level3, 0);
    }
}
