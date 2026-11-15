//! The evaluation cache, under `$XDG_CACHE_HOME/auspex/` (default `~/.cache/auspex/`).
//!
//! Re-evaluating is no faster warm, so every successful evaluation is kept, keyed by
//! `auspex_domain::eval::cache_key` over inputs gathered here, all from the flake
//! directory and each under a timeout:
//!
//! - `git rev-parse --verify HEAD`;
//! - the bytes of `flake.lock` (absent is a distinct input);
//! - `git status --porcelain`, to tell a clean tree from a dirty one; and when dirty,
//!   `git diff HEAD --binary` plus `git ls-files --others --exclude-standard`.
//!
//! A directory that is not a git repository (or has no commit) gets **no caching** rather
//! than a key that could be wrong; so does a diff over [`MAX_DIFF_BYTES`].
//!
//! Layout: `<dir>/eval/<key>/<sha256 of the machine name>.json`, one
//! `auspex_domain::eval::CacheEntry` per file. The name is hashed so that no machine name,
//! whatever it contains, is ever a path component. Writes go to a uniquely named temporary
//! file in the same directory and are renamed into place, so a reader sees a whole entry
//! or none, and two writers never interleave. A failed write is a note, never an error. A
//! corrupt, foreign or mismatched entry is a miss. Only successes are cached.
//!
//! Nothing removes old keys' directories yet.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use auspex_domain::eval::{
    CacheEntry, CacheKey, CacheKeyInputs, EvalOutcome, EvalSuccess, Tree, cache_key,
};
use sha2::{Digest as _, Sha256};
use tokio::process::Command;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::nix::{EvalItem, EvalMachine, EvalParams, FlakeRef, evaluate};
use crate::process::{End, Limits, describe_status, run, spawn_error};
use crate::text::{last_line, lossy};

/// The default git program, looked up on `PATH`.
pub const DEFAULT_GIT: &str = "git";

/// The largest `git diff HEAD` that is keyed: 64 MiB. The domain hashes the diff's bytes,
/// so they are held in memory; a bigger diff means no caching for this run.
pub const MAX_DIFF_BYTES: usize = 64 * 1024 * 1024;

/// The largest `flake.lock` that is keyed: 16 MiB.
pub const MAX_LOCK_BYTES: u64 = 16 * 1024 * 1024;

/// The largest `git status` or untracked-file list read: 16 MiB.
const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;

