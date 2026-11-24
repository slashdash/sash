//! Pure input helpers: link detection, paste shaping, wheel accumulation.

/// A whole-line confirmation is needed when the paste would run more than one command.
pub fn needs_paste_confirmation(text: &str) -> bool {
    let multiline = text.chars().any(|c| c == '\n' || c == '\r');
    let has_content = text.chars().any(|c| !c.is_whitespace());
    multiline && has_content
}

/// Quote one path for a POSIX shell, leaving unremarkable paths alone.
///
/// Quoting stops shell metacharacters, not option parsing: `'-rf'` is still an
/// option to the program receiving it unless the caller passes `--`. Absolute
/// paths from Finder are unaffected.
pub fn shell_quote(path: &str) -> String {
    if !path.is_empty()
        && !path.starts_with('-')
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+'))
    {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

/// Drop bytes the terminal handles as input rather than text: ESC, C0 controls
/// other than tab/LF/CR, DEL and the C1 controls. A clip containing `ESC[201~`
/// would otherwise end bracketed paste early and the rest would run as typed
/// keystrokes.
fn sanitize_paste(text: &str) -> String {
    text.chars()
        .filter(|c| matches!(c, '\t' | '\n' | '\r') || !c.is_control())
        .collect()
}

/// Bytes to send for a paste, honoring bracketed-paste mode.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = sanitize_paste(text);
    if bracketed {
        let mut bytes = Vec::with_capacity(text.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        bytes
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// The http(s) link at cell `x` of `row`, if any. Accepts `www.` by prefixing
/// `https://`; strips trailing punctuation `.,;:!?)]}'"` and a trailing slash only
/// when it is punctuation-ending; rejects anything without a scheme after normalization.
pub fn link_at(row: &str, x: usize) -> Option<String> {
    let mut tokens: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, ch) in row.chars().enumerate() {
        if ch.is_whitespace() {
            if let Some(s) = start.take() {
                tokens.push((s, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        tokens.push((s, row.chars().count()));
    }
    let (token_start, token_end) = tokens.into_iter().find(|(s, e)| x >= *s && x < *e)?;
    let token: String = row
        .chars()
        .skip(token_start)
        .take(token_end - token_start)
        .collect();
    normalize_link(&token)
}

fn normalize_link(token: &str) -> Option<String> {
    let owned;
    let candidate = if token.starts_with("www.") {
        owned = format!("https://{token}");
        owned.as_str()
    } else {
        token
    };
    let lower = candidate.to_ascii_lowercase();
    let scheme_len = if lower.starts_with("https://") {
        8
    } else if lower.starts_with("http://") {
        7
    } else {
        return None;
    };
    let punct = |c: char| {
        matches!(
            c,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"'
        )
    };
    let trimmed = candidate.trim_end_matches(punct);
    let had_punct = trimmed.len() != candidate.len();
    let mut end = trimmed.len();
    if had_punct && trimmed.ends_with('/') {
        end -= 1;
    }
    let candidate = candidate.get(..end)?;
    let rest = candidate.get(scheme_len..)?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return None;
    }
    Some(candidate.to_string())
}

/// Accumulates pixel wheel deltas into whole terminal rows.
#[derive(Default)]
pub struct WheelAccumulator {
    acc: f32,
}

impl WheelAccumulator {
    /// `dy` is Rune's `WheelEvent::dy`: positive when content should move down
    /// (natural scrolling), i.e. toward older output. Returns whole rows to scroll:
    /// positive = older/up. Keeps the sub-row remainder and returns 0 until a full row.
    pub fn rows(&mut self, dy: f32, cell_h: f32) -> i32 {
        if !dy.is_finite() {
            return 0;
        }
        let cell_h = if cell_h.is_finite() && cell_h > 0.0 {
            cell_h
        } else {
            1.0
        };
        self.acc += dy;
        if !self.acc.is_finite() {
            self.acc = 0.0;
            return 0;
        }
        let whole = (self.acc / cell_h).trunc();
        if whole == 0.0 {
            return 0;
        }
        self.acc -= whole * cell_h;
        whole as i32
    }

    pub fn reset(&mut self) {
        self.acc = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_confirmation_only_for_multiline_content() {
        assert!(needs_paste_confirmation("ls\nrm -rf build"));
        assert!(needs_paste_confirmation("a\rb"));
        assert!(!needs_paste_confirmation("ls -la"));
        assert!(!needs_paste_confirmation("\n \r\n"));
        assert!(!needs_paste_confirmation(""));
    }

    #[test]
    fn paste_bytes_wraps_in_bracketed_mode() {
        assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
    }

    #[test]
    fn paste_bytes_normalizes_newlines_when_unbracketed() {
        assert_eq!(paste_bytes("a\r\nb\nc\rd", false), b"a\rb\rc\rd".to_vec());
    }

    #[test]
    fn paste_bytes_strips_escape_and_control_bytes() {
        let bytes = paste_bytes("\x1b[201~rm -rf /\x07\tok\ncaf\u{9b}2J\x7f", true);
        assert!(bytes.starts_with(b"\x1b[200~"));
        assert!(bytes.ends_with(b"\x1b[201~"));
        // Exactly the two wrapper ESC bytes survive, so one wrapper pair remains.
        assert_eq!(bytes.iter().filter(|&&b| b == 0x1b).count(), 2);
        let inner = &bytes[6..bytes.len() - 6];
        assert_eq!(inner, b"[201~rm -rf /\tok\ncaf2J");
        assert!(
            !inner
                .iter()
                .any(|&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r'))
        );
        assert!(!inner.contains(&0x7f));
    }

    #[test]
    fn paste_bytes_strips_controls_when_unbracketed() {
        assert_eq!(
            paste_bytes("a\x1b[201~b\x07\na", false),
            b"a[201~b\ra".to_vec()
        );
    }

    #[test]
    fn link_in_a_sentence() {
        let row = "see https://example.com/a?b=1 now";
        assert_eq!(
            link_at(row, 10).as_deref(),
            Some("https://example.com/a?b=1")
        );
    }

    #[test]
    fn link_with_trailing_period() {
        let row = "visit https://example.com/page.";
        assert_eq!(
            link_at(row, 10).as_deref(),
            Some("https://example.com/page")
        );
        assert_eq!(
            link_at("https://example.com/.", 0).as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn www_link_gets_a_scheme() {
        assert_eq!(
            link_at("www.example.com/path", 4).as_deref(),
            Some("https://www.example.com/path")
        );
    }

    #[test]
    fn scp_style_target_is_not_a_link() {
        assert_eq!(link_at("git@github.com:x", 0), None);
    }

    #[test]
    fn index_on_whitespace_is_not_a_link() {
        assert_eq!(link_at("see https://x.y", 3), None);
    }

    #[test]
    fn unicode_before_the_link() {
        let row = "αβ https://example.com";
        assert_eq!(link_at(row, 3).as_deref(), Some("https://example.com"));
        assert_eq!(link_at(row, 2), None);
    }

    #[test]
    fn empty_host_is_rejected() {
        assert_eq!(link_at("https://", 0), None);
        assert_eq!(link_at("https://.", 0), None);
    }

    #[test]
    fn paths_are_quoted_for_the_shell() {
        assert_eq!(
            shell_quote("/tmp/plain-file_1.txt"),
            "/tmp/plain-file_1.txt"
        );
        assert_eq!(shell_quote("/tmp/My File.txt"), "'/tmp/My File.txt'");
        assert_eq!(shell_quote("/tmp/it's"), "'/tmp/it'\\''s'");
    }

    #[test]
    fn awkward_paths_are_quoted() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("~"), "'~'");
        assert_eq!(shell_quote("~/notes"), "'~/notes'");
        assert_eq!(shell_quote("-rf"), "'-rf'");
    }

    #[test]
    fn wheel_accumulates_fractional_deltas() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(wheel.rows(5.0, 10.0), 0);
        assert_eq!(wheel.rows(5.0, 10.0), 1);
        assert_eq!(wheel.rows(4.0, 10.0), 0);
        assert_eq!(wheel.rows(6.0, 10.0), 1);
        assert_eq!(wheel.rows(2.5, 0.0), 2);
        assert_eq!(wheel.rows(0.5, -1.0), 1);
    }

    #[test]
    fn wheel_handles_negative_direction() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(wheel.rows(-15.0, 10.0), -1);
        assert_eq!(wheel.rows(-5.0, 10.0), -1);
        assert_eq!(wheel.rows(3.0, 10.0), 0);
    }

    #[test]
    fn wheel_reset_drops_the_remainder() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(wheel.rows(9.0, 10.0), 0);
        wheel.reset();
        assert_eq!(wheel.rows(9.0, 10.0), 0);
        assert_eq!(wheel.rows(1.0, 10.0), 1);
    }
}
