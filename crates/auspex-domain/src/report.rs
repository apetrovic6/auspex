//! The `--json` contract: what `auspex --json` prints once the fleet has settled.
//!
//! Scripts (the weekly digest) and coding agents read this, so it is versioned and every
//! field is documented here. The rules for changing it:
//!
//! - Adding a field is compatible: readers must ignore fields they do not know.
//! - Removing or renaming a field, or changing a field's type or meaning, bumps
//!   [`JSON_VERSION`].
//! - `tests/json_contract.rs` fails on any change to the serialised shape, so neither can
//!   happen by accident; update its fixture in the same change, and bump the version if the
//!   change is not an addition.
//!
//! Conventions: field names are `snake_case`; enums are `snake_case` strings; a value that is
//! unknown or not applicable is `null`, never omitted; lists are never `null`, with one
//! documented exception, `failed_units`, where `null` means "unknown" and `[]` means
//! "none".
//!
//! Every string that came from outside auspex (ssh's and nix's text, inventory tags and
//! `buildHost`, fleet error reasons) has had escape sequences, control characters and
//! Unicode format characters removed, so `--plain` and the TUI can print it as is.

use serde::{Deserialize, Serialize};

use crate::ci::FleetCi;
use crate::diff::{DiffState, PackageDiff};
use crate::eval::EvalOutcome;
use crate::fleet::{
    Lint, Machine, MachineClass, ResolveError, ResolvedTarget, SharedIp, TargetSource,
    lint_hostname, shared_ip_lints,
};
use crate::observe::{Health, ProbeOutcome};
use crate::status::{Badge, MachineFacts, machine_status};
use crate::text::sanitize_line;

/// The version of this contract. Bumped on any change that is not a pure addition.
pub const JSON_VERSION: u32 = 1;

/// The whole of `auspex --json`'s output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetReport {
    /// [`JSON_VERSION`]. Check it before reading anything else.
    pub version: u32,
    /// `true` exactly when the scan is over and its result is final, which is all of:
    ///
    /// - every fleet-wide request is back: the machine list, the evaluator's final answer,
    ///   any query for the whole fleet (Prometheus, Alertmanager), address resolution and the
    ///   CI badge's look at the forge;
    /// - the global deadline did not cut the scan short;
    /// - `errors` is empty;
    /// - every machine's `settled` is `true`.
    ///
    /// So a report built while the machine list is still being read (no machines yet) is not
    /// settled, a report cut off by the deadline is not settled even when every row it has is,
    /// and a fleet whose machine list could not be read never reads as a settled, empty fleet.
    pub settled: bool,
    /// Fleet-level failures, each `{"kind": …, "reason": …}`: something that went wrong for
    /// the whole scan rather than for one machine. `kind` is one of `"inventory_failed"`
    /// (the machine list could not be read or parsed, so `machines` is empty or partial),
    /// `"inventory_timed_out"` (reading it took longer than its timeout) and
    /// `"evaluation_failed"` (the evaluator itself could not run, so no machine has an
    /// evaluation) and `"freezes_unknown"` (the freezes made with `f` could not be read: a
    /// corrupt or newer state file, or no state directory; every machine's `frozen` is then
    /// only what `config.toml` declares, so **a reader must not treat `frozen: false` as "may
    /// be updated"**; added to version 1, which readers of version 1 ignore, as they do an
    /// unknown `kind`). `reason` is one line. Always present; `[]` when there are none.
    pub errors: Vec<FleetError>,
    /// One entry per inventory machine, sorted by name.
    pub machines: Vec<MachineReport>,
    /// Addresses more than one machine's target resolves to, each `{"ip": "<address>",
    /// "machines": [names, sorted, at least two]}`, sorted by address. Deploying to any of
    /// these machines is refused. Empty when there are none or resolution was not
    /// attempted.
    pub shared_ips: Vec<SharedIp>,
    /// The CI badge for the fleet flake's current commit (`HEAD` of the fleet checkout):
    /// `{"commit": …, "state": …, "url": …, "reason": …}`. `state` is one of `"success"`,
    /// `"failure"`, `"pending"` (the forge's combined commit status; `error` is a `failure`),
    /// `"none"` (the forge has no status for the commit), `"not_pushed"` (`HEAD` is ahead of its
    /// upstream, so nothing was asked), `"unknown"` (it could not be found out; `reason` says
    /// why) or `"off"` (`[forge]` is not configured, and nothing was read or asked). A dirty
    /// tree is not a state: `HEAD`'s status is shown as it is. `commit` is the full commit id, or
    /// `null` when it was not read; `url` links to the check, or is `null`; `reason` is one line
    /// for `unknown` and `not_pushed`, `null` otherwise. The forge's token never appears in any
    /// of them: text from outside (a link, a transport error, git's words) is checked for it, in
    /// any case and percent-encoded, and replaced when it holds it. A copy a forge disguises in a
    /// way of its own invention (base64, a split) cannot be recognised by any such check. Added to
    /// version 1; a report without it reads as `off`.
    #[serde(default)]
    pub fleet_ci: FleetCi,
}

