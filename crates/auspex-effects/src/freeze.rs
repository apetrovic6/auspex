//! Which machines the operator froze with `f`: one small TOML file, `state.toml`, in auspex's
//! state directory.
//!
//! **Why a state file and not `config.toml`.** A home-manager `config.toml` is a symlink into
//! the read-only store, and auspex never writes it. A freeze made at the keyboard has to survive
//! a restart, so it lives here, beside the action log. (A freeze written in `config.toml`, as
//! `machines.<m>.frozen = true`, is declarative and read by the loader; this file never holds
//! it.)
//!
//! # The file
//!
//! ```toml
//! version = 1
//!
//! [fleets."/home/me/clan/fleet"]
//! frozen = ["luna", "sol"]
//! ```
//!
//! Keyed by the fleet flake's canonical path ([`fleet_key`]), so two fleets do not share
//! freezes. A key this build does not know, inside a fleet or beside `fleets`, is kept when the
//! file is written again (a newer build's additive keys survive an older one's `f`); a
//! **version newer than this build's** is refused and never written over ([`Error::Newer`]),
//! and a file that is not what it says ([`Error::Corrupt`]) is refused too. In both cases the
//! caller treats freezes as *unknown*: it must not guess an empty set and go on, because
//! "nothing is frozen" is the one wrong answer that lets a deploy through. A missing file is
//! the empty set.
//!
//! # Where it lives, and who may touch it
//!
//! [`default_path`]: `$XDG_STATE_HOME/auspex/state.toml`, else `~/.local/state/auspex/state.toml`
//! (a relative `XDG_STATE_HOME` is ignored: the same rule as the action log and the GC roots).
//! The directory is private (0700, ours, not a symlink: [`crate::ssh::prepare_private_dir`], the
//! crate's one rule for a directory it keeps state in) and the file is 0600, a regular file of
//! this user with one name, never followed through a symlink. Tests always pass a path under a
//! temporary directory; a test build refuses the real one.
//!
//! # Writing
//!
//! [`FreezeStore::set`] changes **one machine** and returns the whole set as written, so two
//! running auspex processes do not lose each other's freezes: under an exclusive lock on the
//! directory it reads the file, applies the change, writes a temporary file beside it (0600,
//! `O_EXCL`), flushes it, renames it over `state.toml` and flushes the directory. A reader sees
//! the old file or the new one, never half of one; a crash leaves at most a stray temporary
//! file, which the next write replaces.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt;
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use auspex_domain::fleet::is_plain_name;
use rustix::fs::{AtFlags, FileType, FlockOperation, Mode, OFlags, flock, fsync, openat, renameat};
use toml::{Table, Value};

use crate::safe_fs::{self, ReadError};

/// The schema version this build writes and reads.
pub const VERSION: i64 = 1;

/// The file name inside the state directory.
pub const FILE_NAME: &str = "state.toml";

/// The most that is read: a hand-edited or hostile file cannot make auspex read without bound.
/// A fleet's freezes are a few lines.
pub const MAX_BYTES: u64 = 1 << 20;

/// How long [`FreezeStore::set`] waits for another auspex's write to finish.
pub const LOCK_WAIT: Duration = Duration::from_secs(2);

const HEADER: &str = "# auspex's own state, written by `f` in the TUI. Safe to delete: freezes made with `f`\n\
                      # are then forgotten. Freezes in config.toml (machines.<m>.frozen) are not kept here.\n";

