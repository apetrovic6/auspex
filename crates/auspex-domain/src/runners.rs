//! The CI runners view (AUS-19): which runners the forge knows, what state their systemd units
//! are in, how loaded their host is, and the badges that follow from comparing the three.
//!
//! Nothing here does I/O. The forge's answers, the probe's units and Prometheus's samples come
//! in as values ([`ForgeSlot`], [`MachineFacts`]); [`report`] joins them and returns rows.
//!
//! # What the forge cannot say
//!
//! Forgejo 15's `ActionRunner` carries `status` (`offline`, `idle`, `active`), `labels` and
//! `version`, and its waiting-jobs list (`…/actions/runners/jobs`) carries a `status` and no time
//! at all. So, by what the API gives and not by choice:
//!
//! - a busy (`active`) runner is shown as **busy**; which job it runs is not known and is not
//!   guessed;
//! - queued jobs are a **count**; there is no "oldest waiting for 12 minutes";
//! - there is no "job running longer than a limit" badge: the API does not say when a job began.
//!
//! # Joining
//!
//! A row is `(forge, runner name)`. Its machine is [`RunnersConfig::machines`] when the name is
//! mapped, else the machine whose name the runner's name equals or begins with `<machine>-`
//! (the longest such machine, so `alpha-docker` goes to `alpha-docker` before `alpha`). Its unit
//! is the machine's runner unit whose instance name (`gitea-runner-<instance>.service`, systemd
//! escapes undone, the glob's literal prefix removed) equals the runner's name, or the runner's
//! name without the `<machine>-` prefix. A machine whose probe did not report units has no unit
//! facts, and no row of it is ever called "unit down".

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::ci::segment_ok;
use crate::observe::{Load, Memory, RunnerUnit, UnitActive};
use crate::text::sanitize_line;

/// The default for [`RunnersConfig::memory_warn_percent`].
pub const DEFAULT_MEMORY_WARN_PERCENT: u8 = 90;

/// The longest runner name, label or version kept, in characters.
pub const MAX_TEXT_CHARS: usize = 100;

/// The most labels kept for a runner.
pub const MAX_LABELS: usize = 32;

/// The reason a forge's placeholder row carries while its request is out.
pub const CHECKING: &str = "still being checked";

// ---------------------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------------------

/// Whose runners a forge entry lists (`scope` in the config).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every runner of the instance: `/admin/actions/runners` (needs `read:admin`). The default.
    Admin,
    /// The token owner's: `/user/actions/runners`.
    User,
    /// An organisation's: `/orgs/{org}/actions/runners`.
    Org(String),
    /// A repository's: `/repos/{owner}/{repo}/actions/runners`.
    Repo {
        /// The owner.
        owner: String,
        /// The repository.
        name: String,
    },
}

impl Scope {
    /// `admin`, `user`, `org:<name>` or `repo:<owner>/<name>`.
    ///
    /// # Errors
    ///
    /// One line naming the forms. The text given is not quoted.
    pub fn parse(s: &str) -> Result<Self, String> {
        const FORMS: &str = "must be admin, user, org:<name> or repo:<owner>/<name> (letters, \
                             digits, `.`, `_` and `-`)";
        match s {
            "admin" => Ok(Self::Admin),
            "user" => Ok(Self::User),
            _ => {
                if let Some(org) = s.strip_prefix("org:") {
                    return segment_ok(org)
                        .then(|| Self::Org(org.to_owned()))
                        .ok_or_else(|| FORMS.to_owned());
                }
                if let Some(repo) = s.strip_prefix("repo:") {
                    let mut parts = repo.split('/');
                    if let (Some(owner), Some(name), None) =
                        (parts.next(), parts.next(), parts.next())
                        && segment_ok(owner)
                        && segment_ok(name)
                    {
                        return Ok(Self::Repo {
                            owner: owner.to_owned(),
                            name: name.to_owned(),
                        });
                    }
                }
                Err(FORMS.to_owned())
            }
        }
    }

    /// The path segments after `/api/v1/` that lead to `actions/runners`.
    #[must_use]
    pub fn segments(&self) -> Vec<&str> {
        match self {
            Self::Admin => vec!["admin"],
            Self::User => vec!["user"],
            Self::Org(org) => vec!["orgs", org],
            Self::Repo { owner, name } => vec!["repos", owner, name],
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admin => f.write_str("admin"),
            Self::User => f.write_str("user"),
            Self::Org(o) => write!(f, "org:{o}"),
            Self::Repo { owner, name } => write!(f, "repo:{owner}/{name}"),
        }
    }
}

// ---------------------------------------------------------------------------------------
// What the forge says
// ---------------------------------------------------------------------------------------

/// A runner's state as the forge reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerStatus {
    /// Not connected.
    Offline,
    /// Connected, no job.
    Idle,
    /// Connected and running a job (which one, the API does not say).
    Active,
    /// A word the API does not define: not counted as online, and not called offline either.
    Unknown,
}

impl RunnerStatus {
    /// Reads the forge's `status`.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "offline" => Self::Offline,
            "idle" => Self::Idle,
            "active" => Self::Active,
            _ => Self::Unknown,
        }
    }

    /// Connected: idle or busy.
    #[must_use]
    pub const fn online(self) -> bool {
        matches!(self, Self::Idle | Self::Active)
    }

    /// The word shown.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Idle => "idle",
            Self::Active => "busy",
            Self::Unknown => "unknown",
        }
    }
}

/// What replaces a text that held a secret.
pub const WITHHELD: &str = "(withheld)";

/// One runner the forge listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeRunner {
    /// The forge's own identifier for it, when it gave one: what tells one runner from another of
    /// the same name, and the same runner listed by two scopes of one forge. Not shown, not in
    /// `--json`.
    pub id: Option<i64>,
    /// Its name (not unique on a forge).
    pub name: String,
    /// Its status.
    pub status: RunnerStatus,
    /// Its labels.
    pub labels: Vec<String>,
    /// Its self-reported version.
    pub version: Option<String>,
}

/// `line` cut to [`MAX_TEXT_CHARS`] characters, then `…`.
fn cut(line: String) -> String {
    match line.char_indices().nth(MAX_TEXT_CHARS) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line,
    }
}

/// `text` as it will be shown: sanitised, then checked **whole** for a secret, and only then cut.
/// The order is the point. The sanitiser removes control and format characters, so a secret with
/// one inserted is only the secret once sanitised; and the cut removes the end of a long text, so a
/// secret that straddles it is, in what is left, all but its last character: a guard that looks at
/// either too early is a guard the forge can walk around. A text that held the secret is not shown
/// at all.
fn shown(text: &str, secret: &dyn Fn(&str) -> bool) -> String {
    let line = sanitize_line(text);
    if secret(&line) {
        WITHHELD.to_owned()
    } else {
        cut(line)
    }
}

impl ForgeRunner {
    /// A runner from the forge's own words: every text is made safe for a terminal and cut to
    /// [`MAX_TEXT_CHARS`], at most [`MAX_LABELS`] labels are kept.
    #[must_use]
    pub fn new(name: &str, status: &str, labels: &[String], version: Option<&str>) -> Self {
        Self::new_guarded(name, status, labels, version, &|_| false)
    }

