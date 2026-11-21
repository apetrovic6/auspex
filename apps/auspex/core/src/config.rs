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
    /// The CI badge, or `None` when `[forge]` is not configured (then nothing is read or asked:
    /// the badge is `off`).
    pub forge: Option<ForgeConfig>,
    /// The CI runners view, or `None` when `[runners]` has no forge (then nothing is asked: the
    /// view is empty and the status bar says nothing of runners).
    ///
    /// Boxed: `Config` is carried by `Event::Start`, and the setup would make it (and so every
    /// event) larger than clippy accepts.
    pub runners: Option<Box<RunnersSetup>>,
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
    /// Which machines are frozen: declared in `config.toml` and saved by `f` (see [`FreezeConfig`]).
    pub freeze: FreezeConfig,
    /// How `U` rolls: which tags update one at a time, and how many other machines at once.
    pub rolling: RollingConfig,
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

/// The CI badge's settings the core decides with.
///
/// The URL and the token are the adapter's, as Prometheus's URL is: the core needs the host only,
/// to tell whether the `origin` remote is on the forge, and the repository when the operator
/// names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeConfig {
    /// The host of `forge.url`, which the `origin` remote's host must equal for the repository to
    /// be read from it ([`auspex_domain::ci::plan`]).
    pub host: String,
    /// `forge.repo` (`owner/name`), or `None` to read it from the `origin` remote.
    pub repo: Option<String>,
}

/// The runners view's settings the core decides with. The forges' URLs and tokens are the
/// adapter's, as the badge's are; the core needs a name to show and the rules for joining.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnersSetup {
    /// The forge entries, in config order. A request names one by its index.
    pub forges: Vec<RunnerForge>,
    /// The joining rules and thresholds ([`auspex_domain::runners::report`]).
    pub join: auspex_domain::runners::RunnersConfig,
}

/// One `[[runners.forge]]` entry, as the core shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerForge {
    /// Its name on screen (its host; the scope too when two entries share a host).
    pub forge: String,
    /// Its scope as configured (`admin`, `user`, `org:<name>`, `repo:<owner>/<name>`).
    pub scope: String,
}

/// The package preview's settings. `None` in [`Config::diff`] means the preview is off: no
/// closure is ever queried and `package_diff` stays `null`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffConfig {
    /// `timeouts.diff`: how long one machine's preview may take, from the moment its queries
    /// are requested. Carried on each request, enforced by the shell.
    pub timeout_seconds: u64,
}

/// What is frozen when the run starts: the config's declarations and the saved freezes.
///
/// The shell reads both before [`Event::Start`](crate::Event::Start), so a headless `--json` run
/// reports `frozen` too. The default is "nothing frozen, nothing unknown" (a core that is not
/// told anything has no freezes to know about): the shell sets it explicitly in every real run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreezeConfig {
    /// `machines.<m>.frozen = true` in `config.toml`. Never written, and `f` cannot undo it.
    pub declared: BTreeSet<String>,
    /// What `f` saved in an earlier run.
    pub saved: SavedFreezes,
}

/// The freezes made with `f`, as the shell found them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedFreezes {
    /// These machines. An empty set when nothing was saved yet.
    Known(BTreeSet<String>),
    /// They could not be read (a corrupt file, one written by a newer auspex, no usable state
    /// directory); the text says why. **Not** the same as none: the core refuses `u` for every
    /// machine, and `f`, until it is fixed.
    Unknown(String),
}

impl Default for SavedFreezes {
    fn default() -> Self {
        Self::Known(BTreeSet::new())
    }
}

/// How an update of all (`U`) is rolled: `[rolling.<tag>]`, `[update]` and `[kubernetes]` of
/// `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollingConfig {
    /// The rolling tags and what each one asks for. Machines that share any of these tags never
    /// update at the same time ([`auspex_domain::roll::plan`]). Empty: every machine is free.
    pub rules: auspex_domain::roll::RollRules,
    /// How many machines with no rolling tag update at once (`update.parallel`). At least 1.
    pub parallel: usize,
    /// How long to wait for an updated machine to be healthy (and its node `Ready`) before the
    /// roll gives up on it (`update.ready_timeout`), in seconds.
    pub ready_timeout_seconds: u64,
    /// How often that wait looks again (`update.poll_interval`), in seconds.
    pub poll_interval_seconds: u64,
    /// Why the cluster cannot be used (no kubeconfig could be chosen, `KUBECONFIG` names several
    /// files), or `None`. A Kubernetes rolling group is then refused with this reason; machines
    /// outside it are not affected. The shell owns the kubeconfig and `kubectl`: the core only
    /// needs to know whether asking is possible.
    pub kubernetes_unavailable: Option<String>,
}

/// `update.parallel` when the config says nothing.
pub const DEFAULT_PARALLEL: usize = 2;

/// `update.poll_interval` when the config says nothing: 5 seconds.
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;

/// `update.ready_timeout` when the config says nothing: ten minutes.
pub const DEFAULT_READY_TIMEOUT_SECONDS: u64 = 600;

impl Default for RollingConfig {
    fn default() -> Self {
        Self {
            rules: auspex_domain::roll::RollRules::new(),
            parallel: DEFAULT_PARALLEL,
            ready_timeout_seconds: DEFAULT_READY_TIMEOUT_SECONDS,
            poll_interval_seconds: DEFAULT_POLL_INTERVAL_SECONDS,
            kubernetes_unavailable: None,
        }
    }
}
