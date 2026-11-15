//! The ssh probe: the scripts, their parser, and merging in Prometheus.
//!
//! One probe is one ssh round trip: [`crate::fleet::ssh_args`], then
//! [`crate::fleet::PROBE_REMOTE_COMMAND`], with [`probe_script`] on stdin. Its stdout,
//! stderr and exit status go to [`parse_probe`]. Running it is an effect; nothing here does.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::fleet::MachineClass;
use crate::text::{last_lines, sanitize_line};

// ---------------------------------------------------------------------------------------
// The scripts
// ---------------------------------------------------------------------------------------

// Shared rules for both scripts, each one a trap that fails quietly:
//
// - `printf`, never `echo`: dash's `echo` interprets backslashes, and systemd escapes unit
//   names with them (`dev-disk-by\x2duuid`), so `echo` corrupts the failed-units JSON.
// - `kv` folds a value onto one line: a value with a newline would otherwise start a second,
//   bogus key.
// - `link` tests that the path exists before reading the link, then uses plain `readlink`,
//   never `readlink -f`: with `/run/current-system` missing, `readlink -f` still prints a
//   path and exits 0, and a dangling link (an incomplete uninstall, or nix-darwin's `/run`
//   being `/private/var/run`) still has a target to print. `[ -e ]` follows the link, so a
//   link to nothing is "not installed". The parser also refuses anything outside /nix/store.
// - PATH is extended first: a non-interactive ssh session on darwin has only the system
//   PATH, which has no `darwin-version`.
// - The `auspex_probe` and `auspex_end` markers bracket the output, so a login banner before
//   it is skipped and a cut-off run is told apart from a machine that lacks a key.
// - Uptime, disk and load are always collected; whether to use them or Prometheus's is the
//   caller's decision (`merge_health`), and costs nothing extra in the same round trip.
//
// `tests/probe_scripts.rs` runs both scripts under a local `sh` against a fake root and
// shim commands, and feeds what they print to `parse_probe`, so a renamed key or a dropped
// command fails a test rather than showing every machine as "not installed".

/// The NixOS probe: POSIX `sh`, `key=value` lines, no `jq`.
pub const NIXOS_PROBE: &str = r#"PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH
export PATH
kv() { printf '%s=%s\n' "$1" "$(printf '%s' "$2" | tr -d '\r\n')"; }
link() { if [ -e "$1" ]; then readlink "$1" 2>/dev/null; fi; }
printf 'auspex_probe=1\n'
kv probe nixos
kv current_system "$(link /run/current-system)"
kv booted_kernel "$(link /run/booted-system/kernel)"
kv booted_initrd "$(link /run/booted-system/initrd)"
kv current_kernel "$(link /run/current-system/kernel)"
kv current_initrd "$(link /run/current-system/initrd)"
kv configuration_revision "$(nixos-version --configuration-revision 2>/dev/null)"
kv system_state "$(systemctl is-system-running 2>/dev/null)"
kv failed_units "$(systemctl --failed --output=json --no-pager 2>/dev/null)"
kv uptime_seconds "$(cut -d' ' -f1 /proc/uptime 2>/dev/null)"
kv load "$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null)"
kv root_disk_kb "$(df -Pk / 2>/dev/null | awk 'NR==2 {print $2 " " $3 " " $4}')"
printf 'auspex_end=1\n'
"#;

/// The darwin probe. No systemd, no booted-versus-current kernel.
pub const DARWIN_PROBE: &str = r#"PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH
export PATH
kv() { printf '%s=%s\n' "$1" "$(printf '%s' "$2" | tr -d '\r\n')"; }
link() { if [ -e "$1" ]; then readlink "$1" 2>/dev/null; fi; }
printf 'auspex_probe=1\n'
kv probe darwin
kv current_system "$(link /run/current-system)"
kv configuration_revision "$(darwin-version --configuration-revision 2>/dev/null)"
boot=$(sysctl -n kern.boottime 2>/dev/null | sed -n 's/^{ sec = \([0-9]*\),.*/\1/p')
if [ -n "$boot" ]; then kv uptime_seconds "$(( $(date +%s) - boot ))"; fi
kv load "$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}')"
kv root_disk_kb "$(df -Pk / 2>/dev/null | awk 'NR==2 {print $2 " " $3 " " $4}')"
printf 'auspex_end=1\n'
"#;

