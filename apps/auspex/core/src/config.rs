//! What the core needs to know before it can scan: carried by [`Event::Start`](crate::Event::Start).
//!
//! The shell builds this from clap and `config.toml`; reading either is not the core's
//! business, so there is no parsing here.

use std::collections::{BTreeMap, BTreeSet};

/// The scan's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// The fleet flake (`--flake`, default `.`, env `CLAN_DIR`).
    pub flake: String,
    /// Per-machine target overrides from `config.toml`, by machine name. They win over the
    /// inventory's `deploy.targetHost` ([`auspex_domain::fleet::resolve_target`]). An
    /// override for a machine the inventory does not list is ignored.
    pub target_overrides: BTreeMap<String, String>,
    /// Prometheus, or `None` when it is not configured (then it is never asked, and every
    /// health value comes from ssh).
    pub prometheus: Option<PrometheusConfig>,
    /// Whether a binary cache (ncps) is configured. Without one the "already cached" question
    /// is never asked.
    pub binary_cache: bool,
    /// The package preview, or `None` for none (the default: a core that is not told to
    /// preview never asks for a closure).
    pub diff: Option<DiffConfig>,
    /// Whether this run may ever enter act mode. Only an interactive shell sets it: the
    /// terminal UI, where an operator can answer a confirmation. Headless runs (`--plain`,
    /// `--json`) leave it `false`, so "act mode is never on in headless mode" holds because
    /// the core cannot be put there, not because nothing happens to ask.
    pub allow_act: bool,
    /// This machine's nix system (`x86_64-linux`). A build is refused for a machine whose
    /// evaluated system is another one: auspex never builds another system's derivations. Empty
    /// means unknown, which refuses every build.
    pub local_system: String,
    /// Machines marked public (`machines.<m>.public = true`): updating one asks the operator
    /// to type its name instead of `y`. A name the inventory does not list is ignored.
    pub public_machines: BTreeSet<String>,
    /// How long the checks before an update may take (`timeouts.diff`: they size the same
    /// closure the package preview does), enforced by the shell. `0` is treated as one second
    /// by the shell, never as "no limit".
    pub preflight_timeout_seconds: u64,
}

/// Prometheus settings the core decides with. The URL is the adapter's, not the core's.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrometheusConfig {
    /// Machine name to its `instance` label. A machine without an entry is looked up under
    /// its own name.
    pub instances: BTreeMap<String, String>,
    /// How old a sample may be, in seconds, before the machine falls back to ssh: about two
    /// scrape intervals ([`auspex_domain::observe::merge_health`]).
    pub max_age_seconds: u64,
}

impl PrometheusConfig {
    /// The `instance` label for `machine`.
    #[must_use]
    pub fn instance_for<'a>(&'a self, machine: &'a str) -> &'a str {
        self.instances.get(machine).map_or(machine, String::as_str)
    }
}

/// The package preview's settings. `None` in [`Config::diff`] means the preview is off: no
/// closure is ever queried and `package_diff` stays `null`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffConfig {
    /// `timeouts.diff`: how long one machine's preview may take, from the moment its queries
    /// are requested. Carried on each request, enforced by the shell.
    pub timeout_seconds: u64,
}
