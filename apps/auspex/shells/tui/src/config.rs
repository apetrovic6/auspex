//! `config.toml`: where it is, what it may say, and what each value becomes.
//!
//! Every key is optional. A key this binary does not know is **ignored with a warning**, so a
//! config written for a newer auspex (or by a newer home-manager module) never stops an older
//! binary. A known key with the wrong type, or a value out of range, is an error naming the
//! key: guessing what was meant would scan with settings nobody chose.
//!
//! The file is only ever read. A home-manager config is a read-only symlink into the store,
//! and auspex has nothing to write there anyway.
//!
//! `nix/auspex/expected.toml` is the contract fixture the home-manager module is tested
//! against; `tests/config.rs` loads exactly that file and checks every value arrives.

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use auspex_domain::roll::{RollRule, RollRules};
use auspex_domain::runners::{DEFAULT_MEMORY_WARN_PERCENT, HostCaps, Scope, parse_size};
use toml::{Table, Value};

/// Everything `config.toml` can say, with the defaults filled in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// `flake`: the fleet flake, `~/` already expanded. `CLAN_DIR` and `--flake` override it.
    pub flake: Option<String>,
    /// `[timeouts]`.
    pub timeouts: Timeouts,
    /// `[ssh]`.
    pub ssh: SshSettings,
    /// `[eval]`.
    pub eval: EvalSettings,
    /// `[diff]`.
    pub diff: DiffSettings,
    /// `[prometheus]`, or `None` when there is no `url` (then health comes from ssh).
    pub prometheus: Option<PrometheusSettings>,
    /// `[cache] url`: the binary cache for the "cached" badge, or `None` for no badge.
    pub cache_url: Option<String>,
    /// `[forge]`, or `None` when there is no `url` (then the CI badge is off and nothing is read
    /// or asked).
    pub forge: Option<ForgeSettings>,
    /// `[runners]`, or `None` when it has no `[[runners.forge]]` (then the runners view is off and
    /// nothing is read, asked or listed on any machine).
    pub runners: Option<RunnersSettings>,
    /// `[machines.<name>]`, by machine name.
    pub machines: BTreeMap<String, MachineSettings>,
    /// `[keymap]`: action name to its keys, exactly as written (parsed by `keymap`). An
    /// empty list unbinds the action.
    pub keymap: BTreeMap<String, Vec<String>>,
    /// `[theme] file`, with a leading `~/` expanded, or `None` for the terminal's ANSI 16.
    pub theme_file: Option<PathBuf>,
    /// `[rolling.<tag>]`: machines carrying one of these tags update one at a time under `U`.
    pub rolling: RollRules,
    /// `[update]`.
    pub update: UpdateSettings,
    /// `kubernetes.kubeconfig`, `~/` already expanded: the kubeconfig `U` reads node state and
    /// drains with. `None`: `$KUBECONFIG`, then `~/.kube/config` ([`resolve_kubeconfig`]).
    pub kubeconfig: Option<String>,
}

/// `[update]`: how `U` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateSettings {
    /// `update.parallel`: how many machines with no rolling tag update at once.
    pub parallel: u64,
    /// `update.ready_timeout`: seconds to wait for an updated machine to be healthy (and its node
    /// Ready).
    pub ready_timeout: u64,
    /// `update.poll_interval`: seconds between two looks at an updating machine and its node.
    pub poll_interval: u64,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            parallel: auspex_core::config::DEFAULT_PARALLEL as u64,
            ready_timeout: auspex_core::config::DEFAULT_READY_TIMEOUT_SECONDS,
            poll_interval: auspex_core::config::DEFAULT_POLL_INTERVAL_SECONDS,
        }
    }
}

/// The longest `update.poll_interval` may be, in seconds.
pub const MAX_POLL_INTERVAL: u64 = 60;

/// The most machines `update.parallel` may name: each is a local build and a copy.
pub const MAX_PARALLEL: u64 = 16;

/// `[diff]`: the package preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffSettings {
    /// `diff.enabled`: whether a machine whose update is pending gets a package preview (its
    /// two closures queried and compared). Off, no closure is ever queried and `package_diff`
    /// stays `null`.
    pub enabled: bool,
}

impl Default for DiffSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// SSH connection reuse settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SshSettings {
    /// Idle master lifetime in seconds.
    pub control_persist: u64,
}

impl Default for SshSettings {
    fn default() -> Self {
        Self {
            control_persist: 600,
        }
    }
}

/// `[timeouts]`, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// ssh's `ConnectTimeout`.
    pub ssh_connect: u64,
    /// Connect/authentication allowance, separate from the script deadline.
    pub ssh_approval: u64,
    /// The whole probe of one machine; past it the row is unreachable.
    pub probe: u64,
    /// One evaluation run (and the machine-list eval, which is the same kind of work).
    pub eval: u64,
    /// The global deadline for one refresh: past it everything pending is timed out.
    pub settle: u64,
    /// One machine's package preview, from the moment its two closure queries are requested.
    /// Past it the preview is `failed`; the scan is not. Not bounded by `settle`: the queries
    /// start late in a refresh, so the fleet settles at most this long after the last one.
    pub diff: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            ssh_connect: 5,
            ssh_approval: 60,
            probe: 10,
            eval: 300,
            settle: 120,
            diff: 120,
        }
    }
}

/// `[eval]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvalSettings {
    /// `nix-eval-jobs --workers`.
    pub workers: u32,
    /// `nix-eval-jobs --max-memory-size`, MiB per worker.
    pub max_memory_mb: u32,
}

impl Default for EvalSettings {
    fn default() -> Self {
        Self {
            workers: 4,
            max_memory_mb: 4096,
        }
    }
}

/// `[prometheus]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrometheusSettings {
    /// The server's base URL.
    pub url: String,
    /// Seconds; an older sample falls back to ssh.
    pub stale_after: u64,
}

