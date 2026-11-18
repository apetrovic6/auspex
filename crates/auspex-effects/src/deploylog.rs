//! The local log of everything auspex does to a machine: one SQLite database, one row per
//! action (deploy, build, reboot, rollback, freeze, unfreeze).
//!
//! **The schema is a published contract.** The weekly homelab digest reads the file directly,
//! so the table, its columns and their value sets are documented in this crate's README
//! ("The action log") and pinned by a test (`the_schema_is_pinned`). Changing it means
//! changing that test, which is the point.
//!
//! # Use
//!
//! [`ActionLog::open`] once per run, then [`ActionLog::start`] when an action begins and
//! [`ActionLog::finish`] when it ends. [`ActionLog::recent`] and [`ActionLog::since`] read
//! it back for the timeline. Nothing calls this yet: the core and the shell wire it in with
//! the actions.
//!
//! The caller passes **summaries, never output**: `detail` is the last few sanitised lines
//! of an error (at most [`DETAIL_CAP`] bytes), not clan's log. Clan's output can contain
//! whatever the machine's activation scripts print, and a log other programs read directly
//! is the wrong place for it.
//!
//! # Where it lives
//!
//! [`default_path`]: `$XDG_STATE_HOME/auspex/log.sqlite`, else
//! `~/.local/state/auspex/log.sqlite` (a relative `XDG_STATE_HOME` is ignored). The caller
//! may pass any other path to [`OpenParams`]; tests always do. The directory holding the
//! file must be private (0700, owned by this user, not a symlink: the same rule as the other
//! directories this crate creates) and is created so when missing. The file is created 0600
//! and must not be a symlink; SQLite gives its `-wal` and `-shm` files the same mode. With
//! [`OpenParams::fleet`] set, a path inside the fleet repository is refused.
//!
//! # Interrupted actions
//!
//! A row that was started and never finished means auspex crashed or was killed. The queries
//! report such a row as [`ActionResult::Interrupted`] (with no `ended_at`) **when no auspex
//! process can still be writing it**: the row's `host` is this host, and the process that
//! started it, recorded as its pid and its start time (`/proc/<pid>/stat` field 22 together
//! with the boot id, so a recycled pid or a reboot is not mistaken for the writer), is gone
//! or a zombie. A row from another host, or whose writer is alive, is left as running
//! (`result` is `None`). Nothing is written back: this is a judgement made at read time, so
//! a live process's row is never touched, and [`ActionLog::finish`] itself only completes a
//! row that this process started. Without `/proc` (not Linux) the start time is not
//! recorded and a bare `kill(pid, 0)` decides; that can be fooled by a recycled pid, which is
//! why Linux is the target.
//!
//! # Concurrency
//!
//! The TUI and a `--json` run may write at once, from two processes. The database is in WAL
//! mode (readers never block writers, and a writer waits only for another writer), every
//! statement is one short transaction, and a busy timeout ([`OpenParams::busy_timeout`],
//! default 5 s) makes a writer wait instead of failing with `database is locked`. Every call
//! on [`ActionLog`] is async and runs its SQLite work on tokio's blocking pool, so nothing
//! here blocks the runtime, and so the TUI. A call that is dropped before it completes still
//! finishes its write once submitted: await [`ActionLog::start`] to the end (do not race it
//! against a cancellation), or the row may exist without the caller knowing its id.
//!
//! Unlike the rest of this crate these calls take no timeout and no `CancellationToken`: each
//! is one statement, bounded by the busy timeout, and there is no process to kill.

use std::ffi::OsStr;
use std::fmt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use rustix::fs::{Mode, OFlags};

/// The schema version this build writes and reads (`PRAGMA user_version`).
pub const SCHEMA_VERSION: i64 = 1;

/// The longest `detail` stored, in bytes. Longer text is cut (on a character boundary) and
/// ends in `…`. The table's `CHECK` enforces it too.
pub const DETAIL_CAP: usize = 4096;

/// The longest `machine`, `who` and `host` stored, in bytes. Cut the same way.
pub const NAME_CAP: usize = 255;

/// The most rows [`ActionLog::recent`] returns, whatever the caller asks for.
pub const RECENT_CAP: usize = 1000;

/// The file name inside the state directory.
pub const FILE_NAME: &str = "log.sqlite";

