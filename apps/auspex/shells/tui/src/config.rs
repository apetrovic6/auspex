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
use std::path::{Path, PathBuf};

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
    /// `[machines.<name>]`, by machine name.
    pub machines: BTreeMap<String, MachineSettings>,
    /// `[keymap]`: action name to its keys, exactly as written (parsed by `keymap`). An
    /// empty list unbinds the action.
    pub keymap: BTreeMap<String, Vec<String>>,
    /// `[theme] file`, with a leading `~/` expanded, or `None` for the terminal's ANSI 16.
    pub theme_file: Option<PathBuf>,
}

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
}