/// `[forge]`: the forge whose combined commit status is the fleet's CI badge.
///
/// Debug-printing it is safe: it holds the *path* of the token, never the token (that is read
/// at start by the shell and kept in a type that does not print).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeSettings {
    /// `forge.url`, normalised (`https` only, no credentials).
    pub url: String,
    /// The host of `url`, which the `origin` remote must be on for the repository to be read
    /// from it.
    pub host: String,
    /// `forge.repo` (`owner/name`), or `None` to read it from the fleet's `origin` remote.
    pub repo: Option<String>,
    /// `forge.token_file`, `~/` expanded and absolute, or `None` to ask anonymously.
    pub token_file: Option<PathBuf>,
}

/// `[runners]`: the CI runners view.
///
/// Debug-printing it is safe: it holds the *paths* of the tokens, never a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnersSettings {
    /// `runners.unit_glob`: which systemd units on a machine are runner units.
    pub unit_glob: String,
    /// `runners.memory_warn_percent`: from this share of a host's memory cap a row is badged.
    pub memory_warn_percent: u8,
    /// `[[runners.forge]]`, in file order.
    pub forges: Vec<RunnerForgeSettings>,
    /// `[runners.hosts.<machine>]`: caps for the Prometheus figures. No host is assumed.
    pub hosts: BTreeMap<String, HostCaps>,
    /// `[runners.machines]`: runner name to machine name.
    pub machines: BTreeMap<String, String>,
}

/// One `[[runners.forge]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerForgeSettings {
    /// `url`, normalised (`https` only, no credentials).
    pub url: String,
    /// What the entry is called on screen: the host, port and path, then `(scope)` and `#n` only as
    /// far as two entries would otherwise share a name.
    pub name: String,
    /// Which forge it reads: the host, port and path alone (what [`Self::name`] is before it is told
    /// apart). Two entries with the same site are two scopes of one forge.
    pub site: String,
    /// `scope`, `admin` by default.
    pub scope: Scope,
    /// `token_file`, `~/` expanded and absolute, or `None` to ask anonymously.
    pub token_file: Option<PathBuf>,
}

/// `[machines.<name>]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineSettings {
    /// Wins over the inventory's `deploy.targetHost`.
    pub target: Option<String>,
    /// The Prometheus `instance` label; default `<name>:9100`.
    pub prometheus_instance: Option<String>,
    /// `public`: the machine faces the internet, so updating it asks for its name to be typed
    /// instead of `y` (default `false`).
    pub public: bool,
    /// `frozen = true`: the machine is frozen, declaratively. `u` refuses it and `U` skips it, and
    /// `f` cannot undo it (the file is read-only under home-manager, and a key that edited a
    /// declaration would be undone by the next switch). Default `false`.
    pub frozen: bool,
}

/// The longest timeout accepted, in seconds: a day.
pub const MAX_TIMEOUT: u64 = 86_400;

/// The default `stale_after`: about two scrape intervals at Prometheus's default 30 s.
pub const DEFAULT_STALE_AFTER: u64 = 60;

/// A config that cannot be used. Always names the file, and the key when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// A loaded config and what was odd about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The settings (defaults where the file said nothing, or there was no file).
    pub settings: Settings,
    /// One line per ignored key, for the log.
    pub warnings: Vec<String>,
    /// The file that was read, or `None` when the default file does not exist.
    pub path: Option<PathBuf>,
}

/// Where the config file is when `--config` does not say.
///
/// `$XDG_CONFIG_HOME/auspex/config.toml`, or `~/.config/auspex/config.toml` when
/// `XDG_CONFIG_HOME` is unset, empty or relative (the XDG spec says to ignore a relative
/// one). `None` when neither variable gives an absolute directory.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    let absolute = |v: Option<std::ffi::OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    absolute(std::env::var_os("XDG_CONFIG_HOME"))
        .or_else(|| absolute(std::env::var_os("HOME")).map(|h| h.join(".config")))
        .map(|d| d.join("auspex").join("config.toml"))
}

/// Loads the config: `explicit` (`--config`) if given, which must exist; otherwise the
/// default path, which may be missing.
///
/// # Errors
///
/// The explicit file is missing; a file cannot be read; it is not TOML; a known key has the
/// wrong type or an out-of-range value.
pub fn load(explicit: Option<&Path>) -> Result<Loaded, ConfigError> {
    let (path, must_exist) = explicit.map_or_else(
        || (default_path(), false),
        |p| (Some(p.to_path_buf()), true),
    );
    let Some(path) = path else {
        return Ok(Loaded {
            settings: Settings::default(),
            warnings: Vec::new(),
            path: None,
        });
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !must_exist => {
            return Ok(Loaded {
                settings: Settings::default(),
                warnings: Vec::new(),
                path: None,
            });
        }
        Err(e) => {
            return Err(ConfigError(format!(
                "config file {}: cannot read it: {e}",
                path.display()
            )));
        }
    };
    let (settings, warnings) =
        parse(&text).map_err(|e| ConfigError(format!("config file {}: {e}", path.display())))?;
    Ok(Loaded {
        settings,
        warnings: warnings
            .into_iter()
            .map(|w| format!("config file {}: {w}", path.display()))
            .collect(),
        path: Some(path),
    })
}