/// The statements that create version 1, run in one transaction on an empty database. The
/// README documents each column; `the_schema_is_pinned` pins this text.
///
/// Why these indexes: `actions_machine_started` serves "the last N actions of machine M"
/// (`WHERE machine = ? ORDER BY started_at DESC, id DESC LIMIT N`) without a sort, and
/// `actions_started` serves "everything in the last 7 days" (`WHERE started_at >= ?`) with a
/// range scan. Times are fixed-width UTC text, so text order is time order. The table is
/// small (a handful of rows a day) and these two cover both questions; nothing else is
/// queried by anything but a person.
const SCHEMA_V1: &str = "\
CREATE TABLE actions (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('deploy', 'build', 'reboot', 'rollback', 'freeze', 'unfreeze')),
    machine TEXT NOT NULL CHECK (machine <> '' AND length(CAST(machine AS BLOB)) <= 255),
    fleet_commit TEXT CHECK (fleet_commit IS NULL OR (length(fleet_commit) BETWEEN 4 AND 64 AND fleet_commit NOT GLOB '*[^0-9a-f]*')),
    fleet_dirty INTEGER CHECK (fleet_dirty IS NULL OR fleet_dirty IN (0, 1)),
    started_at TEXT NOT NULL CHECK (started_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9]Z'),
    ended_at TEXT CHECK (ended_at IS NULL OR ended_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9]Z'),
    result TEXT CHECK (result IS NULL OR result IN ('succeeded', 'failed', 'cancelled', 'timed_out', 'interrupted')),
    exit_code INTEGER,
    who TEXT NOT NULL CHECK (who <> ''),
    host TEXT NOT NULL CHECK (host <> ''),
    detail TEXT CHECK (detail IS NULL OR length(CAST(detail AS BLOB)) <= 4096),
    auspex_version TEXT NOT NULL,
    pid INTEGER NOT NULL,
    pid_start TEXT,
    CHECK ((ended_at IS NULL) = (result IS NULL)),
    CHECK (exit_code IS NULL OR ended_at IS NOT NULL)
);
CREATE INDEX actions_machine_started ON actions (machine, started_at DESC, id DESC);
CREATE INDEX actions_started ON actions (started_at, id);
";

/// The columns every query selects, in the order [`row_to_stored`] reads them.
const COLUMNS: &str = "id, kind, machine, fleet_commit, fleet_dirty, started_at, ended_at, \
                       result, exit_code, who, host, detail, auspex_version, pid, pid_start";

// ---------------------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------------------

/// What an action does. The stored text is [`ActionKind::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// `clan machines update`.
    Deploy,
    /// Build only.
    Build,
    /// Reboot.
    Reboot,
    /// Roll back to the previous generation.
    Rollback,
    /// Freeze: `U` skips the machine.
    Freeze,
    /// Unfreeze.
    Unfreeze,
}

impl ActionKind {
    /// The stored text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deploy => "deploy",
            Self::Build => "build",
            Self::Reboot => "reboot",
            Self::Rollback => "rollback",
            Self::Freeze => "freeze",
            Self::Unfreeze => "unfreeze",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "deploy" => Self::Deploy,
            "build" => Self::Build,
            "reboot" => Self::Reboot,
            "rollback" => Self::Rollback,
            "freeze" => Self::Freeze,
            "unfreeze" => Self::Unfreeze,
            _ => return None,
        })
    }
}

/// How an action ended. The stored text is [`ActionResult::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionResult {
    /// It did what it was asked.
    Succeeded,
    /// It ran and failed.
    Failed,
    /// The operator cancelled it.
    Cancelled,
    /// It was stopped because it took too long.
    TimedOut,
    /// auspex stopped before it knew (a handled signal), or the queries found no process
    /// that could still finish it.
    Interrupted,
}

impl ActionResult {
    /// The stored text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "timed_out" => Self::TimedOut,
            "interrupted" => Self::Interrupted,
            _ => return None,
        })
    }
}

/// A moment, in whole milliseconds since the Unix epoch, stored as UTC RFC 3339 text with
/// milliseconds (`2026-11-18T09:30:00.123Z`). Fixed-width, so text order is time order.
///
/// Limited to the years 0000 to 9999 (the text is fixed-width); a value outside is clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

/// 9999-12-31T23:59:59.999Z.
const MAX_MILLIS: i64 = 253_402_300_799_999;
const DAY_MILLIS: i64 = 86_400_000;

