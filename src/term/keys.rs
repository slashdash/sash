//! Rune key events -> the bytes a pty program expects, via libghostty-vt's encoder.

use rune::{Key, KeyEvent, Mods};

use crate::ffi;

use super::Vt;

pub(crate) fn encode(vt: &mut Vt, ev: &KeyEvent) -> Vec<u8> {
    let Some(key) = map_key(&ev.key) else {
        return Vec::new();
    };
    let text = match &ev.key {
        Key::Text(s) => Some(s.as_str()),
        _ => None,
    };
    let unshifted = text.and_then(|t| t.chars().next()).map(unshifted_codepoint);

    let mods = mods_bits(ev.mods);
    let (encoder, event, key_text) = vt.raw();

    unsafe {
        let _ = ffi::ghostty_key_event_set_action(event, ffi::GHOSTTY_KEY_ACTION_PRESS);
        let _ = ffi::ghostty_key_event_set_key(event, key);
        let _ = ffi::ghostty_key_event_set_mods(event, mods);
        // libghostty-vt keeps the pointer instead of copying, so the bytes live in
        // `Vt::key_text` and are only reused by the next encode.
        let bytes = fill_text(key_text, text);
        let _ = ffi::ghostty_key_event_set_utf8(event, bytes);
        let _ = ffi::ghostty_key_event_set_unshifted_codepoint(event, unshifted.unwrap_or(0));

        let mut buf = [0u8; 128];
        let mut written = 0usize;
        let mut r = ffi::ghostty_key_encoder_encode(
            encoder,
            event,
            buf.as_mut_ptr(),
            buf.len(),
            &mut written,
        );
        if r == ffi::GHOSTTY_OUT_OF_SPACE {
            let mut big = vec![0u8; written];
            r = ffi::ghostty_key_encoder_encode(
                encoder,
                event,
                big.as_mut_ptr(),
                big.len(),
                &mut written,
            );
            if r == ffi::GHOSTTY_SUCCESS {
                return big[..written].to_vec();
            }
            return Vec::new();
        }
        if r != ffi::GHOSTTY_SUCCESS {
            return Vec::new();
        }
        buf[..written].to_vec()
    }
}

/// Copy `text` into the event's backing buffer and point libghostty at it. The
/// buffer is cleared for non-text keys. libghostty-vt does not copy what it is given,
/// so the returned string is valid only while `key_text` lives and keeps its allocation.
fn fill_text(key_text: &mut Vec<u8>, text: Option<&str>) -> ffi::GhosttyString {
    key_text.clear();
    if let Some(text) = text {
        key_text.extend_from_slice(text.as_bytes());
    }
    if key_text.is_empty() {
        ffi::GhosttyString::default()
    } else {
        ffi::GhosttyString {
            ptr: key_text.as_ptr(),
            len: key_text.len(),
        }
    }
}

fn mods_bits(m: Mods) -> ffi::GhosttyMods {
    let mut bits = 0u16;
    if m.shift {
        bits |= ffi::GHOSTTY_MODS_SHIFT as u16;
    }
    if m.ctrl {
        bits |= ffi::GHOSTTY_MODS_CTRL as u16;
    }
    if m.alt {
        bits |= ffi::GHOSTTY_MODS_ALT as u16;
    }
    if m.cmd || m.logo {
        bits |= ffi::GHOSTTY_MODS_SUPER as u16;
    }
    bits
}

fn unshifted_codepoint(c: char) -> u32 {
    let c = c.to_ascii_lowercase();
    c as u32
}

fn map_key(key: &Key) -> Option<ffi::GhosttyKey> {
    use ffi::*;
    Some(match key {
        Key::Text(s) => {
            let mut chars = s.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                GHOSTTY_KEY_UNIDENTIFIED
            } else {
                char_key(first)
            }
        }
        // Cmd chords belong to the app shell (tabs, copy, paste), never the pty.
        Key::Command(_) => return None,
        Key::Enter => GHOSTTY_KEY_ENTER,
        Key::Backspace => GHOSTTY_KEY_BACKSPACE,
        Key::Delete => GHOSTTY_KEY_DELETE,
        Key::Left => GHOSTTY_KEY_ARROW_LEFT,
        Key::Right => GHOSTTY_KEY_ARROW_RIGHT,
        Key::Up => GHOSTTY_KEY_ARROW_UP,
        Key::Down => GHOSTTY_KEY_ARROW_DOWN,
        Key::Home => GHOSTTY_KEY_HOME,
        Key::End => GHOSTTY_KEY_END,
        Key::PageUp => GHOSTTY_KEY_PAGE_UP,
        Key::PageDown => GHOSTTY_KEY_PAGE_DOWN,
        Key::Insert => GHOSTTY_KEY_INSERT,
        Key::PrintScreen => GHOSTTY_KEY_PRINT_SCREEN,
        Key::ScrollLock => GHOSTTY_KEY_SCROLL_LOCK,
        Key::Pause => GHOSTTY_KEY_PAUSE,
        Key::Escape => GHOSTTY_KEY_ESCAPE,
        Key::Tab => GHOSTTY_KEY_TAB,
        Key::ContextMenu => GHOSTTY_KEY_CONTEXT_MENU,
        Key::Function(n) => function_key(*n)?,
        Key::Modifier { .. } => return None,
        _ => return None,
    })
}

