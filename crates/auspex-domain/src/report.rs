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
//! unknown or not applicable is `null`, never omitted; lists are never `null`.

use serde::{Deserialize, Serialize};

use crate::eval::EvalOutcome;
use crate::fleet::{
    Lint, Machine, MachineClass, ResolveError, ResolvedTarget, SharedIp, TargetSource,
    lint_hostname, shared_ip_lints,
};
use crate::observe::{Health, ProbeOutcome};
use crate::status::{Badge, MachineFacts, machine_status};

/// The version of this contract. Bumped on any change that is not a pure addition.
pub const JSON_VERSION: u32 = 1;

/// The whole of `auspex --json`'s output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetReport {
    /// [`JSON_VERSION`]. Check it before reading anything else.
    pub version: u32,
    /// `true` when no machine carries a spinner badge (`checking` or `evaluating`) among its
    /// `badges`, not just as its `status`: a machine whose eval failed while its probe is still
    /// out is not settled. `false` when the global timeout passed first.
    pub settled: bool,
    /// One entry per inventory machine, sorted by name.
    pub machines: Vec<MachineReport>,
    /// Addresses more than one machine's target resolves to. Deploying to any of these
    /// machines is refused. Empty when there are none or resolution was not attempted.
    pub shared_ips: Vec<SharedIp>,
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
    /// the primary status.
    pub badges: Vec<Badge>,
    /// One line on why `status` is what it is: ssh's last stderr line, nix's last error line,
    /// or what is being waited for. `null` for `in_sync`.
    pub reason: Option<String>,
    /// `/run/current-system`'s store path, or `null` when not observed or not installed.
    pub current_system: Option<String>,
    /// The evaluated toplevel's store path, or `null` when not evaluated (yet, or at all).
    pub pending_system: Option<String>,
    /// The deployed `system.configurationRevision`, or `null` when the machine does not set
    /// it or was not observed.
    pub deployed_revision: Option<String>,
    /// `systemctl is-system-running` (`"running"`, `"degraded"`, …), or `null` (darwin, or
    /// not observed).
    pub system_state: Option<String>,
    /// Failed systemd units, or `null` when unknown (darwin, or not observed). `[]` means
    /// systemd reported none.
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
/// target cannot disagree.
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
        name: machine.name.clone(),
        class: machine.class,
        tags: machine.tags.clone(),
        target: target.as_ref().ok().map(|t| t.target.to_string()),
        target_source: match target {
            Ok(t) => t.source,
            Err(e) => e.target_source,
        },
        target_error,
        build_host: machine.build_host.clone(),
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

/// Assembles the report, sorting machines by name and computing `settled`.
#[must_use]
pub fn fleet_report(mut machines: Vec<MachineReport>, shared_ips: Vec<SharedIp>) -> FleetReport {
    machines.sort_by(|a, b| a.name.cmp(&b.name));
    FleetReport {
        version: JSON_VERSION,
        settled: machines
            .iter()
            .all(|m| m.badges.iter().all(|b| b.is_settled())),
        machines,
        shared_ips,
    }
}