    /// [`Self::new`], with every text that `secret` says holds a secret replaced by [`WITHHELD`].
    /// `secret` is asked of the text sanitised and **not yet cut** (see `shown`).
    #[must_use]
    pub fn new_guarded(
        name: &str,
        status: &str,
        labels: &[String],
        version: Option<&str>,
        secret: &dyn Fn(&str) -> bool,
    ) -> Self {
        Self {
            id: None,
            name: shown(name, secret),
            status: RunnerStatus::parse(status),
            labels: labels
                .iter()
                .take(MAX_LABELS)
                .map(|l| shown(l, secret))
                .filter(|l| !l.is_empty())
                .collect(),
            version: version.map(|v| shown(v, secret)).filter(|v| !v.is_empty()),
        }
    }

    /// This runner with the forge's identifier for it.
    #[must_use]
    pub const fn with_id(mut self, id: Option<i64>) -> Self {
        self.id = id;
        self
    }
}

/// What one forge entry has answered, or that it has not yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeSlot {
    /// The entry's name on screen (its host; the scope too when two entries share a host).
    pub forge: String,
    /// Its scope, as configured.
    pub scope: String,
    /// Which forge it reads: its host, port and path. Two entries with the same site are two
    /// scopes of one forge, and a runner both list (by the forge's identifier) is one runner.
    pub site: String,
    /// The answer, `None` while the request is out.
    pub answer: Option<ForgeAnswer>,
}

/// What a forge entry answered. The two requests fail independently: a forge whose jobs list is
/// forbidden still has its runners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeAnswer {
    /// The runners, or why they could not be listed (one line, never the token).
    pub runners: Result<Vec<ForgeRunner>, String>,
    /// How many jobs are waiting, or why that is not known.
    pub queued: Result<u64, String>,
}

// ---------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------

/// A host's caps, for the Prometheus figures. Nothing is assumed: a host without a cap has no
/// figure of that kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostCaps {
    /// CPUs the runner host may use (the load average is shown against it).
    pub cpus: Option<u32>,
    /// Memory in bytes the runner host may use (what is in use is shown against it).
    pub memory_bytes: Option<u64>,
}

/// `[runners]`, as the core joins with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnersConfig {
    /// Which units are runner units (`unit_glob`).
    pub unit_glob: String,
    /// The percent of the memory cap from which a row is badged (default 90).
    pub memory_warn_percent: u8,
    /// `[runners.hosts.<machine>]`.
    pub hosts: BTreeMap<String, HostCaps>,
    /// `[runners.machines]`: runner name to machine name.
    pub machines: BTreeMap<String, String>,
}

impl Default for RunnersConfig {
    fn default() -> Self {
        Self {
            unit_glob: crate::observe::DEFAULT_RUNNER_GLOB.to_owned(),
            memory_warn_percent: DEFAULT_MEMORY_WARN_PERCENT,
            hosts: BTreeMap::new(),
            machines: BTreeMap::new(),
        }
    }
}

/// A memory size as `[runners.hosts.<machine>] memory` spells it.
///
/// A whole number and a unit, `B`,
/// binary `KiB` `MiB` `GiB` `TiB` or decimal `kB` `MB` `GB` `TB` (no space, no fraction: `4GiB`,
/// `512MiB`). More than 0 and within `u64`.
///
/// # Errors
///
/// One line. The text given is not quoted.
pub fn parse_size(s: &str) -> Result<u64, String> {
    const FORMS: &str = "must be a whole number and a unit, such as 4GiB or 512MiB (B, KiB, MiB, \
                         GiB, TiB, kB, MB, GB, TB)";
    let at = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| FORMS.to_owned())?;
    let (number, unit) = s.split_at(at);
    let factor: u64 = match unit {
        "B" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        "kB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        _ => return Err(FORMS.to_owned()),
    };
    let n: u64 = number.parse().map_err(|_| FORMS.to_owned())?;
    match n.checked_mul(factor) {
        Some(bytes) if bytes > 0 => Ok(bytes),
        Some(_) => Err("must be more than 0".to_owned()),
        None => Err("is too large".to_owned()),
    }
}

/// What the join needs of one machine.

#[derive(Debug, Clone, Copy)]
pub struct MachineFacts<'a> {
    /// Its inventory name.
    pub name: &'a str,
    /// The runner units its last probe reported; `None` when not known.
    pub units: Option<&'a [RunnerUnit]>,
    /// Memory (Prometheus), when known.
    pub memory: Option<Memory>,
    /// Load averages, when known.
    pub load: Option<Load>,
}

// ---------------------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------------------

/// Why a row is badged. The first three make it red.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerBadge {
    /// The forge says the runner is offline.
    Offline,
    /// The forge says online, and its unit is stopped or failed.
    ForgeUpUnitDown,
    /// The forge says offline, and its unit is running.
    ForgeDownUnitUp,
    /// The host's memory in use is at or above `memory_warn_percent` of its cap.
    MemoryNearCap,
    /// The forge could not be asked: this row stands for the whole forge entry.
    ForgeError,
}

impl RunnerBadge {
    /// What makes a row red.
    #[must_use]
    pub const fn is_red(self) -> bool {
        !matches!(self, Self::MemoryNearCap)
    }

    /// The short word shown.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::ForgeUpUnitDown => "unit down",
            Self::ForgeDownUnitUp => "unit up",
            Self::MemoryNearCap => "memory",
            Self::ForgeError => "forge error",
        }
    }
}

/// The unit a runner's row was joined to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitFacts {
    /// The unit's name.
    pub unit: String,
    /// `ActiveState`.
    pub active: UnitActive,
    /// `SubState`.
    pub sub: String,
}

/// Memory in use on the host against its configured cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryUse {
    /// Total less available.
    pub used_bytes: u64,
    /// The cap from `[runners.hosts.<machine>]`.
    pub cap_bytes: u64,
    /// `used / cap`, in whole percent (may pass 100 when the host has more memory than the cap).
    pub percent: u64,
}

/// The one-minute load against the host's CPUs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LoadUse {
    /// The one-minute load average.
    pub one: f64,
    /// The cap from `[runners.hosts.<machine>]`.
    pub cpus: u32,
}

/// One row of the Runners tab and one entry of `--json`'s `runners`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunnerRow {
    /// The forge entry's name on screen.
    pub forge: String,
    /// The runner's name, or `null` for the row that stands for a forge entry that failed (or is
    /// still being asked).
    pub name: Option<String>,
    /// `offline`, `idle`, `active` or `unknown` (`active` is "busy": which job is not known).
    pub status: RunnerStatus,
    /// The runner's labels.
    pub labels: Vec<String>,
    /// Its version.
    pub version: Option<String>,
    /// The machine it was joined to, or `null`.
    pub machine: Option<String>,
    /// The unit it was joined to, or `null` (no machine, probe not reporting, or no match).
    pub unit: Option<UnitFacts>,
    /// Memory in use against the cap, or `null` (no cap, or no Prometheus figure).
    pub memory: Option<MemoryUse>,
    /// Load against the CPU cap, or `null`.
    pub load: Option<LoadUse>,
    /// Why it is badged, in a fixed order; `[]` when nothing is wrong.
    pub badges: Vec<RunnerBadge>,
    /// One line for a placeholder row, `null` for a runner.
    pub error: Option<String>,
}

