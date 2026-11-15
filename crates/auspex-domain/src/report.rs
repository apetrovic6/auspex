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
    /// `true` when the scan completed: every machine's `settled` is `true`, the global
    /// deadline did not cut the scan short, and `errors` is empty. `false` otherwise, so a
    /// fleet whose machine list could not be read never reads as a settled, empty fleet.
    pub settled: bool,
    /// Fleet-level failures, each `{"kind": …, "reason": …}`: something that went wrong for
    /// the whole scan rather than for one machine. `kind` is one of `"inventory_failed"`
    /// (the machine list could not be read or parsed, so `machines` is empty or partial),
    /// `"inventory_timed_out"` (reading it took longer than its timeout) and
    /// `"evaluation_failed"` (the evaluator itself could not run, so no machine has an
    /// evaluation). `reason` is one line. Always present; `[]` when there are none.
    pub errors: Vec<FleetError>,
    /// One entry per inventory machine, sorted by name.
    pub machines: Vec<MachineReport>,
    /// Addresses more than one machine's target resolves to, each `{"ip": "<address>",
    /// "machines": [names, sorted, at least two]}`, sorted by address. Deploying to any of
    /// these machines is refused. Empty when there are none or resolution was not
    /// attempted.
    pub shared_ips: Vec<SharedIp>,
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

/// How the scan as a whole ended, for [`fleet_report`]. The default is "no deadline, no
/// fleet errors".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FleetFacts {
    /// The global deadline passed before every machine settled.
    pub timed_out: bool,
    /// Fleet-level failures.
    pub errors: Vec<FleetError>,
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
    /// The resolved ssh target in canonical form (`root@[fd00::1]:2222?ProxyJump=j`), or
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
    /// `degraded`, or an alert firing.
    pub badges: Vec<Badge>,
    /// `true` when nothing more is coming for this machine: its probe is back (or its
    /// target was refused, so it is never probed) and its evaluation is back. Computed from
    /// those facts, not from the badges, so an unreachable machine whose evaluation is still
    /// running is not settled.
    pub settled: bool,
    /// One line on why `status` is what it is: ssh's last stderr line, nix's last error line,
    /// or what is being waited for. For `in_sync`, `update_pending` and `evaluating`,
    /// followed by health notes joined with `"; "`: the failed units, `systemd reports
    /// degraded`, alerts firing, or `health unknown: …` when a NixOS machine's systemd state
    /// or failed-unit list could not be read. `null` for `in_sync` with nothing to note.
    pub reason: Option<String>,
    /// `/run/current-system`'s store path, or `null` when not observed or not installed.
    pub current_system: Option<String>,
    /// The evaluated toplevel's store path, or `null` when not evaluated (yet, or at all).
    pub pending_system: Option<String>,
    /// The deployed `system.configurationRevision`, or `null` when the machine does not set
    /// it or was not observed.
    pub deployed_revision: Option<String>,
    /// `systemctl is-system-running` (`"running"`, `"degraded"`, …), or `null` (darwin, not
    /// observed, or not readable).
    pub system_state: Option<String>,
    /// Failed systemd units, or `null` when unknown (darwin, not observed, or not readable).
    /// `[]` means systemd reported none. The one list in this contract that can be `null`.
    pub failed_units: Option<Vec<String>>,
    /// Health values, each `null` or `{"value": …, "source": "prometheus" | "ssh"}`:
    /// `uptime_seconds` (integer), `root_disk` (`{total_bytes, used_bytes,
    /// available_bytes}`), `load` (`{one, five, fifteen}`), `memory` (`{total_bytes,
    /// available_bytes}`, Prometheus only).
    pub health: Health,
    /// Homelab-safety warnings: `{"kind": "hostname_target", "host"}` and `{"kind":
    /// "shared_ip", "ip", "with": [machines]}`.
    pub lints: Vec<Lint>,
    /// Frozen in the config, so `U` skips it.
    pub frozen: bool,
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
        settled: (facts.probe.is_some() || target_error.is_some()) && facts.eval.is_some(),
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
        settled: !fleet.timed_out && errors.is_empty() && machines.iter().all(|m| m.settled),
        errors,
        machines,
        shared_ips,
    }
}