/// The probe script for a machine class.
#[must_use]
pub const fn probe_script(class: MachineClass) -> &'static str {
    match class {
        MachineClass::Nixos => NIXOS_PROBE,
        MachineClass::Darwin => DARWIN_PROBE,
    }
}

// ---------------------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------------------

/// A path in `/nix/store`. Anything else is refused at construction, which is what makes
/// "a value from `readlink` that is not a store path means not installed" a type-level fact.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StorePath(String);

/// The string was not a `/nix/store/<hash>-<name>` path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a store path: {0:?}")]
pub struct NotAStorePath(pub String);

const STORE: &str = "/nix/store/";
const HASH_LEN: usize = 32;

impl StorePath {
    /// Validates a store path: `/nix/store/`, a 32-character hash, `-`, a name, and no
    /// whitespace or control characters anywhere. After the `<hash>-<name>` part, a path
    /// may go on into the store object (`…-linux-6.12.1/bzImage`), but only by plain
    /// components: an empty, `.` or `..` component is refused, because
    /// `…-system/../../../run/current-system` names a path outside the store.
    ///
    /// # Errors
    ///
    /// [`NotAStorePath`] otherwise.
    pub fn new(s: impl Into<String>) -> Result<Self, NotAStorePath> {
        let s = s.into();
        let ok = s.strip_prefix(STORE).is_some_and(|rest| {
            let mut parts = rest.split('/');
            let base = parts.next().unwrap_or("");
            base.len() > HASH_LEN + 1
                && base.is_char_boundary(HASH_LEN)
                && base[..HASH_LEN].bytes().all(|b| b.is_ascii_alphanumeric())
                && base.as_bytes()[HASH_LEN] == b'-'
                && parts.all(|p| !matches!(p, "" | "." | ".."))
        }) && !s.chars().any(|c| c.is_whitespace() || c.is_control());
        if ok {
            Ok(Self(s))
        } else {
            Err(NotAStorePath(s))
        }
    }

    /// The whole path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The 32-character hash part, which is also the ncps `.narinfo` name.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.0[STORE.len()..STORE.len() + HASH_LEN]
    }

    /// The name after `<hash>-`, up to the first `/` (`nixos-system-terra-25.05…`).
    #[must_use]
    pub fn name(&self) -> &str {
        let rest = &self.0[STORE.len() + HASH_LEN + 1..];
        rest.split('/').next().unwrap_or(rest)
    }
}

impl TryFrom<String> for StorePath {
    type Error = NotAStorePath;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<StorePath> for String {
    fn from(p: StorePath) -> Self {
        p.0
    }
}

impl fmt::Display for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Root filesystem usage, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsage {
    /// Filesystem size.
    pub total_bytes: u64,
    /// Bytes in use.
    pub used_bytes: u64,
    /// Bytes available to unprivileged users (less than `total - used` on ext4, by the
    /// reserved blocks).
    pub available_bytes: u64,
}

/// Load averages.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Load {
    /// One-minute load average.
    pub one: f64,
    /// Five-minute load average.
    pub five: f64,
    /// Fifteen-minute load average.
    pub fifteen: f64,
}

/// Memory, in bytes. Prometheus only: the probe does not collect it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    /// Total memory.
    pub total_bytes: u64,
    /// `MemAvailable`, or the platform's nearest equivalent.
    pub available_bytes: u64,
}

/// The health values the probe collects, before merging with Prometheus.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SshHealth {
    /// Seconds since boot.
    pub uptime_seconds: Option<u64>,
    /// Root filesystem usage.
    pub root_disk: Option<DiskUsage>,
    /// Load averages.
    pub load: Option<Load>,
}

