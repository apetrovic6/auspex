//! The evaluation cache, under `$XDG_CACHE_HOME/auspex/` (default `~/.cache/auspex/`).
//!
//! Re-evaluating is no faster warm, so every successful evaluation is kept, keyed by
//! `auspex_domain::eval::cache_key` over inputs gathered here, each under a timeout and the
//! cancellation token.
//!
//! # What is evaluated, and what the key covers
//!
//! **The reference.** [`FlakeRef::new`] turns a plain path inside a git repository into
//! `git+file://<top>/` (with `?dir=<flake directory relative to top>` for a nested flake),
//! and the same reference goes to the machine list (`nix eval`) and to the evaluation
//! (`builtins.getFlake` in nix-eval-jobs). That matters (review finding B1): handed a plain
//! path, `getFlake` builds a `path:` source, which sees untracked files, ignored files and
//! `.git/` and has no `rev`, while the CLI builds a `git+file` one. Now both read the tracked
//! files with their uncommitted worktree changes, and `self.rev`/`self.dirtyRev` are set.
//! A plain path outside any repository becomes `path:`, which is never cached, nor is any
//! other non-git reference or a reference with a query the operator wrote.
//!
//! **The key** is computed from git's view of the same tree, and only when it matches the
//! reference: the flake directory must be the reference's, and git's top level
//! (`rev-parse --show-toplevel`) the one `FlakeRef` found (they can differ: a discovery
//! ceiling, a `.git` that is not a repository). It covers exactly: the cache format version;
//! `HEAD`'s commit id; the bytes of `flake.lock` in the flake directory (or its absence); and,
//! in the domain's `Tree::Dirty { diff }` slot, a digest of the flake directory's path
//! relative to the top level, of the index as `git ls-files -s -v -z` prints it (mode, blob
//! id, stage, path, and the tag that marks assume-unchanged and skip-worktree entries, which
//! change what nix reads; review finding F3), and of every index path's worktree state as
//! nix reads it for a dirty `git+file` flake: a regular file's bytes and owner exec bit, a
//! symlink's target text, or that it is missing or not a regular file. Paths are walked
//! from the top level without following symlinks, regular files are opened non-blocking,
//! at most [`MAX_WORKTREE_BYTES`] are read in all, under [`GitParams::read_timeout`].
//! Untracked and ignored files are not in the key, because nix no longer sees them; the
//! domain's untracked list is passed empty and `Tree::Clean` is never used. What an
//! `--impure` evaluation reads from the environment or from outside the flake is not in
//! the key either: that is the fleet's own choice, and no key can cover it.
//!
//! **A changed tree during a scan** (review finding B2). Every nix-eval-jobs worker reads
//! the tree again when it starts, and workers restart on their memory cap, so a file saved
//! mid-scan can be evaluated under the key of the tree before it. Results are therefore
//! streamed to the caller at once (they are right for the tree nix read) but stored only
//! after the evaluator has exited and a second key, computed then, equals the first;
//! otherwise none of the run's results is stored, with a note. An edit made and undone
//! between the two keys is not seen; that needs a save and a revert within one scan, a
//! window of seconds to a minute, and would need an immutable source (`?rev=` of a clean
//! tree) to close.
//!
//! # Running git safely
//!
//! The repository's own configuration and files are not trusted (review findings F1 and
//! N1). git is asked only what it answers from the index and refs, without reading file
//! contents: the two `rev-parse` commands and `ls-files -s -v -z`; no status, diff, filters,
//! attributes, ignore or exclude files. Every git command runs:
//!
//! - under an address-space limit of [`GIT_MEMORY_LIMIT`], set by util-linux `prlimit`
//!   before it execs git, so the limit holds from git's first instruction (review finding
//!   F4; measured: `ls-files -s` on nixpkgs, about 40,000 files, peaks at 25 MB and needs
//!   about 37 MB of address space). Without `prlimit` on an absolute `PATH` entry, or on
//!   another platform, git is not run and there is no caching;
//! - with a short timeout (2 s by default; the three commands take at most 22 ms on
//!   nixpkgs), and the first failure ends the key;
//! - with `-c core.fsmonitor=false` ([`GIT_HARDENING`]), `GIT_OPTIONAL_LOCKS=0`, and none
//!   of the inherited variables that would point it at another repository or index. The
//!   operator's own global and system configuration apply as usual.
//!
//! What a hostile `.git/` can still cost per refresh is bounded by those two limits
//! (review finding F5): an oversized `HEAD` grows git to the limit (about 417 MB, 0.3 s)
//! before it fails, and a FIFO or endless file in git's config include path costs one
//! timeout. Either way there is no caching, with a note.
//!
//! # Entries
//!
//! `<dir>/eval/<key>/<sha256 of the machine name>.json`, one
//! `auspex_domain::eval::CacheEntry` per file. The name is hashed so that no machine name is
//! ever a path component. Below `<dir>` nothing is followed through a symlink, for reading
//! or writing (review finding F10). Writes go to a uniquely named temporary file in the
//! same directory and are renamed into place, so a reader sees a whole entry or none. A
//! failed write is a note, never an error; a corrupt, foreign, oversized or mismatched entry
//! is a miss; only successes are cached. A hit's display-only `attr` is reset to the
//! machine, so a planted entry cannot carry escape sequences to the screen.
//!
//! Nothing removes old keys' directories yet.

