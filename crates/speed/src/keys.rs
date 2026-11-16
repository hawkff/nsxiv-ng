//! From the names of nsxiv's keys to the keycodes XTEST presses.

/// The keysym of the left Shift key.
pub const SHIFT_L: u32 = 0xffe1;
/// The keysym of the left Control key.
pub const CONTROL_L: u32 = 0xffe3;
/// The keysym of the right Hyper key, the last of the modifiers.
pub const HYPER_R: u32 = 0xffee;

/// The server's core keyboard mapping: `per` keysyms for each keycode from `min_keycode` on.
pub struct Keymap {
    pub min_keycode: u8,
    pub per: usize,
    pub keysyms: Vec<u32>,
}

impl Keymap {
    /// The first keycode that has `keysym` without Shift, else the first that has it with Shift,
    /// and whether Shift is needed.
    fn find(&self, keysym: u32) -> Option<(u8, bool)> {
        (0..self.per.min(2)).find_map(|column| {
            let i = self
                .keysyms
                .chunks(self.per)
                .position(|syms| syms[column] == keysym)?;
            Some((
                self.min_keycode.checked_add(u8::try_from(i).ok()?)?,
                column == 1,
            ))
        })
    }

    /// The keysym a keycode types without modifiers.
    pub fn keysym(&self, keycode: u8) -> u32 {
        let i = usize::from(keycode.wrapping_sub(self.min_keycode)) * self.per;
        self.keysyms.get(i).copied().unwrap_or(0)
    }

    /// The keycodes to press, in order, for a key named as nsxiv's bindings name it: one
    /// printable ASCII character, after `C-` when Control is held. They are released in reverse.
    pub fn press(&self, name: &str) -> Option<Vec<u8>> {
        let (ctrl, key) = name
            .strip_prefix("C-")
            .map_or((false, name), |key| (true, key));
        let mut chars = key.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return None;
        };
        if !(' '..='~').contains(&c) {
            return None;
        }
        // printable ASCII keysyms are the characters' codes
        let (keycode, shift) = self.find(u32::from(c))?;
        let mut codes = Vec::new();
        if ctrl {
            codes.push(self.find(CONTROL_L)?.0);
        }
        if shift {
            codes.push(self.find(SHIFT_L)?.0);
        }
        codes.push(keycode);
        Some(codes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // keycodes 10 to 15 of a US layout: j J, equal plus, bracketright braceright, KP_Add twice,
    // Shift_L, Control_L
    fn keymap() -> Keymap {
        Keymap {
            min_keycode: 10,
            per: 4,
            keysyms: vec![
                0x6a, 0x4a, 0, 0, //
                0x3d, 0x2b, 0, 0, //
                0x5d, 0x7d, 0, 0, //
                0xffab, 0xffab, 0, 0, //
                SHIFT_L, 0, 0, 0, //
                CONTROL_L, 0, 0, 0,
            ],
        }
    }

    #[test]
    fn a_key_without_shift_is_its_keycode_alone() {
        assert_eq!(keymap().press("j"), Some(vec![10]));
        assert_eq!(keymap().press("]"), Some(vec![12]));
        assert_eq!(keymap().press("="), Some(vec![11]));
    }

    #[test]
    fn a_shifted_key_takes_shift_first() {
        assert_eq!(keymap().press("+"), Some(vec![14, 11]));
        assert_eq!(keymap().press("J"), Some(vec![14, 10]));
    }

    #[test]
    fn control_comes_before_the_key() {
        assert_eq!(keymap().press("C-j"), Some(vec![15, 10]));
        assert_eq!(keymap().press("C-}"), Some(vec![15, 14, 12]));
    }

    #[test]
    fn keys_the_layout_lacks_or_names_it_cannot_type_give_none() {
        assert_eq!(keymap().press("q"), None);
        assert_eq!(keymap().press("Return"), None);
        assert_eq!(keymap().press(""), None);
        assert_eq!(keymap().press("é"), None);
    }

    #[test]
    fn a_keycode_types_its_first_keysym() {
        assert_eq!(keymap().keysym(11), 0x3d);
        assert_eq!(keymap().keysym(15), CONTROL_L);
        assert_eq!(keymap().keysym(200), 0);
        assert_eq!(keymap().keysym(3), 0);
    }
}
