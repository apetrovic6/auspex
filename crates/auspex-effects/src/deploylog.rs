//! The local log of everything auspex does to a machine: one SQLite database, one row per
//! action (deploy, build, reboot, rollback, freeze, unfreeze).
//!
//! **The schema is a published contract.** The weekly homelab digest reads the file directly,
//! so the table, its columns and their value sets are documented in this crate's README
//! ("The action log") and pinned by a test (`the_schema_is_pinned`). Changing it means
//! changing that test, which is the point.
//!
//! # Use, and what the caller must never do
//!
//! [`ActionLog::open`] once per run, then [`ActionLog::start`] when an action begins and
//! [`ActionLog::finish`] when it ends. [`ActionLog::recent`] and [`ActionLog::since`] read
//! it back for the timeline. The TUI shell calls it for every `u` (kind `deploy`) and `b` (kind
//! `build`): it opens the log on the first action, writes the row from a task of its own beside
//! the action, and shows a failure as a warning in the status bar (`shell.rs`, `log_action`).
//!
//! **The log is a record of the action, never part of it.** A caller must not await a log
//! call on a deploy's critical path (start the deploy, and write the row beside it, for
//! example from a task of its own), and must never turn a log error into the action's result:
//! a deploy that succeeded did succeed, whatever the log said. Every call can fail (a full
//! disk, a locked file, a read-only file system, a deadline), and the **wait** before a write is
//! bounded (see "Concurrency"), but a statement that has begun is not: a stalled disk stalls
//! the call. That, and a bound of seconds being too long to hold a deploy for anyway, is why
//! this rule is the real protection, not the deadline.
//!
//! What a caller does with an error: run every log call in a task of its own, never `select!`
//! or time one out itself (a drop mid-statement leaves an orphan `start` row), and never feed
//! a result into the action's outcome. Then:
//!
//! - `start` fails with `Timeout`, `Sqlite`, `ReadOnly` or `Worker`: no row was written. Show
//!   "not logged"; keep no id. Retrying is safe.
//! - `finish` fails with `Timeout` or `Sqlite`: the row stays unfinished and reads as running
//!   until this process exits, then as interrupted. Retry; `NotRunning` on a retry means an
//!   earlier attempt landed.
//! - `Invalid` is a bug in the caller. `Corrupt` on a query: show the timeline as unavailable.
//! - [`ActionLog::is_read_only`]: do not attempt writes, and hide the "logging" indicator.
//! - `Unusable`, `NewerSchema`, `NotOurs` at open: run without a log, and show the reason once.
//!
//! The caller passes **summaries, never output**: `detail` is a short summary of the outcome,
//! not clan's log. If it is longer than [`DETAIL_CAP`] bytes its **end** is kept (the cause of
//! a failure is last), prefixed with `…`. Clan's output can contain whatever the machine's
//! activation scripts print, and a log other programs read directly is the wrong place for it.
//!
//! # Where it lives
//!
//! [`default_path`]: `$XDG_STATE_HOME/auspex/log.sqlite`, else
//! `~/.local/state/auspex/log.sqlite` (a relative `XDG_STATE_HOME` is ignored). The caller
//! may pass any other path to [`OpenParams`]; tests always do. The directory holding the
//! file must be private (0700, owned by this user, not a symlink: the same rule as the other
//! directories this crate creates) and is created so when missing. The file is created 0600;
//! it must be a regular file of this user with no group or other permission bits, not a
//! symlink, and with one hard link (two names would give one database two `-wal` files). The
//! `-wal` and `-shm` files auspex creates take the database's mode from SQLite; an existing
//! one is not checked. The location is checked against the fleet repository **before** any
//! directory is created ([`OpenParams::fleet`] is required): a log inside it is refused.
//!
//! **Local file systems only.** SQLite's write-ahead log keeps an index in shared memory,
//! which does not work across hosts: two machines writing one log over NFS can corrupt it.
//! **On Linux** the open refuses a directory that `statfs` reports as NFS, SMB/CIFS, FUSE or
//! 9p (checked before anything is created; this includes local FUSE: bindfs, gocryptfs,
//! ntfs-3g, virtiofs). Elsewhere (macOS) there is no such check: keep the log on a local disk.
//!
//! On a read-only file system the log opens **read-only** when SQLite can read it: the history
//! stays readable ([`ActionLog::is_read_only`]) and `start` and `finish` fail with
//! [`Error::ReadOnly`]. SQLite reads a WAL database only with its `-shm` index, which it must
//! be able to create or find; where it cannot (a directory it cannot write, with no `-shm`
//! there), the open fails and says so, rather than show history that may be stale or torn.
//!
//! # Interrupted actions
//!
//! A row that was started and never finished means auspex crashed or was killed. The queries
//! report such a row as [`ActionResult::Interrupted`] (with no `ended_at`) **when no auspex
//! process can still be writing it**. That is judged only when this process can see the
//! writer's process: the row's `host` is this host **and** its `machine_id` (`/etc/machine-id`;
//! the host name where there is none, as on macOS) and `pid_ns` (the inode of
//! `/proc/self/ns/pid`) are ours. A writer in another pid namespace (a container) or on another
//! machine that shares the host name has a pid that means something else here, so its row is
//! left as running (`result` is `None`), never called dead. For a row that is ours to judge,
//! the process that started it, recorded as its pid and its start time
//! (`/proc/<pid>/stat` field 22 together with the boot id, so a recycled pid or a reboot is
//! not mistaken for the writer), is gone or a zombie. Nothing is written back: this is a
//! judgement made at read time, so a live process's row is never touched, and
//! [`ActionLog::finish`] itself only completes a row that this process started. Without
//! `/proc` (not Linux) the start time is not recorded and a bare `kill(pid, 0)` decides; that
//! can be fooled by a recycled pid, which is why Linux is the target.
//!
//! # Concurrency
//!
//! The TUI and a `--json` run may write at once, from two processes. The database is in WAL
//! mode (readers never block writers, and a writer waits only for another writer), every
//! statement is one short transaction, and the busy timeout makes a writer wait instead of
//! failing with `database is locked`. Every call on [`ActionLog`] is async and runs its
//! SQLite work on tokio's blocking pool, so nothing here blocks the runtime, and so the TUI.
//! Dropping the last [`ActionLog`] closes the connection on the blocking pool too (call
//! [`ActionLog::close`] to wait for it).
//!
//! **Each call has one total deadline** ([`OpenParams::call_timeout`], default 2 s) on its
//! **wait before the statement**: for the connection (all clones share one, so calls queue) and
//! for another process's lock (the busy timeout is cut to what the call has left). `N` calls at
//! once wait at most that long in all, not `N` times the busy timeout. A call that is still
//! waiting at its deadline fails with [`Error::Timeout`] and has written nothing. **A statement
//! that has begun runs to completion**: it is one short `INSERT` or `UPDATE`, it is not
//! interrupted, and a stalled disk stalls the call for as long as it lasts (interrupting it
//! could not promise that a `Timeout` wrote nothing). There is no `CancellationToken`: the
//! deadline is the bound on waiting, and there is no process to kill.
//!
//! **Dropping a call's future** (a `select!`, a cancelled task) stops a call that has not
//! begun its statement: it never starts, and writes nothing. A statement that has begun is
//! one transaction and completes atomically; for [`ActionLog::start`] that leaves a row whose
//! id the caller never saw, which reads as running until this process exits, and then as
//! interrupted. That window runs from the call's last check, just before its statement, to the
//! end of the statement.