use std::ffi::OsStr;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use auspex_domain::eval::{
    CacheEntry, CacheKey, CacheKeyInputs, EvalOutcome, EvalSuccess, Tree, cache_key,
};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::nix::{EvalItem, EvalMachine, EvalParams, FlakeRef, evaluate};
use crate::process::{End, Limits, OnExit, command, describe_status, run, spawn_error};
use crate::safe_fs::{ReadError, kind, open_dir, open_root, read_file, walk};
use crate::text::{last_line, lossy};

/// The default git program, looked up on `PATH`.
pub const DEFAULT_GIT: &str = "git";

/// The most worktree bytes hashed for a key: 256 MiB. A fleet repository is a few megabytes
/// of Nix; past the cap there is no caching for that run.
pub const MAX_WORKTREE_BYTES: u64 = 256 * 1024 * 1024;

/// The address-space limit put on every git command, in bytes: 512 MiB.
pub const GIT_MEMORY_LIMIT: u64 = 512 << 20;

/// The largest `flake.lock` that is keyed: 16 MiB.
pub const MAX_LOCK_BYTES: u64 = 16 * 1024 * 1024;

/// The largest index listing read from git: 64 MiB, including stage and flag metadata.
const MAX_LIST_BYTES: usize = 64 * 1024 * 1024;

/// A cache file larger than this is not an entry (a real one is under a kilobyte).
const MAX_ENTRY_BYTES: u64 = 1024 * 1024;

/// Repository configuration overridden for every git command here: `core.fsmonitor`, a
/// hook command that `ls-files` would run to list changed files (a test plants one).
///
/// That is all the remaining commands need. Round 1 also overrode `core.attributesFile`,
/// `core.excludesFile`, `core.hooksPath` and `core.untrackedCache`; none of `rev-parse` and
/// `ls-files -s` reads attributes or exclude files, runs hooks, or writes the index, so
/// those overrides guarded nothing and no test could fail without them (review round 2).
pub const GIT_HARDENING: &[&str] = &["-c", "core.fsmonitor=false"];

/// Inherited variables that would point git at another repository, index or object
/// store. `GIT_CEILING_DIRECTORIES` is kept: it is the operator's own limit on discovery.
const GIT_ENV_REMOVED: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
];

/// The cache directory from the environment: `$XDG_CACHE_HOME/auspex` when that is an
/// absolute path, else `$HOME/.cache/auspex`; `None` when neither is usable. The one place
/// this module reads the environment.
#[must_use]
pub fn default_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_CACHE_HOME").filter(|d| Path::new(d).is_absolute()) {
        return Some(Path::new(&d).join("auspex"));
    }
    std::env::var_os("HOME")
        .filter(|h| Path::new(h).is_absolute())
        .map(|h| Path::new(&h).join(".cache").join("auspex"))
}