impl RunnerRow {
    /// Whether the row is drawn red.
    #[must_use]
    pub fn is_red(&self) -> bool {
        self.badges.iter().any(|b| b.is_red())
    }
}

/// What one forge entry gave beyond rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeFacts {
    /// The entry's name on screen.
    pub forge: String,
    /// Its scope.
    pub scope: String,
    /// Whether it has answered.
    pub checking: bool,
    /// Why its runners could not be listed, or `null`.
    pub error: Option<String>,
    /// Waiting jobs, or `null` when not known.
    pub queued: Option<u64>,
    /// Why the waiting jobs are not known, or `null`.
    pub queue_error: Option<String>,
}

/// The tab's data: `rows` is `--json`'s `runners`, `forges` its `runner_forges`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RunnersReport {
    /// One entry per configured forge.
    pub forges: Vec<ForgeFacts>,
    /// One row per (forge, runner), forge order then name.
    pub rows: Vec<RunnerRow>,
}

fn instance_of<'a>(unit: &'a str, glob: &str) -> std::borrow::Cow<'a, str> {
    let stem = unit.strip_suffix(".service").unwrap_or(unit);
    let literal = glob.find(['*', '?', '[']).map_or(glob, |at| &glob[..at]);
    let stem = stem.strip_prefix(literal).unwrap_or(stem);
    unescape(stem)
}

/// systemd's `\xNN` escapes undone (a hyphen in an instance name is `\x2d`).
fn unescape(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains("\\x") {
        return std::borrow::Cow::Borrowed(s);
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && bytes.get(i + 1) == Some(&b'x')
            && let Some(hex) = s.get(i + 2..i + 4)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    std::borrow::Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

/// The machine a runner runs on: the configured mapping, else the longest machine name the
/// runner's name equals or begins with `<machine>-`.
#[must_use]
pub fn machine_of<'a>(
    runner: &str,
    config: &RunnersConfig,
    machines: &[MachineFacts<'a>],
) -> Option<&'a str> {
    if let Some(mapped) = config.machines.get(runner) {
        return machines.iter().find(|m| m.name == mapped).map(|m| m.name);
    }
    machines
        .iter()
        .filter(|m| runner == m.name || runner.starts_with(&format!("{}-", m.name)))
        .max_by_key(|m| m.name.len())
        .map(|m| m.name)
}

const fn is_down(a: UnitActive) -> bool {
    matches!(
        a,
        UnitActive::Inactive
            | UnitActive::Failed
            | UnitActive::Deactivating
            | UnitActive::Maintenance
    )
}

const fn is_up(a: UnitActive) -> bool {
    matches!(a, UnitActive::Active | UnitActive::Reloading)
}

/// The distinct runner names each machine was joined to, over every forge (the map included):
/// what tells a machine with one runner from one with several.
type Joined<'a> = BTreeMap<&'a str, BTreeSet<&'a str>>;

/// Whether the runner called `name`, on `machine`, is the runner of `unit` by its name: the unit's
/// instance is the name, or the name without `<machine>-`.
fn names_unit(machine: &str, name: &str, unit: &RunnerUnit, glob: &str) -> bool {
    let instance = instance_of(&unit.unit, glob);
    instance == name
        || name
            .strip_prefix(&format!("{machine}-"))
            .is_some_and(|rest| instance == rest)
}

/// The unit of the runner called `runner` on `machine`, whose runners (all of them, from every
/// forge) are `names`.
///
/// A unit whose instance the runner's name gives is its unit. Failing that, the usual NixOS layout
/// is one instance, `instances.default`, whose unit is `gitea-runner-default.service` and whose
/// runner is named after the host: no name joins them. When the machine has exactly one runner
/// unit, and no runner owns it by name, it is the runner's if that is the machine's only runner, or
/// the one **named for the machine**: another runner whose name merely begins with `<machine>-`
/// (one somebody else registered, on a forge that lists every runner it can see) does not switch the
/// fallback off for it. With several units, or several runners none of which is named for the
/// machine, which is whose is not known, and none is guessed.
fn unit_of<'u>(
    machine: &str,
    units: &'u [RunnerUnit],
    runner: &str,
    names: &BTreeSet<&str>,
    glob: &str,
) -> Option<&'u RunnerUnit> {
    if let Some(own) = units.iter().find(|u| names_unit(machine, runner, u, glob)) {
        return Some(own);
    }
    let [only] = units else {
        return None;
    };
    if names.iter().any(|n| names_unit(machine, n, only, glob)) {
        return None;
    }
    (names.len() == 1 || runner == machine).then_some(only)
}

