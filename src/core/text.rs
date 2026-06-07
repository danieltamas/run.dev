//! Pure text helpers shared by the UI and the controller.
//!
//! [`strip_ansi`] removes escape sequences and control characters from raw
//! process output so it can be rendered by ratatui (whose cell model is wrecked
//! by raw ANSI) *and* copied to the clipboard as clean, pasteable text. Keeping
//! it here — rather than in `ui::logs` — lets both the renderer and the copy
//! path in `app` share one implementation.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

/// Strip ANSI escape sequences and control characters from a string so raw
/// process output doesn't interfere with ratatui's own rendering (or leak
/// escape codes into the clipboard).
///
/// Handles CSI (`\x1b[...X`), OSC (`\x1b]...BEL/ST`), and two-byte sequences
/// (`\x1b(B`, `\x1b>`, etc.). Also strips `\r`, `\x08`, and other C0 controls
/// that can corrupt terminal state over long-running sessions. Newlines are
/// preserved (lines are already split on `\n` upstream).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    // CSI sequence: \x1b[ ... <final byte 0x40–0x7E>
                    chars.next();
                    for c2 in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c2) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC sequence: \x1b] ... (terminated by BEL \x07 or ST \x1b\\)
                    chars.next();
                    while let Some(c2) = chars.next() {
                        if c2 == '\x07' {
                            break;
                        }
                        if c2 == '\x1b' {
                            let _ = chars.next(); // consume the '\\' of ST
                            break;
                        }
                    }
                }
                Some(&c1) => {
                    // ESC followed by an intermediate byte (0x20–0x2F) is a
                    // multi-byte sequence such as a charset designation
                    // (`\x1b(B`, `\x1b)0`): consume intermediates through the
                    // final byte (0x30–0x7E). Otherwise it's a plain two-byte
                    // sequence (`\x1b>`, `\x1b=`) — consume just the one byte.
                    chars.next();
                    if ('\x20'..='\x2f').contains(&c1) {
                        for c2 in chars.by_ref() {
                            if ('\x30'..='\x7e').contains(&c2) {
                                break;
                            }
                        }
                    }
                }
                None => {}
            }
        } else if c == '\r' || c == '\x08' || (c.is_control() && c != '\n') {
            // Strip carriage returns, backspaces, and other C0 controls
            // (keep \n since lines are already split by newline)
            continue;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(strip_ansi("hello world"), "hello world");
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn strips_csi_color_codes() {
        // red "error" reset
        assert_eq!(strip_ansi("\x1b[31merror\x1b[0m"), "error");
        // bold + bright cyan, multi-param
        assert_eq!(strip_ansi("\x1b[1;36mhi\x1b[0m there"), "hi there");
    }

    #[test]
    fn strips_osc_sequences() {
        // OSC terminated by BEL
        assert_eq!(strip_ansi("\x1b]0;window title\x07kept"), "kept");
        // OSC terminated by ST (ESC \)
        assert_eq!(strip_ansi("\x1b]8;;http://x\x1b\\link"), "link");
    }

    #[test]
    fn strips_two_byte_sequences() {
        // True two-byte sequences (ESC + final byte).
        assert_eq!(strip_ansi("\x1b>plain"), "plain");
        assert_eq!(strip_ansi("\x1b=text"), "text");
    }

    #[test]
    fn strips_charset_designation_sequences() {
        // ESC + intermediate '(' + final 'B' is three bytes — no stray 'B'.
        assert_eq!(strip_ansi("\x1b(Bplain"), "plain");
        assert_eq!(strip_ansi("\x1b)0graph"), "graph");
    }

    #[test]
    fn strips_carriage_returns_and_controls_but_keeps_newlines() {
        assert_eq!(strip_ansi("line1\r\nline2"), "line1\nline2");
        assert_eq!(strip_ansi("a\x08b"), "ab");
    }

    #[test]
    fn preserves_unicode() {
        assert_eq!(strip_ansi("\x1b[32m✓ done — café\x1b[0m"), "✓ done — café");
    }
}
