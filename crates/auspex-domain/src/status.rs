//! The status-badge rules: what is known about one machine in, its badges out.
//!
//! The whole table from the plan is modelled, including the rules later steps render
//! (failed units and alerts, reboot needed, frozen), so adding a step adds facts rather than
//! changing these types.
//!
//! # One primary status, several badges
//!
//! A machine can carry several badges at once (`↑` with `◆`; `✓` with `⚠`). The **primary**
//! status answers one question, "what would happen if I acted on this machine now", and is
//! the first of these that applies:
//!
//! 1. [`Badge::Unreachable`]: nothing else can be done or trusted, including a refused
//!    target, which is never probed.
//! 2. [`Badge::EvalError`]: whatever the machine runs, there is nothing to deploy to it.
//! 3. [`Badge::NotInstalled`]: reachable, but there is no system to compare with.
//! 4. [`Badge::Checking`]: the probe is not back.
//! 5. [`Badge::Evaluating`]: online, the toplevel is not back.
//! 6. [`Badge::UpdatePending`] or [`Badge::InSync`].
//!
//! A failed probe counts as unreachable even when an eval error is known, because a dead
//! host is the more urgent of the two; the eval error is still among the badges.
//! [`Badge::Cached`], [`Badge::FailedUnits`], [`Badge::RebootNeeded`] and [`Badge::Frozen`]
//! are never primary: they qualify a sync state rather than replace one.

use serde::{Deserialize, Serialize};

use crate::eval::EvalOutcome;
use crate::fleet::MachineClass;
use crate::observe::{Observation, ProbeOutcome};

/// One badge from the plan's table. Serialised as the snake-case name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Badge {
    /// Grey spinner: the probe is not back yet.
    Checking,
    /// Cyan spinner: online, the toplevel is not back yet.
    Evaluating,
    /// `✓`: `current-system` equals the evaluated toplevel.
    InSync,
    /// `↑`: they differ.
    UpdatePending,
    /// `◆`: the pending toplevel is already in ncps. Only alongside `↑`.
    Cached,
    /// `⚠`: a failed unit, systemd reporting `degraded` or `maintenance`, or an alert firing.
    FailedUnits,
    /// `⟳`: the booted kernel or initrd differs from the current one.
    RebootNeeded,
    /// `?`: reachable, no `/run/current-system`.
    NotInstalled,
    /// `✗`: ssh exit 255, a timeout, or a probe that did not complete.
    Unreachable,
    /// `!`: the toplevel failed to evaluate.
    EvalError,
    /// `❄`: frozen, so `U` skips it.
    Frozen,
}

impl Badge {
    /// The glyph, or `None` for the two spinner states (the shell animates those).
    #[must_use]
    pub const fn glyph(self) -> Option<&'static str> {
        Some(match self {
            Self::Checking | Self::Evaluating => return None,
            Self::InSync => "✓",
            Self::UpdatePending => "↑",
            Self::Cached => "◆",
            Self::FailedUnits => "⚠",
            Self::RebootNeeded => "⟳",
            Self::NotInstalled => "?",
            Self::Unreachable => "✗",
            Self::EvalError => "!",
            Self::Frozen => "❄",
        })
    }

    /// The plan's name for it, for `--plain` and the help overlay.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::Evaluating => "evaluating",
            Self::InSync => "in sync",
            Self::UpdatePending => "update pending",
            Self::Cached => "cached",
            Self::FailedUnits => "failed units / alerts",
            Self::RebootNeeded => "reboot needed",
            Self::NotInstalled => "not installed",
            Self::Unreachable => "unreachable",
            Self::EvalError => "eval error",
            Self::Frozen => "frozen",
        }
    }

    /// Whether the shell shows a spinner rather than a glyph.
    #[must_use]
    pub const fn is_spinner(self) -> bool {
        matches!(self, Self::Checking | Self::Evaluating)
    }

    /// Whether this badge is settled for `--json`'s purposes: anything but a spinner.
    #[must_use]
    pub const fn is_settled(self) -> bool {
        !self.is_spinner()
    }
}

/// The ncps lookup for the pending toplevel.
///
/// The lookup needs the evaluated toplevel, so it can only start after the eval is back;
/// `Pending` is what keeps a machine from settling in that gap, so `--json` does not print
/// before `cached` can appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NarCache {
    /// No lookup applies: no ncps is configured, or there is nothing to look up (no update
    /// pending, or no successful eval).
    #[default]
    NotApplicable,
    /// The lookup was (or is about to be) sent and has not answered. Keeps an update-pending
    /// machine unsettled; ignored when its answer cannot change the row.
    Pending,
    /// The lookup failed (unreachable, timed out, an HTTP error other than 404). The badge
    /// cannot be shown, but nothing more is coming.
    Failed,
    /// The `.narinfo` exists.
    Cached,
    /// It does not.
    NotCached,
}