/// What went wrong for the whole fleet; see [`FleetReport::errors`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetErrorKind {
    /// The machine list could not be read or parsed.
    InventoryFailed,
    /// Reading the machine list took longer than its timeout.
    InventoryTimedOut,
    /// The evaluator could not run at all (not one machine's eval error: those stay on
    /// that machine's row).
    EvaluationFailed,
    /// Which machines were frozen with `f` is not known (the state file could not be read), so
    /// `frozen` says only what the config declares. See [`MachineReport::frozen`].
    FreezesUnknown,
}

/// One fleet-level failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetError {
    /// What failed.
    pub kind: FleetErrorKind,
    /// One line on why, sanitised for the terminal.
    pub reason: String,
}

impl FleetError {
    /// A fleet error with `reason` sanitised (it is usually a tool's stderr).
    #[must_use]
    pub fn new(kind: FleetErrorKind, reason: &str) -> Self {
        Self {
            kind,
            reason: sanitize_line(reason),
        }
    }
}

/// Where the scan as a whole stands, for [`fleet_report`].
///
/// The default is "still in progress, no deadline, no fleet errors": a caller has to say the
/// scan is complete before a report can read as settled, so forgetting to cannot produce a
/// settled, empty fleet.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FleetFacts {
    /// Every fleet-wide request is back: the machine list, the evaluator's final answer
    /// (the process exited, so no more lines are coming), and any query for the whole fleet
    /// (Prometheus, Alertmanager). Keep this `false` while address resolution is outstanding:
    /// its shared-IP result can still change machine lints, even for settled machines.
    pub scan_complete: bool,
    /// The global deadline passed before every machine settled.
    pub timed_out: bool,
    /// Fleet-level failures.
    pub errors: Vec<FleetError>,
    /// The CI badge as it stands ([`FleetCi::off`] when `[forge]` is not configured). While its
    /// request is out it is an `unknown` that says so, and `scan_complete` stays `false`.
    pub fleet_ci: FleetCi,
}