/// Parses and validates one config file's text. Returns the settings and one warning per
/// ignored key.
///
/// # Errors
///
/// Not TOML, or a known key with the wrong type or an out-of-range value; the message names
/// the key.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per section of the schema, in the schema's order; split, it no longer reads as the schema"
)]
pub fn parse(text: &str) -> Result<(Settings, Vec<String>), String> {
    let root: Table = toml::from_str(text).map_err(|e| format!("not valid TOML: {e}"))?;
    let mut w = Walk {
        warnings: Vec::new(),
    };
    let mut s = Settings::default();
    for (key, value) in &root {
        match key.as_str() {
            "flake" => s.flake = Some(expand_flake(&w.string(value, "flake")?, "flake")?),
            "ssh" => {
                for (k, v) in w.table(value, "ssh")? {
                    let name = format!("ssh.{k}");
                    match k.as_str() {
                        "control_persist" => {
                            s.ssh.control_persist = w.bounded(v, &name, 60, 3600)?;
                        }
                        _ => w.unknown(&name),
                    }
                }
            }
            "timeouts" => {
                let t = w.table(value, "timeouts")?;
                for (k, v) in t {
                    let name = format!("timeouts.{k}");
                    match k.as_str() {
                        "ssh_connect" => s.timeouts.ssh_connect = w.timeout(v, &name)?,
                        "ssh_approval" => s.timeouts.ssh_approval = w.bounded(v, &name, 5, 600)?,
                        "probe" => s.timeouts.probe = w.timeout(v, &name)?,
                        "eval" => s.timeouts.eval = w.timeout(v, &name)?,
                        "settle" => s.timeouts.settle = w.timeout(v, &name)?,
                        "diff" => s.timeouts.diff = w.timeout(v, &name)?,
                        _ => w.unknown(&name),
                    }
                }
                // ssh refuses a ConnectTimeout outside what the domain accepts; say so here,
                // naming the key, rather than on every probe.
                auspex_domain::fleet::ConnectTimeout::new(s.timeouts.ssh_connect)
                    .map_err(|e| format!("`timeouts.ssh_connect`: {e}"))?;
                // One warning per cause: a `settle` shorter than `probe` is also shorter than
                // approval plus probe, so the second says nothing the first has not.
                if s.timeouts.settle < s.timeouts.probe {
                    w.warnings.push(format!(
                        "`timeouts.settle` ({} s) is shorter than `timeouts.probe` ({} s), so a slow \
                         host is cut off by the global deadline before its own probe timeout",
                        s.timeouts.settle, s.timeouts.probe
                    ));
                } else if s.timeouts.settle < s.timeouts.ssh_approval + s.timeouts.probe {
                    // Approvals are one at a time, so with several cold hosts the later ones
                    // wait for the earlier ones as well; the sum here is the least one host
                    // may need, not what a fleet needs.
                    w.warnings.push(format!(
                        "`timeouts.settle` ({} s) is shorter than `timeouts.ssh_approval` ({} s) \
                         plus `timeouts.probe` ({} s), so the global deadline can cut off \
                         connection/authentication or the probe (ssh agent approvals are one \
                         at a time, so with several cold hosts the later ones wait for the earlier)",
                        s.timeouts.settle, s.timeouts.ssh_approval, s.timeouts.probe
                    ));
                }
            }
            "eval" => {
                let t = w.table(value, "eval")?;
                for (k, v) in t {
                    let name = format!("eval.{k}");
                    match k.as_str() {
                        "workers" => s.eval.workers = w.positive(v, &name)?,
                        "max_memory_mb" => s.eval.max_memory_mb = w.positive(v, &name)?,
                        _ => w.unknown(&name),
                    }
                }
            }
            "diff" => {
                for (k, v) in w.table(value, "diff")? {
                    let name = format!("diff.{k}");
                    match k.as_str() {
                        "enabled" => s.diff.enabled = w.boolean(v, &name)?,
                        _ => w.unknown(&name),
                    }
                }
            }
            "prometheus" => {
                let t = w.table(value, "prometheus")?;
                let mut url = None;
                let mut stale_after = DEFAULT_STALE_AFTER;
                for (k, v) in t {
                    let name = format!("prometheus.{k}");
                    match k.as_str() {
                        "url" => url = Some(w.string(v, &name)?),
                        "stale_after" => stale_after = w.seconds(v, &name)?,
                        _ => w.unknown(&name),
                    }
                }
                if url.is_none() && !t.is_empty() {
                    w.warnings.push(
                        "`[prometheus]` has settings but no `prometheus.url`, so Prometheus is \
                         off and they are ignored"
                            .to_owned(),
                    );
                }
                s.prometheus = url.map(|url| PrometheusSettings { url, stale_after });
            }
            "cache" => {
                let t = w.table(value, "cache")?;
                for (k, v) in t {
                    let name = format!("cache.{k}");
                    match k.as_str() {
                        "url" => s.cache_url = Some(w.string(v, &name)?),
                        _ => w.unknown(&name),
                    }
                }
            }
            "forge" => {
                let t = w.table(value, "forge")?;
                let (mut url, mut repo, mut token_file) = (None, None, None);
                for (k, v) in t {
                    let name = format!("forge.{k}");
                    match k.as_str() {
                        "url" => url = Some(w.string(v, &name)?),
                        "repo" => repo = Some(w.string(v, &name)?),
                        "token_file" => token_file = Some(token_path(&w, v, &name)?),
                        _ => w.unknown(&name),
                    }
                }
                match url {
                    None if !t.is_empty() => w.warnings.push(
                        "`[forge]` has settings but no `forge.url`, so the CI badge is off and \
                         they are ignored"
                            .to_owned(),
                    ),
                    None => {}
                    Some(url) => {
                        // Neither error quotes the URL: it could carry a password.
                        let url = auspex_effects::forge::forge_url(&url)
                            .map_err(|e| format!("`forge.url`: {e}"))?;
                        let repo = repo
                            .map(|r| auspex_domain::ci::parse_repo(&r))
                            .transpose()
                            .map_err(|e| format!("`forge.repo` {e}"))?;
                        s.forge = Some(ForgeSettings {
                            // Without the brackets an IPv6 host is written in: the core compares
                            // it with the host of the `origin` remote, which has none.
                            host: url
                                .host_str()
                                .unwrap_or_default()
                                .trim_start_matches('[')
                                .trim_end_matches(']')
                                .to_owned(),
                            url: url.to_string(),
                            repo,
                            token_file,
                        });
                    }
                }
            }
            "runners" => s.runners = parse_runners(&mut w, value)?,
            "machines" => {
                let t = w.table(value, "machines")?;
                for (machine, v) in t {
                    let prefix = format!("machines.{machine}");
                    let mt = w.table(v, &prefix)?;
                    let mut m = MachineSettings::default();
                    for (k, v) in mt {
                        let name = format!("{prefix}.{k}");
                        match k.as_str() {
                            "target" => m.target = Some(w.string(v, &name)?),
                            "prometheus_instance" => {
                                m.prometheus_instance = Some(w.string(v, &name)?);
                            }
                            "public" => m.public = w.boolean(v, &name)?,
                            "frozen" => m.frozen = w.boolean(v, &name)?,
                            _ => w.unknown(&name),
                        }
                    }
                    s.machines.insert(machine.clone(), m);
                }
            }
            "rolling" => {
                for (tag, v) in w.table(value, "rolling")? {
                    let prefix = format!("rolling.{tag}");
                    let mut rule = RollRule::default();
                    for (k, v) in w.table(v, &prefix)? {
                        let name = format!("{prefix}.{k}");
                        match k.as_str() {
                            "kubernetes" => rule.kubernetes = w.boolean(v, &name)?,
                            "drain" => rule.drain = w.boolean(v, &name)?,
                            "etcd" => rule.etcd = w.boolean(v, &name)?,
                            "drain_timeout" => rule.drain_timeout_seconds = w.timeout(v, &name)?,
                            _ => w.unknown(&name),
                        }
                    }
                    s.rolling.insert(tag.clone(), rule);
                }
            }
            "update" => {
                let mut poll = None;
                for (k, v) in w.table(value, "update")? {
                    let name = format!("update.{k}");
                    match k.as_str() {
                        "parallel" => s.update.parallel = w.counted(v, &name, MAX_PARALLEL)?,
                        "ready_timeout" => s.update.ready_timeout = w.timeout(v, &name)?,
                        "poll_interval" => {
                            poll = Some(w.counted(v, &name, MAX_POLL_INTERVAL)?);
                        }
                        _ => w.unknown(&name),
                    }
                }
                // Left unset it is the default, kept within a short `ready_timeout` so that a
                // config that only shortens the timeout still loads; only a value the user wrote
                // can contradict it.
                s.update.poll_interval = match poll {
                    Some(p) if p > s.update.ready_timeout => {
                        return Err(format!(
                            "`update.poll_interval` ({p}) must not be more than `update.ready_timeout` ({})",
                            s.update.ready_timeout
                        ));
                    }
                    Some(p) => p,
                    None => auspex_core::config::DEFAULT_POLL_INTERVAL_SECONDS
                        .min(s.update.ready_timeout),
                };
            }
            "kubernetes" => {
                for (k, v) in w.table(value, "kubernetes")? {
                    let name = format!("kubernetes.{k}");
                    match k.as_str() {
                        "kubeconfig" => {
                            let path = expand_flake(&w.string(v, &name)?, &name)?;
                            // `kubectl --kubeconfig` is given exactly this: a relative path would
                            // be read against wherever auspex happened to be started.
                            if !Path::new(&path).is_absolute() {
                                return Err(format!(
                                    "`{name}` must be an absolute path (or start with `~/`), not `{path}`"
                                ));
                            }
                            s.kubeconfig = Some(path);
                        }
                        _ => w.unknown(&name),
                    }
                }
            }
            "keymap" => {
                let t = w.table(value, "keymap")?;
                for (action, v) in t {
                    let name = format!("keymap.{action}");
                    let keys = match v {
                        Value::String(k) => vec![k.clone()],
                        Value::Array(items) => items
                            .iter()
                            .map(|i| w.string(i, &name))
                            .collect::<Result<_, _>>()?,
                        other => return Err(wrong(&name, "a key or a list of keys", other)),
                    };
                    // Unknown action names are the keymap's to judge (and warn about).
                    s.keymap.insert(action.clone(), keys);
                }
            }
            "theme" => {
                let t = w.table(value, "theme")?;
                for (k, v) in t {
                    let name = format!("theme.{k}");
                    match k.as_str() {
                        "file" => s.theme_file = Some(expand_home(&w.string(v, &name)?)),
                        _ => w.unknown(&name),
                    }
                }
            }
            _ => w.unknown(key),
        }
    }
    Ok((s, w.warnings))
}