/// Answers for one machine that are still outstanding.
///
/// That is, other than its probe, eval and ncps lookup, which carry their own "not back
/// yet". Every flag that is `true` keeps the machine from settling. The default is "nothing
/// outstanding". Set each applicable flag before publishing a row that can receive that
/// answer, and clear it only when the answer or a terminal failure arrives. Leaving a
/// flag false while its request is outstanding permits premature settlement and later
/// changes to health or alert badges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outstanding {
    /// The Prometheus answer that feeds this machine's health is not back.
    pub prometheus: bool,
    /// The Alertmanager answer that feeds `alerts_firing` is not back.
    pub alerts: bool,
}

/// Everything known about one machine. `None` means "not back yet".
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MachineFacts<'a> {
    /// The probe's outcome.
    pub probe: Option<&'a ProbeOutcome>,
    /// The evaluation's outcome.
    pub eval: Option<&'a EvalOutcome>,
    /// The ncps lookup for the pending toplevel.
    ///
    /// Defaults to [`NarCache::NotApplicable`], meaning no lookup will change this row.
    /// With a cache configured, set [`NarCache::Pending`] in the same update that makes
    /// an update pending (the probe or eval arriving second), before publishing the row,
    /// even if the request has not been sent yet. It may also be set when a successful
    /// eval arrives first. Replace it with the terminal lookup outcome when it answers.
    /// Leaving the default when a lookup will follow allows premature settlement and a
    /// later change to the cached badge or failure reason.
    pub cache: NarCache,
    /// Frozen in the config.
    pub frozen: bool,
    /// Alertmanager alerts firing for this machine (0 until step 3 collects them).
    pub alerts_firing: u32,
    /// Why the machine's target was refused, when it was. Such a machine is never probed,
    /// so without this it would show "checking" forever and `--json` would never settle;
    /// with it, the machine is unreachable with this reason.
    pub target_error: Option<&'a str>,
    /// The other answers still outstanding for this machine.
    pub outstanding: Outstanding,
}

impl MachineFacts<'_> {
    /// Whether this machine's probe, evaluation and applicable auxiliary answers are final:
    ///
    /// - its probe is back, or its target was refused (so it is never probed); and
    /// - its eval is back; and
    /// - its ncps lookup is not [`NarCache::Pending`], or no update is pending; and
    /// - no [`Outstanding`] flag is set.
    ///
    /// Computed from these facts, never from the badges, so the answer does not depend on
    /// the order in which they arrive.
    /// Shared-IP resolution is fleet-wide: see [`crate::report::FleetFacts::scan_complete`].
    #[must_use]
    pub fn is_settled(&self) -> bool {
        (self.probe.is_some() || self.target_error.is_some())
            && self.eval.is_some()
            && (self.cache != NarCache::Pending || !self.update_pending())
            && !self.outstanding.prometheus
            && !self.outstanding.alerts
    }

    fn update_pending(&self) -> bool {
        match (self.probe, self.eval) {
            (Some(ProbeOutcome::Observed(o)), Some(EvalOutcome::Success(s))) => {
                o.current_system.as_ref().is_some_and(|p| p != &s.out_path)
            }
            _ => false,
        }
    }
}

/// A machine's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineStatus {
    /// The single primary status; see the module docs for the precedence.
    pub primary: Badge,
    /// Every badge that applies, the primary among them, in the plan's table order.
    pub badges: Vec<Badge>,
    /// One line on why, for the detail panel: ssh's or nix's last line, or what is being
    /// waited for, followed by health notes joined with `; ` (all of them for a sync state,
    /// only the ones behind `⚠` for a machine that is not installed). `None` for a `✓` with
    /// nothing to note. It is for people: programs read `badges` and the observation's
    /// structured fields, never this text.
    pub reason: Option<String>,
}