/// A cache file larger than this is not an entry (a real one is under a kilobyte).
const MAX_ENTRY_BYTES: u64 = 1024 * 1024;

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
    /// The program: [`DEFAULT_GIT`] or a path.
    pub program: &'a OsStr,
    /// Each git command's own timeout.
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
    let mut cmd = Command::new(params.program);
    // `core.fsmonitor` off and optional locks off: reading the key must not start a
    // monitor daemon or take the index lock from under the operator's own git.
    cmd.current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", "core.fsmonitor=false"])
        .args(args);
    let limits = Limits {
        stdout: cap,
        stderr: 16 * 1024,
    };
    let what = format!("git {}", args.first().copied().unwrap_or_default());
    let ran = match run(cmd, None, limits, Instant::now() + params.timeout, cancel).await {
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
/// The result's inner `Err` is the reason there is no caching for this run (not a git
/// repository, git failed or timed out, a diff too large to key).
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired; any running git has been killed and reaped.
pub async fn key_for(
    flake_dir: &Path,
    params: &GitParams<'_>,
    cancel: &CancellationToken,
) -> Result<Result<CacheKey, String>, Cancelled> {
    let head = match git(
        params,
        flake_dir,
        &["rev-parse", "--verify", "HEAD"],
        4096,
        cancel,
    )
    .await?
    {
        Ok(h) => lossy(&h),
        Err(e) => return Ok(Err(format!("not a git repository with a commit ({e})"))),
    };
    let status = match git(
        params,
        flake_dir,
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
        MAX_LIST_BYTES,
        cancel,
    )
    .await?
    {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    let lock = match read_lock(flake_dir).await {
        Ok(l) => l,
        Err(e) => return Ok(Err(e)),
    };
    if status.is_empty() {
        return Ok(Ok(cache_key(&CacheKeyInputs {
            head: &head,
            flake_lock: lock.as_deref(),
            tree: Tree::Clean,
        })));
    }
    let diff = match git(
        params,
        flake_dir,
        &[
            "diff",
            "HEAD",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
        ],
        MAX_DIFF_BYTES,
        cancel,
    )
    .await?
    {
        Ok(d) => d,
        Err(e) => return Ok(Err(e)),
    };
    let untracked = match git(
        params,
        flake_dir,
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
    Ok(Ok(cache_key(&CacheKeyInputs {
        head: &head,
        flake_lock: lock.as_deref(),
        tree: Tree::Dirty {
            diff: &diff,
            untracked: &untracked,
        },
    })))
}

async fn read_lock(flake_dir: &Path) -> Result<Option<Vec<u8>>, String> {
    let path = flake_dir.join("flake.lock");
    match tokio::fs::metadata(&path).await {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read flake.lock: {e}")),
        Ok(m) if m.len() > MAX_LOCK_BYTES => {
            return Err(format!("flake.lock is larger than {MAX_LOCK_BYTES} bytes"));
        }
        Ok(_) => {}
    }
    tokio::fs::read(&path)
        .await
        .map(Some)
        .map_err(|e| format!("cannot read flake.lock: {e}"))
}

/// Where `machine`'s entry under `key` lives.
#[must_use]
pub fn entry_path(cache_dir: &Path, key: &CacheKey, machine: &str) -> PathBuf {
    let digest = Sha256::digest(machine.as_bytes());
    let name = digest.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    cache_dir
        .join("eval")
        .join(key.as_str())
        .join(format!("{name}.json"))
}

/// `machine`'s cached result under `key`, or `None` (missing, unreadable, corrupt, another
/// key's or machine's, another format version).
pub async fn lookup(cache_dir: &Path, key: &CacheKey, machine: &str) -> Option<EvalSuccess> {
    let path = entry_path(cache_dir, key, machine);
    let meta = tokio::fs::symlink_metadata(&path).await.ok()?;
    if !meta.is_file() || meta.len() > MAX_ENTRY_BYTES {
        return None;
    }
    let bytes = tokio::fs::read(&path).await.ok()?;
    CacheEntry::read(&bytes, key, machine).ok()
}

static WRITES: AtomicU64 = AtomicU64::new(0);

/// Writes `machine`'s result under `key`: a temporary file in the same directory, then a
/// rename.
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
    let path = entry_path(cache_dir, key, machine);
    let bytes = CacheEntry::new(key.clone(), machine.to_owned(), result.clone()).to_json();
    tokio::task::spawn_blocking(move || write_atomically(&path, bytes.as_bytes()))
        .await
        .map_err(|e| format!("cache write task failed: {e}"))?
        .map_err(|e| format!("cannot write the eval cache: {e}"))
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no parent"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| io::Error::other("bad file name"))?;
    // Unique per process (pid) and per write (counter): two writers, in this process or
    // two auspex processes, never share a temporary file.
    let tmp = dir.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
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
/// When there is no caching (no directory, not a git repository, git failed), one
/// [`EvalItem::Note`] says why and every machine is evaluated. A failed cache write is a
/// note too.
///
/// # Errors
///
/// [`Cancelled`], as for [`evaluate`].
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
        Some(dir) => key_for(cache.flake_dir, &cache.git, cancel)
            .await?
            .map(|k| (dir, k)),
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
    if cancel.is_cancelled() {
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
    // `move`, so that returning early drops `inner_rx`.
    let forward = async move {
        while let Some(item) = inner_rx.recv().await {
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
                // The caller went away: dropping `inner_rx` makes `evaluate` see a closed
                // channel and kill the evaluator.
                return;
            }
        }
    };
    let (result, ()) = tokio::join!(evaluate(flake, &misses, eval, cancel, inner_tx), forward);
    result
}