/// Why the state could not be read or changed. Every message names the file and says what to
/// do, because it is shown to the operator as the reason freezes are unknown.
#[derive(Debug)]
pub enum Error {
    /// The directory or file is not one we may use (a symlink, someone else's, too open, not a
    /// regular file, a file system that refused).
    Unusable(String),
    /// The file is not valid: not TOML, not UTF-8, too big, a wrong type, a bad machine name,
    /// or no version. It was not changed.
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
    /// The file was written by a newer auspex. It was not changed.
    Newer {
        /// The file.
        path: PathBuf,
        /// The `version` it holds.
        found: i64,
    },
    /// Another auspex held the write lock for longer than [`LOCK_WAIT`]. Nothing was written.
    Busy(PathBuf),
    /// Anything else the operating system said no to.
    Io(String),
    /// A machine name that is not a plain name was passed to [`FreezeStore::set`].
    BadName(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unusable(why) | Self::Io(why) => f.write_str(why),
            Self::Corrupt { path, why } => write!(
                f,
                "the state file {} is not valid ({why}); not using or changing it. Fix or delete \
                 it (freezes made with f are then forgotten)",
                path.display()
            ),
            Self::Newer { path, found } => write!(
                f,
                "the state file {} was written by a newer auspex (version {found}, this one \
                 reads {VERSION}); not using or changing it. Update auspex, or delete the file \
                 (freezes made with f are then forgotten)",
                path.display()
            ),
            Self::Busy(path) => write!(
                f,
                "another auspex is writing the state file {}; try again",
                path.display()
            ),
            Self::BadName(name) => write!(f, "{name:?} is not a plain machine name"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

/// Where the state file lives by default.
///
/// `$XDG_STATE_HOME/auspex/state.toml`, else `~/.local/state/auspex/state.toml`; `None` when
/// neither `XDG_STATE_HOME` nor `HOME` is an absolute path.
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

/// The key a fleet's freezes are filed under: its canonical path.
///
/// `flake` as configured (`--flake`, `CLAN_DIR`, `flake` in the config), with a leading `path:`
/// or `git+file://` dropped; when that names something on this machine it is resolved through
/// symlinks and `.`, so `.`, `~/clan/fleet`, `/home/me/clan/fleet`, `path:/home/me/clan/fleet`
/// and `git+file:///home/me/clan/fleet` are one fleet. Anything else (a flake reference that is
/// not a plain local path: a remote one, or one with a query) is used as written, and two
/// different references are two fleets.
///
/// **A fleet that is moved or renamed is another fleet:** the key is its path, so the freezes
/// filed under the old one are not found (and not lost: they are still in the file). The README
/// says so; there is no migration, because the file cannot tell a moved fleet from a second one.
#[must_use]
pub fn fleet_key(flake: &str) -> String {
    let local = local_path(flake).unwrap_or(flake);
    std::fs::canonicalize(local)
        .map_or_else(|_| flake.to_owned(), |p| p.to_string_lossy().into_owned())
}

/// The directory a reference names on this machine, when it is a plain one: `path:/x`,
/// `git+file:///x` or `git+file:/x`. A reference with a query (`?dir=sub`, `?ref=…`) is not
/// one, and needs no rule of its own: the text with the query is no path that exists, so
/// `canonicalize` fails and the reference is used as written.
fn local_path(flake: &str) -> Option<&str> {
    if let Some(rest) = flake.strip_prefix("path:") {
        return Some(rest);
    }
    let rest = flake.strip_prefix("git+file:")?;
    // `git+file://<host>/<path>`: only an empty host is this machine.
    match rest.strip_prefix("//") {
        Some(path) if path.starts_with('/') => Some(path),
        Some(_) => None,
        None => Some(rest),
    }
}

/// One fleet's freezes in one state file. Cheap to clone; every call opens the file afresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreezeStore {
    path: PathBuf,
    fleet: String,
}

impl FreezeStore {
    /// The store for the fleet filed under `fleet` ([`fleet_key`]) in the file at `path`.
    ///
    /// Nothing is read or created yet.
    #[must_use]
    pub const fn new(path: PathBuf, fleet: String) -> Self {
        Self { path, fleet }
    }

    /// The state file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The machines frozen in this fleet. An absent file is the empty set.
    ///
    /// # Errors
    ///
    /// See [`Error`]. The caller must treat freezes as unknown, not as empty.
    pub async fn load(&self) -> Result<BTreeSet<String>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.load_blocking())
            .await
            .map_err(|e| Error::Io(format!("the state file reader stopped: {e}")))?
    }

    /// Freezes (`frozen`) or unfreezes `machine`, and returns every machine frozen in this
    /// fleet afterwards. The file is created (directory 0700, file 0600) when missing.
    ///
    /// # Errors
    ///
    /// See [`Error`]. Nothing was written, except that a failure after the temporary file was
    /// created may leave it behind (the next write replaces it).
    pub async fn set(&self, machine: &str, frozen: bool) -> Result<BTreeSet<String>> {
        self.set_with(machine, frozen, &BTreeSet::new()).await
    }

    /// [`FreezeStore::set`], and freezes every machine of `also_frozen` in the same write: the
    /// freezes the operator made whose own save failed, so the first write that works keeps them
    /// too. Nothing is written if any of the names is not a plain machine name.
    ///
    /// # Errors
    ///
    /// See [`Error`].
    pub async fn set_with(
        &self,
        machine: &str,
        frozen: bool,
        also_frozen: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        let this = self.clone();
        let machine = machine.to_owned();
        let also = also_frozen.clone();
        tokio::task::spawn_blocking(move || this.set_with_blocking(&machine, frozen, &also))
            .await
            .map_err(|e| Error::Io(format!("the state file writer stopped: {e}")))?
    }