/// Applies the plan's status table to one machine.
#[must_use]
#[expect(
    clippy::option_if_let_else,
    reason = "a precedence ladder reads top to bottom; nested map_or_else would not"
)]
pub fn machine_status(facts: &MachineFacts<'_>) -> MachineStatus {
    let observation = match facts.probe {
        Some(ProbeOutcome::Observed(o)) => Some(&**o),
        _ => None,
    };
    let unreachable_reason = match (facts.target_error, facts.probe) {
        (Some(e), _) => Some(format!("invalid target: {e}")),
        (None, Some(ProbeOutcome::Unreachable { reason } | ProbeOutcome::Failed { reason })) => {
            Some(reason.clone())
        }
        _ => None,
    };
    let eval_error = match facts.eval {
        Some(EvalOutcome::Error(e)) => Some(
            e.summary()
                .map_or_else(|| "evaluation failed".to_owned(), str::to_owned),
        ),
        _ => None,
    };
    let pending = match facts.eval {
        Some(EvalOutcome::Success(s)) => Some(&s.out_path),
        _ => None,
    };
    let not_installed = observation.is_some_and(|o| o.current_system.is_none());
    let current = observation.and_then(|o| o.current_system.as_ref());
    let in_sync = pending.is_some() && current == pending;
    let update_pending = facts.update_pending();

    let (primary, reason) = if let Some(r) = unreachable_reason {
        (Badge::Unreachable, Some(r))
    } else if let Some(r) = eval_error.clone() {
        (Badge::EvalError, Some(r))
    } else if not_installed {
        (
            Badge::NotInstalled,
            Some("reachable, but /run/current-system is missing".to_owned()),
        )
    } else if facts.probe.is_none() {
        (Badge::Checking, Some("waiting for the probe".to_owned()))
    } else if facts.eval.is_none() {
        (
            Badge::Evaluating,
            Some("waiting for the evaluation".to_owned()),
        )
    } else if in_sync {
        (Badge::InSync, None)
    } else {
        let name = pending.map_or("", |p| p.name());
        let note = if facts.cache == NarCache::Failed {
            "; cache lookup failed"
        } else {
            ""
        };
        (Badge::UpdatePending, Some(format!("pending {name}{note}")))
    };

    // Health qualifies a machine that is up. All its notes are appended to the reason of a
    // sync state (or of "evaluating", which becomes one). For a machine that is up but not
    // installed, only the warnings are, so the reason explains the ⚠ next to the `?`. They
    // never replace the reason for an unreachable or failing machine.
    let notes = health_notes(observation, facts.alerts_firing);
    let warn = !notes.warnings.is_empty();
    let reason = with_notes(primary, reason, notes);

    let mut badges = vec![primary];
    if facts.probe.is_none() && facts.target_error.is_none() {
        badges.push(Badge::Checking);
    }
    if observation.is_some() && facts.eval.is_none() {
        badges.push(Badge::Evaluating);
    }
    if eval_error.is_some() {
        badges.push(Badge::EvalError);
    }
    if not_installed {
        badges.push(Badge::NotInstalled);
    }
    if in_sync {
        badges.push(Badge::InSync);
    }
    if update_pending {
        badges.push(Badge::UpdatePending);
        if facts.cache == NarCache::Cached {
            badges.push(Badge::Cached);
        }
    }
    if warn {
        badges.push(Badge::FailedUnits);
    }
    if observation.is_some_and(Observation::reboot_needed) {
        badges.push(Badge::RebootNeeded);
    }
    if facts.frozen {
        badges.push(Badge::Frozen);
    }
    badges.sort_by_key(|b| table_order(*b));
    badges.dedup();

    MachineStatus {
        primary,
        badges,
        reason,
    }
}

/// Appends the health notes that apply to `primary` to its reason; see [`machine_status`].
fn with_notes(primary: Badge, reason: Option<String>, notes: HealthNotes) -> Option<String> {
    let appended: Vec<String> = match primary {
        Badge::Evaluating | Badge::InSync | Badge::UpdatePending => {
            notes.warnings.into_iter().chain(notes.info).collect()
        }
        Badge::NotInstalled => notes.warnings,
        _ => Vec::new(),
    };
    if appended.is_empty() {
        return reason;
    }
    let notes = appended.join("; ");
    Some(reason.map_or_else(|| notes.clone(), |r| format!("{r}; {notes}")))
}

/// How many failed units a reason names before it says "and N more".
const UNITS_IN_REASON: usize = 3;

/// What the health facts add to a reason.
struct HealthNotes {
    /// Notes that come with [`Badge::FailedUnits`]: failed units, `degraded`,
    /// `maintenance`, alerts. Non-empty exactly when that badge applies.
    warnings: Vec<String>,
    /// Notes without a badge: health unknown, or a transitional systemd state.
    info: Vec<String>,
}