impl Timestamp {
    /// Now, from the system clock (clamped to the representable range; a clock before 1970
    /// reads as 1970).
    #[must_use]
    pub fn now() -> Self {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(MAX_MILLIS));
        Self::from_unix_millis(ms)
    }

    /// From milliseconds since the epoch, clamped to the years 0000 to 9999.
    #[must_use]
    pub fn from_unix_millis(ms: i64) -> Self {
        Self(ms.clamp(0, MAX_MILLIS))
    }

    /// Milliseconds since the epoch.
    #[must_use]
    pub const fn unix_millis(self) -> i64 {
        self.0
    }

    /// This moment, `days` earlier (never before the epoch).
    #[must_use]
    pub fn minus_days(self, days: i64) -> Self {
        Self::from_unix_millis(self.0.saturating_sub(days.saturating_mul(DAY_MILLIS)))
    }

    /// Parses exactly the stored form, `YYYY-MM-DDTHH:MM:SS.mmmZ`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text.len() != 24 || !text.is_ascii() {
            return None;
        }
        let num = |a: usize, z: usize| -> Option<i64> {
            let digits = &text[a..z];
            digits
                .bytes()
                .all(|c| c.is_ascii_digit())
                .then(|| digits.parse().ok())?
        };
        let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
        let (hour, minute, second, milli) =
            (num(11, 13)?, num(14, 16)?, num(17, 19)?, num(20, 23)?);
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return None;
        }
        let days = days_from_civil(year, month, day);
        let t = Self(days * DAY_MILLIS + ((hour * 60 + minute) * 60 + second) * 1000 + milli);
        // Rejects what the ranges above let through (February 30th, a year-0 oddity).
        (t.to_string() == text).then_some(t)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.0.div_euclid(DAY_MILLIS);
        let rest = self.0.rem_euclid(DAY_MILLIS);
        let (y, m, d) = civil_from_days(days);
        let (secs, ms) = (rest / 1000, rest % 1000);
        write!(
            f,
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        )
    }
}

// Howard Hinnant's `civil_from_days` and `days_from_civil`: proleptic Gregorian, days since
// 1970-01-01. Spelled out because a date crate for two functions is not worth the graph.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The id of a row, as [`ActionLog::start`] returned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActionId(i64);

impl ActionId {
    /// The row's `id`.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// What is being done, as the caller knows it when it begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAction {
    /// Deploy, build, …
    pub kind: ActionKind,
    /// The machine's name. Comes from the inventory: sanitised and capped on the way in.
    pub machine: String,
    /// The fleet repository's commit (lowercase hex, 4 to 64 characters), when known.
    pub fleet_commit: Option<String>,
    /// Whether the fleet tree had uncommitted changes, when known.
    pub fleet_dirty: Option<bool>,
}

/// How an action ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The result.
    pub result: ActionResult,
    /// The process's exit code, when there was one.
    pub exit_code: Option<i32>,
    /// A short summary, never the process's output: sanitised by the domain's rules (its last
    /// lines, control characters removed) and capped at [`DETAIL_CAP`] bytes.
    pub detail: Option<String>,
}

/// Who is acting, from where, with which auspex. Fixed for the life of an [`ActionLog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The operator: `$USER` (else `$LOGNAME`, else `uid:<n>`). Whoever can write to the
    /// file can write any name here, so it records, it does not authenticate.
    pub who: String,
    /// This machine's host name; decides which rows this process may judge interrupted.
    pub host: String,
    /// The version of auspex (the binary's, not this crate's), for the digest to correlate.
    pub auspex_version: String,
}

impl Identity {
    /// The current user and host.
    #[must_use]
    pub fn current(auspex_version: &str) -> Self {
        let who = ["USER", "LOGNAME"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .map(|v| clean_name(&v))
            .find(|v| !v.is_empty())
            .unwrap_or_else(|| format!("uid:{}", rustix::process::getuid().as_raw()));
        let host = clean_name(&rustix::system::uname().nodename().to_string_lossy());
        Self {
            who,
            host: if host.is_empty() {
                "unknown".to_owned()
            } else {
                host
            },
            auspex_version: auspex_version.to_owned(),
        }
    }
}

/// One row, as the queries return it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// The row id.
    pub id: ActionId,
    /// What was done.
    pub kind: ActionKind,
    /// To which machine.
    pub machine: String,
    /// The fleet commit, when it was known.
    pub fleet_commit: Option<String>,
    /// Whether the fleet tree was dirty, when it was known.
    pub fleet_dirty: Option<bool>,
    /// When it began.
    pub started_at: Timestamp,
    /// When it ended; `None` while running, and for an interrupted row nobody finished.
    pub ended_at: Option<Timestamp>,
    /// How it ended; `None` while a live process is still running it. A row nobody can
    /// finish any more reads as `Some(Interrupted)` (see the module documentation).
    pub result: Option<ActionResult>,
    /// The exit code, when there was one.
    pub exit_code: Option<i32>,
    /// The operator.
    pub who: String,
    /// The host it ran on.
    pub host: String,
    /// A short summary of the outcome.
    pub detail: Option<String>,
    /// The auspex version that wrote the row.
    pub auspex_version: String,
}