/// The kubeconfig `U` uses.
///
/// `kubernetes.kubeconfig` (`configured`), else `$KUBECONFIG`, else `~/.kube/config`. Only a path is chosen here: nothing is read, and nothing says the file exists
/// (`kubectl` will say so, at the first use).
///
/// `$KUBECONFIG` may name several files, which `kubectl --kubeconfig` cannot take, and a relative
/// one would be read against the directory auspex was started in: both are refused, saying how
/// to fix it, rather than guessed at.
///
/// # Errors
///
/// Why no kubeconfig can be chosen.
pub fn resolve_kubeconfig(configured: Option<&str>) -> Result<PathBuf, String> {
    resolve_kubeconfig_in(
        configured,
        std::env::var_os("KUBECONFIG").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn resolve_kubeconfig_in(
    configured: Option<&str>,
    env: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, String> {
    if let Some(path) = configured {
        return Ok(PathBuf::from(path));
    }
    if let Some(env) = env.filter(|e| !e.is_empty()) {
        let path = PathBuf::from(env);
        if env.to_string_lossy().contains(':') {
            return Err(
                "`$KUBECONFIG` names several files, which `kubectl --kubeconfig` cannot take: set \
                 `kubernetes.kubeconfig`"
                    .to_owned(),
            );
        }
        if !path.is_absolute() {
            return Err(format!(
                "`$KUBECONFIG` is `{}`, not an absolute path: set `kubernetes.kubeconfig`",
                path.display()
            ));
        }
        return Ok(path);
    }
    home.map(Path::new)
        .filter(|h| h.is_absolute())
        .map(|h| h.join(".kube").join("config"))
        .ok_or_else(|| {
            "no kubeconfig: `kubernetes.kubeconfig` is not set, `$KUBECONFIG` is empty and `HOME` is \
             not an absolute path"
                .to_owned()
        })
}

/// A leading `~/` becomes `$HOME/`; anything else is taken as written. The home-manager
/// module writes `~/.config/auspex/theme.toml` by default.
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) if Path::new(&home).is_absolute() => Path::new(&home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// A fleet flake from the config file or `CLAN_DIR`: a leading `~/` becomes `$HOME/`.
/// Only that form: `~user/…` and a bare `~` are refused, and so is `~/` with no absolute
/// `HOME`, each naming `key`.
fn expand_flake(value: &str, key: &str) -> Result<String, String> {
    expand_flake_in(value, key, std::env::var_os("HOME").as_deref())
}

fn expand_flake_in(
    value: &str,
    key: &str,
    home: Option<&std::ffi::OsStr>,
) -> Result<String, String> {
    if let Some(rest) = value.strip_prefix("~/") {
        let home = home
            .map(Path::new)
            .filter(|h| h.is_absolute())
            .ok_or_else(|| format!("`{key}` starts with `~/`, but HOME is not an absolute path"))?;
        return Ok(home.join(rest).to_string_lossy().into_owned());
    }
    if value.starts_with('~') {
        return Err(format!(
            "`{key}` may start with `~/` only; `{value}` is not expanded (use an absolute path)"
        ));
    }
    Ok(value.to_owned())
}

/// A token file path from the config: `~/` expanded, and absolute (it is read at start, from
/// wherever auspex was started: a relative path would be a different file each time).
fn token_path(w: &Walk, v: &Value, name: &str) -> Result<PathBuf, String> {
    let path = expand_flake(&w.string(v, name)?, name)?;
    if !Path::new(&path).is_absolute() {
        return Err(format!(
            "`{name}` must be an absolute path (or start with `~/`), not `{path}`"
        ));
    }
    Ok(PathBuf::from(path))
}

/// `[runners]`. Every refusal names its key and quotes no URL (it could carry a password).
fn parse_runners(w: &mut Walk, value: &Value) -> Result<Option<RunnersSettings>, String> {
    let t = w.table(value, "runners")?;
    let mut r = RunnersSettings {
        unit_glob: auspex_domain::observe::DEFAULT_RUNNER_GLOB.to_owned(),
        memory_warn_percent: DEFAULT_MEMORY_WARN_PERCENT,
        forges: Vec::new(),
        hosts: BTreeMap::new(),
        machines: BTreeMap::new(),
    };
    for (k, v) in t {
        let name = format!("runners.{k}");
        match k.as_str() {
            "unit_glob" => {
                r.unit_glob = w.string(v, &name)?;
                auspex_domain::observe::valid_unit_glob(&r.unit_glob)
                    .map_err(|e| format!("`{name}` {e}"))?;
            }
            "memory_warn_percent" => {
                r.memory_warn_percent = u8::try_from(w.counted(v, &name, 100)?).unwrap_or(100);
            }
            "forge" => {
                let list = v
                    .as_array()
                    .ok_or_else(|| wrong(&name, "a list of tables ([[runners.forge]])", v))?;
                for (i, entry) in list.iter().enumerate() {
                    let at = format!("runners.forge[{i}]");
                    let t = w.table(entry, &at)?;
                    let (mut url, mut scope, mut token_file) = (None, Scope::Admin, None);
                    for (k, v) in t {
                        let key = format!("{at}.{k}");
                        match k.as_str() {
                            "url" => url = Some(w.string(v, &key)?),
                            "scope" => {
                                scope = Scope::parse(&w.string(v, &key)?)
                                    .map_err(|e| format!("`{key}` {e}"))?;
                            }
                            "token_file" => token_file = Some(token_path(w, v, &key)?),
                            _ => w.unknown(&key),
                        }
                    }
                    let url = url.ok_or_else(|| format!("`{at}.url` is required"))?;
                    let url = auspex_effects::forge::forge_url(&url)
                        .map_err(|e| format!("`{at}.url`: {e}"))?;
                    let site =
                        forge_label(url.host_str().unwrap_or_default(), url.port(), url.path());
                    r.forges.push(RunnerForgeSettings {
                        name: site.clone(),
                        site,
                        url: url.to_string(),
                        scope,
                        token_file,
                    });
                }
            }
            "hosts" => {
                for (machine, v) in w.table(v, &name)? {
                    let at = format!("{name}.{machine}");
                    let mut caps = HostCaps::default();
                    for (k, v) in w.table(v, &at)? {
                        let key = format!("{at}.{k}");
                        match k.as_str() {
                            "cpus" => caps.cpus = Some(w.positive(v, &key)?),
                            "memory" => {
                                caps.memory_bytes = Some(
                                    parse_size(&w.string(v, &key)?)
                                        .map_err(|e| format!("`{key}` {e}"))?,
                                );
                            }
                            _ => w.unknown(&key),
                        }
                    }
                    r.hosts.insert(machine.clone(), caps);
                }
            }
            "machines" => {
                for (runner, v) in w.table(v, &name)? {
                    r.machines
                        .insert(runner.clone(), w.string(v, &format!("{name}.{runner}"))?);
                }
            }
            _ => w.unknown(&name),
        }
    }
    if r.forges.is_empty() {
        if !t.is_empty() {
            w.warnings.push(
                "`[runners]` has settings but no `[[runners.forge]]`, so the runners view is off \
                 and they are ignored"
                    .to_owned(),
            );
        }
        return Ok(None);
    }
    tell_apart(&mut r.forges);
    Ok(Some(r))
}

/// Gives every entry a name of its own. Two entries with the same name (the same forge by two
/// scopes, say) are told apart by their scope, and two that still share one (the same forge and
/// scope with two tokens) by position: a name two entries share would put each one's runners under
/// both headings, and `--json` could not say whose a row is.
fn tell_apart(forges: &mut [RunnerForgeSettings]) {
    let shared = |forges: &[RunnerForgeSettings], f: &RunnerForgeSettings| {
        forges.iter().filter(|o| o.name == f.name).count() > 1
    };
    let by_scope: Vec<bool> = forges.iter().map(|f| shared(forges, f)).collect();
    for (f, clash) in forges.iter_mut().zip(by_scope) {
        if clash {
            f.name = format!("{} ({})", f.name, f.scope);
        }
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for f in forges {
        let nth = seen.entry(f.name.clone()).or_insert(0);
        *nth += 1;
        if *nth > 1 {
            f.name = format!("{} #{nth}", f.name);
        }
    }
}

/// What a forge entry is called on screen: its host, with the port when it is not the scheme's
/// own and the path when it is not the root, so two forges on one host are two names.
fn forge_label(host: &str, port: Option<u16>, path: &str) -> String {
    let mut name = host.to_owned();
    if let Some(port) = port {
        let _ = write!(name, ":{port}");
    }
    name.push_str(path.trim_end_matches('/'));
    name
}

const fn type_name(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "a string",
        Value::Integer(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Boolean(_) => "a boolean",
        Value::Datetime(_) => "a date",
        Value::Array(_) => "a list",
        Value::Table(_) => "a table",
    }
}

fn wrong(key: &str, want: &str, got: &Value) -> String {
    format!("`{key}` must be {want}, not {}", type_name(got))
}

struct Walk {
    warnings: Vec<String>,
}

impl Walk {
    fn unknown(&mut self, key: &str) {
        self.warnings.push(format!(
            "unknown key `{key}` ignored (written for a newer auspex?)"
        ));
    }

    #[expect(
        clippy::unused_self,
        reason = "kept beside `unknown` so every key reads alike"
    )]
    fn table<'v>(&self, v: &'v Value, key: &str) -> Result<&'v Table, String> {
        v.as_table().ok_or_else(|| wrong(key, "a table", v))
    }

    #[expect(
        clippy::unused_self,
        reason = "kept beside `unknown` so every key reads alike"
    )]
    fn string(&self, v: &Value, key: &str) -> Result<String, String> {
        v.as_str()
            .map(str::to_owned)
            .ok_or_else(|| wrong(key, "a string", v))
    }

    #[expect(
        clippy::unused_self,
        reason = "kept beside `unknown` so every key reads alike"
    )]
    fn boolean(&self, v: &Value, key: &str) -> Result<bool, String> {
        v.as_bool().ok_or_else(|| wrong(key, "true or false", v))
    }

    fn bounded(&self, v: &Value, key: &str, min: u64, max: u64) -> Result<u64, String> {
        let seconds = self.seconds(v, key)?;
        if !(min..=max).contains(&seconds) {
            return Err(format!("`{key}` must be between {min} and {max} seconds"));
        }
        Ok(seconds)
    }

    /// A timeout: whole seconds, at least 1 (0 would fail or settle everything at once) and
    /// at most a day. The cap keeps `Instant::now() + timeout` in the adapters far from
    /// overflowing (review finding 2), and no scan step is meant to take longer.
    #[expect(
        clippy::unused_self,
        reason = "kept beside `unknown` so every key reads alike"
    )]
    fn timeout(&self, v: &Value, key: &str) -> Result<u64, String> {
        match v {
            Value::Integer(i) => u64::try_from(*i)
                .ok()
                .filter(|s| (1..=MAX_TIMEOUT).contains(s))
                .ok_or_else(|| {
                    format!("`{key}` must be between 1 and {MAX_TIMEOUT} seconds (a day), not {i}")
                }),
            other => Err(wrong(key, "a whole number of seconds", other)),
        }
    }

    #[expect(
        clippy::unused_self,
        reason = "kept beside `unknown` so every key reads alike"
    )]
    fn seconds(&self, v: &Value, key: &str) -> Result<u64, String> {
        match v {
            Value::Integer(i) => u64::try_from(*i).map_err(|_| {
                format!("`{key}` must be a whole number of seconds, 0 or more, not {i}")
            }),
            other => Err(wrong(key, "a whole number of seconds", other)),
        }
    }

    /// A whole number from 1 to `max`.
    #[expect(
        clippy::unused_self,
        reason = "kept beside `unknown` so every key reads alike"
    )]
    fn counted(&self, v: &Value, key: &str, max: u64) -> Result<u64, String> {
        match v {
            Value::Integer(i) => u64::try_from(*i)
                .ok()
                .filter(|n| (1..=max).contains(n))
                .ok_or_else(|| format!("`{key}` must be a whole number from 1 to {max}, not {i}")),
            other => Err(wrong(key, "a whole number", other)),
        }
    }

    #[expect(
        clippy::unused_self,
        reason = "kept beside `unknown` so every key reads alike"
    )]
    fn positive(&self, v: &Value, key: &str) -> Result<u32, String> {
        match v {
            Value::Integer(i) => u32::try_from(*i)
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("`{key}` must be a whole number, 1 or more, not {i}")),
            other => Err(wrong(key, "a whole number", other)),
        }
    }
}