    /// [`FreezeStore::load`], on the calling thread.
    ///
    /// # Errors
    ///
    /// See [`Error`].
    pub fn load_blocking(&self) -> Result<BTreeSet<String>> {
        #[cfg(test)]
        forbid_default_location_in_tests(&self.path, default_path().as_deref());
        let Some(dir) = self.existing_dir()? else {
            return Ok(BTreeSet::new());
        };
        let table = self.read(&dir)?;
        self.frozen_of(table.as_ref())
    }

    /// [`FreezeStore::set`], on the calling thread.
    ///
    /// # Errors
    ///
    /// See [`Error`].
    pub fn set_blocking(&self, machine: &str, frozen: bool) -> Result<BTreeSet<String>> {
        self.set_with_blocking(machine, frozen, &BTreeSet::new())
    }

    /// [`FreezeStore::set_with`], on the calling thread.
    ///
    /// # Errors
    ///
    /// See [`Error`].
    pub fn set_with_blocking(
        &self,
        machine: &str,
        frozen: bool,
        also_frozen: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        #[cfg(test)]
        forbid_default_location_in_tests(&self.path, default_path().as_deref());
        if let Some(bad) = std::iter::once(machine)
            .chain(also_frozen.iter().map(String::as_str))
            .find(|n| !is_plain_name(n))
        {
            return Err(Error::BadName(bad.to_owned()));
        }
        let (dir, dir_path) = self.prepare_dir()?;
        let _lock = self.lock(&dir)?;
        let mut table = self.read(&dir)?.unwrap_or_else(|| {
            let mut t = Table::new();
            t.insert("version".to_owned(), Value::Integer(VERSION));
            t
        });
        let mut set = self.frozen_of(Some(&table))?;
        set.extend(also_frozen.iter().cloned());
        if frozen {
            set.insert(machine.to_owned());
        } else {
            set.remove(machine);
        }
        self.put_frozen(&mut table, &set)?;
        self.write(&dir, &dir_path, &table)?;
        Ok(set)
    }