/// What one successful probe saw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Which script answered (`probe=`); `None` if the key was missing.
    pub class: Option<MachineClass>,
    /// `/run/current-system`. `None` means **not installed**: the link is missing or
    /// dangling, or points outside the store.
    pub current_system: Option<StorePath>,
    /// `/run/booted-system/kernel`. NixOS only.
    pub booted_kernel: Option<StorePath>,
    /// `/run/booted-system/initrd`. NixOS only.
    pub booted_initrd: Option<StorePath>,
    /// `/run/current-system/kernel`. NixOS only.
    pub current_kernel: Option<StorePath>,
    /// `/run/current-system/initrd`. NixOS only.
    pub current_initrd: Option<StorePath>,
    /// `nixos-version`/`darwin-version --configuration-revision`. `None` when empty or
    /// `null`, which is what an unset `system.configurationRevision` prints.
    pub configuration_revision: Option<String>,
    /// `systemctl is-system-running` (`running`, `degraded`, …). NixOS only.
    pub system_state: Option<String>,
    /// Failed units, by name. `None` when unknown (darwin, or systemctl's output did not
    /// parse); `Some(vec![])` when systemd reported none.
    pub failed_units: Option<Vec<String>>,
    /// Uptime, disk and load as the probe saw them.
    pub ssh_health: SshHealth,
}

impl Observation {
    /// A reboot is needed: the booted kernel or initrd differs from the current one. Only
    /// pairs where both sides are known are compared, so darwin never needs one.
    #[must_use]
    pub fn reboot_needed(&self) -> bool {
        let differs = |a: &Option<StorePath>, b: &Option<StorePath>| matches!((a, b), (Some(a), Some(b)) if a != b);
        differs(&self.booted_kernel, &self.current_kernel)
            || differs(&self.booted_initrd, &self.current_initrd)
    }

    /// Any failed unit.
    #[must_use]
    pub fn has_failed_units(&self) -> bool {
        self.failed_units.as_ref().is_some_and(|u| !u.is_empty())
    }

    /// `systemctl is-system-running` said `degraded`: at least one unit has failed, whether
    /// or not the failed-unit list could be read.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.system_state.as_deref() == Some("degraded")
    }
}

// ---------------------------------------------------------------------------------------
// The parser
// ---------------------------------------------------------------------------------------

/// How the ssh process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "code", rename_all = "snake_case")]
pub enum ProbeExit {
    /// It exited with this status.
    Code(i32),
    /// It was killed by a signal (not by auspex's deadline).
    Signal(i32),
    /// auspex's own deadline passed and it was killed.
    TimedOut,
}

/// What a probe found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeOutcome {
    /// The probe ran to its end marker. Boxed: it is much the largest variant.
    Observed(Box<Observation>),
    /// ssh itself failed (exit 255) or the deadline passed.
    Unreachable {
        /// ssh's last non-empty stderr line, sanitised ("Host key verification failed."),
        /// or a description of the timeout.
        reason: String,
    },
    /// ssh connected but the probe did not complete: the remote shell failed, the output
    /// was cut off, or it is not probe output at all.
    Failed {
        /// What went wrong, with the last stderr line when there is one.
        reason: String,
    },
}

