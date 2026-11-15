//! Turning bytes a process or server printed into text the operator can be shown.

use auspex_domain::eval::ERROR_TAIL_LINES;
use auspex_domain::text::sanitized_tail;

/// Bytes to text, lossily: invalid UTF-8 becomes U+FFFD. What a remote host or a process
/// prints is not guaranteed to be UTF-8, and refusing it would hide the rest.
pub fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The last `ERROR_TAIL_LINES` non-empty lines of `text`, sanitised by the domain
/// (`auspex_domain::text::sanitized_tail`), joined into one reason; `None` when nothing is
/// left.
pub fn reason(text: &str) -> Option<String> {
    let tail = sanitized_tail(text, ERROR_TAIL_LINES);
    (!tail.is_empty()).then(|| tail.join("\n"))
}

/// The last sanitised line, or `None`.
pub fn last_line(text: &str) -> Option<String> {
    sanitized_tail(text, 1).pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_is_sanitised() {
        assert_eq!(
            reason("one\n\n\u{1b}[31merror:\u{1b}[0m boom\u{202e}\n").as_deref(),
            Some("one\nerror: boom")
        );
        assert_eq!(last_line("a\nb\n").as_deref(), Some("b"));
        assert_eq!(reason("\n\n"), None);
    }
}