/// How to compute the key.
#[derive(Debug, Clone, Copy)]
pub struct GitParams<'a> {
    /// The program: [`DEFAULT_GIT`] (resolved on `PATH`) or an absolute path.
    pub program: &'a OsStr,
    /// Each git command's own timeout.
    pub timeout: Duration,
    /// The worktree hash's timeout (reading up to [`MAX_WORKTREE_BYTES`]).
    pub read_timeout: Duration,
}

impl GitParams<'_> {
    /// `git` from `PATH`, 2 s per command, 30 s for reading the worktree.
    #[must_use]
    pub fn new() -> Self {
        Self {
            program: OsStr::new(DEFAULT_GIT),
            timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(30),
        }
    }
}

impl Default for GitParams<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a failed git looks like it ran into [`GIT_MEMORY_LIMIT`]: git's own "Out of
/// memory" or `ENOMEM` messages, or a crash (a failed `mmap` of a stack or library).
fn out_of_memory(stderr: &str, status: std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt as _;
    stderr.contains("Out of memory")
        || stderr.contains("Cannot allocate memory")
        || matches!(status.signal(), Some(6 | 7 | 11))
}

async fn git(
    params: &GitParams<'_>,
    dir: &Path,
    args: &[&str],
    cap: usize,
    cancel: &CancellationToken,
) -> Result<Result<Vec<u8>, String>, Cancelled> {
    let what = format!("git {}", args.first().copied().unwrap_or_default());
    let mut cmd = match command(params.program) {
        Ok(c) => c,
        Err(e) => return Ok(Err(spawn_error(&what, params.program, &e))),
    };
    let deadline = Instant::now() + params.timeout;
    // `GIT_OPTIONAL_LOCKS=0` guards nothing today (none of these commands refreshes or
    // writes the index, and no test can tell); it is kept because it is free and because a
    // git that one day refreshes the index during `ls-files` would otherwise write into the
    // operator's repository.
    cmd.current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(GIT_HARDENING)
        .args(args);
    for v in GIT_ENV_REMOVED {
        cmd.env_remove(v);
    }
    let limits = Limits {
        stdout: cap,
        stderr: 16 * 1024,
        memory: Some(GIT_MEMORY_LIMIT),
    };
    let ran = match run(cmd, None, OnExit::KillGroup, limits, deadline, cancel).await {
        Ok(r) => r,
        Err(e) => return Ok(Err(spawn_error(&what, params.program, &e))),
    };
    Ok(match ran.end {
        End::Cancelled => return Err(Cancelled),
        End::TimedOut => Err(format!("{what} timed out after {:?}", params.timeout)),
        End::Overflow => Err(format!("{what} printed more than {cap} bytes")),
        End::Exited(s) if s.success() => Ok(ran.stdout),
        End::Exited(s) => {
            let stderr = lossy(&ran.stderr);
            let why = last_line(&stderr).unwrap_or_default();
            if out_of_memory(&stderr, s) {
                Err(format!(
                    "{what} ran into auspex's memory limit for git ({} MiB): {why}",
                    GIT_MEMORY_LIMIT >> 20
                ))
            } else {
                Err(format!("{what} {}: {why}", describe_status(Some(s))))
            }
        }
    })
}

/// The paths of `git ls-files -s -v -z` output, or why the index cannot be keyed.
fn index_paths(index: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut paths = Vec::new();
    for rec in index.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let Some(tab) = rec.iter().position(|&b| b == b'\t') else {
            return Err("git ls-files printed a record without a path".to_owned());
        };
        // `-v` prefixes the stage record with its status letter and a space. The raw
        // record is hashed too: assume-unchanged and skip-worktree affect Nix's dirty
        // decision even when every worktree byte and blob id stays the same.
        let Some(meta) = rec[..tab].get(2..) else {
            return Err("git ls-files printed invalid index metadata".to_owned());
        };
        let path = &rec[tab + 1..];
        if meta.starts_with(b"160000 ") {
            return Err(format!(
                "the repository tracks a submodule ({}), whose worktree the cache key does \
                 not cover",
                last_line(&lossy(path)).unwrap_or_default()
            ));
        }
        paths.push(path);
    }
    Ok(paths)
}