    fn name(&self) -> Result<&str> {
        self.path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| {
                Error::Unusable(format!(
                    "the state file path {} has no usable file name",
                    self.path.display()
                ))
            })
    }

    fn corrupt(&self, why: impl Into<String>) -> Error {
        Error::Corrupt {
            path: self.path.clone(),
            why: why.into(),
        }
    }

    /// The state directory opened, or `None` when it does not exist (nothing to read). A
    /// directory that exists must be private.
    fn existing_dir(&self) -> Result<Option<OwnedFd>> {
        let Some(dir) = self.path.parent() else {
            return Err(Error::Unusable(format!(
                "the state file path {} has no directory",
                self.path.display()
            )));
        };
        match std::fs::symlink_metadata(dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(Error::Unusable(format!(
                    "cannot inspect the state directory {}: {e}",
                    dir.display()
                )));
            }
        }
        crate::ssh::prepare_private_dir(dir, "state directory").map_err(Error::Unusable)?;
        safe_fs::open_root(dir).map(Some).map_err(|e| {
            Error::Unusable(format!(
                "cannot open the state directory {}: {}",
                dir.display(),
                describe(&e)
            ))
        })
    }

    /// The state directory, created private when missing, opened.
    fn prepare_dir(&self) -> Result<(OwnedFd, PathBuf)> {
        let Some(dir) = self.path.parent() else {
            return Err(Error::Unusable(format!(
                "the state file path {} has no directory",
                self.path.display()
            )));
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| {
                Error::Unusable(format!(
                    "cannot create the state directory {}: {e}",
                    dir.display()
                ))
            })?;
        crate::ssh::prepare_private_dir(dir, "state directory").map_err(Error::Unusable)?;
        let fd = safe_fs::open_root(dir).map_err(|e| {
            Error::Unusable(format!(
                "cannot open the state directory {}: {}",
                dir.display(),
                describe(&e)
            ))
        })?;
        Ok((fd, dir.to_path_buf()))
    }

    /// An exclusive lock on the directory, held until the returned descriptor is dropped.
    /// `flock` locks belong to the open file description, so two processes (and two threads
    /// that each opened the directory) exclude each other.
    fn lock(&self, dir: &OwnedFd) -> Result<OwnedFd> {
        // A description of its own: `flock` locks belong to the open file description, and a
        // `try_clone` shares the original's, so two calls in one process would not exclude each
        // other.
        let fresh = openat(
            dir,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| Error::Io(format!("cannot lock the state directory: {e}")))?;
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match flock(&fresh, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(fresh),
                Err(rustix::io::Errno::WOULDBLOCK | rustix::io::Errno::INTR) => {
                    if Instant::now() >= deadline {
                        return Err(Error::Busy(self.path.clone()));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(Error::Io(format!("cannot lock the state directory: {e}"))),
            }
        }
    }

    /// The file as a table, or `None` when it does not exist. Checks it is ours to use and
    /// that its version is one this build reads.
    fn read(&self, dir: &OwnedFd) -> Result<Option<Table>> {
        let name = self.name()?;
        let shown = self.path.display();
        let st = match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(e) => {
                return Err(Error::Unusable(format!(
                    "cannot inspect the state file {shown}: {e}"
                )));
            }
        };
        check_file_facts(
            &shown.to_string(),
            FileType::from_raw_mode(st.st_mode),
            st.st_uid,
            rustix::process::getuid().as_raw(),
            st.st_mode,
            st.st_nlink,
        )?;
        let mut bytes = Vec::new();
        safe_fs::read_file(dir, name.as_bytes(), MAX_BYTES, &|| false, &mut |chunk| {
            bytes.extend_from_slice(chunk);
        })
        .map_err(|e| match e {
            ReadError::TooBig => self.corrupt(format!("larger than {MAX_BYTES} bytes")),
            other => Error::Unusable(format!(
                "cannot read the state file {shown}: {}",
                describe(&other)
            )),
        })?;
        let text = String::from_utf8(bytes).map_err(|_| self.corrupt("not UTF-8"))?;
        let table: Table = toml::from_str(&text).map_err(|e| {
            // toml's message quotes the offending line: one line of it is plenty.
            self.corrupt(format!(
                "not TOML: {}",
                e.message().lines().next().unwrap_or("syntax error")
            ))
        })?;
        match table.get("version") {
            Some(Value::Integer(v)) if *v > VERSION => Err(Error::Newer {
                path: self.path.clone(),
                found: *v,
            }),
            Some(Value::Integer(v)) if *v == VERSION => Ok(Some(table)),
            Some(Value::Integer(v)) => Err(self.corrupt(format!("`version` is {v}"))),
            Some(_) => Err(self.corrupt("`version` is not an integer")),
            None => Err(self.corrupt("no `version`")),
        }
    }

    /// This fleet's frozen machines in `table` (`None`: no file, nothing frozen).
    fn frozen_of(&self, table: Option<&Table>) -> Result<BTreeSet<String>> {
        let Some(fleets) = table.and_then(|t| t.get("fleets")) else {
            return Ok(BTreeSet::new());
        };
        let fleets = fleets
            .as_table()
            .ok_or_else(|| self.corrupt("`fleets` is not a table"))?;
        let Some(fleet) = fleets.get(&self.fleet) else {
            return Ok(BTreeSet::new());
        };
        let fleet = fleet
            .as_table()
            .ok_or_else(|| self.corrupt(format!("`fleets.{:?}` is not a table", self.fleet)))?;
        let Some(frozen) = fleet.get("frozen") else {
            return Ok(BTreeSet::new());
        };
        let list = frozen
            .as_array()
            .ok_or_else(|| self.corrupt("`frozen` is not a list"))?;
        list.iter()
            .map(|v| match v.as_str() {
                Some(n) if is_plain_name(n) => Ok(n.to_owned()),
                Some(n) => {
                    Err(self.corrupt(format!("{n:?} in `frozen` is not a plain machine name")))
                }
                None => Err(self.corrupt("`frozen` holds something that is not a string")),
            })
            .collect()
    }

    /// Writes `set` as this fleet's `frozen`, leaving everything else in `table` as it was.
    fn put_frozen(&self, table: &mut Table, set: &BTreeSet<String>) -> Result<()> {
        let fleets = table
            .entry("fleets")
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| self.corrupt("`fleets` is not a table"))?;
        let fleet = fleets
            .entry(self.fleet.clone())
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| self.corrupt(format!("`fleets.{:?}` is not a table", self.fleet)))?;
        if set.is_empty() {
            fleet.remove("frozen");
        } else {
            fleet.insert(
                "frozen".to_owned(),
                Value::Array(set.iter().map(|n| Value::String(n.clone())).collect()),
            );
        }
        Ok(())
    }

    /// Replaces the file with `table`, atomically.
    fn write(&self, dir: &OwnedFd, dir_path: &Path, table: &Table) -> Result<()> {
        let name = self.name()?;
        let body = toml::to_string(table)
            .map_err(|e| Error::Io(format!("cannot encode the state file: {e}")))?;
        let temp = format!(".{name}.{}.tmp", std::process::id());
        let temp_path = dir_path.join(&temp);
        let shown = self.path.display();
        let io = |what: &str, e: &dyn fmt::Display| {
            Error::Io(format!("cannot {what} the state file {shown}: {e}"))
        };
        // A stray file of an earlier run of this very pid is replaced; `O_EXCL` then makes
        // sure the file written is the one created here, never something planted.
        let _ = std::fs::remove_file(&temp_path);
        let fd = openat(
            dir,
            temp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| io("create a temporary file for", &e))?;
        let mut file = std::fs::File::from(fd);
        let written = file
            .write_all(HEADER.as_bytes())
            .and_then(|()| file.write_all(body.as_bytes()))
            .and_then(|()| file.sync_all());
        drop(file);
        if let Err(e) = written {
            let _ = std::fs::remove_file(&temp_path);
            return Err(io("write", &e));
        }
        if let Err(e) = renameat(dir, temp.as_str(), dir, name) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(io("replace", &e));
        }
        // The rename is only as durable as its directory entry.
        fsync(dir).map_err(|e| io("flush the directory of", &e))?;
        Ok(())
    }
}

