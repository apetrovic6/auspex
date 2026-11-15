//! Making remote-controlled text safe to show in a terminal.
//!
//! ssh's stderr and nix's error text are written by programs we do not control (and, for
//! ssh, partly by the remote host: a banner, a `ProxyJump` hop). Printed raw into a TUI,
//! an escape sequence in them can move the cursor, retitle the terminal or worse. So every
//! piece of such text that reaches a status reason goes through [`sanitize_line`] first.

/// Strips ANSI escape sequences and every other control character from one line, and trims
/// it. Tabs become a space; nothing else is kept.
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
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out.trim().to_owned()
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
    fn last_lines_skips_blanks() {
        assert_eq!(last_lines("a\n\nb\n  \nc\n", 2), vec!["b", "c"]);
        assert_eq!(last_lines("", 3), Vec::<String>::new());
    }
}
