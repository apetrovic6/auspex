//! The log.
//!
//! The adapters' non-fatal notes (cache hits, caching off and why, a failed cache
//! write), config warnings and the like. stderr in headless mode, so stdout stays the report;
//! an in-memory list in the TUI, shown on the log line and in the log overlay.
//!
//! Every line is sanitised on the way in: notes carry nix's and git's text.

use std::sync::Mutex;

use auspex_domain::text::sanitize_line;

/// How many lines the TUI keeps. Older ones are dropped.
pub const KEEP: usize = 500;

/// Where log lines go.
#[derive(Debug)]
pub enum Log {
    /// Written to stderr as they come (headless).
    Stderr,
    /// Kept for the TUI.
    Memory(Mutex<Vec<String>>),
}

impl Log {
    /// A log kept in memory.
    #[must_use]
    pub const fn memory() -> Self {
        Self::Memory(Mutex::new(Vec::new()))
    }

    /// Adds one line.
    pub fn line(&self, text: &str) {
        let text = sanitize_line(text);
        match self {
            Self::Stderr => eprintln!("auspex: {text}"),
            Self::Memory(lines) => {
                let mut lines = lines
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if lines.len() >= KEEP {
                    lines.remove(0);
                }
                lines.push(text);
            }
        }
    }

    /// The kept lines (empty for stderr).
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::Stderr => Vec::new(),
            Self::Memory(lines) => lines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_sanitised_and_capped() {
        let log = Log::memory();
        log.line("nix said \u{1b}[31merror\u{1b}[0m\u{202e} here");
        assert_eq!(log.lines(), ["nix said error here"]);
        for i in 0..KEEP + 5 {
            log.line(&i.to_string());
        }
        let lines = log.lines();
        assert_eq!(lines.len(), KEEP);
        assert_eq!(lines.last().map(String::as_str), Some("504"));
    }
}
