//! The evaluation cache, under `$XDG_CACHE_HOME/auspex/` (default `~/.cache/auspex/`).
//!
//! Re-evaluating is no faster warm, so every successful evaluation is kept, keyed by
//! `auspex_domain::eval::cache_key` over inputs gathered here, each under a timeout and the
//! cancellation token.
//!
//! # The key, and why git compares nothing
//!
//! The fleet repository's own git configuration is not trusted (review finding F1): a
//! `.git/config` that came with an unpacked archive or a synced folder can name filter
//! drivers (`filter.<x>.clean`, `filter.<x>.process`) that `git status` and `git diff` run
//! on any stat-dirty file, and `core.attributesFile = /dev/zero` makes them read an endless
//! line into memory. So git is asked only for things it answers from the index and refs,
//! without reading file contents through attributes:
//!
//! - `git rev-parse --show-toplevel` and `git rev-parse --verify HEAD`;
//! - `git ls-files -z`: the tracked paths, from the index;
//! - `git ls-files --others --exclude-standard -z`: the untracked names, from a directory
//!   walk that reads `.gitignore` files and nothing else (no filters, no attributes).
//!
//! Every git command also gets [`GIT_HARDENING`] (no fsmonitor, no attributes or excludes
//! file, no hooks, no untracked cache), no pager, no optional locks, and none of the
//! inherited `GIT_DIR`-style variables.
//!
//! The worktree is then hashed **in Rust**, from the bytes nix would read for a dirty
//! `git+file` flake: every tracked path, walked from the top level without following
//! symlinks (a tracked symlink contributes its target text), regular files only (opened
//! non-blocking, so a FIFO cannot hang it), at most [`MAX_WORKTREE_BYTES`] in all. That
//! digest goes into the domain's `Tree::Dirty { diff, .. }` slot, whose name says "diff":
//! the slot is a byte string to hash, and a digest of the worktree is a stricter input than
//! `git diff HEAD` was. `Tree::Clean` is never used, because telling clean from dirty is
//! exactly the comparison that runs filters; a clean tree still gets a stable key, it is
//! only spelled the dirty way. `flake.lock` is read the same safe way (review finding F5).
//!
//! No caching at all, rather than a key that could be wrong, when: the directory is not a
//! git repository with a commit; git fails or times out; the worktree is over the cap; or
//! the flake reference is not a git one (`path:` sees untracked and ignored files, whose
//! contents are not in the key; review finding F11). Each is a note the caller can log.
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

/// The largest `flake.lock` that is keyed: 16 MiB.
pub const MAX_LOCK_BYTES: u64 = 16 * 1024 * 1024;

/// The largest tracked or untracked path list read from git: 16 MiB.
const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;

/// A cache file larger than this is not an entry (a real one is under a kilobyte).
const MAX_ENTRY_BYTES: u64 = 1024 * 1024;

/// Repository configuration overridden for every git command here, because each can run a
/// program or read an arbitrary path:
///
/// - `core.fsmonitor`: a hook command run to list changed files;
/// - `core.attributesFile`: read in full (`/dev/zero` is an endless line);
/// - `core.excludesFile`: read by the untracked listing (a FIFO there hangs until the
///   timeout). This also drops the operator's global excludes from the untracked list,
///   which can only add names to the key;
/// - `core.hooksPath`: where hooks are looked up;
/// - `core.untrackedCache`: written into the index otherwise.
///
/// Filter drivers need no override: none of the remaining commands reads file contents.
pub const GIT_HARDENING: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.attributesFile=/dev/null",
    "-c",
    "core.excludesFile=/dev/null",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.untrackedCache=false",
];

/// Inherited variables that would point git at another repository, index or object store.
const GIT_ENV_REMOVED: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CEILING_DIRECTORIES",
    "GIT_EXTERNAL_DIFF",
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

/// How to run git for the key.
#[derive(Debug, Clone, Copy)]
pub struct GitParams<'a> {
    /// The program: [`DEFAULT_GIT`] (resolved on `PATH`) or an absolute path.
    pub program: &'a OsStr,
    /// Each git command's own timeout, and the worktree hash's.
    pub timeout: Duration,
}

impl GitParams<'_> {
    /// `git` from `PATH`, 10 s per command.
    #[must_use]
    pub fn new() -> Self {
        Self {
            program: OsStr::new(DEFAULT_GIT),
            timeout: Duration::from_secs(10),
        }
    }
}

impl Default for GitParams<'_> {
    fn default() -> Self {
        Self::new()
    }
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
    cmd.current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .arg("--no-pager")
        .args(GIT_HARDENING)
        .args(args);
    for v in GIT_ENV_REMOVED {
        cmd.env_remove(v);
    }
    let limits = Limits {
        stdout: cap,
        stderr: 16 * 1024,
    };
    let deadline = Instant::now() + params.timeout;
    let ran = match run(cmd, None, OnExit::KillGroup, limits, deadline, cancel).await {
        Ok(r) => r,
        Err(e) => return Ok(Err(spawn_error(&what, params.program, &e))),
    };
    Ok(match ran.end {
        End::Cancelled => return Err(Cancelled),
        End::TimedOut => Err(format!("{what} timed out")),
        End::Overflow => Err(format!("{what} printed more than {cap} bytes")),
        End::Exited(s) if s.success() => Ok(ran.stdout),
        End::Exited(s) => {
            let why = last_line(&lossy(&ran.stderr)).unwrap_or_default();
            Err(format!("{what} {}: {why}", describe_status(Some(s))))
        }
    })
}