use std::ffi::OsStr;
use std::fmt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use rustix::fs::{Mode, OFlags};

/// The schema version this build writes and reads (`PRAGMA user_version`).
pub const SCHEMA_VERSION: i64 = 1;

/// The longest `detail` stored, in bytes. Longer text keeps its **end**, on a character
/// boundary, prefixed with `…` (the prefix counts towards the cap). The table's `CHECK`
/// enforces it too.
pub const DETAIL_CAP: usize = 4096;

/// The longest `machine`, `who`, `host` and `auspex_version` stored, in bytes. Longer text
/// keeps its start and ends in `…`.
pub const NAME_CAP: usize = 255;

/// The most rows [`ActionLog::recent`] returns, whatever the caller asks for.
pub const RECENT_CAP: usize = 1000;

/// The file name inside the state directory.
pub const FILE_NAME: &str = "log.sqlite";

/// The default for [`OpenParams::busy_timeout`].
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The default for [`OpenParams::call_timeout`].
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(2);

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
    machine_id TEXT,
    pid_ns TEXT,
    CHECK ((ended_at IS NULL) = (result IS NULL)),
    CHECK (exit_code IS NULL OR ended_at IS NOT NULL)
);
CREATE INDEX actions_machine_started ON actions (machine, started_at DESC, id DESC);
CREATE INDEX actions_started ON actions (started_at, id);
";

/// The columns every query selects, in the order [`row_to_stored`] reads them.
const COLUMNS: &str = "id, kind, machine, fleet_commit, fleet_dirty, started_at, ended_at, \
                       result, exit_code, who, host, detail, auspex_version, pid, pid_start, \
                       machine_id, pid_ns";

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
    /// A short summary, never the process's output. Every line is sanitised by the domain's
    /// rules (control and format characters removed, empty lines dropped); if the result is
    /// longer than [`DETAIL_CAP`] bytes its **end** is kept, prefixed with `…`.
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
    /// A short summary of the outcome (its end, if it was long).
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
    /// The log was busy: the call was still waiting for the connection or for another process's
    /// lock when its [`OpenParams::call_timeout`] ran out, or SQLite refused at once with busy.
    /// It wrote nothing. (A statement that has begun is not cut short.)
    Timeout,
    /// The caller dropped the call before it began its statement. It wrote nothing.
    Cancelled,
    /// The log was opened read-only (a read-only file system): it can be read, not written.
    ReadOnly,
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
            Self::Timeout => f.write_str("the action log was busy; nothing was written"),
            Self::Cancelled => f.write_str("the action log call was dropped before it began"),
            Self::ReadOnly => f.write_str(
                "the action log is on a read-only file system; it can be read, not written",
            ),
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
    /// The fleet repository. A log inside it is refused, before any directory is created (the
    /// log is not the fleet's, and `git add -A` would commit it). Required, so that the rule
    /// does not hold only when a caller remembers it.
    pub fleet: PathBuf,
    /// How long one statement waits for another process's write lock. Default
    /// [`DEFAULT_BUSY_TIMEOUT`]. Within a call it is further limited by
    /// [`OpenParams::call_timeout`].
    pub busy_timeout: Duration,
    /// The most one call may wait before its statement begins: for the connection and for
    /// another process's lock. Default [`DEFAULT_CALL_TIMEOUT`]. Past it the call fails with
    /// [`Error::Timeout`]. A statement that has begun is not cut short.
    pub call_timeout: Duration,
}