/// Gathers the key's inputs for the flake in `flake_dir` and computes the key. The module
/// documentation says exactly what it covers.
///
/// The result's inner `Err` is the reason there is no caching for this run.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired; any running git has been killed and reaped, and the
/// worktree hash stops at its next chunk.
pub async fn key_for(
    flake_dir: &Path,
    params: &GitParams<'_>,
    cancel: &CancellationToken,
) -> Result<Result<CacheKey, String>, Cancelled> {
    Ok(key_and_top(flake_dir, params, cancel)
        .await?
        .map(|(key, _)| key))
}

/// [`key_for`], also returning the repository top level git reported, so the caller can
/// check that the reference nix evaluates names the same repository.
async fn key_and_top(
    flake_dir: &Path,
    params: &GitParams<'_>,
    cancel: &CancellationToken,
) -> Result<Result<(CacheKey, PathBuf), String>, Cancelled> {
    let top = match git(
        params,
        flake_dir,
        &["rev-parse", "--show-toplevel"],
        4096,
        cancel,
    )
    .await?
    {
        Ok(t) => t,
        Err(e) => return Ok(Err(format!("not a git repository ({e})"))),
    };
    let top = {
        use std::os::unix::ffi::OsStrExt as _;
        PathBuf::from(OsStr::from_bytes(top.strip_suffix(b"\n").unwrap_or(&top)))
    };
    let head = match git(
        params,
        &top,
        &["rev-parse", "--verify", "HEAD"],
        4096,
        cancel,
    )
    .await?
    {
        Ok(h) => lossy(&h),
        Err(e) => return Ok(Err(format!("not a git repository with a commit ({e})"))),
    };
    let index = match git(
        params,
        &top,
        &["ls-files", "-s", "-v", "-z"],
        MAX_LIST_BYTES,
        cancel,
    )
    .await?
    {
        Ok(t) => t,
        Err(e) => return Ok(Err(e)),
    };
    // Two flakes in one repository must not share a key (review finding N2e).
    let rel = match (
        std::fs::canonicalize(flake_dir),
        std::fs::canonicalize(&top),
    ) {
        (Ok(f), Ok(t)) => match f.strip_prefix(&t) {
            Ok(r) => r.to_path_buf(),
            Err(_) => {
                return Ok(Err(
                    "the flake directory is outside its repository".to_owned()
                ));
            }
        },
        _ => return Ok(Err("cannot resolve the flake directory".to_owned())),
    };

    // The reads: off the async threads, stopped by the token or the deadline.
    let token = cancel.clone();
    let deadline = std::time::Instant::now() + params.read_timeout;
    let flake_dir = flake_dir.to_path_buf();
    let reported_top = top.clone();
    let job = tokio::task::spawn_blocking(move || {
        let stop = || token.is_cancelled() || std::time::Instant::now() > deadline;
        let paths = index_paths(&index).map_err(Stop::NoCache)?;
        let lock = read_lock(&flake_dir, &stop)?;
        let digest = worktree_digest(&top, &rel, &index, &paths, &stop)?;
        Ok::<_, Stop>((lock, digest))
    });
    let (lock, digest) = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Cancelled),
        // The job checks the deadline between reads; this covers a read or open(2) that
        // never returns. The job is abandoned (its thread finishes when the call does).
        () = tokio::time::sleep(params.read_timeout + Duration::from_millis(500)) => {
            return Ok(Err("reading the worktree timed out".to_owned()));
        }
        r = job => match r {
            Ok(Ok(v)) => v,
            Ok(Err(Stop::Stopped)) if cancel.is_cancelled() => return Err(Cancelled),
            Ok(Err(Stop::Stopped)) => {
                return Ok(Err("reading the worktree timed out".to_owned()));
            }
            Ok(Err(Stop::NoCache(why))) => return Ok(Err(why)),
            Err(e) => return Ok(Err(format!("reading the worktree failed: {e}"))),
        },
    };
    let key = cache_key(&CacheKeyInputs {
        head: &head,
        flake_lock: lock.as_deref(),
        tree: Tree::Dirty {
            diff: &digest,
            // Untracked names are not in the key: see the module documentation.
            untracked: &[],
        },
    });
    Ok(Ok((key, reported_top)))
}

