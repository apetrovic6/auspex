//! What the core needs to know before it can scan: carried by [`Event::Start`](crate::Event::Start).
//!
//! The shell builds this from clap and `config.toml`; reading either is not the core's
//! business, so there is no parsing here.

use std::collections::BTreeMap;

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