/// The health rule, NixOS only for systemd (darwin has none, so nothing is unknown there).
/// `system_state` and the unit names have already been checked against systemd's own
/// states and character set by the parser, so none of this text is free remote text.
///
/// - `⚠` when systemd lists a failed unit; when `system_state` is `degraded` (a unit has
///   failed, even if the list could not be read or came back empty); when it is
///   `maintenance` (rescue or emergency mode); or when an alert is firing. The notes name
///   the units, or say which state, and count the alerts.
/// - Without a systemd warning: `system_state` unreadable, `offline` or `unknown`, or the
///   failed-unit list unreadable, is "health unknown", a note without a badge (the values
///   stay `null` and the row keeps its sync badge, but no longer reads as a plain `✓`).
///   `starting`, `initializing` and `stopping` are named in a note, without a badge.
fn health_notes(observation: Option<&Observation>, alerts_firing: u32) -> HealthNotes {
    let mut warnings = Vec::new();
    let mut info = Vec::new();
    if let Some(o) = observation {
        let units = o.failed_units.as_deref().unwrap_or_default();
        let state = o.system_state.as_deref();
        if !units.is_empty() {
            let shown = units[..units.len().min(UNITS_IN_REASON)].join(", ");
            warnings.push(match units.len().saturating_sub(UNITS_IN_REASON) {
                0 => format!("failed units: {shown}"),
                more => format!("failed units: {shown} and {more} more"),
            });
        } else if state == Some("degraded") {
            warnings.push("systemd reports degraded".to_owned());
        }
        if state == Some("maintenance") {
            warnings.push("systemd is in maintenance mode (rescue or emergency)".to_owned());
        }
        if o.class == Some(MachineClass::Nixos) && warnings.is_empty() {
            match state {
                None => info.push("health unknown: systemd state could not be read".to_owned()),
                Some(s @ ("offline" | "unknown")) => {
                    info.push(format!("health unknown: systemd reports {s}"));
                }
                Some(s) => {
                    if o.failed_units.is_none() {
                        info.push("health unknown: failed units could not be read".to_owned());
                    }
                    if matches!(s, "starting" | "initializing" | "stopping") {
                        info.push(format!("systemd is {s}"));
                    }
                }
            }
        }
    }
    if alerts_firing > 0 {
        warnings.push(match alerts_firing {
            1 => "1 alert firing".to_owned(),
            n => format!("{n} alerts firing"),
        });
    }
    HealthNotes { warnings, info }
}