/// One machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineReport {
    /// The inventory name.
    pub name: String,
    /// `"nixos"` or `"darwin"`.
    pub class: MachineClass,
    /// The inventory tags.
    pub tags: Vec<String>,
    /// The resolved ssh target in canonical form (`root@[fd00::1]:2222?Compression=yes`), or
    /// `null` when the winning source did not parse (see `target_error`).
    pub target: Option<String>,
    /// Which source the target came from: `"override"` (auspex's config), `"inventory"`
    /// (`deploy.targetHost`) or `"machine_name"`. Set even when the target did not parse.
    pub target_source: TargetSource,
    /// Why the target was refused, or `null`. A refused target is never probed.
    pub target_error: Option<String>,
    /// The inventory's `deploy.buildHost`, unparsed, or `null`.
    pub build_host: Option<String>,
    /// The primary status: one of `"checking"`, `"evaluating"`, `"in_sync"`,
    /// `"update_pending"`, `"not_installed"`, `"unreachable"`, `"eval_error"`. Precedence,
    /// first that applies: unreachable, eval error, not installed, checking, evaluating,
    /// update pending or in sync.
    pub status: Badge,
    /// Every badge that applies, `status` included, in the plan's table order. Adds to the
    /// above `"cached"`, `"failed_units"`, `"reboot_needed"` and `"frozen"`, which are never
    /// the primary status. `"failed_units"` means a failed unit, systemd reporting
    /// `degraded` or `maintenance`, or an alert firing.
    pub badges: Vec<Badge>,
    /// `true` when this machine's probe, evaluation and applicable auxiliary answers are
    /// final: its probe is back (or its target was refused), its evaluation is back, its
    /// ncps lookup is not pending or no update is pending, its package preview is not still
    /// being collected, and neither its Prometheus nor its Alertmanager answer is
    /// outstanding. Computed from facts, never from badges:
    /// an unreachable machine whose evaluation is still running is not settled, nor is
    /// one with an update pending whose cache lookup has not answered. This does not cover
    /// shared-IP resolution, which may still change `lints`; only [`FleetReport::settled`]
    /// covers that fleet-wide work, through [`FleetFacts::scan_complete`].
    pub settled: bool,
    /// One line on why `status` is what it is, **for people**: ssh's last stderr line, nix's
    /// last error line, or what is being waited for. For `in_sync`, `update_pending` and
    /// `evaluating` it is followed by health notes joined with `"; "`: the failed units,
    /// `systemd reports degraded`, maintenance mode, alerts firing, a transitional systemd
    /// state, or `health unknown: …` when a NixOS machine's systemd state or failed-unit list
    /// could not be read (or systemd says `offline` or `unknown`). For `not_installed`, only
    /// the notes behind a `failed_units` badge are added. `null` for `in_sync` with nothing
    /// to note. Programs must read `badges`, `system_state` and `failed_units` instead, never
    /// parse this text.
    /// An update-pending row appends `cache lookup failed` when its cache lookup failed.
    pub reason: Option<String>,
    /// `/run/current-system`'s store path, or `null` when not observed or not installed.
    pub current_system: Option<String>,
    /// The evaluated toplevel's store path, or `null` when not evaluated (yet, or at all).
    pub pending_system: Option<String>,
    /// Package preview; null until gathered. See [`PackageDiff`] for states, caps and
    /// truncation. `not_cached` means unknown until built or cached, not no changes.
    #[serde(default)]
    pub package_diff: Option<PackageDiff>,
    /// The deployed `system.configurationRevision`, or `null` when the machine does not set
    /// it or was not observed.
    pub deployed_revision: Option<String>,
    /// `systemctl is-system-running`: one of `"initializing"`, `"starting"`, `"running"`,
    /// `"degraded"`, `"maintenance"`, `"stopping"`, `"offline"`, `"unknown"`; or `null`
    /// (darwin, not observed, or the probe printed anything else).
    pub system_state: Option<String>,
    /// Failed systemd units, each a systemd unit name (`[A-Za-z0-9:_.\@-]`, at most 255
    /// characters), or `null` when unknown (darwin, not observed, not readable, or holding a
    /// name outside that set). `[]` means systemd reported none. The one list in this
    /// contract that can be `null`.
    pub failed_units: Option<Vec<String>>,
    /// Health values, each `null` or `{"value": …, "source": "prometheus" | "ssh"}`:
    /// `uptime_seconds` (integer), `root_disk` (`{total_bytes, used_bytes,
    /// available_bytes}`), `load` (`{one, five, fifteen}`), `memory` (`{total_bytes,
    /// available_bytes}`, Prometheus only).
    pub health: Health,
    /// Homelab-safety warnings: `{"kind": "hostname_target", "host"}` and `{"kind":
    /// "shared_ip", "ip", "with": [machines]}`.
    pub lints: Vec<Lint>,
    /// Frozen, so `u` refuses it and `U` skips it: by `machines.<m>.frozen = true` in `config.toml`,
    /// or with `f` in the TUI (kept in auspex's state file, per fleet). `false` when neither says
    /// so, and **also when it is not known which machines `f` froze** (the state file could not
    /// be read): then it is only what the config declares, and the report carries a
    /// `"freezes_unknown"` entry in `errors` (see [`FleetReport::errors`]), which a reader must
    /// check before treating `false` as "may be updated". Always present: added to version 1 as
    /// a field, which readers of version 1 ignore.
    pub frozen: bool,
}

fn headline_of(diff: &PackageDiff) -> String {
    let counts = diff.counts;
    let detail = diff
        .highlights
        .iter()
        .find(|h| h.name == "linux")
        .or_else(|| diff.highlights.first())
        .map(|h| {
            let name = h.name.get(..40).unwrap_or(&h.name);
            let before = h.before.first().and_then(|v| v.as_deref());
            let after = h.after.first().and_then(|v| v.as_deref());
            match (before, after) {
                (Some(a), Some(b)) if name.len() + a.len() + b.len() <= 80 => {
                    format!("{name} {a} → {b}, ")
                }
                (None, Some(v)) if name.len() + v.len() <= 80 => {
                    format!("{name} added {v}, ")
                }
                (Some(v), None) if name.len() + v.len() <= 80 => {
                    format!("{name} removed {v}, ")
                }
                _ => format!("{name} changed, "),
            }
        });
    if counts.added + counts.removed + counts.changed == 0
        && diff.rebuilt.is_empty()
        && diff.unversioned.added + diff.unversioned.removed == 0
    {
        "packages unchanged (configuration only)".to_owned()
    } else if let Some(detail) = detail {
        format!(
            "{detail}+{} −{} ~{}",
            counts.added, counts.removed, counts.changed
        )
    } else if let Some(name) = diff.rebuilt.first() {
        format!(
            "{name} rebuilt, +{} −{} ~{}",
            counts.added, counts.removed, counts.changed
        )
    } else {
        format!(
            "packages +{} −{} ~{}",
            counts.added, counts.removed, counts.changed
        )
    }
}

/// The one-line headline of a computed preview, or `None` for any other state.
///
/// What [`MachineReport::with_package_diff`] appends to `reason`, so a shell can show it on
/// its own line without parsing human text out of `reason`.
#[must_use]
pub fn diff_headline(diff: &PackageDiff) -> Option<String> {
    (diff.state == DiffState::Computed).then(|| headline_of(diff))
}

