//! The streamed output of actions: what the Log tab shows.
//!
//! The shell owns it, not the core. A deploy can print hundreds of thousands of lines and
//! nothing the core decides depends on them, so they never pass through an event; the core
//! hears once, when the action ends. One [`Output`] holds the **current** action's lines and
//! the **last** one's, each bounded to the most recent [`KEEP_LINES`].
//!
//! Every line arrives sanitised (the adapter runs the domain's `sanitize_line` over what the
//! process printed), so drawing it is safe.
//!
//! Locking: one mutex, held for one push or for one frame's window. The adapter task pushes
//! from its own thread; the drawing reads the window it needs and does not clone the buffer
//! (a frame is 33 ms and a buffer is up to 20 000 lines).

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

/// How many lines of one action are kept: the most recent.
pub const KEEP_LINES: usize = 10_000;

/// Where a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// The process's standard output.
    Stdout,
    /// The process's standard error.
    Stderr,
    /// auspex's own words about the action ("resolving clan…", how it ended).
    Note,
}

/// One kept line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Where it came from.
    pub stream: Stream,
    /// The sanitised text.
    pub text: String,
}

/// One action's output.
#[derive(Debug, Default)]
struct Run {
    /// The core's number for it, when it was begun with one (an update of all).
    key: Option<u64>,
    title: String,
    lines: VecDeque<Line>,
    /// Lines dropped from the front to stay within [`KEEP_LINES`].
    dropped: u64,
    /// How it ended, once it has.
    end: Option<String>,
}

impl Run {
    /// Rows this run takes in the tab: its heading, its lines, its ending.
    fn rows(&self) -> usize {
        1 + self.lines.len() + usize::from(self.end.is_some())
    }

    fn row(&self, i: usize) -> Line {
        if i == 0 {
            return Line {
                stream: Stream::Note,
                text: format!("── {} ──", self.title),
            };
        }
        self.lines.get(i - 1).map_or_else(
            || Line {
                stream: Stream::Note,
                text: format!("── {} ──", self.end.as_deref().unwrap_or("")),
            },
            Clone::clone,
        )
    }
}

#[derive(Debug, Default)]
struct Inner {
    last: Vec<Run>,
    current: Vec<Run>,
}

impl Inner {
    fn runs(&self) -> impl Iterator<Item = &Run> {
        self.last.iter().chain(self.current.iter())
    }

    /// The run lines go to: the one with `key`, or (no key) the newest of the current group.
    fn run_mut(&mut self, key: Option<u64>) -> Option<&mut Run> {
        match key {
            Some(k) => self.current.iter_mut().find(|r| r.key == Some(k)),
            None => self.current.last_mut(),
        }
    }

    fn run(&self, key: Option<u64>) -> Option<&Run> {
        key.map_or_else(
            || self.current.last(),
            |k| self.current.iter().find(|r| r.key == Some(k)),
        )
    }
}

/// The action output the Log tab draws.
#[derive(Debug, Default)]
pub struct Output {
    inner: Mutex<Inner>,
}

impl Output {
    /// Runs `f` with the buffer locked, and unlocks it again as soon as `f` returns.
    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        f(&mut self.inner.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// A new action begins: the current group becomes the last, and the one before it goes.
    pub fn begin(&self, title: &str) {
        self.begin_run(None, title, true);
    }

    /// Begins run `key` (`None`: a run only the newest-run methods address). With `new_group` the
    /// current group becomes the last and this run is the first of a new one; otherwise it joins
    /// the current group, so an update of all keeps every machine's run beside the roll's own.
    pub fn begin_run(&self, key: Option<u64>, title: &str, new_group: bool) {
        self.with(|inner| {
            let run = Run {
                key,
                title: title.to_owned(),
                ..Run::default()
            };
            if new_group {
                inner.last = std::mem::take(&mut inner.current);
            }
            inner.current.push(run);
        });
    }

    /// Adds a line to the newest run of the current group, dropping its oldest past
    /// [`KEEP_LINES`].
    pub fn push(&self, stream: Stream, text: &str) {
        self.push_to(None, stream, text);
    }

    /// [`Output::push`] to run `key`. A line for a run that is not in the current group (it was
    /// replaced by a newer group) is dropped: it belongs to something no longer shown.
    pub fn push_to(&self, key: Option<u64>, stream: Stream, text: &str) {
        self.with(|inner| {
            if key.is_none() && inner.current.is_empty() {
                inner.current.push(Run::default());
            }
            let Some(run) = inner.run_mut(key) else {
                return;
            };
            if run.lines.len() >= KEEP_LINES {
                run.lines.pop_front();
                run.dropped += 1;
            }
            run.lines.push_back(Line {
                stream,
                text: text.to_owned(),
            });
        });
    }

    /// The newest run is over; `end` is shown under its output.
    pub fn finish(&self, end: &str) {
        self.finish_run(None, end);
    }

    /// [`Output::finish`] for run `key`.
    pub fn finish_run(&self, key: Option<u64>, end: &str) {
        self.with(|inner| {
            if let Some(run) = inner.run_mut(key) {
                run.end = Some(end.to_owned());
            }
        });
    }

    /// How many rows the tab has: for each run of the last and current groups, a heading, its
    /// kept lines and, once over, its ending.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.with(|inner| inner.runs().map(Run::rows).sum())
    }