/// Gathers the key's inputs for the flake in `flake_dir` and computes the key.
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
    let tracked = match git(params, &top, &["ls-files", "-z"], MAX_LIST_BYTES, cancel).await? {
        Ok(t) => t,
        Err(e) => return Ok(Err(e)),
    };
    let untracked = match git(
        params,
        &top,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        MAX_LIST_BYTES,
        cancel,
    )
    .await?
    {
        Ok(u) => u,
        Err(e) => return Ok(Err(e)),
    };
    let untracked: Vec<String> = untracked
        .split(|&b| b == 0)
        .filter(|f| !f.is_empty())
        .map(lossy)
        .collect();

    // The reads: off the async threads, stopped by the token or the deadline.
    let token = cancel.clone();
    let deadline = std::time::Instant::now() + params.timeout;
    let flake_dir = flake_dir.to_path_buf();
    let job = tokio::task::spawn_blocking(move || {
        let stop = || token.is_cancelled() || std::time::Instant::now() > deadline;
        let lock = read_lock(&flake_dir, &stop)?;
        let digest = worktree_digest(&top, &tracked, &stop)?;
        Ok::<_, Stop>((lock, digest))
    });
    let (lock, digest) = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Cancelled),
        // The job checks the deadline between reads; this covers a read or open(2) that
        // never returns. The job is abandoned (its thread finishes when the call does).
        () = tokio::time::sleep(params.timeout + Duration::from_millis(500)) => {
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
    Ok(Ok(cache_key(&CacheKeyInputs {
        head: &head,
        flake_lock: lock.as_deref(),
        tree: Tree::Dirty {
            diff: &digest,
            untracked: &untracked,
        },
    })))
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
fn worktree_digest(top: &Path, tracked: &[u8], stop: &dyn Fn() -> bool) -> Result<Vec<u8>, Stop> {
    let root =
        open_root(top).map_err(|e| Stop::NoCache(format!("cannot open the worktree: {e:?}")))?;
    let mut paths: Vec<&[u8]> = tracked
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    // Unmerged entries are listed once per stage.
    paths.sort_unstable();
    paths.dedup();
    let mut h = Sha256::new();
    h.update(b"auspex worktree digest v1\0");
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
    /// The flake's directory, where git runs and `flake.lock` is read.
    pub flake_dir: &'a Path,
    /// How to run git.
    pub git: GitParams<'a>,
}

/// [`evaluate`] with the cache in front. Serves the core's `Effect::Evaluate`.
///
/// Computes the key; looks each machine up; sends every hit at once as an
/// [`EvalItem::Machine`] with `from_cache: true`; evaluates only the misses, sending each
/// result as it arrives and writing each success through to the cache; and ends with the
/// evaluator's [`EvalItem::Finished`] (or `Finished { error: None }` when everything hit).
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
    let keyed = match cache.cache_dir {
        None => Err("no cache directory".to_owned()),
        Some(_) if !flake.is_git() => Err(format!(
            "a `{}:` flake reference is not a git one, so untracked and ignored files \
             reach the evaluation and their contents are not in the cache key",
            flake.scheme()
        )),
        Some(dir) => tokio::select! {
            biased;
            () = tx.closed() => return Err(Cancelled),
            k = key_for(cache.flake_dir, &cache.git, cancel) => k?.map(|k| (dir, k)),
        },
    };
    let keyed = match keyed {
        Ok(k) => Some(k),
        Err(why) => {
            let _ = tx.send(EvalItem::Note(format!(
                "eval cache off for this run: {why}"
            )));
            None
        }
    };

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
    let Some((dir, key)) = keyed else {
        return evaluate(flake, &misses, eval, cancel, tx).await;
    };

    let (inner_tx, mut inner_rx) = unbounded_channel();
    // `move`, so that returning drops `inner_rx`, which `evaluate` sees as its own receiver
    // closing: it then kills the evaluator. The outer receiver is watched here directly
    // (review finding F2): waiting for the next `send` to fail would wait for nix-eval-jobs
    // to print, which a stuck worker never does.
    let forward = async move {
        loop {
            let item = tokio::select! {
                biased;
                () = tx.closed() => return,
                item = inner_rx.recv() => match item {
                    Some(i) => i,
                    None => return,
                },
            };
            if let EvalItem::Machine {
                machine,
                outcome: EvalOutcome::Success(result),
                from_cache: false,
            } = &item
                && let Err(e) = store(dir, &key, machine, result).await
            {
                let _ = tx.send(EvalItem::Note(format!("{machine}: {e}")));
            }
            if tx.send(item).is_err() {
                return;
            }
        }
    };
    let (result, ()) = tokio::join!(evaluate(flake, &misses, eval, cancel, inner_tx), forward);
    result
}