impl MachineReport {
    /// The tags worth showing next to the class: Clan's inventory adds `all` and the
    /// machine's class (`nixos` or `darwin`) to every machine, and those say nothing the class
    /// does not. [`Self::tags`] and `--json` keep the full list; this is for display only.
    pub fn display_tags(&self) -> impl Iterator<Item = &str> {
        let class = match self.class {
            MachineClass::Nixos => "nixos",
            MachineClass::Darwin => "darwin",
        };
        self.tags
            .iter()
            .map(String::as_str)
            .filter(move |t| *t != "all" && *t != class)
    }

    /// Attaches a pure preview; only computed update-pending rows gain a short headline.
    ///
    /// Status, badges and settlement never depend on the preview. Call this once for a
    /// freshly built report; reason remains human text, not a machine-readable interface.
    #[must_use]
    pub fn with_package_diff(mut self, diff: PackageDiff) -> Self {
        if self.status == Badge::UpdatePending
            && let Some(headline) = diff_headline(&diff)
        {
            if let Some(reason) = &mut self.reason {
                let position = reason.find(';').unwrap_or(reason.len());
                reason.insert_str(position, &format!("; {headline}"));
            } else {
                self.reason = Some(headline);
            }
        }
        self.package_diff = Some(diff);
        self
    }
}

/// Builds one machine's entry.
///
/// `shared` is the output of [`crate::fleet::shared_ips`] (empty when resolution was not
/// attempted); `health` is the output of [`crate::observe::merge_health`]. A refused target
/// makes the machine unreachable whatever `facts.target_error` says, so the report and the
/// target cannot disagree. The name, tags and `buildHost` are sanitised here as well as in
/// [`crate::fleet::parse_inventory`], for a [`Machine`] built some other way.
#[must_use]
pub fn machine_report(
    machine: &Machine,
    target: &Result<ResolvedTarget, ResolveError>,
    facts: &MachineFacts<'_>,
    health: Health,
    shared: &[SharedIp],
) -> MachineReport {
    let target_error = target.as_ref().err().map(|e| e.error.to_string());
    let status = machine_status(&MachineFacts {
        target_error: target_error.as_deref(),
        ..*facts
    });
    let observation = match facts.probe {
        Some(ProbeOutcome::Observed(o)) => Some(&**o),
        _ => None,
    };
    let mut lints: Vec<Lint> = target
        .as_ref()
        .ok()
        .and_then(|t| lint_hostname(&t.target))
        .into_iter()
        .collect();
    lints.extend(shared_ip_lints(&machine.name, shared));
    MachineReport {
        name: sanitize_line(&machine.name),
        class: machine.class,
        tags: machine
            .tags
            .iter()
            .map(|t| sanitize_line(t))
            .filter(|t| !t.is_empty())
            .collect(),
        target: target.as_ref().ok().map(|t| t.target.to_string()),
        target_source: match target {
            Ok(t) => t.source,
            Err(e) => e.target_source,
        },
        build_host: machine
            .build_host
            .as_deref()
            .map(sanitize_line)
            .filter(|b| !b.is_empty()),
        settled: MachineFacts {
            target_error: target_error.as_deref(),
            ..*facts
        }
        .is_settled(),
        target_error,
        status: status.primary,
        badges: status.badges,
        reason: status.reason,
        current_system: observation
            .and_then(|o| o.current_system.as_ref())
            .map(ToString::to_string),
        pending_system: match facts.eval {
            Some(EvalOutcome::Success(s)) => Some(s.out_path.to_string()),
            _ => None,
        },
        package_diff: None,
        deployed_revision: observation.and_then(|o| o.configuration_revision.clone()),
        system_state: observation.and_then(|o| o.system_state.clone()),
        failed_units: observation.and_then(|o| o.failed_units.clone()),
        health,
        lints,
        frozen: facts.frozen,
    }
}

/// Assembles the report: sorts machines by name, sanitises the fleet errors' reasons, and
/// computes `settled` (see [`FleetReport::settled`]).
#[must_use]
pub fn fleet_report(
    mut machines: Vec<MachineReport>,
    shared_ips: Vec<SharedIp>,
    fleet: FleetFacts,
) -> FleetReport {
    machines.sort_by(|a, b| a.name.cmp(&b.name));
    let errors: Vec<FleetError> = fleet
        .errors
        .into_iter()
        .map(|e| FleetError::new(e.kind, &e.reason))
        .collect();
    FleetReport {
        version: JSON_VERSION,
        settled: fleet.scan_complete
            && !fleet.timed_out
            && errors.is_empty()
            && machines.iter().all(|m| m.settled),
        errors,
        machines,
        shared_ips,
        fleet_ci: fleet.fleet_ci,
    }
}
