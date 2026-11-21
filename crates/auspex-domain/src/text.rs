//! Making remote-controlled text safe to show in a terminal.
//!
//! ssh's stderr and nix's error text are written by programs we do not control (and, for
//! ssh, partly by the remote host: a banner, a `ProxyJump` hop). Printed raw into a TUI,
//! an escape sequence in them can move the cursor, retitle the terminal or worse. So every
//! piece of such text that reaches a status reason goes through [`sanitize_line`] first,
//! and so does the inventory text the report prints (tags, `buildHost`).
//!
//! Both functions are public so that an adapter can make its own outside text safe (an
//! HTTP error body from ncps or Prometheus, a tool's stderr) the same way, without going
//! through a parser.

/// Makes one line safe to print in a terminal.
///
/// It strips ANSI escape sequences, every other control character (C0, DEL and C1), every
/// Unicode format character (category Cf: the bidi embeddings, overrides and isolates, the
/// zero-width spaces and joiners, the BOM) and the line and paragraph separators, and trims
/// the result. Tabs become a space; nothing else is kept.
///
/// It is for one line: line breaks are control characters too, so the lines of a
/// multi-line text would run together. Use [`sanitized_tail`] for those.
///
/// ```
/// use auspex_domain::text::sanitize_line;
///
/// assert_eq!(sanitize_line(" \u{1b}[1mHost key verification failed.\u{1b}[0m\r"), "Host key verification failed.");
/// assert_eq!(sanitize_line("a\u{202e}b\tc"), "ab c");
/// ```
///
/// Format characters are invisible but not inert: U+202E reverses the text after it in a
/// terminal that applies bidi, so a reason can be made to read as something it does not say.
#[must_use]
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

/// The last `max_lines` non-empty lines of `text`, sanitised, oldest first.
///
/// Each line goes through [`sanitize_line`]. Lines that are empty after sanitising (blank lines, a line that was only an
/// escape sequence) are skipped and do not count.
///
/// This is the function for multi-line text from outside: nix's error output, ssh's
/// stderr, an HTTP server's error body. The useful part of such text is usually at the end
/// (nix prints its trace first and `error: …` last), so the tail is what is kept.
///
/// ```
/// use auspex_domain::text::sanitized_tail;
///
/// let stderr = "warning: x\n\n\u{1b}[31merror:\u{1b}[0m boom\u{202e}\n";
/// assert_eq!(sanitized_tail(stderr, 1), ["error: boom"]);
/// assert_eq!(sanitized_tail(stderr, 5), ["warning: x", "error: boom"]);
/// ```
#[must_use]
pub fn sanitized_tail(text: &str, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .map(sanitize_line)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(max_lines);
    lines.drain(..start);
    lines
}

/// Quotes `word` for a POSIX shell, only when it needs it.
///
/// A word made of letters, digits and `_@%+=:,./-` is returned as it is, so the common command
/// reads the same; anything else (a space, a quote, `$`, an empty word) is wrapped in single
/// quotes, each single quote written `'\''`. For the by-hand commands the operator is told to
/// paste.
///
/// ```
/// use auspex_domain::text::shell_quote;
///
/// assert_eq!(shell_quote("/k/config"), "/k/config");
/// assert_eq!(shell_quote("/my dir/it's"), r"'/my dir/it'\''s'");
/// ```
#[must_use]
pub fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_leaves_safe_words_bare_and_quotes_the_rest() {
        assert_eq!(shell_quote("/k/config"), "/k/config");
        assert_eq!(shell_quote("rke2-a_b.c:d"), "rke2-a_b.c:d");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("/my dir/kube"), "'/my dir/kube'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a$b;c"), "'a$b;c'");
    }

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
    fn sanitized_tail_zero_keeps_nothing() {
        assert_eq!(sanitized_tail("a\n\nb\n", 0), Vec::<String>::new());
    }

    #[test]
    fn sanitized_tail_skips_blanks() {
        assert_eq!(sanitized_tail("a\n\nb\n  \nc\n", 2), vec!["b", "c"]);
        assert_eq!(sanitized_tail("", 3), Vec::<String>::new());
    }
}