enum Stop {
    Stopped,
    NoCache(String),
}

/// `flake.lock` in `flake_dir`: `None` when absent; refused when it is a symlink, not a
/// regular file, or larger than [`MAX_LOCK_BYTES`].
fn read_lock(flake_dir: &Path, stop: &dyn Fn() -> bool) -> Result<Option<Vec<u8>>, Stop> {
    let no = |why: &str| Stop::NoCache(format!("flake.lock {why}"));
    let root = open_root(flake_dir)
        .map_err(|e| Stop::NoCache(format!("cannot open the flake directory: {e:?}")))?;
    let mut bytes = Vec::new();
    match read_file(&root, b"flake.lock", MAX_LOCK_BYTES, stop, &mut |b| {
        bytes.extend_from_slice(b);
    }) {
        Ok(_) => Ok(Some(bytes)),
        Err(ReadError::Missing) => Ok(None),
        Err(ReadError::NotAFile(what)) => Err(no(&format!("is {what}; refusing to read it"))),
        Err(ReadError::TooBig) => Err(no(&format!("is larger than {MAX_LOCK_BYTES} bytes"))),
        Err(ReadError::Stopped) => Err(Stop::Stopped),
        Err(ReadError::Io(e)) => Err(no(&format!("cannot be read: {e}"))),
    }
}