/// Which source the fleet flake came from, for the detail of a "cannot read the fleet"
/// error. Lowest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlakeSource {
    /// Nothing said otherwise: the current directory.
    Default,
    /// `flake` in the config file.
    ConfigFile,
    /// The `CLAN_DIR` environment variable.
    ClanDir,
    /// `--flake`.
    Flag,
}

impl FlakeSource {
    /// How it is shown.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Default => "the default, the current directory",
            Self::ConfigFile => "`flake` in the config file",
            Self::ClanDir => "the CLAN_DIR environment variable",
            Self::Flag => "--flake",
        }
    }
}

/// The fleet flake and where it came from: `--flake`, then `CLAN_DIR`, then the config
/// file's `flake`, then `.`.
///
/// An empty `CLAN_DIR` counts as unset. `CLAN_DIR` expands a leading `~/` like the config
/// file's `flake` (already expanded by `parse`); `--flake` is left to the shell.
///
/// # Errors
///
/// `CLAN_DIR` starts with `~` but not `~/`, or with `~/` while `HOME` is not absolute.
pub fn resolve_flake(
    flag: Option<&str>,
    clan_dir: Option<&str>,
    config: Option<&str>,
) -> Result<(String, FlakeSource), String> {
    resolve_flake_in(flag, clan_dir, config, std::env::var_os("HOME").as_deref())
}

