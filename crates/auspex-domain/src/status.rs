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
    /// `⚠`: a failed unit, systemd reporting `degraded`, or an alert firing.
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

/// Whether the pending toplevel is in ncps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NarCache {
    /// Not asked yet, or the request failed.
    #[default]
    Unknown,
    /// The `.narinfo` exists.
    Cached,
    /// It does not.
    NotCached,
}

/// Everything known about one machine. `None` means "not back yet".
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MachineFacts<'a> {
    /// The probe's outcome.
    pub probe: Option<&'a ProbeOutcome>,
    /// The evaluation's outcome.
    pub eval: Option<&'a EvalOutcome>,
    /// The ncps lookup for the pending toplevel.
    pub cache: NarCache,
    /// Frozen in the config.
    pub frozen: bool,
    /// Alertmanager alerts firing for this machine (0 until step 3 collects them).
    pub alerts_firing: u32,
    /// Why the machine's target was refused, when it was. Such a machine is never probed,
    /// so without this it would show "checking" forever and `--json` would never settle;
    /// with it, the machine is unreachable with this reason.
    pub target_error: Option<&'a str>,
}

/// A machine's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineStatus {
    /// The single primary status; see the module docs for the precedence.
    pub primary: Badge,
    /// Every badge that applies, the primary among them, in the plan's table order.
    pub badges: Vec<Badge>,
    /// One line on why, for the detail panel: ssh's or nix's last line, or what is being
    /// waited for, followed for a sync state by any health notes (failed units, `degraded`,
    /// alerts, or health unknown), joined with `; `. `None` for a `✓` with nothing to note.
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
    let update_pending = pending.is_some() && current.is_some() && !in_sync;

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
        (Badge::UpdatePending, Some(format!("pending {name}")))
    };

    // Health qualifies a machine that is up and has a system: it is appended to the reason
    // of a sync state (or of "evaluating", which becomes one), and never replaces the reason
    // for an unreachable, failed or uninstalled machine.
    let (notes, warn) = health_notes(observation, facts.alerts_firing);
    let reason = if matches!(
        primary,
        Badge::Evaluating | Badge::InSync | Badge::UpdatePending
    ) && !notes.is_empty()
    {
        let notes = notes.join("; ");
        Some(reason.map_or_else(|| notes.clone(), |r| format!("{r}; {notes}")))
    } else {
        reason
    };

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

/// How many failed units a reason names before it says "and N more".
const UNITS_IN_REASON: usize = 3;

/// The health notes for the reason, and whether they raise [`Badge::FailedUnits`].
///
/// The rule, NixOS only (darwin has no systemd, so nothing is unknown there):
///
/// - `⚠` when systemd lists a failed unit, when `system_state` is `degraded` (which means a
///   unit has failed, even if the list could not be read or came back empty), or when an
///   alert is firing. The reason names the units, or says systemd reports degraded, and
///   counts the alerts.
/// - Without `⚠`: if `system_state` or the failed-unit list could not be read, the reason
///   says health is unknown. There is no badge for that: the values stay `null` and the row
///   keeps its sync badge, but it no longer reads as a plain `✓`.
/// - Any other state than `running` (`starting`, `maintenance`, …) is named in the reason.
fn health_notes(observation: Option<&Observation>, alerts_firing: u32) -> (Vec<String>, bool) {
    let mut notes = Vec::new();
    let mut warn = false;
    if let Some(o) = observation {
        let units = o.failed_units.as_deref().unwrap_or_default();
        if !units.is_empty() {
            warn = true;
            let shown = units[..units.len().min(UNITS_IN_REASON)].join(", ");
            notes.push(match units.len().saturating_sub(UNITS_IN_REASON) {
                0 => format!("failed units: {shown}"),
                more => format!("failed units: {shown} and {more} more"),
            });
        } else if o.is_degraded() {
            warn = true;
            notes.push("systemd reports degraded".to_owned());
        }
        if o.class == Some(MachineClass::Nixos) && !warn {
            match (o.system_state.as_deref(), &o.failed_units) {
                (None, _) => {
                    notes.push("health unknown: systemd state could not be read".to_owned());
                }
                (Some(_), None) => {
                    notes.push("health unknown: failed units could not be read".to_owned());
                }
                (Some("running"), Some(_)) => {}
                (Some(state), Some(_)) => notes.push(format!("systemd is {state}")),
            }
        }
    }
    if alerts_firing > 0 {
        warn = true;
        notes.push(match alerts_firing {
            1 => "1 alert firing".to_owned(),
            n => format!("{n} alerts firing"),
        });
    }
    (notes, warn)
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
            (NarCache::Unknown, &[B_::UpdatePending][..]),
        ] {
            let s = machine_status(&MachineFacts {
                probe: Some(&p),
                eval: Some(&o),
                cache,
                ..MachineFacts::default()
            });
            assert_eq!(s.primary, B_::UpdatePending);
            assert_eq!(s.badges, want);
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
        // Health never replaces the reason of an unhealthy primary.
        let p = probe("current_system=\nsystem_state=degraded\n");
        let s = status(Some(&p), Some(&o));
        assert_eq!(s.primary, B_::NotInstalled);
        assert_eq!(
            s.reason.as_deref(),
            Some("reachable, but /run/current-system is missing")
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
}