impl OpenParams {
    /// `path`, written by `identity`, with the log kept out of `fleet`, and the default
    /// timeouts.
    #[must_use]
    pub const fn new(path: PathBuf, identity: Identity, fleet: PathBuf) -> Self {
        Self {
            path,
            identity,
            fleet,
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
            call_timeout: DEFAULT_CALL_TIMEOUT,
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

/// What identifies the world a pid belongs to: the machine, and the pid namespace on it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Space {
    machine_id: Option<String>,
    pid_ns: Option<String>,
}

impl Space {
    /// This process's. The machine id is `/etc/machine-id` (or dbus's copy) on Linux, and the
    /// host name elsewhere (macOS has an `IOPlatformUUID`, which is not worth a dependency
    /// here: a host name shared by two Macs sharing a state directory is not a case worth
    /// more). The pid namespace is the link text of `/proc/self/ns/pid`, `pid:[<inode>]`.
    fn current(host: &str) -> Self {
        let machine_id = if cfg!(target_os = "linux") {
            ["/etc/machine-id", "/var/lib/dbus/machine-id"]
                .iter()
                .find_map(|p| std::fs::read_to_string(p).ok())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        } else {
            Some(host.to_owned())
        };
        let pid_ns = std::fs::read_link("/proc/self/ns/pid")
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        Self { machine_id, pid_ns }
    }
}

struct Inner {
    /// `None` only once the connection has been taken to be closed (`close`, `Drop`).
    conn: Mutex<Option<Connection>>,
    path: PathBuf,
    identity: Identity,
    pid: i64,
    pid_start: Option<String>,
    space: Space,
    busy_timeout: Duration,
    call_timeout: Duration,
    read_only: bool,
}

/// One call's budget and its cancellation flag, shared with the future that made it.
struct Call {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl Call {
    fn within(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// Sets the flag when the future that owns it is dropped: a call that has not begun its
/// statement by then does not begin it.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The connection, locked, for one call.
struct Conn<'a>(MutexGuard<'a, Option<Connection>>);

impl std::ops::Deref for Conn<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.0
            .as_ref()
            .expect("the connection is taken only by close and drop, which own the Inner")
    }
}

impl ActionLog {
    /// Opens the log, creating it (directory 0700, file 0600, schema version 1) when it does
    /// not exist, and refusing when it is not ours to use (see [`Error`]). A file written by
    /// a newer auspex is refused before anything in it is changed. On a read-only file system
    /// it opens read-only ([`ActionLog::is_read_only`]).
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

    /// Whether the log was opened read-only (a read-only file system): the queries work,
    /// `start` and `finish` fail with [`Error::ReadOnly`].
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.inner.read_only
    }

    /// Records that an action began, now, and returns its id. The row has no end and no
    /// result until [`ActionLog::finish`].
    ///
    /// See the module documentation: not on a deploy's critical path, and its error is never
    /// the action's result.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a machine that is empty once sanitised or a commit that is not
    /// hex; [`Error::Timeout`] past the call's deadline; [`Error::ReadOnly`]; [`Error::Sqlite`]
    /// otherwise.
    pub async fn start(&self, action: NewAction) -> Result<ActionId> {
        self.blocking(move |i, call| i.start(&action, Timestamp::now(), call))
            .await
    }

    /// Records how the action `id` (started by this process) ended, now.
    ///
    /// # Errors
    ///
    /// [`Error::NotRunning`] when there is no unfinished row of this process with that id;
    /// [`Error::Timeout`] past the call's deadline; [`Error::ReadOnly`].
    pub async fn finish(&self, id: ActionId, outcome: Outcome) -> Result<()> {
        self.blocking(move |i, call| i.finish(id, &outcome, Timestamp::now(), call))
            .await
    }

    /// The last `limit` actions of `machine` (at most [`RECENT_CAP`]), newest first. The
    /// name is sanitised and capped the way it is on the way in, so the same text finds it.
    ///
    /// # Errors
    ///
    /// [`Error::Sqlite`], [`Error::Timeout`], or [`Error::Corrupt`] for a row that violates
    /// the schema.
    pub async fn recent(&self, machine: &str, limit: usize) -> Result<Vec<Action>> {
        let machine = clean_machine(machine);
        let limit = limit.min(RECENT_CAP);
        self.blocking(move |i, call| i.recent(&machine, limit, call))
            .await
    }

    /// Every action that began at or after `time`, oldest first.
    ///
    /// # Errors
    ///
    /// As [`ActionLog::recent`].
    pub async fn since(&self, time: Timestamp) -> Result<Vec<Action>> {
        self.blocking(move |i, call| i.since(time, call)).await
    }

    /// Closes the connection, on the blocking pool, and waits for it **if this is the last
    /// handle** (with other clones alive, or a dropped call's task still holding one, it returns
    /// at once and the last one to go closes it). Dropping the last clone closes it there too,
    /// without waiting. The close of the last connection to a database
    /// checkpoints and deletes the WAL: synchronous I/O that must not run on the runtime.
    pub async fn close(self) {
        if let Ok(mut inner) = Arc::try_unwrap(self.inner)
            && let Some(conn) = inner.take_connection()
        {
            let _ = tokio::task::spawn_blocking(move || close_connection(conn)).await;
        }
    }

    /// Runs `f` on the blocking pool. The one place any database work is started from, so
    /// that none of it can run on the async runtime. The call's deadline starts here.
    async fn blocking<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Inner, &Call) -> Result<T> + Send + 'static,
    {
        let call = Call::within(self.inner.call_timeout);
        // Dropped with this future (or at its end, when it no longer matters).
        let _cancel_if_dropped = CancelOnDrop(Arc::clone(&call.cancelled));
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || f(&inner, &call))
            .await
            .map_err(|e| Error::Worker(e.to_string()))?
    }
}

