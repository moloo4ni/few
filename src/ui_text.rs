//! Sanitization of any text that reaches the terminal.
//!
//! Tool output, diffs, model text and file contents are untrusted input for
//! rendering: raw ESC sequences would inject ANSI into the TUI, `\r` would
//! overwrite line starts, tabs collapse to zero width. Everything passes
//! through [`clean`] exactly once - when a transcript block is created.

/// Make arbitrary text safe to render in the terminal:
/// - strips ANSI escape sequences (CSI, OSC, and two-char escapes);
/// - drops `\r` and other C0 control characters (keeps `\n`);
/// - expands tabs to 4 spaces;
/// - drops DEL.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        // CSI: parameter/intermediate bytes, then final @-~
                        while let Some(&n) = chars.peek() {
                            chars.next();
                            if ('@'..='~').contains(&n) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        // OSC: terminated by BEL or ST (ESC \)
                        while let Some(&n) = chars.peek() {
                            if n == '\x07' {
                                chars.next();
                                break;
                            }
                            if n == '\x1b' {
                                chars.next();
                                chars.next(); // consume '\\'
                                break;
                            }
                            chars.next();
                        }
                    }
                    Some(_) => {
                        chars.next(); // two-char escape like ESC 7
                    }
                    None => {}
                }
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            '\r' | '\x7f' => {}
            c if (c as u32) < 0x20 => {} // other C0 control characters
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_and_osc() {
        assert_eq!(clean("\x1b[31mred\x1b[0m plain"), "red plain");
        assert_eq!(clean("a\x1b]0;title\x07b"), "ab");
        assert_eq!(clean("a\x1b]8;;http://x\x1b\\b"), "ab");
        assert_eq!(clean("\x1b7saved"), "saved");
    }

    #[test]
    fn control_chars_and_tabs() {
        assert_eq!(clean("col1\tcol2"), "col1    col2");
        assert_eq!(clean("a\rb\n"), "ab\n");
        assert_eq!(clean("x\u{7f}y"), "xy");
        assert_eq!(clean("bell\x07!"), "bell!");
        assert_eq!(clean("nul\u{0}!"), "nul!");
    }

    #[test]
    fn plain_text_untouched() {
        let t = "привет ✓ — keep unicode text\nsecond line";
        assert_eq!(clean(t), t);
    }

    #[test]
    fn unterminated_escape_eats_to_end() {
        assert_eq!(clean("ok\x1b[31"), "ok");
        assert_eq!(clean("ok\x1b"), "ok");
    }
}