fn function_key(n: u8) -> Option<ffi::GhosttyKey> {
    use ffi::*;
    Some(match n {
        1 => GHOSTTY_KEY_F1,
        2 => GHOSTTY_KEY_F2,
        3 => GHOSTTY_KEY_F3,
        4 => GHOSTTY_KEY_F4,
        5 => GHOSTTY_KEY_F5,
        6 => GHOSTTY_KEY_F6,
        7 => GHOSTTY_KEY_F7,
        8 => GHOSTTY_KEY_F8,
        9 => GHOSTTY_KEY_F9,
        10 => GHOSTTY_KEY_F10,
        11 => GHOSTTY_KEY_F11,
        12 => GHOSTTY_KEY_F12,
        13 => GHOSTTY_KEY_F13,
        14 => GHOSTTY_KEY_F14,
        15 => GHOSTTY_KEY_F15,
        16 => GHOSTTY_KEY_F16,
        17 => GHOSTTY_KEY_F17,
        18 => GHOSTTY_KEY_F18,
        19 => GHOSTTY_KEY_F19,
        20 => GHOSTTY_KEY_F20,
        21 => GHOSTTY_KEY_F21,
        22 => GHOSTTY_KEY_F22,
        23 => GHOSTTY_KEY_F23,
        24 => GHOSTTY_KEY_F24,
        _ => return None,
    })
}

/// The physical key a produced character sits on (US layout), plus the character's
/// codepoint for the kitty protocol's "unshifted key" field.
fn char_key(c: char) -> ffi::GhosttyKey {
    use ffi::*;
    match c {
        'a'..='z' | 'A'..='Z' => letter_key(c.to_ascii_lowercase()),
        '0' => GHOSTTY_KEY_DIGIT_0,
        '1' | '!' => GHOSTTY_KEY_DIGIT_1,
        '2' | '@' => GHOSTTY_KEY_DIGIT_2,
        '3' | '#' => GHOSTTY_KEY_DIGIT_3,
        '4' | '$' => GHOSTTY_KEY_DIGIT_4,
        '5' | '%' => GHOSTTY_KEY_DIGIT_5,
        '6' | '^' => GHOSTTY_KEY_DIGIT_6,
        '7' | '&' => GHOSTTY_KEY_DIGIT_7,
        '8' | '*' => GHOSTTY_KEY_DIGIT_8,
        '9' | '(' => GHOSTTY_KEY_DIGIT_9,
        ')' => GHOSTTY_KEY_DIGIT_0,
        ' ' => GHOSTTY_KEY_SPACE,
        '`' | '~' => GHOSTTY_KEY_BACKQUOTE,
        '-' | '_' => GHOSTTY_KEY_MINUS,
        '=' | '+' => GHOSTTY_KEY_EQUAL,
        '[' | '{' => GHOSTTY_KEY_BRACKET_LEFT,
        ']' | '}' => GHOSTTY_KEY_BRACKET_RIGHT,
        '\\' | '|' => GHOSTTY_KEY_BACKSLASH,
        ';' | ':' => GHOSTTY_KEY_SEMICOLON,
        '\'' | '"' => GHOSTTY_KEY_QUOTE,
        ',' | '<' => GHOSTTY_KEY_COMMA,
        '.' | '>' => GHOSTTY_KEY_PERIOD,
        '/' | '?' => GHOSTTY_KEY_SLASH,
        _ => GHOSTTY_KEY_UNIDENTIFIED,
    }
}

fn letter_key(c: char) -> ffi::GhosttyKey {
    use ffi::*;
    match c {
        'a' => GHOSTTY_KEY_A,
        'b' => GHOSTTY_KEY_B,
        'c' => GHOSTTY_KEY_C,
        'd' => GHOSTTY_KEY_D,
        'e' => GHOSTTY_KEY_E,
        'f' => GHOSTTY_KEY_F,
        'g' => GHOSTTY_KEY_G,
        'h' => GHOSTTY_KEY_H,
        'i' => GHOSTTY_KEY_I,
        'j' => GHOSTTY_KEY_J,
        'k' => GHOSTTY_KEY_K,
        'l' => GHOSTTY_KEY_L,
        'm' => GHOSTTY_KEY_M,
        'n' => GHOSTTY_KEY_N,
        'o' => GHOSTTY_KEY_O,
        'p' => GHOSTTY_KEY_P,
        'q' => GHOSTTY_KEY_Q,
        'r' => GHOSTTY_KEY_R,
        's' => GHOSTTY_KEY_S,
        't' => GHOSTTY_KEY_T,
        'u' => GHOSTTY_KEY_U,
        'v' => GHOSTTY_KEY_V,
        'w' => GHOSTTY_KEY_W,
        'x' => GHOSTTY_KEY_X,
        'y' => GHOSTTY_KEY_Y,
        'z' => GHOSTTY_KEY_Z,
        _ => GHOSTTY_KEY_UNIDENTIFIED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_text_reuses_its_buffer_and_clears_for_non_text_keys() {
        let mut key_text = Vec::new();
        let text = fill_text(&mut key_text, Some("é"));
        assert_eq!(
            unsafe { std::slice::from_raw_parts(text.ptr, text.len) },
            "é".as_bytes()
        );
        let capacity = key_text.capacity();

        let cleared = fill_text(&mut key_text, None);
        assert!(cleared.ptr.is_null());
        assert_eq!(cleared.len, 0);
        assert_eq!(
            key_text.capacity(),
            capacity,
            "the buffer must be reused, not replaced"
        );

        let text = fill_text(&mut key_text, Some("a"));
        assert_eq!(
            unsafe { std::slice::from_raw_parts(text.ptr, text.len) },
            b"a"
        );
        assert_eq!(key_text.capacity(), capacity);
    }
}