/// Why the log could not be used.
#[derive(Debug)]
pub enum Error {
    /// The directory or file is not one we may use (a symlink, someone else's, too open,
    /// inside the fleet). The text names it and says why.
    Unusable(String),
    /// The file was written by a newer auspex. It was not changed.
    NewerSchema {
        /// The file.
        path: PathBuf,
        /// The `user_version` it holds.
        found: i64,
    },
    /// The file is a database, but not this one (a non-zero `user_version` below ours, or
    /// tables with `user_version` 0). It was not changed.
    NotOurs(PathBuf),
    /// A value the caller passed cannot be stored (an empty machine name, a commit that is
    /// not hex).
    Invalid(&'static str),
    /// [`ActionLog::finish`] found no running row of this process with that id: it was
    /// finished already, or another process started it.
    NotRunning(ActionId),
    /// A row in the file is not valid (edited by hand, or a bug).
    Corrupt(String),
    /// SQLite said no.
    Sqlite(rusqlite::Error),
    /// The blocking task did not complete (the runtime is shutting down).
    Worker(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unusable(m) => f.write_str(m),
            Self::NewerSchema { path, found } => write!(
                f,
                "the action log {} has schema version {found}, newer than the {SCHEMA_VERSION} \
                 this auspex understands; refusing to touch it (update auspex)",
                path.display()
            ),
            Self::NotOurs(path) => write!(
                f,
                "{} is a database, but not an auspex action log; refusing to touch it",
                path.display()
            ),
            Self::Invalid(what) => write!(f, "cannot record: {what}"),
            Self::NotRunning(id) => write!(
                f,
                "action {} is not running here (finished already, or started by another process)",
                id.0
            ),
            Self::Corrupt(m) => write!(f, "the action log has an invalid row: {m}"),
            Self::Sqlite(e) => write!(f, "the action log: {e}"),
            Self::Worker(m) => write!(f, "the action log's worker stopped: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------------------

/// Where the log lives by default.
///
/// `$XDG_STATE_HOME/auspex/log.sqlite`, else `~/.local/state/auspex/log.sqlite`; `None` when
/// neither `XDG_STATE_HOME` nor `HOME` is an absolute path. The same state directory as the GC
/// roots. Never inside the fleet.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    default_path_from(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn default_path_from(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    Some(crate::clan::state_dir_from(xdg_state_home, home)?.join(FILE_NAME))
}

/// How to open the log.
#[derive(Debug, Clone)]
pub struct OpenParams {
    /// The database file. [`default_path`] for the real one; tests pass a path of their own.
    pub path: PathBuf,
    /// Who is writing.
    pub identity: Identity,
    /// The fleet repository: a log inside it is refused (the log is not the fleet's, and
    /// `git add -A` would commit it).
    pub fleet: Option<PathBuf>,
    /// How long a writer waits for another writer before failing. Default 5 s.
    pub busy_timeout: Duration,
}

impl OpenParams {
    /// `path`, written by `identity`, with the default busy timeout and no fleet check.
    #[must_use]
    pub const fn new(path: PathBuf, identity: Identity) -> Self {
        Self {
            path,
            identity,
            fleet: None,
            busy_timeout: Duration::from_secs(5),
        }
    }
}

/// The action log, open. Cheap to clone; clones share one connection.
#[derive(Clone)]
pub struct ActionLog {
    inner: Arc<Inner>,
}

impl fmt::Debug for ActionLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionLog")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

struct Inner {
    conn: Mutex<Connection>,
    path: PathBuf,
    identity: Identity,
    pid: i64,
    pid_start: Option<String>,
}

impl ActionLog {
    /// Opens the log, creating it (directory 0700, file 0600, schema version 1) when it does
    /// not exist, and refusing when it is not ours to use (see [`Error`]). A file written by
    /// a newer auspex is refused before anything in it is changed.
    ///
    /// # Errors
    ///
    /// See [`Error`].
    pub async fn open(params: OpenParams) -> Result<Self> {
        let inner = tokio::task::spawn_blocking(move || Inner::open(params))
            .await
            .map_err(|e| Error::Worker(e.to_string()))??;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// The database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Records that an action began, now, and returns its id. The row has no end and no
    /// result until [`ActionLog::finish`].
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a machine that is empty once sanitised or a commit that is not
    /// hex; [`Error::Sqlite`] otherwise.
    pub async fn start(&self, action: NewAction) -> Result<ActionId> {
        self.blocking(move |i| i.start(&action, Timestamp::now()))
            .await
    }

    /// Records how the action `id` (started by this process) ended, now.
    ///
    /// # Errors
    ///
    /// [`Error::NotRunning`] when there is no unfinished row of this process with that id.
    pub async fn finish(&self, id: ActionId, outcome: Outcome) -> Result<()> {
        self.blocking(move |i| i.finish(id, &outcome, Timestamp::now()))
            .await
    }

    /// The last `limit` actions of `machine` (at most [`RECENT_CAP`]), newest first. The
    /// name is sanitised and capped the way it is on the way in, so the same text finds it.
    ///
    /// # Errors
    ///
    /// [`Error::Sqlite`], or [`Error::Corrupt`] for a row that violates the schema.
    pub async fn recent(&self, machine: &str, limit: usize) -> Result<Vec<Action>> {
        let machine = clean_machine(machine);
        let limit = limit.min(RECENT_CAP);
        self.blocking(move |i| i.recent(&machine, limit)).await
    }

    /// Every action that began at or after `time`, oldest first.
    ///
    /// # Errors
    ///
    /// As [`ActionLog::recent`].
    pub async fn since(&self, time: Timestamp) -> Result<Vec<Action>> {
        self.blocking(move |i| i.since(time)).await
    }

    /// Runs `f` on the blocking pool. The one place any database work is started from, so
    /// that none of it can run on the async runtime.
    async fn blocking<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Inner) -> Result<T> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || f(&inner))
            .await
            .map_err(|e| Error::Worker(e.to_string()))?
    }
}

impl Inner {
    fn open(params: OpenParams) -> Result<Self> {
        Self::open_guarded(params, default_path().as_deref())
    }