fn resolve_flake_in(
    flag: Option<&str>,
    clan_dir: Option<&str>,
    config: Option<&str>,
    home: Option<&std::ffi::OsStr>,
) -> Result<(String, FlakeSource), String> {
    if let Some(f) = flag {
        return Ok((f.to_owned(), FlakeSource::Flag));
    }
    if let Some(d) = clan_dir.filter(|d| !d.is_empty()) {
        return Ok((expand_flake_in(d, "CLAN_DIR", home)?, FlakeSource::ClanDir));
    }
    if let Some(c) = config {
        return Ok((c.to_owned(), FlakeSource::ConfigFile));
    }
    Ok((".".to_owned(), FlakeSource::Default))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_defaults() {
        let (s, w) = parse("").unwrap();
        assert_eq!(s, Settings::default());
        assert_eq!(w, Vec::<String>::new());
    }

    #[test]
    fn a_nested_unknown_key_is_named() {
        let (_, w) = parse("[timeouts]\nprobe = 3\nwarp = 9\n").unwrap();
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("`timeouts.warp`"), "{w:?}");
    }

    #[test]
    fn out_of_range_values_name_their_key() {
        assert!(
            parse("[eval]\nworkers = 0\n")
                .unwrap_err()
                .contains("`eval.workers`")
        );
        assert!(
            parse("[timeouts]\nprobe = -1\n")
                .unwrap_err()
                .contains("`timeouts.probe`")
        );
        assert!(
            parse("[timeouts]\nssh_connect = 100000\n")
                .unwrap_err()
                .contains("`timeouts.ssh_connect`")
        );
    }

    fn home(h: &str) -> &std::ffi::OsStr {
        std::ffi::OsStr::new(h)
    }

    #[test]
    fn a_tilde_slash_flake_resolves_against_home() {
        assert_eq!(
            expand_flake_in("~/clan/fleet", "flake", Some(home("/home/me"))).unwrap(),
            "/home/me/clan/fleet"
        );
        assert_eq!(
            expand_flake_in("/abs/fleet", "flake", None).unwrap(),
            "/abs/fleet"
        );
    }

    #[test]
    fn other_tilde_forms_are_refused_naming_the_key() {
        for bad in ["~user/x", "~"] {
            let e = expand_flake_in(bad, "flake", Some(home("/home/me"))).unwrap_err();
            assert!(e.contains("`flake`"), "{e}");
        }
        assert!(
            parse("flake = \"~user/x\"\n")
                .unwrap_err()
                .contains("`flake`")
        );
        assert!(parse("flake = \"~\"\n").unwrap_err().contains("`flake`"));
    }

    #[test]
    fn a_relative_or_missing_home_is_refused() {
        for h in [Some(home("relative/home")), None] {
            let e = expand_flake_in("~/x", "flake", h).unwrap_err();
            assert!(e.contains("`flake`") && e.contains("HOME"), "{e}");
        }
    }

    #[test]
    fn clan_dir_expands_a_tilde_slash() {
        let (flake, source) =
            resolve_flake_in(None, Some("~/clan"), Some("/cfg"), Some(home("/home/me"))).unwrap();
        assert_eq!(
            (flake.as_str(), source),
            ("/home/me/clan", FlakeSource::ClanDir)
        );
        let e = resolve_flake_in(None, Some("~root/x"), None, Some(home("/home/me"))).unwrap_err();
        assert!(e.contains("`CLAN_DIR`"), "{e}");
        // --flake is the shell's to expand.
        let (flake, _) =
            resolve_flake_in(Some("~/x"), Some("~/y"), None, Some(home("/h"))).unwrap();
        assert_eq!(flake, "~/x");
    }

    #[test]
    fn a_prometheus_table_without_a_url_is_no_prometheus() {
        let (s, _) = parse("[prometheus]\nstale_after = 30\n").unwrap();
        assert_eq!(s.prometheus, None);
    }

    // --- `U`: rolling rules, `[update]` and the kubeconfig ---

    #[test]
    fn a_rolling_tag_with_no_keys_is_a_rolling_tag_with_the_defaults() {
        let (s, w) = parse("[rolling.taghmata]\n").unwrap();
        assert_eq!(w, Vec::<String>::new());
        assert_eq!(s.rolling["taghmata"], RollRule::default());
        assert!(!RollRule::default().drain, "drain is opt-in");
        assert!(!RollRule::default().kubernetes);
        assert!(!RollRule::default().etcd, "etcd is opt-in");
        assert_eq!(RollRule::default().drain_timeout_seconds, 600);
    }

    #[test]
    fn etcd_is_read_per_rolling_tag() {
        let (s, w) =
            parse("[rolling.cp]\netcd = true\n[rolling.workers]\nkubernetes = true\n").unwrap();
        assert_eq!(w, Vec::<String>::new());
        assert!(s.rolling["cp"].etcd);
        assert!(!s.rolling["workers"].etcd);
    }

    #[test]
    fn rolling_and_update_keys_name_themselves_when_wrong() {
        for (text, key) in [
            (
                "[rolling.a]\nkubernetes = \"yes\"\n",
                "rolling.a.kubernetes",
            ),
            ("[rolling.a]\ndrain = 1\n", "rolling.a.drain"),
            ("[rolling.a]\netcd = 1\n", "rolling.a.etcd"),
            (
                "[rolling.a]\ndrain_timeout = 0\n",
                "rolling.a.drain_timeout",
            ),
            (
                "[rolling.a]\ndrain_timeout = 86401\n",
                "rolling.a.drain_timeout",
            ),
            ("rolling = 3\n", "rolling"),
            ("[rolling]\na = 3\n", "rolling.a"),
            ("[update]\nparallel = 0\n", "update.parallel"),
            ("[update]\nparallel = 17\n", "update.parallel"),
            ("[update]\nparallel = \"2\"\n", "update.parallel"),
            ("[update]\nready_timeout = 0\n", "update.ready_timeout"),
            ("[update]\npoll_interval = 0\n", "update.poll_interval"),
            ("[update]\npoll_interval = 61\n", "update.poll_interval"),
            ("[update]\npoll_interval = \"5\"\n", "update.poll_interval"),
            (
                "[update]\npoll_interval = 5\nready_timeout = 4\n",
                "update.poll_interval",
            ),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(&format!("`{key}`")), "{text:?}: {e}");
        }
        let (s, _) = parse("[update]\nparallel = 16\nready_timeout = 86400\n").unwrap();
        assert_eq!((s.update.parallel, s.update.ready_timeout), (16, 86_400));
        // Only a shorter ready_timeout: valid before poll_interval existed, still valid, and the
        // default poll is clamped to it rather than refused.
        for (rt, poll) in [(1, 1), (3, 3), (5, 5), (6, 5)] {
            let (s, _) = parse(&format!("[update]\nready_timeout = {rt}\n")).unwrap();
            assert_eq!((s.update.ready_timeout, s.update.poll_interval), (rt, poll));
        }
        let (s, _) = parse("[update]\npoll_interval = 60\n").unwrap();
        assert_eq!(s.update.poll_interval, 60);
        let (s, _) = parse("[update]\npoll_interval = 2\nready_timeout = 2\n").unwrap();
        assert_eq!(s.update.poll_interval, 2);
        let (s, w) = parse("[rolling.a]\nwarp = 1\n[update]\nwarp = 1\n").unwrap();
        assert_eq!(w.len(), 2, "{w:?}");
        assert_eq!(s.update, UpdateSettings::default());
    }

    #[test]
    fn the_kubeconfig_setting_expands_a_tilde_slash_like_flake_and_is_never_relative() {
        let (s, _) = parse("[kubernetes]\nkubeconfig = \"/abs/kube\"\n").unwrap();
        assert_eq!(s.kubeconfig.as_deref(), Some("/abs/kube"));
        for bad in ["~user/kube", "~", "kube/config", "config"] {
            let e = parse(&format!("[kubernetes]\nkubeconfig = \"{bad}\"\n")).unwrap_err();
            assert!(e.contains("`kubernetes.kubeconfig`"), "{bad}: {e}");
        }
        let e = expand_flake_in("~/kube", "kubernetes.kubeconfig", None).unwrap_err();
        assert!(
            e.contains("`kubernetes.kubeconfig`") && e.contains("HOME"),
            "{e}"
        );
        assert_eq!(
            expand_flake_in(
                "~/.kube/fleet",
                "kubernetes.kubeconfig",
                Some(home("/home/me"))
            )
            .unwrap(),
            "/home/me/.kube/fleet"
        );
    }

    #[test]
    fn the_kubeconfig_is_the_setting_then_kubeconfig_then_the_home_default() {
        let os = std::ffi::OsStr::new;
        let me = Some(home("/home/me"));
        assert_eq!(
            resolve_kubeconfig_in(Some("/set/config"), Some(os("/env/config")), me).unwrap(),
            PathBuf::from("/set/config"),
            "the setting wins"
        );
        assert_eq!(
            resolve_kubeconfig_in(None, Some(os("/env/config")), me).unwrap(),
            PathBuf::from("/env/config")
        );
        for empty in [None, Some(os(""))] {
            assert_eq!(
                resolve_kubeconfig_in(None, empty, me).unwrap(),
                PathBuf::from("/home/me/.kube/config")
            );
        }
        // Several files cannot be given to `--kubeconfig`, a relative one means whatever
        // directory auspex was started in, and no home means no default: each is said, naming
        // the way out, and nothing is guessed.
        let several = resolve_kubeconfig_in(None, Some(os("/a:/b")), me).unwrap_err();
        assert!(
            several.contains("several files") && several.contains("kubernetes.kubeconfig"),
            "{several}"
        );
        let relative = resolve_kubeconfig_in(None, Some(os("kube/config")), me).unwrap_err();
        assert!(relative.contains("not an absolute path"), "{relative}");
        for bad_home in [None, Some(home("relative"))] {
            let none = resolve_kubeconfig_in(None, None, bad_home).unwrap_err();
            assert!(none.contains("no kubeconfig"), "{none}");
        }
    }
}