fn row_of(
    forge: &str,
    runner: &ForgeRunner,
    config: &RunnersConfig,
    machines: &[MachineFacts<'_>],
    joined: &Joined<'_>,
) -> RunnerRow {
    let machine = machine_of(&runner.name, config, machines);
    let facts = machine.and_then(|n| machines.iter().find(|m| m.name == n));
    let none = BTreeSet::new();
    let unit = machine
        .zip(facts.and_then(|m| m.units))
        .and_then(|(machine, units)| {
            let names = joined.get(machine).unwrap_or(&none);
            unit_of(machine, units, &runner.name, names, &config.unit_glob)
        });
    let caps = machine.and_then(|n| config.hosts.get(n));
    let memory = facts
        .and_then(|m| m.memory)
        .zip(caps.and_then(|c| c.memory_bytes))
        .filter(|(_, cap)| *cap > 0)
        .map(|(mem, cap)| {
            let used = mem.total_bytes.saturating_sub(mem.available_bytes);
            let percent = u128::from(used) * 100 / u128::from(cap);
            MemoryUse {
                used_bytes: used,
                cap_bytes: cap,
                percent: u64::try_from(percent).unwrap_or(u64::MAX),
            }
        });
    let load = facts
        .and_then(|m| m.load)
        .zip(caps.and_then(|c| c.cpus))
        .map(|(l, cpus)| LoadUse { one: l.one, cpus });

    let mut badges = Vec::new();
    if runner.status == RunnerStatus::Offline {
        badges.push(RunnerBadge::Offline);
    }
    if let Some(u) = unit {
        if runner.status.online() && is_down(u.active) {
            badges.push(RunnerBadge::ForgeUpUnitDown);
        }
        if runner.status == RunnerStatus::Offline && is_up(u.active) {
            badges.push(RunnerBadge::ForgeDownUnitUp);
        }
    }
    if memory.is_some_and(|m| m.percent >= u64::from(config.memory_warn_percent)) {
        badges.push(RunnerBadge::MemoryNearCap);
    }
    RunnerRow {
        forge: forge.to_owned(),
        name: Some(runner.name.clone()),
        status: runner.status,
        labels: runner.labels.clone(),
        version: runner.version.clone(),
        machine: machine.map(str::to_owned),
        unit: unit.map(|u| UnitFacts {
            unit: u.unit.clone(),
            active: u.active,
            sub: u.sub.clone(),
        }),
        memory,
        load,
        badges,
        error: None,
    }
}

fn placeholder(slot: &ForgeSlot, error: &str, failing: bool) -> RunnerRow {
    RunnerRow {
        forge: slot.forge.clone(),
        name: None,
        status: RunnerStatus::Unknown,
        labels: Vec::new(),
        version: None,
        machine: None,
        unit: None,
        memory: None,
        load: None,
        badges: if failing {
            vec![RunnerBadge::ForgeError]
        } else {
            Vec::new()
        },
        error: Some(error.to_owned()),
    }
}

/// Joins the forges' runners with the machines' units and figures.
///
/// A forge that failed (or has not answered) gives one placeholder row and never removes or
/// delays another forge's rows; its failure is on its own row.
#[must_use]
pub fn report(
    slots: &[ForgeSlot],
    machines: &[MachineFacts<'_>],
    config: &RunnersConfig,
) -> RunnersReport {
    let mut out = RunnersReport::default();
    let mut joined: Joined<'_> = BTreeMap::new();
    for runner in slots
        .iter()
        .filter_map(|s| s.answer.as_ref())
        .filter_map(|a| a.runners.as_ref().ok())
        .flatten()
    {
        if let Some(m) = machine_of(&runner.name, config, machines) {
            joined.entry(m).or_default().insert(&runner.name);
        }
    }
    // A runner both of two scopes of one forge list (the forge's identifier says it is the same
    // one) is one runner: listed once, under the first entry that has it, and counted once. The
    // identifier is per forge, so the same number on another forge is another runner; a runner the
    // forge gave none is not merged with anything (names are not unique).
    let mut listed: BTreeSet<(&str, i64)> = BTreeSet::new();
    for slot in slots {
        let mut facts = ForgeFacts {
            forge: slot.forge.clone(),
            scope: slot.scope.clone(),
            checking: slot.answer.is_none(),
            error: None,
            queued: None,
            queue_error: None,
        };
        match &slot.answer {
            None => out.rows.push(placeholder(slot, CHECKING, false)),
            Some(answer) => {
                match &answer.queued {
                    Ok(n) => facts.queued = Some(*n),
                    Err(why) => facts.queue_error = Some(why.clone()),
                }
                match &answer.runners {
                    Ok(runners) => {
                        let mut rows: Vec<RunnerRow> = runners
                            .iter()
                            .filter(|r| r.id.is_none_or(|id| listed.insert((&slot.site, id))))
                            .map(|r| row_of(&slot.forge, r, config, machines, &joined))
                            .collect();
                        rows.sort_by(|a, b| a.name.cmp(&b.name));
                        out.rows.extend(rows);
                    }
                    Err(why) => {
                        facts.error = Some(why.clone());
                        out.rows.push(placeholder(slot, why, true));
                    }
                }
            }
        }
        out.forges.push(facts);
    }
    out
}

/// The status bar's line, `runners: 7/8 online, 2 busy, 3 queued`, from a report's parts (`--json`'s
/// `runner_forges` and `runners`).
///
/// Counts are of the runners the forges listed. A forge that failed adds `, 1 forge down`;
/// a queue that is not known is `?`; while a forge is still being asked, `runners: …`. When
/// no forge could list its runners it is `runners: unknown`. `None` when there is no forge
/// entry (nothing to say, as for Prometheus).
#[must_use]
pub fn summary_line(forges: &[ForgeFacts], rows: &[RunnerRow]) -> Option<String> {
    if forges.is_empty() {
        return None;
    }
    if forges.iter().any(|f| f.checking) {
        return Some("runners: …".to_owned());
    }
    let down = forges.iter().filter(|f| f.error.is_some()).count();
    if down == forges.len() {
        return Some("runners: unknown".to_owned());
    }
    let listed = rows.iter().filter(|r| r.name.is_some());
    let (mut total, mut online, mut busy) = (0_usize, 0_usize, 0_usize);
    for r in listed {
        total += 1;
        online += usize::from(r.status.online());
        busy += usize::from(r.status == RunnerStatus::Active);
    }
    let answering = forges.iter().filter(|f| f.error.is_none());
    let queued = answering
        .map(|f| f.queued)
        .try_fold(0_u64, |sum, q| q.map(|n| sum.saturating_add(n)));
    let mut line = format!("runners: {online}/{total} online, {busy} busy, ");
    match queued {
        Some(n) => {
            let _ = write!(line, "{n} queued");
        }
        None => line.push_str("? queued"),
    }
    if down > 0 {
        let _ = write!(line, ", {down} forge down");
    }
    Some(line)
}

impl RunnersReport {
    /// [`summary_line`] of this report.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        summary_line(&self.forges, &self.rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str, active: UnitActive) -> RunnerUnit {
        RunnerUnit {
            unit: name.to_owned(),
            active,
            sub: if is_up(active) { "running" } else { "dead" }.to_owned(),
        }
    }

    fn runner(name: &str, status: &str) -> ForgeRunner {
        ForgeRunner::new(name, status, &["docker".to_owned()], Some("v9.0.0"))
    }

    fn slot(forge: &str, runners: Vec<ForgeRunner>, queued: u64) -> ForgeSlot {
        ForgeSlot {
            forge: forge.to_owned(),
            scope: "admin".to_owned(),
            site: forge.to_owned(),
            answer: Some(ForgeAnswer {
                runners: Ok(runners),
                queued: Ok(queued),
            }),
        }
    }

    fn machine<'a>(name: &'a str, units: Option<&'a [RunnerUnit]>) -> MachineFacts<'a> {
        MachineFacts {
            name,
            units,
            memory: None,
            load: None,
        }
    }

    #[test]
    fn scopes() {
        assert_eq!(Scope::parse("admin"), Ok(Scope::Admin));
        assert_eq!(Scope::parse("user"), Ok(Scope::User));
        assert_eq!(Scope::parse("org:acme"), Ok(Scope::Org("acme".into())));
        assert_eq!(
            Scope::parse("repo:acme/fleet"),
            Ok(Scope::Repo {
                owner: "acme".into(),
                name: "fleet".into()
            })
        );
        for bad in [
            "",
            "Admin",
            "org:",
            "org:a/b",
            "repo:a",
            "repo:a/b/c",
            "repo:../x",
            "org:a b",
            "global",
        ] {
            assert!(Scope::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            Scope::parse("repo:o/n").unwrap().segments(),
            ["repos", "o", "n"]
        );
        assert_eq!(Scope::Org("x".into()).to_string(), "org:x");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4GiB"), Ok(4 << 30));
        assert_eq!(parse_size("512MiB"), Ok(512 << 20));
        assert_eq!(parse_size("2GB"), Ok(2_000_000_000));
        assert_eq!(parse_size("1TiB"), Ok(1 << 40));
        assert_eq!(parse_size("100B"), Ok(100));
        assert_eq!(parse_size("7kB"), Ok(7_000));
        for bad in [
            "",
            "4",
            "GiB",
            "4 GiB",
            "4gib",
            "4GIB",
            "1.5GiB",
            "-4GiB",
            "4Gi",
            "4G",
            "0GiB",
            "0B",
            "+4GiB",
            "4GiB ",
            "99999999999TiB",
            "4\u{1b}GiB",
        ] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn statuses() {
        assert_eq!(RunnerStatus::parse("idle"), RunnerStatus::Idle);
        assert_eq!(RunnerStatus::parse("active"), RunnerStatus::Active);
        assert_eq!(RunnerStatus::parse("offline"), RunnerStatus::Offline);
        assert_eq!(RunnerStatus::parse("paused"), RunnerStatus::Unknown);
        assert!(RunnerStatus::Idle.online() && RunnerStatus::Active.online());
        assert!(!RunnerStatus::Offline.online() && !RunnerStatus::Unknown.online());
    }

    #[test]
    fn a_runner_from_the_forge_is_clean_and_bounded() {
        let labels: Vec<String> = (0..40).map(|i| format!("l{i}\u{1b}[31m")).collect();
        let r = ForgeRunner::new("a\u{202e}b\n", "idle", &labels, Some(&"v".repeat(500)));
        assert_eq!(r.name, "ab");
        assert_eq!(r.labels.len(), MAX_LABELS);
        assert_eq!(r.labels[0], "l0");
        assert_eq!(r.version.unwrap().chars().count(), MAX_TEXT_CHARS + 1);
    }

    #[test]
    fn a_runner_joins_the_machine_its_name_equals_or_starts_with() {
        let cfg = RunnersConfig::default();
        let ms = [
            machine("alpha", None),
            machine("alpha-docker", None),
            machine("beta", None),
        ];
        assert_eq!(machine_of("alpha", &cfg, &ms), Some("alpha"));
        assert_eq!(machine_of("beta-2", &cfg, &ms), Some("beta"));
        // The longest machine wins.
        assert_eq!(
            machine_of("alpha-docker-1", &cfg, &ms),
            Some("alpha-docker")
        );
        // A prefix without the hyphen is not a match.
        assert_eq!(machine_of("betamax", &cfg, &ms), None);
        assert_eq!(machine_of("gamma", &cfg, &ms), None);
    }

    #[test]
    fn the_map_wins_and_must_name_a_known_machine() {
        let mut cfg = RunnersConfig::default();
        cfg.machines.insert("alpha-docker".into(), "beta".into());
        cfg.machines.insert("lonely".into(), "nowhere".into());
        let ms = [machine("alpha", None), machine("beta", None)];
        assert_eq!(machine_of("alpha-docker", &cfg, &ms), Some("beta"));
        // Mapped to a machine that is not in the fleet: no machine, and no fallback guess.
        assert_eq!(machine_of("lonely", &cfg, &ms), None);
    }

    #[test]
    fn instances_are_read_from_unit_names() {
        let g = "gitea-runner-*";
        assert_eq!(instance_of("gitea-runner-alpha.service", g), "alpha");
        assert_eq!(
            instance_of(r"gitea-runner-alpha\x2ddocker.service", g),
            "alpha-docker"
        );
        assert_eq!(instance_of("other.service", g), "other");
        assert_eq!(instance_of("ci-x.service", "ci-?"), "x");
    }

    #[test]
    fn rows_get_their_unit_and_the_unit_badges() {
        let cfg = RunnersConfig::default();
        let units = [
            unit("gitea-runner-alpha.service", UnitActive::Failed),
            unit(r"gitea-runner-beta\x2ddocker.service", UnitActive::Active),
        ];
        let ms = [
            machine("alpha", Some(&units[..1])),
            machine("beta", Some(&units[1..])),
        ];
        let rep = report(
            &[slot(
                "forge.example.org",
                vec![
                    runner("alpha", "idle"),
                    runner("beta-docker", "offline"),
                    runner("gamma", "active"),
                ],
                0,
            )],
            &ms,
            &cfg,
        );
        let by = |n: &str| {
            rep.rows
                .iter()
                .find(|r| r.name.as_deref() == Some(n))
                .unwrap()
        };
        // Online by the forge, failed on the machine.
        assert_eq!(by("alpha").badges, [RunnerBadge::ForgeUpUnitDown]);
        assert!(by("alpha").is_red());
        assert_eq!(
            by("alpha").unit.as_ref().unwrap().active,
            UnitActive::Failed
        );
        // Offline by the forge, running on the machine (the name is `<machine>-docker`, the
        // unit's instance is `beta-docker` with its hyphen escaped).
        assert_eq!(
            by("beta-docker").badges,
            [RunnerBadge::Offline, RunnerBadge::ForgeDownUnitUp]
        );
        // No machine, no unit, nothing to compare: busy and fine.
        assert!(by("gamma").badges.is_empty() && by("gamma").unit.is_none());
        assert_eq!(by("gamma").status, RunnerStatus::Active);
    }

    #[test]
    fn a_runner_matches_its_unit_without_the_machine_prefix() {
        let cfg = RunnersConfig::default();
        let units = [unit("gitea-runner-docker.service", UnitActive::Inactive)];
        let ms = [machine("alpha", Some(&units))];
        let rep = report(
            &[slot("f", vec![runner("alpha-docker", "idle")], 0)],
            &ms,
            &cfg,
        );
        assert_eq!(rep.rows[0].badges, [RunnerBadge::ForgeUpUnitDown]);
    }

    /// N6: every state the README names is pinned: a unit that is deactivating, in maintenance,
    /// failed or inactive is down under an online runner; one that is reloading or active is up
    /// under an offline one.
    /// N-B: another runner whose name merely joins the machine (`alpha-b`, registered by someone
    /// else on a forge that lists every runner it can see) does not switch the fallback off for the
    /// runner that is named for the machine itself.
    #[test]
    fn another_runners_name_does_not_switch_the_single_unit_fallback_off() {
        let cfg = RunnersConfig::default();
        let one = [unit("gitea-runner-default.service", UnitActive::Failed)];
        let ms = [machine("alpha", Some(&one))];
        let rep = report(
            &[slot(
                "f",
                vec![runner("alpha", "idle"), runner("alpha-b", "idle")],
                0,
            )],
            &ms,
            &cfg,
        );
        let by = |n: &str| {
            rep.rows
                .iter()
                .find(|r| r.name.as_deref() == Some(n))
                .unwrap()
        };
        assert_eq!(
            by("alpha").badges,
            [RunnerBadge::ForgeUpUnitDown],
            "{rep:?}"
        );
        assert!(by("alpha-b").unit.is_none() && by("alpha-b").badges.is_empty());
        // From another forge, as the same: the one named for the machine joins, the stranger not.
        let rep = report(
            &[
                slot("f", vec![runner("alpha-b", "idle")], 0),
                slot("g", vec![runner("alpha", "idle")], 0),
            ],
            &ms,
            &cfg,
        );
        let by = |n: &str| {
            rep.rows
                .iter()
                .find(|r| r.name.as_deref() == Some(n))
                .unwrap()
        };
        assert!(by("alpha").unit.is_some() && by("alpha-b").unit.is_none());
        // A runner that already matches the unit by its name owns it: the one named for the
        // machine does not take it too.
        let owned = [unit("gitea-runner-b.service", UnitActive::Failed)];
        let ms2 = [machine("alpha", Some(&owned))];
        let rep = report(
            &[slot(
                "f",
                vec![runner("alpha", "idle"), runner("alpha-b", "idle")],
                0,
            )],
            &ms2,
            &cfg,
        );
        let by = |n: &str| {
            rep.rows
                .iter()
                .find(|r| r.name.as_deref() == Some(n))
                .unwrap()
        };
        assert!(by("alpha").unit.is_none(), "{rep:?}");
        assert_eq!(by("alpha-b").badges, [RunnerBadge::ForgeUpUnitDown]);
    }

    /// N-C (R6): the count of runners on a machine is over every forge, not the first.
    /// R-B1: sanitise, check whole, then cut. A secret split by a character the sanitiser removes,
    /// or straddling the cut, withholds the whole text; one that is neither is cut as ever.
    #[test]
    fn a_secret_is_looked_for_after_the_sanitiser_and_before_the_cut() {
        let secret = "0123456789abcdef0123456789abcdef01234567";
        let holds = |t: &str| t.contains(secret);
        let make = |name: &str, label: &str, version: &str| {
            ForgeRunner::new_guarded(name, "idle", &[label.to_owned()], Some(version), &holds)
        };
        // Split by what the sanitiser takes out.
        let (head, tail) = secret.split_at(10);
        let split = format!("{head}\u{200b}{tail}");
        let r = make(
            &split,
            &format!("{head}\u{7}{tail}"),
            &format!("v{head}\u{1b}[0m{tail}"),
        );
        assert_eq!(
            (r.name.as_str(), r.labels.as_slice(), r.version.as_deref()),
            (WITHHELD, [WITHHELD.to_owned()].as_slice(), Some(WITHHELD))
        );
        // Straddling the cut: 39 of its 40 characters would have been shown.
        let straddle = format!("{}{secret}", "x".repeat(MAX_TEXT_CHARS - 39));
        let r = make(&straddle, &straddle, &straddle);
        assert_eq!(r.name, WITHHELD);
        assert_eq!(r.labels, [WITHHELD]);
        assert_eq!(r.version.as_deref(), Some(WITHHELD));
        // Past the cut altogether: not shown anyway, and still not shown.
        let late = format!("{}{secret}", "y".repeat(MAX_TEXT_CHARS + 20));
        assert_eq!(make(&late, "l", "v").name, WITHHELD);
        // Neither: cut, as ever.
        let r = make(&"z".repeat(150), "l", "v");
        assert_eq!(r.name, format!("{}…", "z".repeat(MAX_TEXT_CHARS)));
        // Without a secret to look for nothing is withheld.
        assert_eq!(
            ForgeRunner::new(&straddle, "idle", &[], None)
                .name
                .chars()
                .count(),
            MAX_TEXT_CHARS + 1
        );
    }

    /// N-A: two scopes of one forge list the same runner (`visible=true` makes `admin` list every
    /// runner of the instance): by the forge's own identifier it is one runner, listed once and
    /// counted once. The identifier is per forge; one the forge did not give merges nothing.
    #[test]
    fn a_runner_two_scopes_of_one_forge_both_list_is_one_runner() {
        let cfg = RunnersConfig::default();
        let on = |site: &str, forge: &str, runners: Vec<ForgeRunner>, queued: u64| ForgeSlot {
            site: site.to_owned(),
            forge: forge.to_owned(),
            ..slot(forge, runners, queued)
        };
        let alpha = || runner("alpha", "active").with_id(Some(1));
        let beta = || runner("beta", "idle").with_id(Some(2));
        let rep = report(
            &[
                on(
                    "forge.example.org",
                    "forge.example.org (admin)",
                    vec![alpha()],
                    3,
                ),
                on(
                    "forge.example.org",
                    "forge.example.org (repo:o/n)",
                    vec![alpha(), beta()],
                    3,
                ),
            ],
            &[],
            &cfg,
        );
        let names: Vec<(&str, &str)> = rep
            .rows
            .iter()
            .map(|r| (r.forge.as_str(), r.name.as_deref().unwrap()))
            .collect();
        // Once, under the first entry that has it; the second keeps what only it has.
        assert_eq!(
            names,
            [
                ("forge.example.org (admin)", "alpha"),
                ("forge.example.org (repo:o/n)", "beta")
            ]
        );
        assert_eq!(
            rep.summary().unwrap(),
            "runners: 2/2 online, 1 busy, 6 queued",
            "the runners once; the queues are each entry's own, as the README says"
        );
        // The same number on another forge is another runner.
        let rep = report(
            &[
                on("a.example.org", "a", vec![alpha()], 0),
                on("b.example.org", "b", vec![alpha()], 0),
            ],
            &[],
            &cfg,
        );
        assert_eq!(rep.rows.len(), 2);
        // No identifier, nothing merged: names are not unique, and two runners may share one.
        let rep = report(
            &[
                on("s", "x", vec![runner("alpha", "idle")], 0),
                on("s", "y", vec![runner("alpha", "idle")], 0),
            ],
            &[],
            &cfg,
        );
        assert_eq!(rep.rows.len(), 2);
        // Within one entry the forge's list is its own: it is not thinned.
        let rep = report(
            &[on(
                "s",
                "x",
                vec![alpha(), runner("alpha", "idle").with_id(Some(9))],
                0,
            )],
            &[],
            &cfg,
        );
        assert_eq!(rep.rows.len(), 2);
    }

    #[test]
    fn the_runners_of_every_forge_count_toward_a_machine() {
        let cfg = RunnersConfig::default();
        let one = [unit("gitea-runner-default.service", UnitActive::Failed)];
        let ms = [machine("alpha", Some(&one))];
        let rep = report(
            &[
                slot("f", vec![runner("alpha-a", "idle")], 0),
                slot("g", vec![runner("alpha-b", "idle")], 0),
            ],
            &ms,
            &cfg,
        );
        assert!(
            rep.rows
                .iter()
                .all(|r| r.unit.is_none() && r.badges.is_empty()),
            "two different runners for one machine on two forges: {rep:?}"
        );
    }

    /// N-C (R7): `[runners.machines]` places a runner for the count too: a mapped runner is the
    /// machine's only one (and joins its single unit), and a mapped one beside a name-joined one
    /// makes two.
    #[test]
    fn a_runner_placed_by_the_map_counts_toward_its_machine() {
        let mut cfg = RunnersConfig::default();
        cfg.machines.insert("ci-box".into(), "alpha".into());
        let one = [unit("gitea-runner-default.service", UnitActive::Failed)];
        let ms = [machine("alpha", Some(&one))];
        let rep = report(&[slot("f", vec![runner("ci-box", "idle")], 0)], &ms, &cfg);
        assert_eq!(rep.rows[0].machine.as_deref(), Some("alpha"));
        assert_eq!(
            rep.rows[0].badges,
            [RunnerBadge::ForgeUpUnitDown],
            "{rep:?}"
        );
        // Beside a name-joined runner there are two, and neither is named for the machine.
        let rep = report(
            &[slot(
                "f",
                vec![runner("ci-box", "idle"), runner("alpha-x", "idle")],
                0,
            )],
            &ms,
            &cfg,
        );
        assert!(
            rep.rows
                .iter()
                .all(|r| r.unit.is_none() && r.badges.is_empty()),
            "{rep:?}"
        );
    }

    #[test]
    fn each_unit_state_is_up_or_down_as_the_readme_says() {
        let cfg = RunnersConfig::default();
        let badges = |active: UnitActive, status: &str| {
            let units = [unit("gitea-runner-alpha.service", active)];
            let ms = [machine("alpha", Some(&units))];
            report(&[slot("f", vec![runner("alpha", status)], 0)], &ms, &cfg).rows[0]
                .badges
                .clone()
        };
        for down in [
            UnitActive::Inactive,
            UnitActive::Failed,
            UnitActive::Deactivating,
            UnitActive::Maintenance,
        ] {
            assert_eq!(
                badges(down, "idle"),
                [RunnerBadge::ForgeUpUnitDown],
                "{down:?}"
            );
            assert_eq!(
                badges(down, "active"),
                [RunnerBadge::ForgeUpUnitDown],
                "{down:?}"
            );
            assert_eq!(badges(down, "offline"), [RunnerBadge::Offline], "{down:?}");
        }
        for up in [UnitActive::Active, UnitActive::Reloading] {
            assert_eq!(badges(up, "idle"), Vec::<RunnerBadge>::new(), "{up:?}");
            assert_eq!(
                badges(up, "offline"),
                [RunnerBadge::Offline, RunnerBadge::ForgeDownUnitUp],
                "{up:?}"
            );
        }
    }

    /// N5: the common NixOS layout is one instance, `instances.default`, whose unit is
    /// `gitea-runner-default.service` and whose runner is named after the host. When a machine
    /// has exactly one runner unit and exactly one runner, they are the same thing.
    #[test]
    fn a_machine_with_one_unit_and_one_runner_joins_them_whatever_they_are_called() {
        let cfg = RunnersConfig::default();
        let one = [unit("gitea-runner-default.service", UnitActive::Failed)];
        let ms = [machine("alpha", Some(&one))];
        let rep = report(&[slot("f", vec![runner("alpha", "idle")], 0)], &ms, &cfg);
        assert_eq!(rep.rows[0].badges, [RunnerBadge::ForgeUpUnitDown]);
        assert_eq!(
            rep.rows[0].unit.as_ref().unwrap().unit,
            "gitea-runner-default.service"
        );
        // The same runner on two forges is one runner: both rows join.
        let rep = report(
            &[
                slot("f", vec![runner("alpha", "idle")], 0),
                slot("g", vec![runner("alpha", "idle")], 0),
            ],
            &ms,
            &cfg,
        );
        assert!(rep.rows.iter().all(|r| r.unit.is_some()), "{rep:?}");
        // Two runners, neither named for the machine: which unit is whose is not known, so none
        // is guessed.
        let rep = report(
            &[slot(
                "f",
                vec![runner("alpha-a", "idle"), runner("alpha-b", "idle")],
                0,
            )],
            &ms,
            &cfg,
        );
        assert!(
            rep.rows
                .iter()
                .all(|r| r.unit.is_none() && r.badges.is_empty()),
            "{rep:?}"
        );
        // Two units for one runner: likewise.
        let two = [
            unit("gitea-runner-default.service", UnitActive::Failed),
            unit("gitea-runner-other.service", UnitActive::Active),
        ];
        let ms2 = [machine("alpha", Some(&two))];
        let rep = report(&[slot("f", vec![runner("alpha", "idle")], 0)], &ms2, &cfg);
        assert!(rep.rows[0].unit.is_none() && rep.rows[0].badges.is_empty());
        // A name that matches wins over the fallback, and a runner on no machine has no unit.
        let named = [
            unit("gitea-runner-alpha.service", UnitActive::Active),
            unit("gitea-runner-default.service", UnitActive::Failed),
        ];
        let ms3 = [machine("alpha", Some(&named))];
        let rep = report(&[slot("f", vec![runner("alpha", "idle")], 0)], &ms3, &cfg);
        assert_eq!(
            rep.rows[0].unit.as_ref().unwrap().unit,
            "gitea-runner-alpha.service"
        );
        let rep = report(&[slot("f", vec![runner("gamma", "idle")], 0)], &ms, &cfg);
        assert!(rep.rows[0].unit.is_none());
        // A machine whose probe gave no units still has none to join.
        let ms4 = [machine("alpha", None)];
        let rep = report(&[slot("f", vec![runner("alpha", "idle")], 0)], &ms4, &cfg);
        assert!(rep.rows[0].unit.is_none());
    }

    #[test]
    fn unknown_units_and_in_between_states_are_not_called_down() {
        let cfg = RunnersConfig::default();
        // The probe did not report units: nothing to compare.
        let ms = [machine("alpha", None)];
        let rep = report(&[slot("f", vec![runner("alpha", "idle")], 0)], &ms, &cfg);
        assert!(rep.rows[0].badges.is_empty() && rep.rows[0].unit.is_none());
        // A unit that is starting (or waiting to restart) is neither up nor down.
        let units = [unit("gitea-runner-alpha.service", UnitActive::Activating)];
        let ms = [machine("alpha", Some(&units))];
        for status in ["idle", "offline"] {
            let rep = report(&[slot("f", vec![runner("alpha", status)], 0)], &ms, &cfg);
            assert!(
                !rep.rows[0].badges.contains(&RunnerBadge::ForgeUpUnitDown)
                    && !rep.rows[0].badges.contains(&RunnerBadge::ForgeDownUnitUp),
                "{status}"
            );
        }
        // An unknown forge status is not offline.
        let rep = report(&[slot("f", vec![runner("alpha", "paused")], 0)], &ms, &cfg);
        assert_eq!(rep.rows[0].badges, Vec::<RunnerBadge>::new());
    }

    #[test]
    fn the_threshold_itself_is_badged() {
        let mut cfg = RunnersConfig::default();
        cfg.hosts.insert(
            "alpha".into(),
            HostCaps {
                cpus: None,
                memory_bytes: Some(10),
            },
        );
        let at = |used: u64| MachineFacts {
            memory: Some(Memory {
                total_bytes: 100,
                available_bytes: 100 - used,
            }),
            ..machine("alpha", None)
        };
        let ask = |used| {
            report(
                &[slot("f", vec![runner("alpha", "idle")], 0)],
                &[at(used)],
                &cfg,
            )
        };
        // 8 of 10 is 80, 9 of 10 is exactly 90: the configured percent is itself a warning.
        assert_eq!(ask(8).rows[0].badges, Vec::<RunnerBadge>::new());
        assert_eq!(ask(9).rows[0].memory.unwrap().percent, 90);
        assert_eq!(ask(9).rows[0].badges, [RunnerBadge::MemoryNearCap]);
    }

    #[test]
    fn memory_is_against_the_configured_cap_only() {
        let mut cfg = RunnersConfig::default();
        let gib = 1024_u64 * 1024 * 1024;
        let mem = |used: u64| Memory {
            total_bytes: 16 * gib,
            available_bytes: 16 * gib - used,
        };
        let with = |used: u64| MachineFacts {
            memory: Some(mem(used)),
            load: Some(Load {
                one: 1.5,
                five: 0.0,
                fifteen: 0.0,
            }),
            ..machine("alpha", None)
        };
        let ask = |cfg: &RunnersConfig, used: u64| {
            report(
                &[slot("f", vec![runner("alpha", "idle")], 0)],
                &[with(used)],
                cfg,
            )
        };
        // No cap configured: no figure, no badge, however full the host is.
        let r = ask(&cfg, 15 * gib);
        assert!(r.rows[0].memory.is_none() && r.rows[0].load.is_none());
        assert_eq!(r.rows[0].badges, Vec::<RunnerBadge>::new());

        cfg.hosts.insert(
            "alpha".into(),
            HostCaps {
                cpus: Some(2),
                memory_bytes: Some(4 * gib),
            },
        );
        let r = ask(&cfg, 3 * gib);
        assert_eq!(r.rows[0].memory.unwrap().percent, 75);
        assert_eq!(r.rows[0].load.unwrap().cpus, 2);
        assert_eq!(r.rows[0].badges, Vec::<RunnerBadge>::new());
        // 90% of 4 GiB is 3.6 GiB: just under, then at.
        assert_eq!(
            ask(&cfg, 3 * gib + 600 * 1024 * 1024).rows[0].badges,
            Vec::<RunnerBadge>::new()
        );
        let near = ask(&cfg, 3 * gib + 700 * 1024 * 1024);
        assert_eq!(near.rows[0].badges, [RunnerBadge::MemoryNearCap]);
        assert!(!near.rows[0].is_red(), "near the cap is a warning, not red");
        // The threshold is the configured one.
        cfg.memory_warn_percent = 50;
        assert_eq!(
            ask(&cfg, 3 * gib).rows[0].badges,
            [RunnerBadge::MemoryNearCap]
        );
        // A host with more memory in use than the cap says so rather than wrapping.
        assert_eq!(ask(&cfg, 8 * gib).rows[0].memory.unwrap().percent, 200);
        // A zero cap is no cap.
        cfg.hosts.insert(
            "alpha".into(),
            HostCaps {
                cpus: None,
                memory_bytes: Some(0),
            },
        );
        assert!(ask(&cfg, gib).rows[0].memory.is_none());
    }

    #[test]
    fn a_failing_forge_has_its_own_row_and_leaves_the_others() {
        let cfg = RunnersConfig::default();
        let failing = ForgeSlot {
            forge: "beta.example.org".into(),
            scope: "admin".into(),
            site: String::new(),
            answer: Some(ForgeAnswer {
                runners: Err("forge: the forge refused the token (HTTP 403)".into()),
                queued: Err("forge: the forge refused the token (HTTP 403)".into()),
            }),
        };
        let pending = ForgeSlot {
            forge: "gamma.example.org".into(),
            scope: "user".into(),
            site: String::new(),
            answer: None,
        };
        let rep = report(
            &[
                slot("forge.example.org", vec![runner("a", "idle")], 1),
                failing,
                pending,
            ],
            &[],
            &cfg,
        );
        assert_eq!(rep.rows.len(), 3);
        assert_eq!(rep.rows[0].name.as_deref(), Some("a"));
        assert_eq!(rep.rows[1].name, None);
        assert_eq!(rep.rows[1].badges, [RunnerBadge::ForgeError]);
        assert!(rep.rows[1].error.as_deref().unwrap().contains("403"));
        // Still being asked is not an error and not red.
        assert_eq!(rep.rows[2].error.as_deref(), Some(CHECKING));
        assert_eq!(rep.rows[2].badges, Vec::<RunnerBadge>::new());
        assert!(rep.forges[2].checking && !rep.forges[1].checking);
    }

    #[test]
    fn a_queue_that_cannot_be_read_does_not_hide_the_runners() {
        let cfg = RunnersConfig::default();
        let s = ForgeSlot {
            forge: "f".into(),
            scope: "admin".into(),
            site: String::new(),
            answer: Some(ForgeAnswer {
                runners: Ok(vec![runner("a", "idle")]),
                queued: Err("no".into()),
            }),
        };
        let rep = report(&[s], &[], &cfg);
        assert_eq!(rep.rows.len(), 1);
        assert_eq!(rep.forges[0].queued, None);
        assert_eq!(rep.forges[0].queue_error.as_deref(), Some("no"));
        assert_eq!(
            rep.summary().unwrap(),
            "runners: 1/1 online, 0 busy, ? queued"
        );
    }

    #[test]
    fn the_summary_line() {
        let cfg = RunnersConfig::default();
        let mut runners = vec![
            runner("a", "idle"),
            runner("b", "active"),
            runner("c", "active"),
            runner("d", "idle"),
            runner("e", "idle"),
            runner("f", "idle"),
            runner("g", "idle"),
            runner("h", "offline"),
        ];
        let rep = report(&[slot("f", runners.clone(), 3)], &[], &cfg);
        assert_eq!(
            rep.summary().unwrap(),
            "runners: 7/8 online, 2 busy, 3 queued"
        );
        // Two forges add up.
        let rep = report(
            &[slot("f", runners.split_off(4), 1), slot("g", runners, 2)],
            &[],
            &cfg,
        );
        assert_eq!(
            rep.summary().unwrap(),
            "runners: 7/8 online, 2 busy, 3 queued"
        );
        // One forge down is said, and its rows are not counted.
        let down = ForgeSlot {
            forge: "x".into(),
            scope: "admin".into(),
            site: String::new(),
            answer: Some(ForgeAnswer {
                runners: Err("e".into()),
                queued: Err("e".into()),
            }),
        };
        let rep = report(
            &[slot("f", vec![runner("a", "idle")], 0), down.clone()],
            &[],
            &cfg,
        );
        assert_eq!(
            rep.summary().unwrap(),
            "runners: 1/1 online, 0 busy, 0 queued, 1 forge down"
        );
        // Every forge down: no counts at all.
        assert_eq!(
            report(&[down], &[], &cfg).summary().unwrap(),
            "runners: unknown"
        );
        // Still asking.
        let ask = ForgeSlot {
            forge: "x".into(),
            scope: "admin".into(),
            site: String::new(),
            answer: None,
        };
        assert_eq!(report(&[ask], &[], &cfg).summary().unwrap(), "runners: …");
        // Not configured: nothing to say.
        assert_eq!(report(&[], &[], &cfg).summary(), None);
    }

    #[test]
    fn the_json_shape() {
        let cfg = RunnersConfig::default();
        let rep = report(&[slot("f", vec![runner("a", "active")], 0)], &[], &cfg);
        let v = serde_json::to_value(&rep.rows).unwrap();
        assert_eq!(
            v[0],
            serde_json::json!({
                "forge": "f", "name": "a", "status": "active", "labels": ["docker"],
                "version": "v9.0.0", "machine": null, "unit": null, "memory": null,
                "load": null, "badges": [], "error": null
            })
        );
    }
}