/// Whether the file with these facts may be used: a regular file (not a symlink), this user's, with
/// no group or other permission bit, and at most one name. A function of the numbers, so each
/// rule is tested without another user's file.
///
/// `mode` and `links` are generic over what converts to the widest width: `Stat`'s `st_mode` is a
/// `u32` on Linux and a `u16` on darwin, and `st_nlink` a `u64` on x86_64 Linux, a `u32` on
/// aarch64 Linux and a `u16` on darwin, so a fixed width here breaks the build on a target the
/// development shell cannot build for (the test `the_rules_take_every_width…` is the guard).
fn check_file_facts(
    shown: &str,
    kind: FileType,
    owner: u32,
    me: u32,
    mode: impl Into<u32>,
    links: impl Into<u64>,
) -> Result<()> {
    let (mode, links) = (mode.into(), links.into());
    match kind {
        FileType::RegularFile => {}
        FileType::Symlink => {
            return Err(Error::Unusable(format!(
                "the state file {shown} is a symlink; refusing to use it"
            )));
        }
        _ => {
            return Err(Error::Unusable(format!(
                "the state file {shown} is not a regular file; refusing to use it"
            )));
        }
    }
    if owner != me {
        return Err(Error::Unusable(format!(
            "the state file {shown} is owned by uid {owner}, not by this user (uid {me}); \
             refusing to use it"
        )));
    }
    let mode = mode & 0o7777;
    if mode & 0o077 != 0 {
        return Err(Error::Unusable(format!(
            "the state file {shown} has mode {mode:04o}, expected 0600; refusing to use it"
        )));
    }
    // More than one name. Not `!= 1`: a writer renaming a new file over this one between our
    // lookup and the kernel reading its attributes leaves the old inode with no names at all
    // (0), which is a file that was just replaced, not a second name. The test that reads
    // while the file is rewritten found it.
    if links > 1 {
        return Err(Error::Unusable(format!(
            "the state file {shown} has {links} hard links; refusing to use it"
        )));
    }
    Ok(())
}

fn describe(e: &ReadError) -> String {
    match e {
        ReadError::Missing => "it is not there".to_owned(),
        ReadError::NotAFile(what) => format!("it is {what}"),
        ReadError::TooBig => "it is too big".to_owned(),
        ReadError::Stopped => "stopped".to_owned(),
        ReadError::Io(e) => e.to_string(),
    }
}

/// Panics if a test is about to touch the real state file. Called from every public blocking
/// entry point in test builds only, so that every test in this module (and any child process
/// it starts, which is this same test binary) is covered, whatever it forgets.
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
        "a test tried to use the real state file {}; every test passes a path under a \
         temporary directory",
        path.display()
    );
}

#[cfg(test)]
mod tests;
