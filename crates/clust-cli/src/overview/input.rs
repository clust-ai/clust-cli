use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Convert a crossterm `KeyEvent` into the raw byte sequence a terminal
/// would send for that key. Returns `None` for keys that should not be
/// forwarded (e.g. modifier-only presses).
pub fn key_event_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+letter → ASCII control code (0x01–0x1a)
            if c.is_ascii_lowercase() || c.is_ascii_uppercase() {
                Some(vec![(c.to_ascii_lowercase() as u8) & 0x1f])
            } else {
                match c {
                    // Common ctrl sequences for special chars
                    '@' => Some(vec![0x00]),
                    '[' => Some(vec![0x1b]),
                    '\\' => Some(vec![0x1c]),
                    ']' => Some(vec![0x1d]),
                    '^' => Some(vec![0x1e]),
                    '_' => Some(vec![0x1f]),
                    _ => None,
                }
            }
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            Some(s.as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(n) => Some(f_key_bytes(n)),
        _ => None,
    }
}

fn f_key_bytes(n: u8) -> Vec<u8> {
    let code = match n {
        1 => "OP",
        2 => "OQ",
        3 => "OR",
        4 => "OS",
        5 => "[15~",
        6 => "[17~",
        7 => "[18~",
        8 => "[19~",
        9 => "[20~",
        10 => "[21~",
        11 => "[23~",
        12 => "[24~",
        _ => return Vec::new(),
    };
    let mut bytes = vec![0x1b];
    bytes.extend_from_slice(code.as_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn make_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn test_regular_char() {
        let bytes = key_event_to_bytes(&make_key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(bytes, Some(vec![b'a']));
    }

    #[test]
    fn test_ctrl_c() {
        let bytes = key_event_to_bytes(&make_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(bytes, Some(vec![0x03]));
    }

    #[test]
    fn test_enter() {
        let bytes = key_event_to_bytes(&make_key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(bytes, Some(vec![b'\r']));
    }

    #[test]
    fn test_arrow_up() {
        let bytes = key_event_to_bytes(&make_key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(bytes, Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn test_f1() {
        let bytes = key_event_to_bytes(&make_key(KeyCode::F(1), KeyModifiers::NONE));
        assert_eq!(bytes, Some(b"\x1bOP".to_vec()));
    }

    #[test]
    fn test_unicode() {
        let bytes = key_event_to_bytes(&make_key(KeyCode::Char('ñ'), KeyModifiers::NONE));
        assert_eq!(bytes, Some("ñ".as_bytes().to_vec()));
    }
}