fn is_key(k: &str) -> bool {
    !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn store_path(v: &str) -> Option<StorePath> {
    StorePath::new(v.trim()).ok()
}

fn text_value(v: &str) -> Option<String> {
    let v = sanitize_line(v);
    (!v.is_empty() && v != "null").then_some(v)
}

fn parse_failed_units(v: &str) -> Option<Vec<String>> {
    #[derive(Deserialize)]
    struct Unit {
        unit: String,
    }
    let units: Vec<Unit> = serde_json::from_str(v.trim()).ok()?;
    Some(
        units
            .into_iter()
            .map(|u| sanitize_line(&u.unit))
            .filter(|u| !u.is_empty())
            .collect(),
    )
}

fn parse_uptime(v: &str) -> Option<u64> {
    let v = v.trim();
    let int = v.split_once('.').map_or(v, |(i, _)| i);
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    int.parse().ok()
}

fn parse_load(v: &str) -> Option<Load> {
    let mut it = v.split_whitespace().map(str::parse::<f64>);
    let (one, five, fifteen) = (it.next()?.ok()?, it.next()?.ok()?, it.next()?.ok()?);
    let ok = |x: f64| x.is_finite() && x >= 0.0;
    (it.next().is_none() && ok(one) && ok(five) && ok(fifteen)).then_some(Load {
        one,
        five,
        fifteen,
    })
}

fn parse_disk(v: &str) -> Option<DiskUsage> {
    let mut it = v.split_whitespace().map(str::parse::<u64>);
    let total = it.next()?.ok()?;
    let used = it.next()?.ok()?;
    let avail = it.next()?.ok()?;
    if it.next().is_some() {
        return None;
    }
    Some(DiskUsage {
        total_bytes: total.checked_mul(1024)?,
        used_bytes: used.checked_mul(1024)?,
        available_bytes: avail.checked_mul(1024)?,
    })
}

fn last_stderr_line(stderr: &str) -> Option<String> {
    last_lines(stderr, 1).pop()
}

/// Turns one probe's stdout, stderr and exit into a [`ProbeOutcome`].
///
/// - Exit 255 is ssh's own failure, and a [`ProbeExit::TimedOut`] is auspex's deadline:
///   both are [`ProbeOutcome::Unreachable`] with ssh's last non-empty stderr line.
/// - Otherwise, the output between the `auspex_probe=1` and `auspex_end=1` markers is
///   parsed, whatever the exit status. Before the start marker (a login banner) is ignored,
///   and so is everything after the end marker. No end marker, or an `auspex_end` with any
///   value but `1` (a run cut off mid-line), is [`ProbeOutcome::Failed`].
/// - Within it: CRLF, blank lines, lines that are not `key=value` and unknown keys are
///   ignored; a value may contain `=` (split at the first); a repeated key keeps its last
///   value; a missing or unparsable value is `None`.
#[must_use]
pub fn parse_probe(stdout: &str, stderr: &str, exit: ProbeExit) -> ProbeOutcome {
    match exit {
        ProbeExit::Code(255) => {
            return ProbeOutcome::Unreachable {
                reason: last_stderr_line(stderr).unwrap_or_else(|| "ssh exited 255".to_owned()),
            };
        }
        ProbeExit::TimedOut => {
            return ProbeOutcome::Unreachable {
                reason: last_stderr_line(stderr)
                    .map_or_else(|| "timed out".to_owned(), |l| format!("timed out: {l}")),
            };
        }
        ProbeExit::Code(_) | ProbeExit::Signal(_) => {}
    }

    let mut obs = Observation {
        class: None,
        current_system: None,
        booted_kernel: None,
        booted_initrd: None,
        current_kernel: None,
        current_initrd: None,
        configuration_revision: None,
        system_state: None,
        failed_units: None,
        ssh_health: SshHealth::default(),
    };
    let mut started = false;
    let mut ended = false;
    for line in stdout.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !started {
            started = key == "auspex_probe" && value.trim() == "1";
            continue;
        }
        if !is_key(key) {
            continue;
        }
        match key {
            // Only the complete marker ends a probe. A run cut off just after `auspex_end=`,
            // or any other value, is not a finished probe.
            "auspex_end" => {
                ended = value.trim() == "1";
                break;
            }
            "probe" => {
                obs.class = match value.trim() {
                    "nixos" => Some(MachineClass::Nixos),
                    "darwin" => Some(MachineClass::Darwin),
                    _ => None,
                };
            }
            "current_system" => obs.current_system = store_path(value),
            "booted_kernel" => obs.booted_kernel = store_path(value),
            "booted_initrd" => obs.booted_initrd = store_path(value),
            "current_kernel" => obs.current_kernel = store_path(value),
            "current_initrd" => obs.current_initrd = store_path(value),
            "configuration_revision" => obs.configuration_revision = text_value(value),
            "system_state" => obs.system_state = text_value(value),
            "failed_units" => obs.failed_units = parse_failed_units(value),
            "uptime_seconds" => obs.ssh_health.uptime_seconds = parse_uptime(value),
            "load" => obs.ssh_health.load = parse_load(value),
            "root_disk_kb" => obs.ssh_health.root_disk = parse_disk(value),
            _ => {}
        }
    }

    if ended {
        return ProbeOutcome::Observed(Box::new(obs));
    }
    let what = match exit {
        ProbeExit::Code(0) => "probe output cut off".to_owned(),
        ProbeExit::Code(c) => format!("probe exited {c}"),
        ProbeExit::Signal(s) => format!("probe killed by signal {s}"),
        // Returned early above; spelled out rather than `unreachable!` so the function
        // provably cannot panic.
        ProbeExit::TimedOut => "timed out".to_owned(),
    };
    let what = if started {
        what
    } else {
        format!("{what}, no probe output")
    };
    ProbeOutcome::Failed {
        reason: match last_stderr_line(stderr) {
            Some(l) => format!("{what}: {l}"),
            None => what,
        },
    }
}