    /// [`Inner::open`], with the real default location passed in so that a test can show the
    /// guard is wired in without opening it.
    #[cfg_attr(not(test), allow(unused_variables))]
    fn open_guarded(params: OpenParams, default: Option<&Path>) -> Result<Self> {
        #[cfg(test)]
        forbid_default_location_in_tests(&params.path, default);

        let OpenParams {
            path,
            mut identity,
            fleet,
            busy_timeout,
        } = params;
        identity.who = clean_name(&identity.who);
        identity.host = clean_name(&identity.host);
        if identity.who.is_empty() || identity.host.is_empty() {
            return Err(Error::Invalid("who and host must not be empty"));
        }

        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| Error::Unusable(format!("{} has no directory", path.display())))?;
        if path.file_name().is_none() {
            return Err(Error::Unusable(format!("{} names no file", path.display())));
        }
        prepare_dir(dir)?;
        if let Some(fleet) = fleet {
            refuse_inside(dir, &fleet, &path)?;
        }
        prepare_file(&path)?;

        // No URI parsing (a `?` in the path is a character), no create (prepare_file made
        // it), and SQLite itself refuses a symlink in the last component too.
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.busy_timeout(busy_timeout)?;
        init_schema_retrying(&conn, &path, busy_timeout)?;

        let pid = i64::from(std::process::id());
        Ok(Self {
            conn: Mutex::new(conn),
            path,
            identity,
            pid,
            pid_start: process_stamp(std::process::id()).map(|(_, stamp)| stamp),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic while the lock was held leaves a usable connection (SQLite rolled the
        // statement back); refusing every later call would turn one bug into all of them.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start(&self, a: &NewAction, at: Timestamp) -> Result<ActionId> {
        let machine = clean_machine(&a.machine);
        if machine.is_empty() {
            return Err(Error::Invalid("the machine name is empty"));
        }
        let commit = match &a.fleet_commit {
            None => None,
            Some(c) if (4..=64).contains(&c.len()) && c.bytes().all(is_lower_hex) => {
                Some(c.as_str())
            }
            Some(_) => return Err(Error::Invalid("the fleet commit is not 4 to 64 hex digits")),
        };
        let conn = self.lock();
        conn.prepare(
            "INSERT INTO actions (kind, machine, fleet_commit, fleet_dirty, started_at, \
             who, host, auspex_version, pid, pid_start) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?
        .execute(params![
            a.kind.as_str(),
            machine,
            commit,
            a.fleet_dirty,
            at.to_string(),
            self.identity.who,
            self.identity.host,
            self.identity.auspex_version,
            self.pid,
            self.pid_start,
        ])?;
        Ok(ActionId(conn.last_insert_rowid()))
    }

    fn finish(&self, id: ActionId, o: &Outcome, at: Timestamp) -> Result<()> {
        let detail = o.detail.as_deref().and_then(clean_detail);
        let conn = self.lock();
        // Only a row this process started and nobody has finished: another process's row is
        // not ours to complete, whatever its id.
        let changed = conn
            .prepare(
                "UPDATE actions SET ended_at = ?1, result = ?2, exit_code = ?3, detail = ?4 \
                 WHERE id = ?5 AND ended_at IS NULL AND pid = ?6 AND pid_start IS ?7 \
                 AND host = ?8",
            )?
            .execute(params![
                at.to_string(),
                o.result.as_str(),
                o.exit_code,
                detail,
                id.0,
                self.pid,
                self.pid_start,
                self.identity.host,
            ])?;
        drop(conn);
        if changed == 0 {
            return Err(Error::NotRunning(id));
        }
        Ok(())
    }

    fn recent(&self, machine: &str, limit: usize) -> Result<Vec<Action>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM actions WHERE machine = ?1 \
             ORDER BY started_at DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(params![machine, limit], row_to_stored)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        drop(conn);
        rows.into_iter().map(|r| Ok(self.judge(r?))).collect()
    }

    fn since(&self, time: Timestamp) -> Result<Vec<Action>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM actions WHERE started_at >= ?1 \
             ORDER BY started_at ASC, id ASC"
        ))?;
        let rows = stmt
            .query_map(params![time.to_string()], row_to_stored)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        drop(conn);
        rows.into_iter().map(|r| Ok(self.judge(r?))).collect()
    }

