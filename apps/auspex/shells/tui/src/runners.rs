//! What the runners tab looks like as text (AUS-19): one block per forge entry, one row per
//! runner, and the notes about what the forge's API does not say.
//!
//! Pure over [`FleetReport`]: everything shown is in `runners` and `runner_forges`, which is what
//! `--json` prints, so the TUI can never show a runner the JSON does not carry.
//!
//! Two layouts, chosen by the pane's width: **wide** (from [`WIDE_ROWS`] columns) is one line per
//! runner with its columns; **narrow** is two, the runner and its state first and the machine's
//! unit, memory and badges under it, because a line that is cut at 40 columns would cut off the
//! very badge that says what is wrong.

use std::fmt::Write as _;

use auspex_domain::report::FleetReport;
use auspex_domain::runners::{ForgeFacts, RunnerBadge, RunnerRow, summary_line};

/// The pane width, in cells, from which a runner is one line.
pub const WIDE_ROWS: usize = 100;

/// What a line is, for styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A forge entry's heading.
    Heading,
    /// A runner that is fine.
    Ok,
    /// A runner with a warning only (memory near its cap).
    Warn,
    /// A runner that is offline, disagrees with its unit, or whose forge could not be asked.
    Red,
    /// Something to read: a continuation line, a note, a forge still being asked.
    Note,
}

/// A line of the tab.
pub type Line = (Kind, String);

/// The words under the last forge: what the API does not say, so nobody reads a guess into it.
pub const NOTES: [&str; 2] = [
    "busy: the forge does not say which job a runner runs",
    "queued is a count of waiting jobs: the API gives no time to age them by",
];

/// The tab's title: the status bar's summary line, or what to do when nothing is configured.
#[must_use]
pub fn title(report: &FleetReport) -> String {
    summary_line(&report.runner_forges, &report.runners)
        .unwrap_or_else(|| "runners: not configured".to_owned())
}

const fn status_word(r: &RunnerRow) -> &'static str {
    if r.name.is_none() {
        "-"
    } else {
        r.status.word()
    }
}

fn kind_of(r: &RunnerRow) -> Kind {
    if r.is_red() {
        Kind::Red
    } else if r.badges.contains(&RunnerBadge::MemoryNearCap) {
        Kind::Warn
    } else if r.name.is_none() {
        Kind::Note
    } else {
        Kind::Ok
    }
}

fn unit_text(r: &RunnerRow) -> String {
    r.unit.as_ref().map_or_else(
        || "-".to_owned(),
        |u| {
            if u.sub == u.active.word() {
                format!("unit {}", u.active.word())
            } else {
                format!("unit {}/{}", u.active.word(), u.sub)
            }
        },
    )
}

fn memory_text(r: &RunnerRow) -> Option<String> {
    r.memory.map(|m| format!("mem {}%", m.percent))
}

fn load_text(r: &RunnerRow) -> Option<String> {
    r.load.map(|l| format!("load {:.1}/{}", l.one, l.cpus))
}

fn badges_text(r: &RunnerRow) -> String {
    r.badges
        .iter()
        .map(|b| format!("[{}]", b.word()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn heading(f: &ForgeFacts) -> String {
    let mut s = format!("{} ({})", f.forge, f.scope);
    if f.checking {
        s.push_str(": asking…");
    } else if let Some(why) = &f.error {
        let _ = write!(s, ": {why}");
    } else {
        match (f.queued, &f.queue_error) {
            (Some(n), _) => {
                let _ = write!(s, ": {n} queued");
            }
            (None, Some(why)) => {
                let _ = write!(s, ": queue unknown, {why}");
            }
            (None, None) => {}
        }
    }
    s
}

/// `text` in a cell `width` wide: padded, or cut with `…`.
fn cell(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n <= width {
        format!("{text:<width$}")
    } else {
        let keep: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{keep}…")
    }
}

/// One line: the badges come right after the status, before the details, so a pane that cuts the
/// line cuts the version and the labels and not what is wrong.
fn wide_line(r: &RunnerRow) -> String {
    let cells = [
        cell(r.name.as_deref().unwrap_or("-"), 18),
        cell(status_word(r), 8),
        cell(&badges_text(r), 28),
        cell(r.machine.as_deref().unwrap_or("-"), 10),
        cell(&unit_text(r), 22),
        cell(&memory_text(r).unwrap_or_default(), 8),
        cell(&load_text(r).unwrap_or_default(), 11),
        cell(r.version.as_deref().unwrap_or(""), 9),
        r.labels.join(","),
    ];
    cells.join(" ").trim_end().to_owned()
}

/// Up to three short lines: the runner and its state; what is wrong with it; what its machine
/// says (unit, memory, load). Each line is its own, so none is cut at 40 columns before it has
/// said what it is for.
fn narrow_lines(r: &RunnerRow) -> Vec<String> {
    let name = r.name.as_deref().unwrap_or("-");
    let machine = r
        .machine
        .as_deref()
        .map(|m| format!(" on {m}"))
        .unwrap_or_default();
    let mut lines = vec![format!("{name} {}{machine}", status_word(r))];
    let badges = badges_text(r);
    if !badges.is_empty() {
        lines.push(format!("  {badges}"));
    }
    let mut facts: Vec<String> = Vec::new();
    if r.unit.is_some() {
        facts.push(unit_text(r));
    }
    facts.extend(memory_text(r));
    facts.extend(load_text(r));
    if !facts.is_empty() {
        lines.push(format!("  {}", facts.join(" ")));
    }
    lines
}

/// Every line of the tab, for a pane `width` cells wide.
#[must_use]
pub fn lines(report: &FleetReport, width: usize) -> Vec<Line> {
    if report.runner_forges.is_empty() {
        return vec![(
            Kind::Note,
            "no [[runners.forge]] is configured: add one to config.toml (see the README)"
                .to_owned(),
        )];
    }
    let wide = width >= WIDE_ROWS;
    let mut out: Vec<Line> = Vec::new();
    for f in &report.runner_forges {
        // A heading can say why the forge failed: wrapped, so a narrow pane has the reason.
        let kind = if f.error.is_some() {
            Kind::Red
        } else {
            Kind::Heading
        };
        for (i, part) in crate::ui::wrap(&heading(f), width.max(1))
            .into_iter()
            .enumerate()
        {
            out.push((kind, if i == 0 { part } else { format!("  {part}") }));
        }
        for r in report.runners.iter().filter(|r| r.forge == f.forge) {
            if r.name.is_none() {
                // A forge that is being asked or failed: its heading says why, the row marks it.
                continue;
            }
            let kind = kind_of(r);
            if wide {
                out.push((kind, wide_line(r)));
            } else {
                let mut it = narrow_lines(r).into_iter();
                out.extend(it.next().map(|l| (kind, l)));
                out.extend(it.map(|l| (if kind == Kind::Ok { Kind::Note } else { kind }, l)));
            }
        }
        if report
            .runners
            .iter()
            .filter(|r| r.forge == f.forge && r.name.is_some())
            .count()
            == 0
            && !f.checking
            && f.error.is_none()
        {
            out.push((Kind::Note, "  no runners".to_owned()));
        }
        if f.error.is_some() {
            out.push((Kind::Red, "  [forge error]".to_owned()));
        }
    }
    for note in NOTES {
        out.extend(
            crate::ui::wrap(note, width.max(1))
                .into_iter()
                .map(|l| (Kind::Note, l)),
        );
    }
    out
}