// ---------------------------------------------------------------------------------------
// Prometheus
// ---------------------------------------------------------------------------------------

/// One machine's values from the fleet-wide Prometheus query. Any value may be missing (no
/// series for that instance).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PromSample {
    /// Seconds since boot.
    pub uptime_seconds: Option<u64>,
    /// Root filesystem usage.
    pub root_disk: Option<DiskUsage>,
    /// Load averages.
    pub load: Option<Load>,
    /// Memory.
    pub memory: Option<Memory>,
    /// Seconds between the sample's timestamp and the query. Computed by the caller: the
    /// clock is an effect.
    pub age_seconds: u64,
}

/// Where a health value came from. The detail panel shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthSource {
    /// The fleet-wide Prometheus query.
    Prometheus,
    /// The ssh probe.
    Ssh,
}

/// A value and its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sourced<T> {
    /// The value.
    pub value: T,
    /// Where it came from.
    pub source: HealthSource,
}

/// The merged health values for one machine.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Health {
    /// Seconds since boot.
    pub uptime_seconds: Option<Sourced<u64>>,
    /// Root filesystem usage.
    pub root_disk: Option<Sourced<DiskUsage>>,
    /// Load averages.
    pub load: Option<Sourced<Load>>,
    /// Memory (Prometheus only).
    pub memory: Option<Sourced<Memory>>,
}

fn pick<T: Copy>(prom: Option<T>, ssh: Option<T>) -> Option<Sourced<T>> {
    prom.map(|value| Sourced {
        value,
        source: HealthSource::Prometheus,
    })
    .or_else(|| {
        ssh.map(|value| Sourced {
            value,
            source: HealthSource::Ssh,
        })
    })
}