    /// Turns a stored row into an [`Action`], reading an unfinished row whose writer is gone
    /// as interrupted. Runs after the statement is done and the lock released: the process
    /// checks read `/proc`, and no database lock should wait for that.
    fn judge(&self, s: Stored) -> Action {
        let mut action = s.action;
        if action.result.is_none()
            && action.host == self.identity.host
            && !writer_alive(s.pid, s.pid_start.as_deref())
        {
            action.result = Some(ActionResult::Interrupted);
        }
        action
    }
}

/// A row, with the two columns only the interrupted check uses.
struct Stored {
    action: Action,
    pid: i64,
    pid_start: Option<String>,
}

fn row_to_stored(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Stored>> {
    let corrupt = |what: &str, v: &str| Error::Corrupt(format!("{what} {v:?}"));
    let id: i64 = row.get(0)?;
    let kind: String = row.get(1)?;
    let started: String = row.get(5)?;
    let ended: Option<String> = row.get(6)?;
    let result: Option<String> = row.get(7)?;
    let parsed = (|| {
        Ok(Stored {
            action: Action {
                id: ActionId(id),
                kind: ActionKind::parse(&kind).ok_or_else(|| corrupt("kind", &kind))?,
                machine: row.get(2)?,
                fleet_commit: row.get(3)?,
                fleet_dirty: row.get::<_, Option<i64>>(4)?.map(|v| v != 0),
                started_at: Timestamp::parse(&started)
                    .ok_or_else(|| corrupt("started_at", &started))?,
                ended_at: ended
                    .as_deref()
                    .map(|e| Timestamp::parse(e).ok_or_else(|| corrupt("ended_at", e)))
                    .transpose()?,
                result: result
                    .as_deref()
                    .map(|r| ActionResult::parse(r).ok_or_else(|| corrupt("result", r)))
                    .transpose()?,
                exit_code: row.get(8)?,
                who: row.get(9)?,
                host: row.get(10)?,
                detail: row.get(11)?,
                auspex_version: row.get(12)?,
            },
            pid: row.get(13)?,
            pid_start: row.get(14)?,
        })
    })();
    Ok(parsed)
}

const fn is_lower_hex(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

/// [`init_schema`], tried again while it is refused for being busy, for as long as the busy
/// timeout allows.
///
/// The timeout alone is not enough here: switching a database to WAL, or creating its schema,
/// while another process does the same can fail with `SQLITE_BUSY` at once, without the busy
/// handler being asked (SQLite cannot wait for a lock it would deadlock on). The two
/// processes that start together on a new database are the TUI and a `--json` run, so it
/// happens; after the loser waits a moment, the winner has finished and the second try finds
/// the work done.
fn init_schema_retrying(conn: &Connection, path: &Path, patience: Duration) -> Result<()> {
    let give_up = std::time::Instant::now() + patience;
    loop {
        match init_schema(conn, path) {
            Err(Error::Sqlite(e)) if is_busy(&e) && std::time::Instant::now() < give_up => {
                // A different pause per process, so two that collided do not collide again.
                std::thread::sleep(Duration::from_millis(
                    5 + u64::from(std::process::id() % 16),
                ));
            }
            other => return other,
        }
    }
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// Creates the schema on an empty database, or checks the version of an existing one.
///
/// The version is read before anything else is done to the file: switching the journal mode
/// of a newer auspex's database would already be a change to it.
fn init_schema(conn: &Connection, path: &Path) -> Result<()> {
    // One snapshot for both reads: another process creating the schema between them would
    // otherwise show up as "tables, but version 0", which is what a foreign database looks like.
    let (version, objects) = {
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
        (user_version(&tx)?, object_count(&tx)?)
    };
    if version > SCHEMA_VERSION {
        return Err(Error::NewerSchema {
            path: path.to_owned(),
            found: version,
        });
    }
    if version == 0 && objects != 0 {
        return Err(Error::NotOurs(path.to_owned()));
    }
    if version < 0 || (version != 0 && version != SCHEMA_VERSION) {
        return Err(Error::NotOurs(path.to_owned()));
    }

    // WAL: a reader never blocks the writer and vice versa. Persistent in the file, and a
    // no-op once set. SQLite answers with the mode it ended up in, which is not "wal" on a
    // file system that cannot do it (and then two processes would lock each other out).
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Error::Unusable(format!(
            "{} cannot use write-ahead logging (journal mode {mode}); two auspex processes \
             could not share it",
            path.display()
        )));
    }

    if version == 0 {
        // Two processes may both find the database empty. IMMEDIATE takes the write lock
        // first, so the second waits, then sees the first one's schema, so it re-reads.
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        match user_version(&tx)? {
            0 => {
                if object_count(&tx)? != 0 {
                    return Err(Error::NotOurs(path.to_owned()));
                }
                tx.execute_batch(SCHEMA_V1)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                tx.commit()?;
            }
            SCHEMA_VERSION => {}
            v if v > SCHEMA_VERSION => {
                return Err(Error::NewerSchema {
                    path: path.to_owned(),
                    found: v,
                });
            }
            _ => return Err(Error::NotOurs(path.to_owned())),
        }
    }
    Ok(())
}

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

/// Tables, indexes, views and triggers in the file.
fn object_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?)
}