    /// Lines dropped from the front, over both groups: a scrolled-up view shifts by the growth of
    /// this.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.with(|inner| inner.runs().map(|r| r.dropped).sum())
    }

    /// How many rows the older of the two groups takes (0 when there is only one), so a shift
    /// caused by the current group's drops does not move the older one's rows.
    #[must_use]
    pub fn last_rows(&self) -> usize {
        self.with(|inner| inner.last.iter().map(Run::rows).sum())
    }

    /// `count` rows from row `start`.
    #[must_use]
    pub fn window(&self, start: usize, count: usize) -> Vec<Line> {
        self.with(|inner| {
            let mut out = Vec::with_capacity(count);
            let mut offset = 0;
            for run in inner.runs() {
                let rows = run.rows();
                let from = start.saturating_sub(offset);
                if from < rows {
                    for i in from..rows.min(from + count - out.len()) {
                        out.push(run.row(i));
                    }
                }
                offset += rows;
                if out.len() >= count {
                    break;
                }
            }
            out
        })
    }

    /// How many lines the newest run printed on standard output and error, counting those
    /// already dropped from the front.
    #[must_use]
    pub fn printed_lines(&self) -> u64 {
        self.printed_lines_of(None)
    }

    /// [`Output::printed_lines`] of run `key`.
    #[must_use]
    pub fn printed_lines_of(&self, key: Option<u64>) -> u64 {
        self.with(|inner| {
            inner.run(key).map_or(0, |run| {
                let kept = run
                    .lines
                    .iter()
                    .filter(|l| l.stream != Stream::Note)
                    .count();
                run.dropped + kept as u64
            })
        })
    }

    /// The last `n` lines of the newest run that came from `stream`, oldest first.
    #[must_use]
    pub fn tail(&self, stream: Stream, n: usize) -> Vec<String> {
        self.tail_of(None, stream, n)
    }

    /// [`Output::tail`] of run `key`.
    #[must_use]
    pub fn tail_of(&self, key: Option<u64>, stream: Stream, n: usize) -> Vec<String> {
        self.with(|inner| Self::tail_in(inner, key, n, |l| l.stream == stream))
    }

    /// The last `n` lines of the newest run, whatever stream, oldest first.
    #[must_use]
    pub fn last_lines(&self, n: usize) -> Vec<String> {
        self.last_lines_of(None, n)
    }

    /// [`Output::last_lines`] of run `key`.
    #[must_use]
    pub fn last_lines_of(&self, key: Option<u64>, n: usize) -> Vec<String> {
        self.with(|inner| Self::tail_in(inner, key, n, |_| true))
    }

    fn tail_in(
        inner: &Inner,
        key: Option<u64>,
        n: usize,
        keep: impl Fn(&Line) -> bool,
    ) -> Vec<String> {
        let Some(run) = inner.run(key) else {
            return Vec::new();
        };
        let mut found: Vec<String> = run
            .lines
            .iter()
            .rev()
            .filter(|l| keep(l))
            .take(n)
            .map(|l| l.text.clone())
            .collect();
        found.reverse();
        found
    }

    /// Whether the tab has anything to show.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.with(|inner| inner.runs().next().is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_keeps_the_last_ten_thousand_lines_per_action() {
        assert_eq!(KEEP_LINES, 10_000, "the brief says ten thousand");
        let out = Output::default();
        out.begin("update luna");
        for i in 0..KEEP_LINES + 25 {
            out.push(Stream::Stdout, &format!("line {i}"));
        }
        // heading + KEEP_LINES, no ending yet
        assert_eq!(out.rows(), 1 + KEEP_LINES);
        assert_eq!(out.dropped(), 25);
        let first = &out.window(1, 1)[0];
        assert_eq!(first.text, "line 25", "the oldest 25 are gone");
        let last = &out.window(KEEP_LINES, 1)[0];
        assert_eq!(last.text, format!("line {}", KEEP_LINES + 24));
    }

    #[test]
    fn the_last_action_is_kept_beside_the_current_one() {
        let out = Output::default();
        out.begin("update luna");
        out.push(Stream::Stdout, "one");
        out.finish("succeeded");
        out.begin("build sol");
        out.push(Stream::Stderr, "two");
        out.begin("update terra");
        out.push(Stream::Stdout, "three");
        let all: Vec<String> = out.window(0, 100).into_iter().map(|l| l.text).collect();
        assert_eq!(
            all,
            ["── build sol ──", "two", "── update terra ──", "three"],
            "only the current and the last remain"
        );
    }

    #[test]
    fn a_window_crosses_actions_and_stops_at_the_end() {
        let out = Output::default();
        out.begin("a");
        out.push(Stream::Stdout, "a1");
        out.finish("done");
        out.begin("b");
        out.push(Stream::Stderr, "b1");
        let w = out.window(2, 3);
        let texts: Vec<&str> = w.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["── done ──", "── b ──", "b1"]);
        assert_eq!(out.window(99, 5), Vec::new());
    }

    #[test]
    fn tails_are_per_stream_and_of_the_current_action_only() {
        let out = Output::default();
        out.begin("old");
        out.push(Stream::Stderr, "old error");
        out.begin("new");
        for t in ["o1", "o2"] {
            out.push(Stream::Stdout, t);
        }
        for t in ["e1", "e2", "e3"] {
            out.push(Stream::Stderr, t);
        }
        assert_eq!(out.tail(Stream::Stderr, 2), ["e2", "e3"]);
        assert_eq!(out.tail(Stream::Stdout, 5), ["o1", "o2"]);
        assert_eq!(out.last_lines(2), ["e2", "e3"]);
    }
}