/// Where a connection is closed. Every close goes through here, on a blocking thread.
fn close_connection(conn: Connection) {
    #[cfg(test)]
    tests_hook::closed_on_this_thread(&conn);
    let _ = conn.close();
}

impl Drop for Inner {
    fn drop(&mut self) {
        let Some(conn) = self.take_connection() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            // Not here: this may be the runtime's own thread, and the close of the last
            // connection to a file is a checkpoint and a delete.
            Ok(runtime) => drop(runtime.spawn_blocking(move || close_connection(conn))),
            Err(_) => close_connection(conn),
        }
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
            call_timeout,
        } = params;
        identity.who = clean_name(&identity.who);
        identity.host = clean_name(&identity.host);
        identity.auspex_version = clean_name(&identity.auspex_version);
        if identity.who.is_empty() || identity.host.is_empty() {
            return Err(Error::Invalid("who and host must not be empty"));
        }
        if identity.auspex_version.is_empty() {
            "unknown".clone_into(&mut identity.auspex_version);
        }

        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| Error::Unusable(format!("{} has no directory", path.display())))?;
        if path.file_name().is_none() {
            return Err(Error::Unusable(format!("{} names no file", path.display())));
        }
        // Before anything is created: a refusal must leave the fleet as it was.
        refuse_inside(dir, &fleet, &path)?;
        // Likewise before the directory is made: an NFS home is not left with an empty
        // `~/.local/state/auspex/` behind. A directory that exists is checked itself; one that
        // does not, by the nearest ancestor that does, which is the file system it would be on.
        refuse_network_fs(nearest_existing(dir), &path)?;
        prepare_dir(dir)?;
        let read_only = prepare_file(&path)?;

        let conn = if read_only {
            open_read_only(&path, busy_timeout)?
        } else {
            // No URI parsing (a `?` in the path is a character), no create (prepare_file
            // made it), and SQLite itself refuses a symlink in the last component too.
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            conn.busy_timeout(busy_timeout)?;
            init_schema_retrying(&conn, &path, busy_timeout)?;
            conn
        };

        let space = Space::current(&identity.host);
        Ok(Self {
            conn: Mutex::new(Some(conn)),
            path,
            identity,
            pid: pid_in_proc(),
            // `/proc/self`, not `/proc/<pid>`: in a pid namespace without its own /proc, our
            // pid names another process there.
            pid_start: process_stamp("self").map(|(_, stamp)| stamp),
            space,
            busy_timeout,
            call_timeout,
            read_only,
        })
    }

    fn take_connection(&mut self) -> Option<Connection> {
        self.conn
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Locks the connection for `call`: waits for it (all clones share one) no longer than
    /// the call's deadline, gives up at once if the call's future has been dropped, and
    /// limits the busy timeout to what the call has left, so the three waits together are the
    /// one deadline.
    fn lock_for(&self, call: &Call) -> Result<Conn<'_>> {
        loop {
            if call.cancelled.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            match self.conn.try_lock() {
                Ok(guard) => return self.arm(Conn(guard), call),
                Err(TryLockError::Poisoned(p)) => {
                    // A panic while the lock was held leaves a usable connection (SQLite
                    // rolled the statement back); refusing every later call would turn one
                    // bug into all of them.
                    return self.arm(Conn(p.into_inner()), call);
                }
                Err(TryLockError::WouldBlock) => {}
            }
            if call.remaining().is_zero() {
                return Err(Error::Timeout);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn arm<'a>(&self, conn: Conn<'a>, call: &Call) -> Result<Conn<'a>> {
        // The last look before a statement begins: after this the call completes.
        if call.cancelled.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        let left = call.remaining();
        if left.is_zero() {
            return Err(Error::Timeout);
        }
        conn.busy_timeout(self.busy_timeout.min(left))
            .map_err(call_error)?;
        Ok(conn)
    }

    fn start(&self, a: &NewAction, at: Timestamp, call: &Call) -> Result<ActionId> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
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
        let conn = self.lock_for(call)?;
        conn.prepare(
            "INSERT INTO actions (kind, machine, fleet_commit, fleet_dirty, started_at, \
             who, host, auspex_version, pid, pid_start, machine_id, pid_ns) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .map_err(call_error)?
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
            self.space.machine_id,
            self.space.pid_ns,
        ])
        .map_err(call_error)?;
        Ok(ActionId(conn.last_insert_rowid()))
    }

    fn finish(&self, id: ActionId, o: &Outcome, at: Timestamp, call: &Call) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let detail = o.detail.as_deref().and_then(clean_detail);
        let conn = self.lock_for(call)?;
        // Only a row this process started and nobody has finished: another process's row is
        // not ours to complete, whatever its id.
        let changed = conn
            .prepare(
                "UPDATE actions SET ended_at = ?1, result = ?2, exit_code = ?3, detail = ?4 \
                 WHERE id = ?5 AND ended_at IS NULL AND pid = ?6 AND pid_start IS ?7 \
                 AND host = ?8",
            )
            .map_err(call_error)?
            .execute(params![
                at.to_string(),
                o.result.as_str(),
                o.exit_code,
                detail,
                id.0,
                self.pid,
                self.pid_start,
                self.identity.host,
            ])
            .map_err(call_error)?;
        drop(conn);
        if changed == 0 {
            return Err(Error::NotRunning(id));
        }
        Ok(())
    }

    fn recent(&self, machine: &str, limit: usize, call: &Call) -> Result<Vec<Action>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.lock_for(call)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM actions WHERE machine = ?1 \
                 ORDER BY started_at DESC, id DESC LIMIT ?2"
            ))
            .map_err(call_error)?;
        let rows = stmt
            .query_map(params![machine, limit], row_to_stored)
            .map_err(call_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(call_error)?;
        drop(stmt);
        drop(conn);
        rows.into_iter().map(|r| Ok(self.judge(r?))).collect()
    }

    fn since(&self, time: Timestamp, call: &Call) -> Result<Vec<Action>> {
        let conn = self.lock_for(call)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM actions WHERE started_at >= ?1 \
                 ORDER BY started_at ASC, id ASC"
            ))
            .map_err(call_error)?;
        let rows = stmt
            .query_map(params![time.to_string()], row_to_stored)
            .map_err(call_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(call_error)?;
        drop(stmt);
        drop(conn);
        rows.into_iter().map(|r| Ok(self.judge(r?))).collect()
    }

    /// Turns a stored row into an [`Action`], reading an unfinished row whose writer is gone
    /// as interrupted. Runs after the statement is done and the lock released: the process
    /// checks read `/proc`, and no database lock should wait for that.
    ///
    /// Only a row whose writer this process can see is judged: same host name, same machine,
    /// same pid namespace. Anything else is left running, never called dead.
    fn judge(&self, s: Stored) -> Action {
        let mut action = s.action;
        let ours = s.space == self.space;
        if action.result.is_none()
            && action.host == self.identity.host
            && ours
            && !writer_alive(s.pid, s.pid_start.as_deref())
        {
            action.result = Some(ActionResult::Interrupted);
        }
        action
    }
}