// ---------------------------------------------------------------------------------------
// The directory and the file
// ---------------------------------------------------------------------------------------

/// Creates `dir` (and missing parents, 0700) and requires it to be a real directory of ours
/// with mode 0700: the crate's one rule for a directory it keeps state in.
fn prepare_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| {
            Error::Unusable(format!(
                "cannot create the action log directory {}: {e}",
                dir.display()
            ))
        })?;
    crate::ssh::prepare_private_dir(dir, "action log directory").map_err(Error::Unusable)
}

/// Refuses a log inside the fleet repository.
fn refuse_inside(dir: &Path, fleet: &Path, path: &Path) -> Result<()> {
    let (Ok(dir), Ok(fleet)) = (dir.canonicalize(), fleet.canonicalize()) else {
        return Err(Error::Unusable(format!(
            "cannot tell whether {} is inside the fleet {}; refusing to use it",
            path.display(),
            fleet.display()
        )));
    };
    if dir.starts_with(&fleet) {
        return Err(Error::Unusable(format!(
            "the action log {} is inside the fleet repository {}; refusing to use it",
            path.display(),
            fleet.display()
        )));
    }
    Ok(())
}

/// Creates the database file 0600 if missing, and requires what is there to be a regular
/// file of ours that nobody else can open. Opened with `O_NOFOLLOW`: a symlink is refused,
/// not followed to wherever it points. The descriptor is only for the checks; SQLite opens
/// the path again (in a directory only this user can write to, which is what makes the gap
/// harmless).
fn prepare_file(path: &Path) -> Result<()> {
    let shown = path.display();
    let fd = rustix::fs::open(
        path,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|e| match e {
        rustix::io::Errno::LOOP => Error::Unusable(format!(
            "the action log {shown} is a symlink; refusing to use it"
        )),
        e => Error::Unusable(format!("cannot open the action log {shown}: {e}")),
    })?;
    let meta = std::fs::File::from(fd)
        .metadata()
        .map_err(|e| Error::Unusable(format!("cannot inspect the action log {shown}: {e}")))?;
    if !meta.is_file() {
        return Err(Error::Unusable(format!(
            "the action log {shown} is not a regular file; refusing to use it"
        )));
    }
    let me = rustix::process::getuid().as_raw();
    if meta.uid() != me {
        return Err(Error::Unusable(format!(
            "the action log {shown} is owned by uid {}, not by this user (uid {me}); \
             refusing to use it",
            meta.uid()
        )));
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Err(Error::Unusable(format!(
            "the action log {shown} has mode {mode:04o}, expected 0600; refusing to use it"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Untrusted text
// ---------------------------------------------------------------------------------------

/// A name from outside (an inventory machine, a user, a host): one line, the domain's
/// sanitiser (escape sequences, control characters, bidi and zero-width characters gone),
/// at most [`NAME_CAP`] bytes.
fn clean_name(s: &str) -> String {
    cap(&auspex_domain::text::sanitize_line(s), NAME_CAP)
}

fn clean_machine(s: &str) -> String {
    clean_name(s)
}

/// Clan's or ssh's text, reduced to its last sanitised lines (`text::reason`) and capped at
/// [`DETAIL_CAP`] bytes; `None` when nothing is left.
fn clean_detail(s: &str) -> Option<String> {
    crate::text::reason(s).map(|r| cap(&r, DETAIL_CAP))
}

/// At most `max` bytes, cut on a character boundary, ending in `…` when something was cut
/// (the ellipsis counts towards `max`).
fn cap(s: &str, max: usize) -> String {
    const ELLIPSIS: &str = "…";
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max - ELLIPSIS.len();
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &s[..end])
}

// ---------------------------------------------------------------------------------------
// Is the writer of an unfinished row still there?
// ---------------------------------------------------------------------------------------

/// `(state, "<boot id>/<start time>")` of process `pid` from `/proc`: the start time is in
/// clock ticks since boot, which with the boot id names one process for ever (a recycled pid
/// starts later, and after a reboot the boot id differs). `None` when there is no such
/// process or no `/proc`.
fn process_stamp(pid: u32) -> Option<(char, String)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat).map(|(state, ticks)| (state, format!("{}/{ticks}", boot_id())))
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// The state (field 3) and start time (field 22) of a `/proc/<pid>/stat` line. The command
/// name (field 2) is in parentheses and may contain anything, spaces and `)` included, so
/// the fields are counted from the last `)`.
fn parse_stat(stat: &str) -> Option<(char, u64)> {
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    let started = fields.nth(18)?.parse().ok()?;
    Some((state, started))
}

/// Whether the process that wrote a row can still be writing it.
fn writer_alive(pid: i64, recorded: Option<&str>) -> bool {
    let Ok(pid) = u32::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    let has_proc = Path::new("/proc/self/stat").exists();
    match (recorded, has_proc) {
        // The same process: same start time on the same boot, and not a zombie (dead, but
        // not yet reaped by its parent: it can write nothing).
        (Some(recorded), true) => process_stamp(pid)
            .is_some_and(|(state, now)| now == recorded && !matches!(state, 'Z' | 'X')),
        // No start time to compare, or nowhere to read it: is there a process of that pid?
        _ => rustix::process::Pid::from_raw(i32::try_from(pid).unwrap_or(0))
            .is_some_and(|p| rustix::process::test_kill_process(p) != Err(rustix::io::Errno::SRCH)),
    }
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

/// Panics if a test is about to open the real state directory's log. Called from
/// [`Inner::open`] in test builds only, so that every test in this module (and the child
/// processes it starts, which are this same test binary) is covered, whatever it forgets.
#[cfg(test)]
fn forbid_default_location_in_tests(path: &Path, default: Option<&Path>) {
    let real = default.is_some_and(|d| {
        let same_dir = d.parent().and_then(|d| d.canonicalize().ok());
        let this_dir = path.parent().and_then(|p| p.canonicalize().ok());
        d == path
            || (same_dir.is_some() && same_dir == this_dir && d.file_name() == path.file_name())
    });
    assert!(
        !real,
        "a test tried to open the real action log {}; every test passes a path under a \
         temporary directory",
        path.display()
    );
}

#[cfg(test)]
mod tests;
