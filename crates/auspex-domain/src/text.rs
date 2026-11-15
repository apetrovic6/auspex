//! Making remote-controlled text safe to show in a terminal.
//!
//! ssh's stderr and nix's error text are written by programs we do not control (and, for
//! ssh, partly by the remote host: a banner, a `ProxyJump` hop). Printed raw into a TUI,
//! an escape sequence in them can move the cursor, retitle the terminal or worse. So every
//! piece of such text that reaches a status reason goes through [`sanitize_line`] first,
//! and so does the inventory text the report prints (tags, `buildHost`).

/// Strips ANSI escape sequences, every other control character (C0, DEL and C1), every
/// Unicode format character (category Cf: the bidi embeddings, overrides and isolates, the
/// zero-width spaces and joiners, the BOM) and the line and paragraph separators, and trims
/// the result. Tabs become a space; nothing else is kept.
///
/// Format characters are invisible but not inert: U+202E reverses the text after it in a
/// terminal that applies bidi, so a reason can be made to read as something it does not say.
pub fn sanitize_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                // CSI: ESC [ parameters… final byte in @..~
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] … terminated by BEL or ESC \
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Any other two-byte escape.
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if c == '\t' {
            out.push(' ');
        } else if !c.is_control() && !is_format(c) {
            out.push(c);
        }
    }
    out.trim().to_owned()
}

/// Unicode general category Cf (format), as of Unicode 16, plus U+2028 and U+2029 (Zl and
/// Zp), which some terminals treat as line breaks. Spelled out rather than taken from a
/// Unicode crate: the set is small and changes rarely, and a dependency for one table is not
/// worth it.
const fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{600}'..='\u{605}'
            | '\u{61c}'
            | '\u{6dd}'
            | '\u{70f}'
            | '\u{890}'..='\u{891}'
            | '\u{8e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

/// The last `n` non-empty lines of `text`, sanitised, oldest first.
pub fn last_lines(text: &str, n: usize) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .map(sanitize_line)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(n);
    lines.drain(..start);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_osc_and_controls() {
        assert_eq!(
            sanitize_line("\u{1b}[31;1merror:\u{1b}[0m bad\r"),
            "error: bad"
        );
        assert_eq!(sanitize_line("\u{1b}]0;pwned\u{7}text"), "text");
        assert_eq!(sanitize_line("\u{1b}]0;pwned\u{1b}\\text"), "text");
        assert_eq!(sanitize_line("a\u{8}b\tc\u{0}"), "ab c");
    }

    #[test]
    fn strips_c1_del_and_bel() {
        // U+009B is the one-character CSI and U+009D the one-character OSC: C1 controls some
        // terminals still honour.
        assert_eq!(
            sanitize_line("a\u{9b}31mb\u{9d}c\u{85}d\u{7f}e\u{7}f"),
            "a31mbcdef"
        );
        for c in ('\u{0}'..='\u{1f}').chain('\u{7f}'..='\u{9f}') {
            let s = sanitize_line(&format!("x{c}y"));
            assert!(!s.contains(c), "{c:?} survived: {s:?}");
        }
    }

    #[test]
    fn strips_bidi_and_other_format_characters() {
        assert_eq!(
            sanitize_line("missing \u{202e}tsoh ruoy\u{202c} at \u{2066}x\u{2069}"),
            "missing tsoh ruoy at x"
        );
        for c in [
            '\u{ad}',
            '\u{61c}',
            '\u{200b}',
            '\u{200d}',
            '\u{200e}',
            '\u{200f}',
            '\u{2028}',
            '\u{2029}',
            '\u{202a}',
            '\u{202b}',
            '\u{202d}',
            '\u{2060}',
            '\u{2067}',
            '\u{2068}',
            '\u{feff}',
            '\u{e0001}',
            '\u{e007f}',
        ] {
            assert_eq!(sanitize_line(&format!("x{c}y")), "xy", "{c:?}");
        }
        // Ordinary non-ASCII text is kept.
        assert_eq!(
            sanitize_line("… while evaluating é ✓"),
            "… while evaluating é ✓"
        );
    }

    #[test]
    fn is_idempotent() {
        let once = sanitize_line(" \u{1b}[1ma\u{202e}b\u{1b}]0;t\u{7}c\t ");
        assert_eq!(once, "abc");
        assert_eq!(sanitize_line(&once), once);
    }

    #[test]
    fn last_lines_skips_blanks() {
        assert_eq!(last_lines("a\n\nb\n  \nc\n", 2), vec!["b", "c"]);
        assert_eq!(last_lines("", 3), Vec::<String>::new());
    }
}