/// A call's SQLite failure, as the caller sees it: a lock that stayed busy for the whole
/// budget is the call's deadline passing.
fn call_error(e: rusqlite::Error) -> Error {
    if is_busy(&e) {
        Error::Timeout
    } else {
        Error::Sqlite(e)
    }
}

/// A row, with the columns only the interrupted check uses.
struct Stored {
    action: Action,
    pid: i64,
    pid_start: Option<String>,
    space: Space,
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
            space: Space {
                machine_id: row.get(15)?,
                pid_ns: row.get(16)?,
            },
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
    let give_up = Instant::now() + patience;
    loop {
        match init_schema(conn, path) {
            Err(Error::Sqlite(e)) if is_busy(&e) && Instant::now() < give_up => {
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

/// Refuses a journal mode that is not WAL. SQLite answers a request for WAL with the mode it
/// ended up in, which is something else on a file system that cannot do it, and then two
/// processes would lock each other out.
fn require_wal(mode: &str, path: &Path) -> Result<()> {
    if mode.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    Err(Error::Unusable(format!(
        "{} cannot use write-ahead logging (journal mode {mode}); two auspex processes \
         could not share it",
        path.display()
    )))
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
    check_version(version, objects, path)?;

    // WAL: a reader never blocks the writer and vice versa. Persistent in the file, and a
    // no-op once set.
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    require_wal(&mode, path)?;

    if version == 0 {
        // Two processes may both find the database empty. IMMEDIATE takes the write lock
        // first, so the second waits, then sees the first one's schema, so it re-reads.
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let version = user_version(&tx)?;
        let objects = object_count(&tx)?;
        check_version(version, objects, path)?;
        if version == 0 {
            tx.execute_batch(SCHEMA_V1)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        }
    }
    add_missing_columns(conn)?;
    Ok(())
}

/// Nullable columns added to version 1 after it first shipped, with their types. A file made by
/// an earlier build lacks them; the open adds them (`ALTER TABLE ... ADD COLUMN`), which is
/// the mechanism behind the README's rule that a new nullable column keeps version 1. A fresh
/// file has them from `SCHEMA_V1` already. Constants only: they are spliced into SQL.
const ADDED_COLUMNS: &[(&str, &str)] = &[("machine_id", "TEXT"), ("pid_ns", "TEXT")];

/// The [`ADDED_COLUMNS`] this database does not have.
fn missing_columns(conn: &Connection) -> Result<Vec<(&'static str, &'static str)>> {
    let have: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('actions')")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(ADDED_COLUMNS
        .iter()
        .filter(|(name, _)| !have.iter().any(|h| h == name))
        .copied()
        .collect())
}

/// Adds the missing columns under the write lock, re-reading once it is held: two processes
/// opening one old file both see a column missing, and only the first may add it.
fn add_missing_columns(conn: &Connection) -> Result<()> {
    if missing_columns(conn)?.is_empty() {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    for (name, ty) in missing_columns(&tx)? {
        tx.execute_batch(&format!("ALTER TABLE actions ADD COLUMN {name} {ty}"))?;
    }
    tx.commit()?;
    Ok(())
}

/// What a database's version and contents say about whether it is ours: newer is refused,
/// tables with version 0 or an unknown version are not ours, an empty file is ours to
/// initialise.
fn check_version(version: i64, objects: i64, path: &Path) -> Result<()> {
    if version > SCHEMA_VERSION {
        return Err(Error::NewerSchema {
            path: path.to_owned(),
            found: version,
        });
    }
    if (version == 0 && objects != 0) || version < 0 || (version != 0 && version != SCHEMA_VERSION)
    {
        return Err(Error::NotOurs(path.to_owned()));
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

/// Opens the database for reading only (a read-only file system), checking its version.
///
/// SQLite reads a WAL database read-only only with its `-shm` index, which it must create or
/// find; where it cannot, the first read fails and so does the open, with a message that says
/// why. Not `immutable`: that reads with no locks and no change detection, so while another
/// process writes the same file (a read-only bind mount of a directory the TUI writes through
/// a writable path: `EROFS` describes this mount, not the file) it shows stale rows and, once
/// its cached pages mix with fresh ones, torn ones. No history is better than that.
fn open_read_only(path: &Path, busy_timeout: Duration) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(busy_timeout)?;
    let (version, objects) = match (user_version(&conn), object_count(&conn)) {
        (Ok(v), Ok(o)) => (v, o),
        (Err(e), _) | (_, Err(e)) => {
            return Err(Error::Unusable(format!(
                "cannot read the action log {} on a read-only file system: SQLite needs its \
                 -shm index file, which it can neither find nor create there ({e}); showing \
                 no history rather than history that may be stale",
                path.display()
            )));
        }
    };
    check_version(version, objects, path)?;
    if version == 0 {
        // Empty, and nothing can be created: there is no history to show.
        return Err(Error::Unusable(format!(
            "the action log {} is empty and its file system is read-only; nothing to show",
            path.display()
        )));
    }
    if !missing_columns(&conn)?.is_empty() {
        return Err(Error::Unusable(format!(
            "the action log {} predates columns this auspex needs, and its file system is \
             read-only, so it cannot be upgraded",
            path.display()
        )));
    }
    Ok(conn)
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

/// `p` made absolute, with its longest existing prefix resolved through symlinks and the
/// rest (which does not exist yet) appended as it is. An error when the rest holds a `..`:
/// where that points cannot be said without creating it.
fn resolve_through_existing(p: &Path) -> std::io::Result<PathBuf> {
    let abs = std::path::absolute(p)?;
    let mut rest = Vec::new();
    let mut cur = abs.as_path();
    loop {
        match cur.canonicalize() {
            Ok(mut base) => {
                base.extend(rest.iter().rev());
                return Ok(base);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (Some(name), Some(parent)) = (cur.file_name(), cur.parent()) else {
                    return Err(e);
                };
                rest.push(name.to_owned());
                cur = parent;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Refuses a log inside the fleet repository. Done before any directory is created, so a
/// refusal leaves the fleet exactly as it was.
fn refuse_inside(dir: &Path, fleet: &Path, path: &Path) -> Result<()> {
    let (Ok(dir), Ok(fleet)) = (
        resolve_through_existing(dir),
        resolve_through_existing(fleet),
    ) else {
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

/// The kind of network or userspace file system SQLite's WAL cannot be trusted on, by the
/// magic number `statfs` reports (`linux/magic.h`), or `None` for anything else.
const fn network_fs_name(f_type: i128) -> Option<&'static str> {
    match f_type & 0xFFFF_FFFF {
        0x6969 => Some("NFS"),
        0x517B => Some("SMB"),
        0xFF53_4D42 | 0xFE53_4D42 => Some("CIFS/SMB"),
        0x6573_5546 => Some("FUSE"),
        0x0102_1997 => Some("9p"),
        _ => None,
    }
}

/// `p`, or the nearest ancestor of it that exists.
fn nearest_existing(p: &Path) -> &Path {
    let mut cur = p;
    while !cur.exists() {
        match cur.parent() {
            Some(parent) => cur = parent,
            None => break,
        }
    }
    cur
}

/// Refuses a directory on NFS, SMB/CIFS, FUSE or 9p: WAL's shared-memory index does not work
/// across hosts, and two machines writing one log could corrupt it.
fn refuse_network_fs(dir: &Path, path: &Path) -> Result<()> {
    #[cfg(test)]
    if tests_hook::pretends(path, tests_hook::Pretend::Network) {
        return Err(network_fs_error(path, "NFS"));
    }
    #[cfg(target_os = "linux")]
    {
        let fd = rustix::fs::open(
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| Error::Unusable(format!("cannot open {}: {e}", dir.display())))?;
        let st = rustix::fs::fstatfs(&fd).map_err(|e| {
            Error::Unusable(format!("cannot tell what {} is on: {e}", dir.display()))
        })?;
        if let Some(name) = network_fs_name(i128::from(st.f_type)) {
            return Err(network_fs_error(path, name));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = dir;
    Ok(())
}

fn network_fs_error(path: &Path, name: &str) -> Error {
    Error::Unusable(format!(
        "the action log {} is on a {name} file system; SQLite's write-ahead log does not work \
         across hosts and could corrupt it. Use a local file system (XDG_STATE_HOME)",
        path.display()
    ))
}

/// The facts about the database file that decide whether it may be used, as a function of
/// numbers so that each rule is tested without needing another user's file.
fn check_file_facts(
    shown: &str,
    is_file: bool,
    owner: u32,
    me: u32,
    mode: u32,
    links: u64,
) -> Result<()> {
    if !is_file {
        return Err(Error::Unusable(format!(
            "the action log {shown} is not a regular file; refusing to use it"
        )));
    }
    if owner != me {
        return Err(Error::Unusable(format!(
            "the action log {shown} is owned by uid {owner}, not by this user (uid {me}); \
             refusing to use it"
        )));
    }
    let mode = mode & 0o7777;
    if mode & 0o077 != 0 {
        return Err(Error::Unusable(format!(
            "the action log {shown} has mode {mode:04o}, expected 0600; refusing to use it"
        )));
    }
    if links != 1 {
        // Two names for one database give it two `-wal` files, a known way to corrupt one.
        return Err(Error::Unusable(format!(
            "the action log {shown} has {links} hard links; refusing to use it"
        )));
    }
    Ok(())
}

/// Creates the database file 0600 if missing, and requires what is there to be a regular
/// file of ours that nobody else can open (see [`check_file_facts`]). Opened with
/// `O_NOFOLLOW`: a symlink is refused, not followed to wherever it points. The descriptor is
/// only for the checks; SQLite opens the path again (in a directory only this user can write
/// to, which is what makes the gap harmless).
///
/// On a read-only file system the file cannot be opened for writing: it is opened for reading
/// instead and `true` is returned, so the history can still be shown.
fn prepare_file(path: &Path) -> Result<bool> {
    let shown = path.display().to_string();
    let open_error = |e: rustix::io::Errno| match e {
        rustix::io::Errno::LOOP => Error::Unusable(format!(
            "the action log {shown} is a symlink; refusing to use it"
        )),
        e => Error::Unusable(format!("cannot open the action log {shown}: {e}")),
    };
    #[cfg(test)]
    let pretend_read_only = tests_hook::pretends(path, tests_hook::Pretend::ReadOnly);
    #[cfg(not(test))]
    let pretend_read_only = false;
    let writable = if pretend_read_only {
        Err(rustix::io::Errno::ROFS)
    } else {
        rustix::fs::open(
            path,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
    };
    let (fd, read_only) = match writable {
        Ok(fd) => (fd, false),
        Err(rustix::io::Errno::ROFS) => (
            rustix::fs::open(
                path,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| match e {
                rustix::io::Errno::NOENT => Error::Unusable(format!(
                    "the action log {shown} does not exist and its file system is read-only, so \
                     it cannot be created"
                )),
                e => open_error(e),
            })?,
            true,
        ),
        Err(e) => return Err(open_error(e)),
    };
    let meta = std::fs::File::from(fd)
        .metadata()
        .map_err(|e| Error::Unusable(format!("cannot inspect the action log {shown}: {e}")))?;
    check_file_facts(
        &shown,
        meta.is_file(),
        meta.uid(),
        rustix::process::getuid().as_raw(),
        meta.mode(),
        meta.nlink(),
    )?;
    Ok(read_only)
}

// ---------------------------------------------------------------------------------------
// Untrusted text
// ---------------------------------------------------------------------------------------

/// A name from outside (an inventory machine, a user, a host, a version): one line, the
/// domain's sanitiser (escape sequences, control characters, bidi and zero-width characters
/// gone), at most [`NAME_CAP`] bytes.
fn clean_name(s: &str) -> String {
    cap(&auspex_domain::text::sanitize_line(s), NAME_CAP)
}

fn clean_machine(s: &str) -> String {
    clean_name(s)
}

/// A summary from outside: every line sanitised by the domain (`sanitize_line`), empty lines
/// dropped, the rest joined with `\n`, and, if that is longer than [`DETAIL_CAP`] bytes, its
/// **end** kept (see [`cap_end`]). No line-based tail: the caller's summary is kept whole,
/// and when something has to go it is the oldest text, because the cause of a failure is
/// last. `None` when nothing is left.
fn clean_detail(s: &str) -> Option<String> {
    let lines: Vec<String> = s
        .split('\n')
        .map(auspex_domain::text::sanitize_line)
        .filter(|l| !l.is_empty())
        .collect();
    (!lines.is_empty()).then(|| cap_end(&lines.join("\n"), DETAIL_CAP))
}

/// At most `max` bytes, cut on a character boundary, ending in `…` when something was cut
/// (the ellipsis counts towards `max`). Keeps the start.
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

/// At most `max` bytes, cut on a character boundary, starting with `…` when something was
/// cut (the ellipsis counts towards `max`). Keeps the **end**.
fn cap_end(s: &str, max: usize) -> String {
    const ELLIPSIS: &str = "…";
    if s.len() <= max {
        return s.to_owned();
    }
    let mut start = s.len() - (max - ELLIPSIS.len());
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("{ELLIPSIS}{}", &s[start..])
}

// ---------------------------------------------------------------------------------------
// Is the writer of an unfinished row still there?
// ---------------------------------------------------------------------------------------

/// `(state, "<boot id>/<start time>")` of a process from `/proc/<which>/stat` (`which` is a
/// pid, or `self`): the start time is in clock ticks since boot, which with the boot id names
/// one process for ever (a recycled pid starts later, and after a reboot the boot id
/// differs). `None` when there is no such process or no `/proc`.
fn process_stamp(which: &str) -> Option<(char, String)> {
    let stat = std::fs::read_to_string(format!("/proc/{which}/stat")).ok()?;
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

/// The pid this process has in the `/proc` that is mounted (the number `/proc/self` resolves
/// to), which is what `/proc/<pid>` lookups mean. It differs from `getpid()` in a pid namespace
/// that did not remount `/proc` (`unshare -p` without `--mount-proc`): there `getpid()` is 1
/// and `/proc/1` is somebody else. Falls back to `getpid()` without `/proc`.
fn pid_in_proc() -> i64 {
    std::fs::read_link("/proc/self")
        .ok()
        .and_then(|p| p.to_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| i64::from(std::process::id()))
}

/// Whether the process that wrote a row can still be writing it. Only asked of a row whose
/// writer is in this process's own pid space (see [`Inner::judge`]).
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
        (Some(recorded), true) => process_stamp(&pid.to_string())
            .is_some_and(|(state, now)| now == recorded && !matches!(state, 'Z' | 'X')),
        // No start time to compare, or nowhere to read it: is there a process of that pid?
        // (`test_kill_process` is `kill(pid, 0)`: it sends nothing.)
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

/// What the tests need to see inside, or to pretend about, in the code above.
#[cfg(test)]
mod tests_hook {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::thread::ThreadId;

    use rusqlite::Connection;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Pretend {
        /// The log's file system reports as NFS.
        Network,
        /// The log's file system is read-only.
        ReadOnly,
    }

    static PRETENDING: Mutex<Vec<(PathBuf, Pretend)>> = Mutex::new(Vec::new());
    pub static CLOSED_ON: Mutex<Vec<(String, ThreadId)>> = Mutex::new(Vec::new());

    /// From now on, pretend `path` (exactly) is on such a file system. Keyed by path, so a
    /// test that pretends about its own temporary path does not touch any other test's.
    pub fn pretend(path: &Path, what: Pretend) {
        PRETENDING.lock().unwrap().push((path.to_owned(), what));
    }

    pub fn pretends(path: &Path, what: Pretend) -> bool {
        PRETENDING
            .lock()
            .unwrap()
            .iter()
            .any(|(p, w)| p == path && *w == what)
    }

    /// Records on which thread a connection to a file is closed.
    pub fn closed_on_this_thread(conn: &Connection) {
        if let Some(path) = conn.path() {
            CLOSED_ON
                .lock()
                .unwrap()
                .push((path.to_owned(), std::thread::current().id()));
        }
    }
}

#[cfg(test)]
mod tests;