/// Feeds one tagged, length-prefixed field, so no two inputs share a byte stream.
fn field(h: &mut Sha256, tag: u8, bytes: &[u8]) {
    h.update([tag]);
    h.update((bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}

/// The digest of every tracked path's worktree state under `top`.
fn worktree_digest(
    top: &Path,
    rel: &Path,
    index: &[u8],
    paths: &[&[u8]],
    stop: &dyn Fn() -> bool,
) -> Result<Vec<u8>, Stop> {
    use std::os::unix::ffi::OsStrExt as _;
    let root =
        open_root(top).map_err(|e| Stop::NoCache(format!("cannot open the worktree: {e:?}")))?;
    let mut paths = paths.to_vec();
    // Unmerged entries are listed once per stage.
    paths.sort_unstable();
    paths.dedup();
    let mut h = Sha256::new();
    h.update(b"auspex worktree digest v2\0");
    // Which flake in the repository (N2e), and the index itself, staged state included
    // (N2b).
    field(&mut h, b'r', rel.as_os_str().as_bytes());
    field(&mut h, b'i', &Sha256::digest(index));
    let mut left = MAX_WORKTREE_BYTES;
    for p in paths {
        if stop() {
            return Err(Stop::Stopped);
        }
        field(&mut h, b'p', p);
        let Ok((dir, name)) = walk(&root, p) else {
            // A directory on the way is missing or a symlink: the file is not there for
            // nix either.
            field(&mut h, b'x', &[]);
            continue;
        };
        match kind(&dir, name) {
            Err(_) => field(&mut h, b'd', &[]),
            Ok(FileType::Symlink) => {
                let target = rustix::fs::readlinkat(&dir, name, Vec::new())
                    .map(std::ffi::CString::into_bytes)
                    .unwrap_or_default();
                field(&mut h, b'l', &target);
            }
            Ok(FileType::RegularFile) => {
                let mut fh = Sha256::new();
                let mut n: u64 = 0;
                match read_file(&dir, name, left, stop, &mut |b| {
                    n += b.len() as u64;
                    fh.update(b);
                }) {
                    Ok(exec) => {
                        left -= n;
                        field(&mut h, if exec { b'X' } else { b'f' }, &fh.finalize());
                    }
                    Err(ReadError::TooBig) => {
                        return Err(Stop::NoCache(format!(
                            "the tracked files are larger than {MAX_WORKTREE_BYTES} bytes"
                        )));
                    }
                    Err(ReadError::Stopped) => return Err(Stop::Stopped),
                    // Replaced between the stat and the open, or unreadable.
                    Err(_) => field(&mut h, b'e', &[]),
                }
            }
            // A submodule (a gitlink is a directory), a FIFO, a device: its kind only.
            Ok(other) => field(&mut h, b'o', &other.as_raw_mode().to_be_bytes()),
        }
    }
    Ok(h.finalize().to_vec())
}

fn file_name(machine: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(machine.as_bytes());
    let mut name = digest.iter().fold(String::with_capacity(69), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    name.push_str(".json");
    name
}

/// Where `machine`'s entry under `key` lives.
#[must_use]
pub fn entry_path(cache_dir: &Path, key: &CacheKey, machine: &str) -> PathBuf {
    cache_dir
        .join("eval")
        .join(key.as_str())
        .join(file_name(machine))
}

/// The key's directory, opened without following symlinks below `cache_dir`.
fn key_dir(cache_dir: &Path, key: &CacheKey, create: bool) -> Result<OwnedFd, ReadError> {
    if create {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(cache_dir)
            .map_err(ReadError::Io)?;
    }
    let root = open_root(cache_dir)?;
    let eval = open_dir(&root, b"eval", create)?;
    open_dir(&eval, key.as_str().as_bytes(), create)
}

/// `machine`'s cached result under `key`, or `None` (missing, unreadable, corrupt, a
/// symlink anywhere below `cache_dir`, over 1 MiB, another key's or machine's, another
/// format version).
pub async fn lookup(cache_dir: &Path, key: &CacheKey, machine: &str) -> Option<EvalSuccess> {
    let (dir, key, machine) = (cache_dir.to_path_buf(), key.clone(), machine.to_owned());
    tokio::task::spawn_blocking(move || {
        let kd = key_dir(&dir, &key, false).ok()?;
        let mut bytes = Vec::new();
        read_file(
            &kd,
            file_name(&machine).as_bytes(),
            MAX_ENTRY_BYTES,
            &|| false,
            &mut |b| bytes.extend_from_slice(b),
        )
        .ok()?;
        let mut hit = CacheEntry::read(&bytes, &key, &machine).ok()?;
        hit.attr = machine;
        Some(hit)
    })
    .await
    .ok()
    .flatten()
}

static WRITES: AtomicU64 = AtomicU64::new(0);

/// Writes `machine`'s result under `key`: a temporary file in the same directory, then a
/// rename, with no symlink followed below `cache_dir`.
///
/// # Errors
///
/// Why it could not be written. The caller reports it as a note and carries on.
pub async fn store(
    cache_dir: &Path,
    key: &CacheKey,
    machine: &str,
    result: &EvalSuccess,
) -> Result<(), String> {
    let bytes = CacheEntry::new(key.clone(), machine.to_owned(), result.clone()).to_json();
    let (dir, key, name) = (cache_dir.to_path_buf(), key.clone(), file_name(machine));
    tokio::task::spawn_blocking(move || write_atomically(&dir, &key, &name, bytes.as_bytes()))
        .await
        .map_err(|e| format!("cache write task failed: {e}"))?
        .map_err(|e| format!("cannot write the eval cache: {e}"))
}

fn write_atomically(dir: &Path, key: &CacheKey, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let kd = key_dir(dir, key, true).map_err(|e| match e {
        ReadError::Io(e) => e,
        ReadError::NotAFile(what) => {
            std::io::Error::other(format!("{what} in the cache directory"))
        }
        other => std::io::Error::other(format!("{other:?}")),
    })?;
    // Unique per process (pid) and per write (counter): two writers, in this process or
    // two auspex processes, never share a temporary file.
    let tmp = format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    );
    let result = (|| {
        let fd = rustix::fs::openat(
            &kd,
            tmp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let mut f = std::fs::File::from(fd);
        f.write_all(bytes)?;
        f.sync_all()?;
        rustix::fs::renameat(&kd, tmp.as_str(), &kd, name)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&kd, tmp.as_str(), AtFlags::empty());
    }
    result
}

/// Where and how to cache.
#[derive(Debug, Clone, Copy)]
pub struct CacheParams<'a> {
    /// The cache directory ([`default_dir`]), or `None` for no caching.
    pub cache_dir: Option<&'a Path>,
    /// The flake's directory, where git runs and `flake.lock` is read. Its canonical
    /// path must match the evaluated local worktree; a mismatch disables caching.
    pub flake_dir: &'a Path,
    /// How to run git.
    pub git: GitParams<'a>,
}

/// [`evaluate`] with the cache in front. Serves the core's `Effect::Evaluate`.
///
/// Computes the key; looks each machine up; sends every hit at once as an
/// [`EvalItem::Machine`] with `from_cache: true`; evaluates only the misses, sending each
/// result as it arrives; keeps successes until a second key confirms the tree is unchanged;
/// and ends with the evaluator's [`EvalItem::Finished`] (or no evaluation when all hit).
/// When there is no caching (no directory, a non-git flake reference, not a git repository,
/// git failed), one [`EvalItem::Note`] says why and every machine is evaluated. A failed
/// cache write is a note too.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired or `tx`'s receiver was dropped, as for [`evaluate`]:
/// the evaluator's process group has been killed and reaped, and no `Finished` is sent.
pub async fn evaluate_cached(
    flake: &FlakeRef,
    machines: &[EvalMachine],
    eval: &EvalParams<'_>,
    cache: &CacheParams<'_>,
    cancel: &CancellationToken,
    tx: UnboundedSender<EvalItem>,
) -> Result<(), Cancelled> {
    let keyed = match cache_key_or_why(flake, cache, cancel, &tx).await? {
        Ok(k) => Some(k),
        Err(why) => {
            let _ = tx.send(EvalItem::Note(format!(
                "eval cache off for this run: {why}"
            )));
            None
        }
    };
    evaluate_with(flake, machines, eval, cache, cancel, tx, keyed).await
}

/// The key and the cache directory, or why there is no caching for this run.
async fn cache_key_or_why<'c>(
    flake: &FlakeRef,
    cache: &CacheParams<'c>,
    cancel: &CancellationToken,
    tx: &UnboundedSender<EvalItem>,
) -> Result<Result<(&'c Path, CacheKey), String>, Cancelled> {
    Ok(match cache.cache_dir {
        None => Err("no cache directory".to_owned()),
        Some(_) if flake.has_query() => Err(
            "the flake reference has a query (?ref=, ?rev=, ?dir=), so nix reads something \
             other than the worktree the cache key covers"
                .to_owned(),
        ),
        Some(_) if !flake.is_git() => Err(format!(
            "a `{}:` flake reference is not a git one, so untracked and ignored files \
             reach the evaluation and their contents are not in the cache key",
            flake.scheme()
        )),
        Some(_)
            if flake.worktree_dir() != std::fs::canonicalize(cache.flake_dir).ok().as_deref() =>
        {
            Err("the cache flake directory does not match the evaluated worktree".to_owned())
        }
        Some(dir) => {
            // A child token also handles a dropped receiver: git is killed and reaped;
            // blocking hashing notices cancellation at its next chunk. A filesystem
            // call already in progress can outlive this future.
            let key_cancel = cancel.child_token();
            let keying = key_and_top(cache.flake_dir, &cache.git, &key_cancel);
            tokio::pin!(keying);
            tokio::select! {
                biased;
                () = tx.closed() => {
                    key_cancel.cancel();
                    let _ = keying.await;
                    return Err(Cancelled);
                }
                k = &mut keying => match k? {
                    // The reference nix evaluates was built by `FlakeRef::new` from the first
                    // ancestor with a `.git`; the key from git's own discovery. Where they
                    // disagree (a ceiling, a `.git` that is not a repository, `core.worktree`)
                    // the two describe different trees, so there is no caching.
                    Ok((key, top))
                        if std::fs::canonicalize(&top).ok().as_deref() == flake.repo_top() =>
                    {
                        Ok((dir, key))
                    }
                    Ok(_) => Err(
                        "git's repository top level is not the one the flake reference names"
                            .to_owned(),
                    ),
                    Err(why) => Err(why),
                },
            }
        }
    })
}

/// Streams misses immediately, but publishes cache entries only after rechecking the key.
async fn evaluate_with(
    flake: &FlakeRef,
    machines: &[EvalMachine],
    eval: &EvalParams<'_>,
    cache: &CacheParams<'_>,
    cancel: &CancellationToken,
    tx: UnboundedSender<EvalItem>,
    keyed: Option<(&Path, CacheKey)>,
) -> Result<(), Cancelled> {
    let mut misses = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for m in machines {
        if !seen.insert(m.name.as_str()) {
            continue;
        }
        let hit = match &keyed {
            Some((dir, key)) => lookup(dir, key, &m.name).await,
            None => None,
        };
        match hit {
            Some(result) => {
                if tx
                    .send(EvalItem::Machine {
                        machine: m.name.clone(),
                        outcome: EvalOutcome::Success(result),
                        from_cache: true,
                    })
                    .is_err()
                {
                    return Err(Cancelled);
                }
            }
            None => misses.push(m.clone()),
        }
    }
    if cancel.is_cancelled() || tx.is_closed() {
        return Err(Cancelled);
    }
    if misses.is_empty() {
        let _ = tx.send(EvalItem::Finished { error: None });
        return Ok(());
    }
    let Some((_, key)) = keyed else {
        return evaluate(flake, &misses, eval, cancel, tx).await;
    };

    let (inner_tx, mut inner_rx) = unbounded_channel();
    // `move`, so that returning drops `inner_rx`, which `evaluate` sees as its own receiver
    // closing: it then kills the evaluator. The outer receiver is watched here directly
    // (review finding F2): waiting for the next `send` to fail would wait for nix-eval-jobs
    // to print, which a stuck worker never does.
    let output = tx.clone();
    let forward = async move {
        let mut pending = Vec::new();
        let mut finished = None;
        loop {
            let item = tokio::select! {
                biased;
                () = output.closed() => return None,
                item = inner_rx.recv() => match item {
                    Some(i) => i,
                    None => return Some((pending, finished)),
                },
            };
            match item {
                EvalItem::Finished { error } => finished = Some(EvalItem::Finished { error }),
                item => {
                    if let EvalItem::Machine {
                        machine,
                        outcome: EvalOutcome::Success(result),
                        from_cache: false,
                    } = &item
                    {
                        pending.push((machine.clone(), result.clone()));
                    }
                    if output.send(item).is_err() {
                        return None;
                    }
                }
            }
        }
    };
    let (result, forwarded) =
        tokio::join!(evaluate(flake, &misses, eval, cancel, inner_tx), forward);
    result?;
    let Some((pending, finished)) = forwarded else {
        return Err(Cancelled);
    };
    if cancel.is_cancelled() || tx.is_closed() {
        return Err(Cancelled);
    }
    if !pending.is_empty() {
        store_checked(flake, cache, cancel, &tx, &key, pending).await?;
    }
    if let Some(finished) = finished {
        tx.send(finished).map_err(|_| Cancelled)?;
    }
    Ok(())
}

/// Publish only results whose before/after keys agree.
async fn store_checked(
    flake: &FlakeRef,
    cache: &CacheParams<'_>,
    cancel: &CancellationToken,
    tx: &UnboundedSender<EvalItem>,
    key: &CacheKey,
    pending: Vec<(String, EvalSuccess)>,
) -> Result<(), Cancelled> {
    match cache_key_or_why(flake, cache, cancel, tx).await? {
        Ok((dir, after)) if after == *key => {
            for (machine, result) in pending {
                if cancel.is_cancelled() || tx.is_closed() {
                    return Err(Cancelled);
                }
                if let Err(e) = store(dir, key, &machine, &result).await {
                    let _ = tx.send(EvalItem::Note(format!("{machine}: {e}")));
                }
            }
        }
        after => {
            let why = match after {
                Ok(_) => "the worktree changed during evaluation".to_owned(),
                Err(why) => why,
            };
            let _ = tx.send(EvalItem::Note(format!("eval results not cached: {why}")));
        }
    }
    Ok(())
}