/// Merges per value: Prometheus when it has the value and the sample is no older than
/// `max_age_seconds` (about two scrape intervals), otherwise the ssh probe's.
///
/// The decision is per value rather than per machine, so a Prometheus that has uptime but
/// no filesystem series for a host still fills the disk from ssh.
#[must_use]
pub fn merge_health(
    ssh: Option<&SshHealth>,
    prom: Option<&PromSample>,
    max_age_seconds: u64,
) -> Health {
    let prom = prom.filter(|p| p.age_seconds <= max_age_seconds);
    Health {
        uptime_seconds: pick(
            prom.and_then(|p| p.uptime_seconds),
            ssh.and_then(|s| s.uptime_seconds),
        ),
        root_disk: pick(
            prom.and_then(|p| p.root_disk),
            ssh.and_then(|s| s.root_disk),
        ),
        load: pick(prom.and_then(|p| p.load), ssh.and_then(|s| s.load)),
        memory: pick(prom.and_then(|p| p.memory), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYS: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-terra-25.05";

    fn observed(o: ProbeOutcome) -> Observation {
        match o {
            ProbeOutcome::Observed(o) => *o,
            other => panic!("expected observed, got {other:?}"),
        }
    }

    #[test]
    fn store_path_validation() {
        let p = StorePath::new(SYS).unwrap();
        assert_eq!(p.hash(), "0123456789abcdfghijklmnpqrsvwxyz");
        assert_eq!(p.name(), "nixos-system-terra-25.05");
        let k = StorePath::new("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-linux-6.12.1/bzImage")
            .unwrap();
        assert_eq!(k.name(), "linux-6.12.1");
        for bad in [
            "",
            "/run/current-system",
            "/nix/store/",
            "/nix/store/short-name",
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz",
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-",
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz_name",
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-a b",
            "/nix/store/0123456789abcdfghijklmnpqrsvwxé-name",
            "nix/store/0123456789abcdfghijklmnpqrsvwxyz-name",
            "/nix/storeX/0123456789abcdfghijklmnpqrsvwxyz-name",
        ] {
            assert!(StorePath::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn tolerates_crlf_blanks_banners_unknown_keys_and_equals() {
        let out = format!(
            "Welcome to terra!\r\nmotd: key=value lookalike\r\n\r\nauspex_probe=1\r\n\
             probe=nixos\r\n\r\ncurrent_system={SYS}\r\nsome_future_key=x\r\n\
             Not A Key=1\r\nconfiguration_revision=abc=def\r\nauspex_end=1\r\n"
        );
        let o = observed(parse_probe(&out, "", ProbeExit::Code(0)));
        assert_eq!(o.class, Some(MachineClass::Nixos));
        assert_eq!(o.current_system.as_ref().map(StorePath::as_str), Some(SYS));
        assert_eq!(o.configuration_revision.as_deref(), Some("abc=def"));
    }

    #[test]
    fn readlink_f_output_is_not_installed() {
        // What `readlink -f /run/current-system` prints when the link is missing.
        let out = "auspex_probe=1\ncurrent_system=/run/current-system\nauspex_end=1\n";
        let o = observed(parse_probe(out, "", ProbeExit::Code(0)));
        assert_eq!(o.current_system, None);
    }

    #[test]
    fn repeated_key_keeps_last() {
        let out = format!(
            "auspex_probe=1\nsystem_state=starting\nsystem_state=running\ncurrent_system={SYS}\nauspex_end=1\n"
        );
        let o = observed(parse_probe(&out, "", ProbeExit::Code(0)));
        assert_eq!(o.system_state.as_deref(), Some("running"));
    }

    #[test]
    fn missing_markers() {
        assert_eq!(
            parse_probe("Welcome\n", "sh: not found\n", ProbeExit::Code(127)),
            ProbeOutcome::Failed {
                reason: "probe exited 127, no probe output: sh: not found".to_owned()
            }
        );
        assert_eq!(
            parse_probe("auspex_probe=1\nprobe=nixos\n", "", ProbeExit::Code(0)),
            ProbeOutcome::Failed {
                reason: "probe output cut off".to_owned()
            }
        );
        // A completed probe is used even if the shell's exit status is not 0.
        assert!(matches!(
            parse_probe("auspex_probe=1\nauspex_end=1\n", "", ProbeExit::Code(1)),
            ProbeOutcome::Observed(_)
        ));
    }

    #[test]
    fn unreachable() {
        assert_eq!(
            parse_probe(
                "",
                "Warning: something\n\u{1b}[1mHost key verification failed.\u{1b}[0m\r\n\n",
                ProbeExit::Code(255)
            ),
            ProbeOutcome::Unreachable {
                reason: "Host key verification failed.".to_owned()
            }
        );
        assert_eq!(
            parse_probe("", "", ProbeExit::Code(255)),
            ProbeOutcome::Unreachable {
                reason: "ssh exited 255".to_owned()
            }
        );
        assert_eq!(
            parse_probe("auspex_probe=1\n", "", ProbeExit::TimedOut),
            ProbeOutcome::Unreachable {
                reason: "timed out".to_owned()
            }
        );
        // Exit 255 wins even over complete output: ssh says it failed.
        assert!(matches!(
            parse_probe("auspex_probe=1\nauspex_end=1\n", "x", ProbeExit::Code(255)),
            ProbeOutcome::Unreachable { .. }
        ));
    }

    #[test]
    fn value_parsers() {
        assert_eq!(parse_uptime("12345.67"), Some(12345));
        assert_eq!(parse_uptime("42"), Some(42));
        assert_eq!(parse_uptime(""), None);
        assert_eq!(parse_uptime("-1"), None);
        assert_eq!(parse_uptime("1e9"), None);
        assert_eq!(
            parse_load(" 0.50 1.25 2.00 "),
            Some(Load {
                one: 0.5,
                five: 1.25,
                fifteen: 2.0
            })
        );
        assert_eq!(parse_load("0.5 1.2"), None);
        assert_eq!(parse_load("NaN 1 1"), None);
        assert_eq!(parse_load("1 2 3 4"), None);
        assert_eq!(
            parse_disk("100 40 50"),
            Some(DiskUsage {
                total_bytes: 102_400,
                used_bytes: 40_960,
                available_bytes: 51_200
            })
        );
        assert_eq!(parse_disk("100 40"), None);
        assert_eq!(parse_disk(&format!("{} 0 0", u64::MAX)), None);
        assert_eq!(
            parse_failed_units(
                r#"[{"unit":"a.service","load":"loaded"},{"unit":"dev-disk-by\\x2duuid.mount"}]"#
            ),
            Some(vec![
                "a.service".to_owned(),
                r"dev-disk-by\x2duuid.mount".to_owned()
            ])
        );
        assert_eq!(parse_failed_units("[]"), Some(vec![]));
        assert_eq!(parse_failed_units(""), None);
        assert_eq!(parse_failed_units("garbage"), None);
        assert_eq!(text_value("null"), None);
        assert_eq!(text_value("  "), None);
    }

    #[test]
    fn reboot_needed_compares_known_pairs() {
        let out = |bk: &str, ck: &str| {
            format!("auspex_probe=1\nbooted_kernel={bk}\ncurrent_kernel={ck}\nauspex_end=1\n")
        };
        let a = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-linux-6.12.1/bzImage";
        let b = "/nix/store/zyxwvsrqpnmlkjihgfdcba9876543210-linux-6.12.2/bzImage";
        assert!(!observed(parse_probe(&out(a, a), "", ProbeExit::Code(0))).reboot_needed());
        assert!(observed(parse_probe(&out(a, b), "", ProbeExit::Code(0))).reboot_needed());
        assert!(!observed(parse_probe(&out(a, ""), "", ProbeExit::Code(0))).reboot_needed());
    }

    #[test]
    fn scripts_follow_the_rules() {
        for s in [NIXOS_PROBE, DARWIN_PROBE] {
            // No `readlink -f`, `-m` or `-e`: each prints a path for a missing link.
            assert!(!s.contains("readlink -"));
            // Every link is read through `link`, which tests existence first.
            assert!(
                s.contains(r#"link() { if [ -e "$1" ]; then readlink "$1" 2>/dev/null; fi; }"#)
            );
            assert_eq!(s.matches("readlink").count(), 1);
            assert!(!s.contains("jq"));
            assert!(!s.contains("echo"));
            assert!(s.contains("printf 'auspex_probe=1\\n'"));
            assert!(s.trim_end().ends_with("printf 'auspex_end=1\\n'"));
        }
        assert!(NIXOS_PROBE.contains("systemctl"));
        assert!(!DARWIN_PROBE.contains("systemctl"));
        assert!(!DARWIN_PROBE.contains("booted"));
    }

    fn prom(age: u64) -> PromSample {
        PromSample {
            uptime_seconds: Some(100),
            root_disk: None,
            load: Some(Load {
                one: 1.0,
                five: 1.0,
                fifteen: 1.0,
            }),
            memory: Some(Memory {
                total_bytes: 8,
                available_bytes: 4,
            }),
            age_seconds: age,
        }
    }

    #[test]
    fn merge_prefers_fresh_prometheus_per_value() {
        let ssh = SshHealth {
            uptime_seconds: Some(99),
            root_disk: Some(DiskUsage {
                total_bytes: 10,
                used_bytes: 5,
                available_bytes: 5,
            }),
            load: None,
        };
        let h = merge_health(Some(&ssh), Some(&prom(30)), 60);
        assert_eq!(h.uptime_seconds.unwrap().source, HealthSource::Prometheus);
        assert_eq!(h.uptime_seconds.unwrap().value, 100);
        assert_eq!(h.root_disk.unwrap().source, HealthSource::Ssh);
        assert_eq!(h.load.unwrap().source, HealthSource::Prometheus);
        assert_eq!(h.memory.unwrap().source, HealthSource::Prometheus);

        // Stale: everything from ssh, and no memory.
        let h = merge_health(Some(&ssh), Some(&prom(61)), 60);
        assert_eq!(h.uptime_seconds.unwrap().source, HealthSource::Ssh);
        assert_eq!(h.uptime_seconds.unwrap().value, 99);
        assert!(h.load.is_none());
        assert!(h.memory.is_none());

        // Boundary: exactly max_age is fresh.
        let h = merge_health(None, Some(&prom(60)), 60);
        assert_eq!(h.uptime_seconds.unwrap().source, HealthSource::Prometheus);

        assert_eq!(merge_health(None, None, 60), Health::default());
    }

    /// Correctness 1: only `auspex_end=1` completes a probe.
    #[test]
    fn end_marker_must_be_complete() {
        for input in [
            format!("auspex_probe=1\ncurrent_system={SYS}\nauspex_end="),
            format!("auspex_probe=1\ncurrent_system={SYS}\nauspex_end=\n"),
            "auspex_probe=1\nauspex_end=0\n".to_owned(),
            "auspex_probe=1\nauspex_end=garbage\n".to_owned(),
            "auspex_probe=1\nauspex_end=11\n".to_owned(),
        ] {
            assert_eq!(
                parse_probe(&input, "", ProbeExit::Code(0)),
                ProbeOutcome::Failed {
                    reason: "probe output cut off".to_owned()
                },
                "{input:?}"
            );
        }
        // Surrounding whitespace and CRLF are still fine.
        assert!(matches!(
            parse_probe(
                "auspex_probe=1\r\nauspex_end= 1 \r\n",
                "",
                ProbeExit::Code(0)
            ),
            ProbeOutcome::Observed(_)
        ));
    }

    /// M20/M31: a start marker of another version is not this probe's output.
    #[test]
    fn start_marker_must_be_version_1() {
        for start in ["auspex_probe=2", "auspex_probe=", "auspex_probe=10"] {
            let out = format!("{start}\ncurrent_system={SYS}\nauspex_end=1\n");
            assert_eq!(
                parse_probe(&out, "", ProbeExit::Code(0)),
                ProbeOutcome::Failed {
                    reason: "probe output cut off, no probe output".to_owned()
                },
                "{start}"
            );
        }
    }

    /// Correctness 3: dot components are not store paths.
    #[test]
    fn store_path_components_are_plain() {
        for bad in [
            format!("{SYS}/../../../run/current-system"),
            format!("{SYS}/.."),
            format!("{SYS}/."),
            format!("{SYS}/./kernel"),
            format!("{SYS}//kernel"),
            format!("{SYS}/"),
        ] {
            assert!(StorePath::new(bad.as_str()).is_err(), "{bad:?}");
        }
        for good in [
            format!("{SYS}/kernel"),
            format!("{SYS}/sw/bin/..x"),
            format!("{SYS}/.x"),
        ] {
            assert!(StorePath::new(good.as_str()).is_ok(), "{good:?}");
        }
        let out = format!(
            "auspex_probe=1\ncurrent_system={SYS}/../../../run/current-system\nauspex_end=1\n"
        );
        assert_eq!(
            observed(parse_probe(&out, "", ProbeExit::Code(0))).current_system,
            None
        );
    }

    /// M39 and M45: remote text in the observation is sanitised.
    #[test]
    fn unit_names_and_revision_are_sanitised() {
        let out = "auspex_probe=1\nconfiguration_revision=\u{1b}[2Jabc\u{202e}def\n\
                   failed_units=[{\"unit\":\"x\\u001b]0;t\\u0007.service\"},{\"unit\":\"\\u202e\"}]\n\
                   auspex_end=1\n";
        let o = observed(parse_probe(out, "", ProbeExit::Code(0)));
        assert_eq!(o.configuration_revision.as_deref(), Some("abcdef"));
        assert_eq!(o.failed_units, Some(vec!["x.service".to_owned()]));
    }
}