/// The plan's table order, which is also the display order of badges.
const fn table_order(b: Badge) -> u8 {
    match b {
        Badge::Checking => 0,
        Badge::Evaluating => 1,
        Badge::InSync => 2,
        Badge::UpdatePending => 3,
        Badge::Cached => 4,
        Badge::FailedUnits => 5,
        Badge::RebootNeeded => 6,
        Badge::NotInstalled => 7,
        Badge::Unreachable => 8,
        Badge::EvalError => 9,
        Badge::Frozen => 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{EvalFailure, EvalSuccess};
    use crate::observe::{ProbeExit, StorePath, parse_probe};

    const A: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-terra-a";
    const B: &str = "/nix/store/zyxwvsrqpnmlkjihgfdcba9876543210-nixos-system-terra-b";
    const K1: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-linux-6.12.1/bzImage";
    const K2: &str = "/nix/store/zyxwvsrqpnmlkjihgfdcba9876543210-linux-6.12.2/bzImage";

    fn probe(lines: &str) -> ProbeOutcome {
        parse_probe(
            &format!("auspex_probe=1\nprobe=nixos\n{lines}auspex_end=1\n"),
            "",
            ProbeExit::Code(0),
        )
    }

    fn at(current: &str) -> ProbeOutcome {
        probe(&format!(
            "current_system={current}\nsystem_state=running\nfailed_units=[]\n"
        ))
    }

    fn ok(out: &str) -> EvalOutcome {
        EvalOutcome::Success(EvalSuccess {
            attr: "terra".to_owned(),
            attr_path: vec![],
            drv_path: StorePath::new(format!("{out}.drv")).unwrap(),
            out_path: StorePath::new(out).unwrap(),
        })
    }

    fn err() -> EvalOutcome {
        EvalOutcome::Error(EvalFailure {
            attr: "terra".to_owned(),
            attr_path: vec![],
            error_tail: vec!["error: attribute 'x' missing".to_owned()],
        })
    }

    fn unreachable() -> ProbeOutcome {
        ProbeOutcome::Unreachable {
            reason: "Host key verification failed.".to_owned(),
        }
    }

    fn status(probe: Option<&ProbeOutcome>, eval: Option<&EvalOutcome>) -> MachineStatus {
        machine_status(&MachineFacts {
            probe,
            eval,
            ..MachineFacts::default()
        })
    }

    use Badge as B_;

    #[test]
    fn row_checking() {
        let s = status(None, None);
        assert_eq!(s.primary, B_::Checking);
        assert_eq!(s.badges, [B_::Checking]);
        // An evaluation that is back does not end "checking".
        let o = ok(A);
        let s = status(None, Some(&o));
        assert_eq!(s.primary, B_::Checking);
        assert_eq!(s.badges, [B_::Checking]);
    }

    #[test]
    fn row_evaluating() {
        let p = at(A);
        let s = status(Some(&p), None);
        assert_eq!(s.primary, B_::Evaluating);
        assert_eq!(s.badges, [B_::Evaluating]);
    }

    #[test]
    fn row_in_sync() {
        let (p, o) = (at(A), ok(A));
        let s = status(Some(&p), Some(&o));
        assert_eq!(s.primary, B_::InSync);
        assert_eq!(s.badges, [B_::InSync]);
        assert_eq!(s.reason, None);
    }

    #[test]
    fn row_update_pending_and_cached() {
        let (p, o) = (at(A), ok(B));
        let s = status(Some(&p), Some(&o));
        assert_eq!(s.primary, B_::UpdatePending);
        assert_eq!(s.badges, [B_::UpdatePending]);
        assert_eq!(s.reason.as_deref(), Some("pending nixos-system-terra-b"));

        for (cache, want) in [
            (NarCache::Cached, &[B_::UpdatePending, B_::Cached][..]),
            (NarCache::NotCached, &[B_::UpdatePending][..]),
            (NarCache::NotApplicable, &[B_::UpdatePending][..]),
            (NarCache::Pending, &[B_::UpdatePending][..]),
            (NarCache::Failed, &[B_::UpdatePending][..]),
        ] {
            let s = machine_status(&MachineFacts {
                probe: Some(&p),
                eval: Some(&o),
                cache,
                ..MachineFacts::default()
            });
            assert_eq!(s.primary, B_::UpdatePending);
            assert_eq!(s.badges, want);
            assert_eq!(
                s.reason.as_deref(),
                Some(if cache == NarCache::Failed {
                    "pending nixos-system-terra-b; cache lookup failed"
                } else {
                    "pending nixos-system-terra-b"
                }),
                "{cache:?}"
            );
        }

        // Cached is only shown with ↑.
        let same = ok(A);
        let s = machine_status(&MachineFacts {
            probe: Some(&p),
            eval: Some(&same),
            cache: NarCache::Cached,
            ..MachineFacts::default()
        });
        assert_eq!(s.badges, [B_::InSync]);
    }

    #[test]
    fn row_failed_units_and_alerts() {
        let p = probe(&format!(
            "current_system={A}\nsystem_state=degraded\nfailed_units=[{{\"unit\":\"x.service\"}}]\n"
        ));
        let o = ok(A);
        let s = status(Some(&p), Some(&o));
        assert_eq!(s.primary, B_::InSync);
        assert_eq!(s.badges, [B_::InSync, B_::FailedUnits]);

        let clean = at(A);
        let s = machine_status(&MachineFacts {
            probe: Some(&clean),
            eval: Some(&o),
            alerts_firing: 2,
            ..MachineFacts::default()
        });
        assert_eq!(s.badges, [B_::InSync, B_::FailedUnits]);
    }

    #[test]
    fn row_reboot_needed() {
        let p = probe(&format!(
            "current_system={A}\nbooted_kernel={K1}\ncurrent_kernel={K2}\n"
        ));
        let o = ok(B);
        let s = status(Some(&p), Some(&o));
        assert_eq!(s.primary, B_::UpdatePending);
        assert_eq!(s.badges, [B_::UpdatePending, B_::RebootNeeded]);
    }

    #[test]
    fn row_not_installed() {
        let p = probe("current_system=\n");
        let s = status(Some(&p), None);
        assert_eq!(s.primary, B_::NotInstalled);
        assert_eq!(s.badges, [B_::Evaluating, B_::NotInstalled]);
        let o = ok(A);
        let s = status(Some(&p), Some(&o));
        assert_eq!(s.primary, B_::NotInstalled);
        assert_eq!(s.badges, [B_::NotInstalled]);
        // With an eval error, the error is primary.
        let e = err();
        let s = status(Some(&p), Some(&e));
        assert_eq!(s.primary, B_::EvalError);
        assert_eq!(s.badges, [B_::NotInstalled, B_::EvalError]);
    }

    #[test]
    fn row_unreachable() {
        let p = unreachable();
        for eval in [None, Some(ok(A)), Some(err())] {
            let s = status(Some(&p), eval.as_ref());
            assert_eq!(s.primary, B_::Unreachable);
            assert_eq!(s.reason.as_deref(), Some("Host key verification failed."));
        }
        let e = err();
        assert_eq!(
            status(Some(&p), Some(&e)).badges,
            [B_::Unreachable, B_::EvalError]
        );
        // A probe that connected but did not complete is shown as unreachable too.
        let f = ProbeOutcome::Failed {
            reason: "probe output cut off".to_owned(),
        };
        assert_eq!(status(Some(&f), None).primary, B_::Unreachable);
        // A refused target is never probed, and must not stay "checking".
        let s = machine_status(&MachineFacts {
            target_error: Some("invalid host \"-x\""),
            ..MachineFacts::default()
        });
        assert_eq!(s.primary, B_::Unreachable);
        assert_eq!(s.badges, [B_::Unreachable]);
        assert!(s.primary.is_settled());
    }

    #[test]
    fn row_eval_error() {
        let (p, e) = (at(A), err());
        let s = status(Some(&p), Some(&e));
        assert_eq!(s.primary, B_::EvalError);
        assert_eq!(s.badges, [B_::EvalError]);
        assert_eq!(s.reason.as_deref(), Some("error: attribute 'x' missing"));
        // Known before the probe returns: shown, with the probe still pending.
        let s = status(None, Some(&e));
        assert_eq!(s.primary, B_::EvalError);
        assert_eq!(s.badges, [B_::Checking, B_::EvalError]);
    }

    #[test]
    fn row_frozen() {
        let (p, o) = (at(A), ok(B));
        let s = machine_status(&MachineFacts {
            probe: Some(&p),
            eval: Some(&o),
            cache: NarCache::Cached,
            frozen: true,
            ..MachineFacts::default()
        });
        assert_eq!(s.primary, B_::UpdatePending);
        assert_eq!(s.badges, [B_::UpdatePending, B_::Cached, B_::Frozen]);
    }

    #[test]
    fn glyphs_and_spinners() {
        assert_eq!(Badge::Checking.glyph(), None);
        assert_eq!(Badge::Evaluating.glyph(), None);
        assert!(Badge::Checking.is_spinner() && Badge::Evaluating.is_spinner());
        let glyphs: Vec<_> = [
            Badge::InSync,
            Badge::UpdatePending,
            Badge::Cached,
            Badge::FailedUnits,
            Badge::RebootNeeded,
            Badge::NotInstalled,
            Badge::Unreachable,
            Badge::EvalError,
            Badge::Frozen,
        ]
        .into_iter()
        .map(|b| b.glyph().unwrap())
        .collect();
        assert_eq!(glyphs, ["✓", "↑", "◆", "⚠", "⟳", "?", "✗", "!", "❄"]);
    }

    /// F4: `degraded` raises `⚠` with a reason, even when the unit list is unreadable,
    /// empty, or of the wrong shape.
    #[test]
    fn degraded_raises_the_warning_badge() {
        let o = ok(A);
        for units in [
            "",
            "[]",
            "{\"unit\":\"x.service\"}",
            "[{\"name\":\"x.service\"}]",
            "[{\"unit\":\"x.service\"},null]",
        ] {
            let p = probe(&format!(
                "current_system={A}\nsystem_state=degraded\nfailed_units={units}\n"
            ));
            let s = status(Some(&p), Some(&o));
            assert_eq!(s.primary, B_::InSync, "{units}");
            assert_eq!(s.badges, [B_::InSync, B_::FailedUnits], "{units}");
            assert_eq!(
                s.reason.as_deref(),
                Some("systemd reports degraded"),
                "{units}"
            );
        }
        // With the units known, the reason names them, after the primary's own reason.
        let p = probe(&format!(
            "current_system={A}\nsystem_state=degraded\nfailed_units=[{{\"unit\":\"a.service\"}},\
             {{\"unit\":\"b.service\"}},{{\"unit\":\"c.mount\"}},{{\"unit\":\"d.timer\"}}]\n"
        ));
        let next = ok(B);
        let s = status(Some(&p), Some(&next));
        assert_eq!(s.badges, [B_::UpdatePending, B_::FailedUnits]);
        assert_eq!(
            s.reason.as_deref(),
            Some(
                "pending nixos-system-terra-b; failed units: a.service, b.service, c.mount and 1 more"
            )
        );
    }

    /// F4: unknown health is said in the reason, without a badge.
    #[test]
    fn unknown_health_is_a_reason_not_a_badge() {
        let o = ok(A);
        for (lines, why) in [
            // What the real probe prints when systemctl cannot reach the bus.
            (
                "system_state=\nfailed_units=\n",
                "health unknown: systemd state could not be read",
            ),
            (
                "failed_units=[]\n",
                "health unknown: systemd state could not be read",
            ),
            (
                "system_state=running\nfailed_units=garbage\n",
                "health unknown: failed units could not be read",
            ),
            (
                "system_state=starting\nfailed_units=[]\n",
                "systemd is starting",
            ),
        ] {
            let p = probe(&format!("current_system={A}\n{lines}"));
            let s = status(Some(&p), Some(&o));
            assert_eq!(s.badges, [B_::InSync], "{lines:?}");
            assert_eq!(s.reason.as_deref(), Some(why), "{lines:?}");
            // Still waiting for the eval: appended to that reason.
            let s = status(Some(&p), None);
            assert_eq!(
                s.reason,
                Some(format!("waiting for the evaluation; {why}")),
                "{lines:?}"
            );
        }
        // Darwin has no systemd, so nothing is unknown there.
        let p = parse_probe(
            &format!("auspex_probe=1\nprobe=darwin\ncurrent_system={A}\nauspex_end=1\n"),
            "",
            ProbeExit::Code(0),
        );
        assert_eq!(status(Some(&p), Some(&o)).reason, None);
        // For a machine that is not installed, only the notes behind ⚠ are added, after its
        // own reason (N7: the reason mentions both). Health unknown is not.
        let p = probe("current_system=\nsystem_state=degraded\n");
        let s = status(Some(&p), Some(&o));
        assert_eq!(s.primary, B_::NotInstalled);
        assert_eq!(s.badges, [B_::FailedUnits, B_::NotInstalled]);
        assert_eq!(
            s.reason.as_deref(),
            Some("reachable, but /run/current-system is missing; systemd reports degraded")
        );
        let p = probe("current_system=\n");
        assert_eq!(
            status(Some(&p), Some(&o)).reason.as_deref(),
            Some("reachable, but /run/current-system is missing")
        );
        // And never to an unreachable or failing machine.
        let e = err();
        let p = probe(&format!("current_system={A}\nsystem_state=degraded\n"));
        assert_eq!(
            status(Some(&p), Some(&e)).reason.as_deref(),
            Some("error: attribute 'x' missing")
        );
    }

    #[test]
    fn alerts_are_named_in_the_reason() {
        let (p, o) = (at(A), ok(A));
        for (n, why) in [(1, "1 alert firing"), (3, "3 alerts firing")] {
            let s = machine_status(&MachineFacts {
                probe: Some(&p),
                eval: Some(&o),
                alerts_firing: n,
                ..MachineFacts::default()
            });
            assert_eq!(s.badges, [B_::InSync, B_::FailedUnits]);
            assert_eq!(s.reason.as_deref(), Some(why));
        }
    }

    /// M43: a changed initrd alone needs a reboot.
    #[test]
    fn initrd_only_change_needs_a_reboot() {
        const I1: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-initrd-linux-6.12.1/initrd";
        const I2: &str = "/nix/store/zyxwvsrqpnmlkjihgfdcba9876543210-initrd-linux-6.12.1/initrd";
        let p = probe(&format!(
            "current_system={A}\nsystem_state=running\nfailed_units=[]\n\
             booted_kernel={K1}\ncurrent_kernel={K1}\nbooted_initrd={I1}\ncurrent_initrd={I2}\n"
        ));
        let o = ok(A);
        assert_eq!(
            status(Some(&p), Some(&o)).badges,
            [B_::InSync, B_::RebootNeeded]
        );
        let same = probe(&format!(
            "current_system={A}\nsystem_state=running\nfailed_units=[]\n\
             booted_kernel={K1}\ncurrent_kernel={K1}\nbooted_initrd={I1}\ncurrent_initrd={I1}\n"
        ));
        assert_eq!(status(Some(&same), Some(&o)).badges, [B_::InSync]);
    }

    /// N7: what each of systemd's states does to the row.
    #[test]
    fn each_systemd_state() {
        let o = ok(A);
        for (state, badges, reason) in [
            ("running", &[B_::InSync][..], None),
            (
                "degraded",
                &[B_::InSync, B_::FailedUnits][..],
                Some("systemd reports degraded"),
            ),
            (
                "maintenance",
                &[B_::InSync, B_::FailedUnits][..],
                Some("systemd is in maintenance mode (rescue or emergency)"),
            ),
            ("starting", &[B_::InSync][..], Some("systemd is starting")),
            (
                "initializing",
                &[B_::InSync][..],
                Some("systemd is initializing"),
            ),
            ("stopping", &[B_::InSync][..], Some("systemd is stopping")),
            (
                "offline",
                &[B_::InSync][..],
                Some("health unknown: systemd reports offline"),
            ),
            (
                "unknown",
                &[B_::InSync][..],
                Some("health unknown: systemd reports unknown"),
            ),
            (
                "rescue",
                &[B_::InSync][..],
                Some("health unknown: systemd state could not be read"),
            ),
        ] {
            let p = probe(&format!(
                "current_system={A}\nsystem_state={state}\nfailed_units=[]\n"
            ));
            let s = status(Some(&p), Some(&o));
            assert_eq!(s.badges, badges, "{state}");
            assert_eq!(s.reason.as_deref(), reason, "{state}");
        }
        // Maintenance with failed units names both.
        let p = probe(&format!(
            "current_system={A}\nsystem_state=maintenance\nfailed_units=[{{\"unit\":\"a.service\"}}]\n"
        ));
        assert_eq!(
            status(Some(&p), Some(&o)).reason.as_deref(),
            Some("failed units: a.service; systemd is in maintenance mode (rescue or emergency)")
        );
        // A listed failed unit warns whatever the state says.
        for state in ["running", "", "starting"] {
            let p = probe(&format!(
                "current_system={A}\nsystem_state={state}\nfailed_units=[{{\"unit\":\"a.service\"}}]\n"
            ));
            let s = status(Some(&p), Some(&o));
            assert_eq!(s.badges, [B_::InSync, B_::FailedUnits], "{state:?}");
            assert_eq!(
                s.reason.as_deref(),
                Some("failed units: a.service"),
                "{state:?}"
            );
        }
    }

    /// N7: remote text cannot forge a note, so the reason's `; ` pieces are all auspex's.
    #[test]
    fn remote_text_cannot_forge_a_note() {
        let o = ok(A);
        for lines in [
            "system_state=running; 3 alerts firing\nfailed_units=[]\n".to_owned(),
            "system_state=running\nfailed_units=[{\"unit\":\"a.service; 9 alerts firing\"}]\n"
                .to_owned(),
            "system_state=degraded\nfailed_units=[{\"unit\":\"a.service; systemd reports degraded; 9 alerts firing\"}]\n"
                .to_owned(),
        ] {
            let p = probe(&format!("current_system={A}\n{lines}"));
            let s = status(Some(&p), Some(&o));
            let reason = s.reason.unwrap_or_default();
            assert!(!reason.contains("alerts firing"), "{lines:?}: {reason}");
            for note in reason.split("; ") {
                assert!(
                    [
                        "systemd reports degraded",
                        "health unknown: systemd state could not be read",
                        "health unknown: failed units could not be read",
                    ]
                    .contains(&note),
                    "{lines:?}: unexpected note {note:?}"
                );
            }
        }
    }

    /// N2: the settled rule, fact by fact.
    #[test]
    fn settled_needs_every_answer() {
        let (p, o) = (at(A), ok(B));
        let done = MachineFacts {
            probe: Some(&p),
            eval: Some(&o),
            cache: NarCache::Cached,
            ..MachineFacts::default()
        };
        assert!(done.is_settled());
        for cache in [
            NarCache::NotApplicable,
            NarCache::Failed,
            NarCache::Cached,
            NarCache::NotCached,
        ] {
            assert!(MachineFacts { cache, ..done }.is_settled(), "{cache:?}");
        }
        assert!(
            !MachineFacts {
                cache: NarCache::Pending,
                ..done
            }
            .is_settled()
        );
        for outstanding in [
            Outstanding {
                prometheus: true,
                alerts: false,
            },
            Outstanding {
                prometheus: false,
                alerts: true,
            },
        ] {
            assert!(
                !MachineFacts {
                    outstanding,
                    ..done
                }
                .is_settled(),
                "{outstanding:?}"
            );
        }
        assert!(
            !MachineFacts {
                probe: None,
                ..done
            }
            .is_settled()
        );
        assert!(!MachineFacts { eval: None, ..done }.is_settled());
        assert!(
            MachineFacts {
                probe: None,
                target_error: Some("invalid host"),
                ..done
            }
            .is_settled()
        );
    }
    #[test]
    fn refused_target_preserves_observed_update_badges() {
        let p = at(A);
        let e = ok(B);
        let facts = MachineFacts {
            probe: Some(&p),
            eval: Some(&e),
            target_error: Some("refused"),
            cache: NarCache::Cached,
            ..MachineFacts::default()
        };
        let status = machine_status(&facts);
        assert_eq!(status.primary, Badge::Unreachable);
        assert_eq!(
            status.badges,
            [Badge::UpdatePending, Badge::Cached, Badge::Unreachable]
        );
        assert_eq!(status.reason.as_deref(), Some("invalid target: refused"));
        assert!(
            !MachineFacts {
                cache: NarCache::Pending,
                ..facts
            }
            .is_settled()
        );
        assert!(facts.is_settled());
    }

    #[test]
    fn irrelevant_cache_lookup_does_not_delay_settlement() {
        let observed = at(A);
        let absent = probe("current_system=\n");
        let dead = unreachable();
        let failed = ProbeOutcome::Failed {
            reason: "cut off".to_owned(),
        };
        let same = ok(A);
        let different = ok(B);
        let error = err();
        for target_error in [None, Some("refused")] {
            for p in [
                None,
                Some(&observed),
                Some(&absent),
                Some(&dead),
                Some(&failed),
            ] {
                for e in [None, Some(&same), Some(&different), Some(&error)] {
                    let facts = MachineFacts {
                        probe: p,
                        eval: e,
                        target_error,
                        cache: NarCache::Pending,
                        ..MachineFacts::default()
                    };
                    let waiting_for_cache = p == Some(&observed) && e == Some(&different);
                    let expected = (p.is_some() || target_error.is_some())
                        && e.is_some()
                        && !waiting_for_cache;
                    assert_eq!(facts.is_settled(), expected, "{facts:?}");
                    if expected {
                        for cache in [NarCache::Cached, NarCache::NotCached, NarCache::Failed] {
                            assert_eq!(
                                machine_status(&facts),
                                machine_status(&MachineFacts { cache, ..facts }),
                                "{facts:?}: {cache:?}"
                            );
                        }
                    }
                    for outstanding in [
                        Outstanding {
                            prometheus: true,
                            alerts: false,
                        },
                        Outstanding {
                            prometheus: false,
                            alerts: true,
                        },
                    ] {
                        assert!(
                            !MachineFacts {
                                outstanding,
                                ..facts
                            }
                            .is_settled()
                        );
                    }
                }
            }
        }
    }
}
