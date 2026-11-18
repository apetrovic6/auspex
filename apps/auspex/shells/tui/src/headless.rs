//! `--plain` and `--json`: what is printed once the fleet has settled.
//!
//! `--json` is `view.report` serialised exactly as `auspex_domain::report` defines it, and
//! nothing else on stdout. `--plain` is one aligned line per machine (badges, name, status,
//! target, target source, reason), then one line per fleet-level error; colour only when
//! stdout is a terminal. Both come from the same view model the TUI draws.

use std::fmt::Write as _;

use auspex_core::ViewModel;
use auspex_domain::fleet::TargetSource;
use auspex_domain::report::{FleetErrorKind, FleetReport, MachineReport};
use ratatui::style::Color;
use unicode_width::UnicodeWidthStr as _;

use crate::packages;
use crate::theme::badge_colour;
use crate::ui::{BADGE_CELLS, badge_glyphs, fit};

/// The exit status.
///
/// 0 for a scan that completed with no fleet-level error, 1 when
/// `report.errors` is not empty or the deadline cut the scan short. Unreachable or
/// out-of-date machines are findings, not failures.
#[must_use]
pub fn exit_status(view: &ViewModel) -> u8 {
    u8::from(!view.report.errors.is_empty() || view.refresh.timed_out)
}

/// `--json`: one document and a newline.
///
/// # Panics
///
/// Never: the report is plain data with string keys.
#[must_use]
pub fn json(report: &FleetReport) -> String {
    let mut s = serde_json::to_string_pretty(report).expect("the report serialises");
    s.push('\n');
    s
}

const fn source(s: TargetSource) -> &'static str {
    match s {
        TargetSource::Override => "override",
        TargetSource::Inventory => "inventory",
        TargetSource::MachineName => "machine_name",
    }
}

const fn kind(k: FleetErrorKind) -> &'static str {
    match k {
        FleetErrorKind::InventoryFailed => "inventory_failed",
        FleetErrorKind::InventoryTimedOut => "inventory_timed_out",
        FleetErrorKind::EvaluationFailed => "evaluation_failed",
    }
}

const fn sgr(c: Color) -> &'static str {
    match c {
        Color::Red => "31",
        Color::Green => "32",
        Color::Yellow => "33",
        Color::Blue => "34",
        Color::Magenta => "35",
        Color::Cyan => "36",
        Color::Gray => "37",
        Color::DarkGray => "90",
        Color::LightRed => "91",
        Color::LightGreen => "92",
        Color::LightYellow => "93",
        Color::LightBlue => "94",
        Color::LightMagenta => "95",
        Color::LightCyan => "96",
        _ => "39",
    }
}

/// The reason column: the machine's `reason`, which already ends in the package headline
/// for a computed preview, plus a few words for the two states that have none, so a scripted
/// reader of `--plain` is told why a pending update has no preview.
fn reason(m: &MachineReport) -> String {
    let base = m.reason.as_deref().unwrap_or("-");
    match packages::plain_note(m) {
        Some(note) if m.reason.is_some() => format!("{base}; {note}"),
        Some(note) => note,
        None => base.to_owned(),
    }
}

/// `--plain`.
#[must_use]
pub fn plain(view: &ViewModel, colour: bool, settle_seconds: u64) -> String {
    let report = &view.report;
    let width = |f: fn(&auspex_domain::report::MachineReport) -> String| {
        report
            .machines
            .iter()
            .map(|m| f(m).width())
            .max()
            .unwrap_or(0)
    };
    let name_w = width(|m| m.name.clone());
    let status_w = width(|m| m.status.label().to_owned());
    let target_w = width(|m| m.target.clone().unwrap_or_else(|| "-".to_owned()));
    let source_w = width(|m| source(m.target_source).to_owned());
    let mut out = String::new();
    // An empty fleet must not look like a silent failure (review C5).
    if report.machines.is_empty() {
        out.push_str("no machines\n");
    }
    for m in &report.machines {
        // Spinners do not survive settling, so a frame number is not needed; `0` keeps the
        // output fixed if one ever does.
        let mut used = 0;
        for (b, g) in badge_glyphs(m, 0) {
            if used + g.width() > BADGE_CELLS {
                break;
            }
            used += g.width();
            if colour {
                let _ = write!(out, "\x1b[{}m{g}\x1b[0m", sgr(badge_colour(b).1));
            } else {
                out.push_str(g);
            }
        }
        out.push_str(&" ".repeat(BADGE_CELLS - used));
        let target = m.target.clone().unwrap_or_else(|| "-".to_owned());
        let line = format!(
            "  {}  {}  {}  {}  {}",
            fit(&m.name, name_w),
            fit(m.status.label(), status_w),
            fit(&target, target_w),
            fit(source(m.target_source), source_w),
            reason(m),
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    for e in &report.errors {
        let _ = writeln!(out, "error: {}: {}", kind(e.kind), e.reason);
    }
    if view.refresh.timed_out {
        let _ = writeln!(
            out,
            "error: the scan did not settle within timeouts.settle ({settle_seconds} s); \
             what was still pending is marked as timed out"
        );
    }
    out
}
