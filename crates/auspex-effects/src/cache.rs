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
//!
//! **What decides clean or dirty** (review of PR #8, N1). nix sets `self.rev` or
//! `self.dirtyRev` from libgit2's status, and the plan's `configurationRevision` puts it in
//! every toplevel, so the key has to cover whatever that status depends on. It depends on the
//! index (keyed), the worktree's bytes and owner exec bit (keyed), and four things more:
//!
//! - `core.fileMode`, `core.symlinks` and `core.ignorecase`, which change the answer for the
//!   same bytes: keyed when they are not libgit2's defaults ([`STATUS_SETTINGS`]), read with
//!   `git config` from the repository and the operator's global and system files. git is run
//!   without `GIT_CONFIG_COUNT` and its `KEY_n`/`VALUE_n`, `GIT_CONFIG_PARAMETERS`,
//!   `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM` and `GIT_CONFIG_NOSYSTEM` (libgit2 ignores them,
//!   review of PR #37, finding 4), so it reads the files libgit2 reads. Not verified: a
//!   configuration file that libgit2 searches and git does not;
//! - the CRLF/`text` filter (review of PR #37, finding 1). libgit2 runs the worktree's bytes
//!   through it before comparing them with the index, so the same bytes are clean or dirty
//!   depending on `core.autocrlf`, `core.eol` and `core.safecrlf` ([`FILTER_SETTINGS`], keyed
//!   as git prints them when they are not the defaults) and on the attributes that apply: the
//!   tracked `.gitattributes` files (their bytes are in the key already), and the three that
//!   are not in the tree, whose **bytes** are keyed: `info/attributes` in the common git
//!   directory, `core.attributesFile` (else `$XDG_CONFIG_HOME/git/attributes`, else
//!   `~/.config/git/attributes`) and the system `/etc/gitattributes`. A symlink is followed, as
//!   libgit2 does; a path with nothing there is absent, which adds nothing to the key (the key
//!   of a repository without them is the key it always was) and differs from a present empty
//!   file; a file that is not a regular file or is over 1 MiB means no caching. Their stamps
//!   are in the run guard, with `config.worktree` and the user's and system's git
//!   configuration files (`~/.gitconfig`, `$XDG_CONFIG_HOME/git/config`, `/etc/gitconfig`; a
//!   different system file compiled into git is not seen);
//! - the index's stat data (size, mtime, `ctime`, inode, owner): libgit2 trusts it, and calls a
//!   file unmodified when it matches the entry, without reading the bytes. With the default
//!   `core.trustctime=true` it cannot match after a write, since no ordinary program sets
//!   `ctime` back, so a changed file is always read and the bytes the key hashes decide; a
//!   `touch` makes libgit2 read the file, find the committed bytes, and call it unmodified, as
//!   before. The stat data is therefore not needed in the key. With `core.trustctime=false`
//!   it would be (a same-size rewrite that restores the mtime is "unmodified" to nix), and it
//!   cannot be keyed without missing after every `touch`, so there is no caching then, with a
//!   note. What is left: a write in the same filesystem timestamp tick as the index entry's
//!   own (a coarse-timestamp filesystem; see the guard's limits below).
//!
//! Untracked and ignored files are not in the key, because nix no longer sees them; the
//! domain's untracked list is passed empty and `Tree::Clean` is never used. What an
//! `--impure` evaluation reads from the environment or from outside the flake is not in
//! the key either: that is the fleet's own choice, and no key can cover it. One case worth
//! naming (review of PR #8, N8): a tracked absolute symlink to a file outside the tree. The
//! key holds the link's target *text*, nix reads the file behind it, so editing only that file
//! leaves the key as it was. `--impure` is this crate's choice, because `getFlake` of a dirty
//! `git+file` reference needs it, so auspex evaluates, and caches, fleets that the operator's
//! own pure `nix build` would refuse. A pure evaluation of `?rev=<HEAD>` for clean trees would
//! close this and the edit-and-undo window below, and is not done.
//!
//! **A changed tree during a scan** (review finding B2). Every nix-eval-jobs worker reads
//! the tree again when it starts, and workers restart on their memory cap, so a file saved
//! mid-scan can be evaluated under the key of the tree before it. Results are therefore
//! streamed to the caller at once (they are right for the tree nix read) but stored only
//! after the evaluator has exited, a second key, computed then, equals the first, and the
//! two keys' run guards are equal too; otherwise none of the run's results is stored, with
//! a note ("the worktree changed during evaluation").
//!
//! **The run guard** (review finding N2) is what catches a change made *and undone* while
//! the evaluator runs, which leaves the two keys equal: `git stash` then `git stash pop`,
//! `git checkout other` then `git checkout -`, a pre-commit hook that stashes and restores,
//! an editor's undo. Each key computation also records, without entering the key, the
//! identity and change time (`dev`, `ino`, `ctime` with nanoseconds, `size`) of:
//!
//! - every tracked path: a regular file's from the descriptor opened to hash it (`fstat`,
//!   before the first byte; no second lookup), a symlink's or other entry's from the
//!   `lstat` that classified it, and for a path with nothing there, the deepest directory
//!   the walk reached (it changes when the name, or a directory on the way, is created). A
//!   regular file that cannot be opened gets its own `lstat` and its directory's (a `chmod`
//!   and an edit move only the file's; review of PR #9, F3);
//! - `flake.lock` when it is present (from its read);
//! - in the git directory (`rev-parse --absolute-git-dir`, printed by the same command as
//!   `HEAD`'s id, so a linked worktree's own files), each by one `lstat`: `HEAD` and
//!   `index`;
//! - in the common git directory (named by the git directory's `commondir` file, else the
//!   git directory itself): `config`; the loose file of the ref `HEAD` names (`rev-parse
//!   --symbolic-full-name HEAD`, the end of any chain of symbolic refs; per-worktree refs
//!   are in the git directory), or that it is not loose; `packed-refs`; and
//!   `reftable/tables.list` under the reftable backend. A detached `HEAD` is the `HEAD`
//!   file itself.
//!
//! The git directories are opened `O_PATH`, so search permission is enough, as it is for
//! git (review of PR #9, F6). When one cannot be opened at all, the note says that the
//! git directory cannot be checked for changes during the scan, and there is no caching.
//!
//! A write and its revert each set `ctime` to the current time, and no unprivileged call
//! sets it back (a revert that restores the old mtime, as `touch -r` or `cp
//! --preserve=all` do, is still seen); deleting and recreating, or saving through a
//! rename, gives a new inode or a new `ctime`; a checkout there and back rewrites `HEAD`
//! and every file that differs. `HEAD` alone catches a move to a commit with the same tree
//! (nix's `self.rev` differs). The index alone catches `git add` of an untracked file,
//! which nix then reads, and `git reset` of it. The ref's own file catches what writes
//! neither: `reset --soft HEAD~` then `reset --soft ORIG_HEAD`, `commit --amend` then
//! `reset --soft HEAD@{1}`, `commit --allow-empty` then `reset --soft HEAD~`, and
//! `update-ref` there and back (review of PR #9, B1). Every update of a ref writes its
//! loose file by lock and rename, even when it was packed; a ref that stays packed across
//! the scan is seen through `packed-refs`. Reflogs are not used, so they need not be on.
//!
//! What it costs: a store skipped when something rewrites one of these during a scan
//! without changing the tree. Measured with git 2.56 in a quiet 50-file repository:
//!
//! - the index: no rewrite by `git status` in a quiet repository. The first `git status`
//!   after any commit or `add` rewrites it whenever it comes (seen at 2 s and 5 s after),
//!   and a tracked file with a future mtime makes every `git status` rewrite it until the
//!   clock passes that mtime (300 of 300). A `git status` with `GIT_OPTIONAL_LOCKS=0`, as
//!   editors and prompts often run it, never did;
//! - `packed-refs`, rewritten by `git gc` or `pack-refs` during a scan;
//! - the deepest-directory stamp, while a tracked path is deleted but not committed: any
//!   entry created or removed in that directory (an editor's swap file, a `result`
//!   symlink in the top for a deleted directory) skips the store.
//!
//! A skipped store costs one re-evaluation, and the next quiet run stores (review of PR
//! #9, F9); a stale hit is a wrong answer the operator trusts.
//!
//! The guard is for one run only. A key that changed with `ctime` would never hit again
//! after a checkout, a clone or a `touch`. So a `touch` (which moves `ctime`) during a scan
//! means that run is not stored, and the next undisturbed run is stored and hits as usual.
//!
//! What the guard still cannot see (review of PR #9, F5):
//!
//! - a filesystem that stores coarse timestamps: 1 s (ext3, ext4 with 128-byte inodes),
//!   2 s (FAT, which keeps no change time at all and reports the settable mtime), or
//!   whatever an NFS or FUSE server returns. There an edit and its revert within the same
//!   second as the previous change to that file can pass. Not observed (it needs a mount).
//!   A kernel whose timestamps are only jiffy-coarse in memory is not such a case: a
//!   write and its revert must both fall in the tick of the key's `stat`, and nix's first
//!   read came 259 ms after it (measured by the reviewer; fine-grained `ctime` verified
//!   here on tmpfs, btrfs and overlayfs over tmpfs, and its absence on ramfs; the kernel
//!   list of multigrain filesystems is not verified);
//! - a write through a shared memory mapping whose page was already dirty at the first key:
//!   it does not move `ctime` (a clean page's first write fault does). No editor writes
//!   that way (review of PR #9, F8);
//! - a tracked directory renamed away and back, when every path in it is a file: a
//!   directory's rename changes its own `ctime`, not its files' (directories are stamped
//!   only for paths that are missing). Confirmed with real nix; not an ordinary operation
//!   (review of PR #9, F7);
//! - refs other than the one `HEAD` names, which no key input depends on;
//! - root writing to the block device, setting `ctime` through a debugger such as
//!   `debugfs`, or setting the clock back to the nanosecond: none is an ordinary program;
//! - anything the key does not cover either (see above, and the `--impure` reads).
//!
//! An immutable source (`?rev=` of a clean tree) would close the window for clean trees.
//!
//! # Running git safely
//!
//! The repository's own configuration and files are not trusted (review findings F1 and
//! N1). git is asked only what it answers from the index and refs, without reading file
//! contents: the three `rev-parse` commands and `ls-files -s -v -z`; no status, diff, filters,
//! attributes, ignore or exclude files. Every git command runs:
//!
//! - under an address-space limit of [`GIT_MEMORY_LIMIT`], set by util-linux `prlimit`
//!   before it execs git, so the limit holds from git's first instruction (review finding
//!   F4; measured: `ls-files -s` on nixpkgs, about 40,000 files, peaks at 25 MB and needs
//!   about 37 MB of address space). Without `prlimit` on an absolute `PATH` entry, or on
//!   another platform, git is not run and there is no caching;
//! - with a short timeout (2 s by default; the commands take at most 22 ms on
//!   nixpkgs), and the first failure ends the key;
//! - with `-c core.fsmonitor=false` ([`GIT_HARDENING`]), `GIT_OPTIONAL_LOCKS=0`, and none
//!   of the inherited variables that would point it at another repository or index. The
//!   operator's own global and system configuration apply as usual;
//! - and its output counts only when it was read to its end and within its cap. The exit
//!   can be seen before the last bytes are read; a listing over the cap is "printed more
//!   than", and one whose pipe something outside git's process group still holds open is
//!   "not read to the end". Either is no caching, never a truncated listing taken for the
//!   whole (PR #9 fix round 1: a truncated index listing would leave tracked files out of
//!   the key and the guard).
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

use std::collections::BTreeSet;
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
use crate::safe_fs::{
    ReadError, Stamp, kind, open_dir, open_path_root, open_root, read_file, walk, walk_path,
};
use crate::text::{last_line, lossy};

/// The default git program, looked up on `PATH`.
pub const DEFAULT_GIT: &str = "git";

/// The most worktree bytes hashed for a key: 1 GiB.
///
/// The cap bounds what a hostile tree can make one key computation read (the key is
/// computed twice per scan); past it there is no caching for that run. A fleet's Nix is a
/// few megabytes, but a fleet that tracks a few hundred MiB of assets, such as wallpapers,
/// was left uncached by a 256 MiB cap. At the cap a computation costs about 1.2 s with a
/// warm page cache, and is bounded by [`GitParams::read_timeout`] on a cold disk. Raising
/// the cap replaced a per-file digest memo, which kept being found to serve a stale result;
/// re-reading the bytes cannot.
pub const MAX_WORKTREE_BYTES: u64 = 1024 * 1024 * 1024;

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
/// store, or give it configuration that nix's libgit2 does not see. `GIT_CEILING_DIRECTORIES`
/// is kept: it is the operator's own limit on discovery.
///
/// The configuration ones matter to the key (review of PR #37, finding 4): `status_settings`
/// asks git for the settings that decide what nix reads as modified, and git honours
/// `GIT_CONFIG_COUNT` with its `KEY_n`/`VALUE_n`, `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_GLOBAL`,
/// `GIT_CONFIG_SYSTEM` and `GIT_CONFIG_NOSYSTEM`, which libgit2 ignores. Under one of them the
/// key would carry a value nix never used, and a later run without it would share the key.
/// Removed, git reads the files libgit2 reads. The indexed ones are removed by prefix
/// ([`GIT_ENV_REMOVED_PREFIXES`]).
const GIT_ENV_REMOVED: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
];

/// Variables removed whatever the number after the prefix: `GIT_CONFIG_KEY_0`, `GIT_CONFIG_VALUE_0`, ….
const GIT_ENV_REMOVED_PREFIXES: &[&str] = &["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

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
    /// `git` from `PATH`, 2 s per command, 30 s for reading the worktree (each key computation:
    /// 1 GiB in 30 s needs only about 35 MiB/s, below any disk worth scanning a fleet from).
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

/// Why a git command did not give an answer.
#[derive(Debug)]
pub(crate) enum GitFailure {
    /// git ran and exited non-zero (out of memory aside). `code` is its exit status, which a few
    /// commands give a meaning (`git remote get-url` exits 2 for "no such remote").
    Exited { code: Option<i32>, message: String },
    /// Anything else: it could not be run, timed out, printed too much, or ran out of memory.
    Other(String),
}

impl GitFailure {
    /// One line on why.
    pub(crate) fn into_message(self) -> String {
        match self {
            Self::Exited { message, .. } | Self::Other(message) => message,
        }
    }
}

/// [`git_typed`] with the failure as one line.
pub(crate) async fn git(
    params: &GitParams<'_>,
    dir: &Path,
    args: &[&str],
    cap: usize,
    cancel: &CancellationToken,
) -> Result<Result<Vec<u8>, String>, Cancelled> {
    Ok(git_typed(params, dir, args, cap, cancel)
        .await?
        .map_err(GitFailure::into_message))
}

pub(crate) async fn git_typed(
    params: &GitParams<'_>,
    dir: &Path,
    args: &[&str],
    cap: usize,
    cancel: &CancellationToken,
) -> Result<Result<Vec<u8>, GitFailure>, Cancelled> {
    // The subcommand, for messages: the first argument that is not a global option or the
    // value of `-c` (the tree check passes `--no-optional-locks -c k=v status …`).
    let mut rest = args.iter();
    let mut subcommand = "";
    while let Some(a) = rest.next() {
        if *a == "-c" {
            rest.next();
        } else if !a.starts_with('-') {
            subcommand = a;
            break;
        }
    }
    let what = format!("git {subcommand}");
    let mut cmd = match command(params.program) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Err(GitFailure::Other(spawn_error(
                &what,
                params.program,
                &e,
            ))));
        }
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
    for (name, _) in std::env::vars_os() {
        let name_s = name.to_string_lossy();
        if GIT_ENV_REMOVED_PREFIXES
            .iter()
            .any(|p| name_s.starts_with(p))
        {
            cmd.env_remove(&name);
        }
    }
    let limits = Limits {
        stdout: cap,
        stderr: 16 * 1024,
        memory: Some(GIT_MEMORY_LIMIT),
    };
    let ran = match run(cmd, None, OnExit::KillGroup, limits, deadline, cancel).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(Err(GitFailure::Other(spawn_error(
                &what,
                params.program,
                &e,
            ))));
        }
    };
    Ok(match ran.end {
        End::Cancelled => return Err(Cancelled),
        End::TimedOut => Err(GitFailure::Other(format!(
            "{what} timed out after {:?}",
            params.timeout
        ))),
        End::Overflow => Err(GitFailure::Other(format!(
            "{what} printed more than {cap} bytes"
        ))),
        // Only an output read to its end and within the cap is git's answer: the exit can
        // be seen before the last bytes are read, and a truncated listing would leave
        // tracked files out of the key (PR #9, fix round 1).
        End::Exited(s) if s.success() => match ran.whole_stdout() {
            Ok(_) => Ok(ran.stdout),
            Err(true) => Err(GitFailure::Other(format!(
                "{what} printed more than {cap} bytes"
            ))),
            Err(false) => Err(GitFailure::Other(format!(
                "{what} exited, but its output was not read to the end (something still \
                 held it open)"
            ))),
        },
        End::Exited(s) => {
            let stderr = lossy(&ran.stderr);
            let why = last_line(&stderr).unwrap_or_default();
            if out_of_memory(&stderr, s) {
                Err(GitFailure::Other(format!(
                    "{what} ran into auspex's memory limit for git ({} MiB): {why}",
                    GIT_MEMORY_LIMIT >> 20
                )))
            } else {
                Err(GitFailure::Exited {
                    code: s.code(),
                    message: format!("{what} {}: {why}", describe_status(Some(s))),
                })
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
    Ok(key_and_top(flake_dir, params, cancel).await?.map(|k| k.key))
}

/// A fingerprint of the tracked files of `flake_dir`'s repository, as nix reads them.
///
/// It is the 64 lowercase hex digits of the worktree digest the cache key is made of (see the
/// module documentation), with one difference: the index enters as each record's status letter,
/// stage and path, **not** its mode and blob id ([`fingerprint_index`]). Every index path's
/// worktree state is in it as nix reads it for a dirty `git+file` flake: a regular file's bytes
/// and owner exec bit, a symlink's target, or that it is missing. So `git add` of a file that is
/// already edited, or a stash popped back, does not change it; an edit does, staged or not.
///
/// This is what the update-time check of the fleet tree compares between the confirmed plan
/// and a machine's turn (AUS-42). It is the same read, with the same bounds, as the cache's
/// own: [`MAX_WORKTREE_BYTES`] in all, [`GitParams::read_timeout`] for the reads and
/// [`GitParams::timeout`] for each git command. It is **not** `git diff HEAD --binary`: that
/// runs git's diff machinery (textconv, external diff drivers, filters named by attributes),
/// which the tree check is built never to run in an untrusted repository, and it would also
/// disagree with what nix evaluates (a `diff` is relative to `HEAD`; the digest is the
/// state itself). Untracked files are not in it: auspex evaluates the fleet as a `git+file`
/// flake, which does not see them, so an untracked file cannot change what is deployed.
///
/// The inner `Err` is one line on why there is none (no cache directory is involved).
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired; any running git has been killed and reaped.
pub async fn worktree_fingerprint(
    flake_dir: &Path,
    params: &GitParams<'_>,
    cancel: &CancellationToken,
) -> Result<Result<String, String>, Cancelled> {
    Ok(key_and_top(flake_dir, params, cancel).await?.map(|k| {
        k.worktree.iter().fold(String::new(), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
    }))
}

/// The run guard: a digest of the [`Stamp`] of every path the key read or looked at, and of
/// the git directory's `HEAD` and `index`. It is compared between the two key computations
/// around one evaluation and never enters the key (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Guard(Vec<u8>);

/// What one key computation found.
struct Keyed {
    key: CacheKey,
    /// The repository top level git reported, so the caller can check that the reference
    /// nix evaluates names the same repository.
    top: PathBuf,
    guard: Guard,
    /// The worktree digest alone, without `HEAD` and `flake.lock` that the key also holds.
    worktree: Vec<u8>,
}

/// [`key_for`], also returning git's top level and the run [`Guard`].
async fn key_and_top(
    flake_dir: &Path,
    params: &GitParams<'_>,
    cancel: &CancellationToken,
) -> Result<Result<Keyed, String>, Cancelled> {
    let top = match git_typed(
        params,
        flake_dir,
        &["rev-parse", "--show-toplevel"],
        4096,
        cancel,
    )
    .await?
    {
        Ok(t) => t,
        // Only git itself saying no is "not a git repository" (or a repository it cannot open:
        // an unreadable configuration is one). Everything else on the first
        // command (no `prlimit`, an inherited address-space limit, a timeout) is reported as
        // it is: this label used to go in front of all of them, and sent the operator to the
        // repository (review of PR #8, N4).
        Err(GitFailure::Exited { message: e, .. }) => {
            return Ok(Err(format!(
                "not a git repository, or one git cannot open ({e})"
            )));
        }
        Err(GitFailure::Other(e)) => return Ok(Err(e)),
    };
    let top = {
        use std::os::unix::ffi::OsStrExt as _;
        PathBuf::from(OsStr::from_bytes(top.strip_suffix(b"\n").unwrap_or(&top)))
    };
    let (git_dir, head, head_ref) = match head_of(params, &top, cancel).await? {
        Ok(h) => h,
        Err(e) => return Ok(Err(e)),
    };
    let StatusInputs {
        settings,
        attribute_files,
    } = match status_settings(params, &top, cancel).await? {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
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
    let rel = match flake_rel(flake_dir, &top) {
        Ok(r) => r,
        Err(why) => return Ok(Err(why)),
    };

    // The reads: off the async threads, stopped by the token or the deadline.
    let token = cancel.clone();
    let deadline = std::time::Instant::now() + params.read_timeout;
    let flake_dir = flake_dir.to_path_buf();
    let tree_top = top.clone();
    let job = tokio::task::spawn_blocking(move || {
        let stop = || token.is_cancelled() || std::time::Instant::now() > deadline;
        let paths = index_paths(&index).map_err(Stop::NoCache)?;
        let mut guard = Sha256::new_with_prefix(b"auspex run guard v1\0");
        let lock = read_lock(&flake_dir, &stop, &mut guard)?;
        let digest = worktree_digest::<MAX_WORKTREE_BYTES>(
            &tree_top, &rel, &index, &paths, &settings, &stop, &mut guard,
        )?;
        let common = git_dir_stamps(&git_dir, &head_ref, &stop, &mut guard)?;
        let digest = with_attributes(digest, &common, attribute_files, &stop, &mut guard)?;
        stamp_user_config(&mut guard);
        Ok::<_, Stop>((lock, digest, Guard(guard.finalize().to_vec())))
    });
    let (lock, digest, guard) = tokio::select! {
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
            diff: &digest.0,
            // Untracked names are not in the key: see the module documentation.
            untracked: &[],
        },
    });
    Ok(Ok(Keyed {
        key,
        top,
        guard,
        worktree: digest.1,
    }))
}

/// The flake directory relative to the repository top level, both canonical.
fn flake_rel(flake_dir: &Path, top: &Path) -> Result<PathBuf, String> {
    match (std::fs::canonicalize(flake_dir), std::fs::canonicalize(top)) {
        (Ok(f), Ok(t)) => f
            .strip_prefix(&t)
            .map(Path::to_path_buf)
            .map_err(|_| "the flake directory is outside its repository".to_owned()),
        _ => Err("cannot resolve the flake directory".to_owned()),
    }
}

/// The settings nix's libgit2 consults when it decides whether a tracked file is modified (and
/// so whether `self.rev` or `self.dirtyRev` is set, and with it `configurationRevision` and
/// every toplevel that records it), with the values libgit2 assumes when one is unset
/// (review of PR #8, N1). Read the way git reads them: the repository's own configuration,
/// then the operator's global and system files.
const STATUS_SETTINGS: &[(&str, &str)] = &[
    // A mode-only change (`chmod +x`) is modified, or is not.
    ("core.filemode", "true"),
    // A symlink is compared as a link, or as a file holding its target.
    ("core.symlinks", "true"),
    // How paths are matched against the index.
    ("core.ignorecase", "false"),
    // Whether `ctime` takes part in "the stat data still matches the index"; see below.
    ("core.trustctime", "true"),
];

/// The settings that drive libgit2's CRLF/`text` filter, which the worktree's bytes pass through
/// before they are compared with the index (review of PR #37, finding 1), with the value
/// libgit2 assumes when one is unset. Their values are not booleans (`core.autocrlf` is `true`,
/// `false` or `input`), so each is keyed as git prints it, lower-cased, when it differs from the
/// default; a different spelling of the same value is a harmless miss. `core.safecrlf` is keyed
/// whenever it is set, since whether libgit2 consults it was not established.
const FILTER_SETTINGS: &[(&str, &str)] = &[
    ("core.autocrlf", "false"),
    ("core.eol", "native"),
    ("core.safecrlf", ""),
];

/// What [`status_settings`] found.
struct StatusInputs {
    /// The non-default settings, as `name=value\0` each: empty when every one is a default.
    settings: Vec<u8>,
    /// The attributes files outside the tree that libgit2 reads: `core.attributesFile`, or its
    /// default location, and the system file.
    attribute_files: Vec<PathBuf>,
}

/// The non-default values of [`STATUS_SETTINGS`], as `name=value\0` each, to go into the key;
/// empty when every setting is at its default. Or why there is no caching.
///
/// `core.filemode`, `core.symlinks` and `core.ignorecase` are keyed: they change what nix
/// reads as modified for the same bytes, and the same key must not stand for two answers.
/// Nothing is fed for a default, so keys written before this was keyed still hit.
///
/// `core.trustctime=false` is not keyed but refused. With it, nix calls a tracked file
/// unmodified when its size, mtime, inode and owner still match the index entry, whatever
/// its bytes are, and "matches the index" depends on stat data that no key can hold without
/// missing after every `touch` and checkout: a same-size rewrite that puts the mtime back
/// reads as the committed file to nix and as the new bytes to the key. With the default
/// (`ctime` compared) the stat data cannot match after a write, because no ordinary program
/// sets `ctime` back, so the bytes the key hashes decide. Stat data is therefore the one
/// input of that decision that stays outside the key, and only under that setting.
async fn status_settings(
    params: &GitParams<'_>,
    top: &Path,
    cancel: &CancellationToken,
) -> Result<Result<StatusInputs, String>, Cancelled> {
    let mut keyed = Vec::new();
    for (name, default) in STATUS_SETTINGS {
        // `--type=bool` spells `yes`, `on`, `1` and `true` one way, and a value that is no
        // boolean is git's error (exit 128), so there is no caching rather than a guess.
        let args = ["config", "--type=bool", "--default", default, "--get", name];
        let out = match git(params, top, &args, 64, cancel).await? {
            Ok(out) => out,
            Err(e) => return Ok(Err(e)),
        };
        let value = lossy(&out);
        let value = value.trim_end();
        if *name == "core.trustctime" && value == "false" {
            return Ok(Err(
                "core.trustctime is false in git's configuration for this repository (its own file, \
                 or the global or system one), so nix decides whether a tracked file changed from size, mtime and inode alone and \
                 may read a rewritten file as the committed one, which the cache key cannot \
                 follow; set it to true (the default) for caching"
                    .to_owned(),
            ));
        }
        if !matches!(value, "true" | "false") {
            return Ok(Err(format!("git config printed {value:?} for {name}")));
        }
        if value != *default {
            keyed.extend_from_slice(format!("{name}={value}\0").as_bytes());
        }
    }
    for (name, default) in FILTER_SETTINGS {
        let args = ["config", "--default", default, "--get", name];
        let out = match git(params, top, &args, 256, cancel).await? {
            Ok(out) => out,
            Err(e) => return Ok(Err(e)),
        };
        let value = lossy(&out).trim().to_ascii_lowercase();
        if value != *default {
            keyed.extend_from_slice(format!("{name}={value}\0").as_bytes());
        }
    }
    // `--path` expands a leading `~/`, as git does for this setting.
    let args = [
        "config",
        "--path",
        "--default",
        "",
        "--get",
        "core.attributesFile",
    ];
    let configured = match git(params, top, &args, 4096, cancel).await? {
        Ok(out) => lossy(&out).trim_end_matches('\n').to_owned(),
        Err(e) => return Ok(Err(e)),
    };
    let mut attribute_files = Vec::new();
    attribute_files.extend(global_attributes(
        &configured,
        top,
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    ));
    attribute_files.push(PathBuf::from("/etc/gitattributes"));
    Ok(Ok(StatusInputs {
        settings: keyed,
        attribute_files,
    }))
}

/// The global attributes file libgit2 reads: `core.attributesFile` when set (a relative one is
/// relative to `top`), else `$XDG_CONFIG_HOME/git/attributes` when `XDG_CONFIG_HOME` is set and
/// not empty, else `$HOME/.config/git/attributes`; `None` with neither.
fn global_attributes(
    configured: &str,
    top: &Path,
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    if !configured.is_empty() {
        return Some(top.join(configured));
    }
    xdg_config_home.filter(|d| !d.is_empty()).map_or_else(
        || {
            home.filter(|h| !h.is_empty())
                .map(|h| Path::new(h).join(".config/git/attributes"))
        },
        |d| Some(Path::new(d).join("git/attributes")),
    )
}

/// The worktree `digest` with the attributes files outside the tree: the common git directory's
/// `info/attributes` and the files `status_settings` found. Only a file that is there adds to
/// it, so a repository with none of them has the key it always had.
fn with_attributes(
    digest: Vec<u8>,
    common: &Path,
    attribute_files: Vec<PathBuf>,
    stop: &dyn Fn() -> bool,
    g: &mut Sha256,
) -> Result<Vec<u8>, Stop> {
    let mut files = vec![common.join("info/attributes")];
    files.extend(attribute_files);
    let extra = attribute_digest(&files, stop, g)?;
    if extra.is_empty() {
        return Ok(digest);
    }
    let mut h = Sha256::new_with_prefix(b"auspex attribute files v1\0");
    h.update(&digest);
    h.update(&extra);
    Ok(h.finalize().to_vec())
}

/// The guard record of a name outside the tree: its own `lstat`, or, when nothing is there, its
/// directory's `stat` (which changes when the name is created in it, so a file created and
/// removed again during a scan is seen).
fn stamp_name(g: &mut Sha256, tag: u8, path: &Path) {
    if let Ok(st) = rustix::fs::lstat(path) {
        stamp(g, tag, Some(Stamp::of(&st)));
    } else {
        stamp(g, tag, None);
        let dir = path.parent().and_then(|p| rustix::fs::stat(p).ok());
        stamp(g, tag.to_ascii_uppercase(), dir.as_ref().map(Stamp::of));
    }
}

/// The most an attributes file may hold and still be keyed: 1 MiB.
const MAX_ATTRIBUTES_BYTES: u64 = 1024 * 1024;

/// For each of `files` that is there, a tagged digest of its bytes; empty when none is (the
/// key then is what it was before these files were keyed). Each file's own and resolved
/// stamp goes to the guard `g`, there or not.
///
/// A symlink is followed, as libgit2 does (a home-manager `~/.config/git/attributes` is one),
/// by resolving the path first and then reading the resolved name without following anything
/// more. A path with nothing there, or a dangling link, is absent. A file that is not a
/// regular file (a FIFO, a device), is larger than [`MAX_ATTRIBUTES_BYTES`] or cannot be read
/// means no caching: its bytes would decide clean or dirty, and the key cannot hold them.
fn attribute_digest(
    files: &[PathBuf],
    stop: &dyn Fn() -> bool,
    g: &mut Sha256,
) -> Result<Vec<u8>, Stop> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = Vec::new();
    for path in files {
        field(g, b'a', path.as_os_str().as_bytes());
        stamp_name(g, b'a', path);
        let no = |what: &str| {
            Stop::NoCache(format!(
                "the git attributes file {} {what}, so there is no caching",
                path.display()
            ))
        };
        let resolved = match std::fs::canonicalize(path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(no(&format!("cannot be resolved ({e})"))),
        };
        let (Some(parent), Some(name)) = (resolved.parent(), resolved.file_name()) else {
            return Err(no("is not a file"));
        };
        let dir = open_root(parent).map_err(|e| no(&format!("cannot be opened ({e:?})")))?;
        let mut h = Sha256::new();
        match read_file(
            &dir,
            name.as_bytes(),
            MAX_ATTRIBUTES_BYTES,
            stop,
            &mut |b| h.update(b),
        ) {
            Ok(read) => stamp(g, b'A', Some(read.stamp)),
            Err(ReadError::Missing) => continue,
            Err(ReadError::NotAFile(what)) => return Err(no(&format!("is {what}"))),
            Err(ReadError::TooBig) => {
                return Err(no(&format!("is larger than {MAX_ATTRIBUTES_BYTES} bytes")));
            }
            Err(ReadError::Stopped) => return Err(Stop::Stopped),
            Err(ReadError::Io(e)) => return Err(no(&format!("cannot be read ({e})"))),
        }
        let digest = h.finalize();
        for (tag, bytes) in [(b'a', path.as_os_str().as_bytes()), (b'd', &digest[..])] {
            out.push(tag);
            out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            out.extend_from_slice(bytes);
        }
    }
    Ok(out)
}

/// The user's and the system's git configuration files, one `lstat` and one `stat` each, in
/// the guard: a keyed setting changed in one of them and put back during a scan leaves two
/// equal keys. Which files git reads is `~/.gitconfig`, `$XDG_CONFIG_HOME/git/config` (else
/// `~/.config/git/config`) and `/etc/gitconfig`; a different system file compiled into git is
/// not seen.
fn stamp_user_config(g: &mut Sha256) {
    use std::os::unix::ffi::OsStrExt as _;
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty());
    let xdg = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty());
    let mut files = vec![PathBuf::from("/etc/gitconfig")];
    files.extend(home.as_deref().map(|h| Path::new(h).join(".gitconfig")));
    files.push(match (xdg.as_deref(), home.as_deref()) {
        (Some(d), _) => Path::new(d).join("git/config"),
        (None, Some(h)) => Path::new(h).join(".config/git/config"),
        (None, None) => PathBuf::from("/nonexistent/git/config"),
    });
    for f in files {
        field(g, b'u', f.as_os_str().as_bytes());
        // Only the file's own stamp: a missing one would stamp $HOME or /etc, which change all
        // the time and would skip stores for nothing.
        stamp(g, b'u', rustix::fs::lstat(&f).ok().as_ref().map(Stamp::of));
        stamp(g, b'v', rustix::fs::stat(&f).ok().as_ref().map(Stamp::of));
    }
}

/// The git directory, HEAD's commit id (with its newline, as the key has always hashed it)
/// and the ref HEAD names, from two `rev-parse` commands in `top`.
///
/// The git directory comes with the id, from the same command: it is printed first and
/// may contain a newline, so the id is the last line. The ref is `HEAD` itself when HEAD
/// is detached, else the end of its chain of symbolic refs (review of `ca2cc3f`, B1); a
/// ref name has no newline.
async fn head_of(
    params: &GitParams<'_>,
    top: &Path,
    cancel: &CancellationToken,
) -> Result<Result<(PathBuf, String, Vec<u8>), String>, Cancelled> {
    let no_commit = |e: String| format!("not a git repository with a commit ({e})");
    let (git_dir, head) = match git(
        params,
        top,
        &["rev-parse", "--absolute-git-dir", "--verify", "HEAD"],
        8192,
        cancel,
    )
    .await?
    {
        Ok(out) => match git_dir_and_head(&out) {
            Some(v) => v,
            None => {
                return Ok(Err(
                    "git rev-parse printed no git directory before HEAD's id, so changes \
                     during the scan cannot be checked, and there is no caching"
                        .to_owned(),
                ));
            }
        },
        Err(e) => return Ok(Err(no_commit(e))),
    };
    let head_ref = match git(
        params,
        top,
        &["rev-parse", "--symbolic-full-name", "HEAD"],
        4096,
        cancel,
    )
    .await?
    {
        Ok(out) => out.strip_suffix(b"\n").unwrap_or(&out).to_vec(),
        Err(e) => return Ok(Err(no_commit(e))),
    };
    Ok(Ok((git_dir, head, head_ref)))
}

/// `rev-parse --absolute-git-dir --verify HEAD` output: the git directory, which may
/// contain a newline, then the commit id on the last line, kept with its newline as the key
/// has always hashed it.
fn git_dir_and_head(out: &[u8]) -> Option<(PathBuf, String)> {
    use std::os::unix::ffi::OsStrExt as _;
    let cut = out.strip_suffix(b"\n")?.iter().rposition(|&b| b == b'\n')?;
    Some((
        PathBuf::from(OsStr::from_bytes(&out[..cut])),
        lossy(&out[cut + 1..]),
    ))
}

enum Stop {
    Stopped,
    NoCache(String),
}

/// Feeds one guard record: a tag and a [`Stamp`], or the tag alone when there is none.
fn stamp(g: &mut Sha256, tag: u8, s: Option<Stamp>) {
    g.update([tag]);
    match s {
        Some(s) => {
            g.update([1]);
            g.update(s.to_bytes());
        }
        None => g.update([0]),
    }
}

/// Refs that live in each worktree's own git directory; every other ref is shared, in the
/// common one (git's rule, `gitrepository-layout(5)`).
const PER_WORKTREE_REFS: &[&[u8]] = &[b"refs/bisect/", b"refs/worktree/", b"refs/rewritten/"];

/// The guard records of the git directory, each from one `lstat`, every directory opened
/// `O_PATH` (search permission is all git needs, and all this needs; review of `ca2cc3f`,
/// F6):
///
/// - `HEAD` and `index` in the worktree's git directory. `HEAD` is rewritten by every
///   checkout and `symbolic-ref`, and holds the id when detached; the index by `add`,
///   `reset`, `commit`, `checkout`, `stash`, and any index refresh;
/// - the ref `HEAD` names (`head_ref`), as a loose file, in the common git directory unless
///   it is a per-worktree ref, or that it is not loose. Every update of a ref, even a
///   packed one, writes its loose file by lock and rename, so `reset --soft`,
///   `commit --amend` and `update-ref` there and back are seen (review of `ca2cc3f`, B1);
/// - `packed-refs`, and `reftable/tables.list` (rewritten by every ref update under the
///   reftable backend), in the common directory, or that they are absent.
///
/// The common directory is git's: the `commondir` file in the git directory names it,
/// relative to the git directory unless absolute; without one it is the git directory.
fn git_dir_stamps(
    git_dir: &Path,
    head_ref: &[u8],
    stop: &dyn Fn() -> bool,
    g: &mut Sha256,
) -> Result<PathBuf, Stop> {
    let unusable = |what: &str, e: &dyn std::fmt::Debug| {
        Stop::NoCache(format!(
            "the git directory {} cannot be checked for changes during the scan ({what}: \
             {e:?}), so there is no caching",
            git_dir.display()
        ))
    };
    let dir = open_path_root(git_dir).map_err(|e| unusable("cannot open it", &e))?;
    for (tag, name) in [(b'H', &b"HEAD"[..]), (b'I', b"index")] {
        stamp(g, tag, kind(&dir, name).ok().map(|(_, s)| s));
    }
    // A linked worktree's own configuration, under `extensions.worktreeConfig`.
    stamp(g, b'W', kind(&dir, b"config.worktree").ok().map(|(_, s)| s));
    let mut named = Vec::new();
    let mut common_path = git_dir.to_path_buf();
    let common = match read_file(&dir, b"commondir", 4096, stop, &mut |b| {
        named.extend_from_slice(b);
    }) {
        Ok(_) => {
            use std::os::unix::ffi::OsStrExt as _;
            let named = named.strip_suffix(b"\n").unwrap_or(&named);
            let common = git_dir.join(OsStr::from_bytes(named));
            let fd = open_path_root(&common)
                .map_err(|e| unusable("cannot open its common directory", &e))?;
            common_path = common;
            Some(fd)
        }
        Err(ReadError::Missing) => None,
        Err(ReadError::Stopped) => return Err(Stop::Stopped),
        Err(e) => return Err(unusable("cannot read its commondir file", &e)),
    };
    let common = common.as_ref().unwrap_or(&dir);
    if head_ref != b"HEAD" {
        let base = if PER_WORKTREE_REFS.iter().any(|p| head_ref.starts_with(p)) {
            &dir
        } else {
            common
        };
        field(g, b'r', head_ref);
        // Not loose (packed, or under reftable): nothing to stamp here. When an update
        // writes the loose file, this record changes; while it stays packed,
        // `packed-refs` does.
        let loose = walk_path(base, head_ref)
            .ok()
            .and_then(|(d, name)| kind(&d, name).ok());
        stamp(g, b'R', loose.map(|(_, s)| s));
    }
    stamp(g, b'P', kind(common, b"packed-refs").ok().map(|(_, s)| s));
    // The repository's own configuration: the settings that are keyed (`status_settings`)
    // changed and put back during the scan leave two equal keys and a different file.
    stamp(g, b'G', kind(common, b"config").ok().map(|(_, s)| s));
    let tables = walk_path(common, b"reftable/tables.list")
        .ok()
        .and_then(|(d, name)| kind(&d, name).ok());
    stamp(g, b'T', tables.map(|(_, s)| s));
    Ok(common_path)
}

/// `flake.lock` in `flake_dir`: `None` when absent; refused when it is a symlink, not a
/// regular file, or larger than [`MAX_LOCK_BYTES`]. Its stamp goes to the guard `g` (it is
/// usually tracked too, but need not be).
fn read_lock(
    flake_dir: &Path,
    stop: &dyn Fn() -> bool,
    g: &mut Sha256,
) -> Result<Option<Vec<u8>>, Stop> {
    let no = |why: &str| Stop::NoCache(format!("flake.lock {why}"));
    let root = open_root(flake_dir)
        .map_err(|e| Stop::NoCache(format!("cannot open the flake directory: {e:?}")))?;
    let mut bytes = Vec::new();
    match read_file(&root, b"flake.lock", MAX_LOCK_BYTES, stop, &mut |b| {
        bytes.extend_from_slice(b);
    }) {
        Ok(read) => {
            stamp(g, b'L', Some(read.stamp));
            Ok(Some(bytes))
        }
        Err(ReadError::Missing) => {
            // An untracked lock appearing is not read by nix; a tracked one is a tracked
            // path, guarded with the others.
            stamp(g, b'M', None);
            Ok(None)
        }
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

/// The two digests the worktree read feeds: the cache key's, and the update check's fingerprint
/// (AUS-42). They are fed the same path and file records; they differ only in how the index
/// enters (below), so one read of the files serves both.
struct Both {
    key: Sha256,
    fingerprint: Sha256,
}

impl Both {
    fn field(&mut self, tag: u8, bytes: &[u8]) {
        field(&mut self.key, tag, bytes);
        field(&mut self.fingerprint, tag, bytes);
    }

    /// The fingerprint learns that `path` is there (a path with nothing on disk never is: see
    /// [`fingerprint_index`]); the key already has it from `field(b'p', ..)`.
    fn present(&mut self, path: &[u8]) {
        field(&mut self.fingerprint, b'p', path);
    }
}

/// The index without what nix does not read from it: each record's status letter, stage and
/// path, **not** its mode and blob id.
///
/// A dirty `git+file` flake is evaluated from the worktree, so staging a file that is already
/// edited (`git add`, or a stash popped back, which comes back unstaged) changes nothing nix
/// sees. What the index does decide is *which* paths are tracked (a `git add` of a new file
/// makes nix see it), their stage (an unmerged path) and the assume-unchanged and skip-worktree
/// tags, and those stay. The cache key keeps the whole listing: a key that misses once is
/// cheap, a stale hit is not.
fn fingerprint_index(index: &[u8], missing: &BTreeSet<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for rec in index.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        // `<tag> <mode> <oid> <stage>\t<path>`
        let Some(tab) = rec.iter().position(|&b| b == b'\t') else {
            continue;
        };
        // A path with nothing on disk is not there for nix, whether its deletion is staged (no
        // record at all) or not (a record of a missing file): neither says anything.
        if missing.contains(&rec[tab + 1..]) {
            continue;
        }
        let meta = &rec[..tab];
        out.extend_from_slice(&meta[..meta.len().min(1)]);
        out.push(b' ');
        out.extend_from_slice(meta.rsplit(|&b| b == b' ').next().unwrap_or_default());
        out.push(b'\t');
        out.extend_from_slice(&rec[tab + 1..]);
        out.push(0);
    }
    out
}

/// The digests of every tracked path's worktree state under `top`: the cache key's and the
/// update check's fingerprint ([`Both`]). Each path's [`Stamp`]
/// goes to the guard `g`: the opened file's own, a symlink's or other entry's from the
/// `lstat` that classified it, or, when nothing is there, the deepest directory reached
/// (it changes when the name is created in it).
fn worktree_digest<const MAX_BYTES: u64>(
    top: &Path,
    rel: &Path,
    index: &[u8],
    paths: &[&[u8]],
    settings: &[u8],
    stop: &dyn Fn() -> bool,
    g: &mut Sha256,
) -> Result<(Vec<u8>, Vec<u8>), Stop> {
    use std::os::unix::ffi::OsStrExt as _;
    let root =
        open_root(top).map_err(|e| Stop::NoCache(format!("cannot open the worktree: {e:?}")))?;
    let mut paths = paths.to_vec();
    // Unmerged entries are listed once per stage.
    paths.sort_unstable();
    paths.dedup();
    let mut both = Both {
        key: Sha256::new(),
        fingerprint: Sha256::new(),
    };
    both.key.update(b"auspex worktree digest v2\0");
    both.fingerprint.update(b"auspex worktree fingerprint v1\0");
    // Which flake in the repository (N2e), and the index itself, staged state included
    // (N2b).
    both.field(b'r', rel.as_os_str().as_bytes());
    field(&mut both.key, b'i', &Sha256::digest(index));
    // The git settings that change what nix reads as modified, when they are not the
    // defaults (N1); nothing is fed for the defaults, so those keys are what they always were.
    // The update check's fingerprint takes them too: it says what nix reads, and a setting that
    // flips clean and dirty changes the deployed toplevel as an edit does.
    if !settings.is_empty() {
        both.field(b'c', settings);
    }
    // (The fingerprint's own index field comes after the walk: it leaves out the paths with nothing
    // on disk.)
    let mut missing: BTreeSet<&[u8]> = BTreeSet::new();
    let mut left = MAX_BYTES;
    for p in paths {
        if stop() {
            return Err(Stop::Stopped);
        }
        field(&mut both.key, b'p', p);
        field(g, b'p', p);
        let (dir, name) = match walk(&root, p) {
            Ok(found) => found,
            Err((_, reached)) => {
                // A directory on the way is missing or a symlink: the file is not there
                // for nix either.
                field(&mut both.key, b'x', &[]);
                missing.insert(p);
                stamp(g, b'x', reached.and_then(|d| Stamp::of_fd(&d).ok()));
                continue;
            }
        };
        match kind(&dir, name) {
            Err(_) => {
                field(&mut both.key, b'd', &[]);
                missing.insert(p);
                stamp(g, b'd', Stamp::of_fd(&dir).ok());
            }
            Ok((FileType::Symlink, s)) => {
                both.present(p);
                let target = rustix::fs::readlinkat(&dir, name, Vec::new())
                    .map(std::ffi::CString::into_bytes)
                    .unwrap_or_default();
                both.field(b'l', &target);
                stamp(g, b'l', Some(s));
            }
            Ok((FileType::RegularFile, lstat)) => {
                both.present(p);
                let mut fh = Sha256::new();
                let mut n: u64 = 0;
                match read_file(&dir, name, left, stop, &mut |b| {
                    n += b.len() as u64;
                    fh.update(b);
                }) {
                    Ok(read) => {
                        left -= n;
                        both.field(if read.exec { b'X' } else { b'f' }, &fh.finalize());
                        stamp(g, b'f', Some(read.stamp));
                    }
                    Err(ReadError::TooBig) => {
                        return Err(Stop::NoCache(format!(
                            "the tracked files are larger than {} MiB",
                            MAX_BYTES >> 20
                        )));
                    }
                    Err(ReadError::Stopped) => return Err(Stop::Stopped),
                    // Replaced between the stat and the open, or unreadable.
                    Err(_) => {
                        both.field(b'e', &[]);
                        // The file's own `lstat` too: a `chmod` and an edit move only
                        // its `ctime` (review of `ca2cc3f`, F3).
                        stamp(g, b'e', Some(lstat));
                        stamp(g, b'E', Stamp::of_fd(&dir).ok());
                    }
                }
            }
            // A submodule (a gitlink is a directory), a FIFO, a device: its kind only.
            Ok((other, s)) => {
                both.present(p);
                both.field(b'o', &other.as_raw_mode().to_be_bytes());
                stamp(g, b'o', Some(s));
            }
        }
    }
    field(
        &mut both.fingerprint,
        b'i',
        &Sha256::digest(fingerprint_index(index, &missing)),
    );
    Ok((
        both.key.finalize().to_vec(),
        both.fingerprint.finalize().to_vec(),
    ))
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
/// result as it arrives; keeps successes until a second key and run guard confirm that the
/// tree was not changed during the evaluation, not even changed and undone; and ends with
/// the evaluator's [`EvalItem::Finished`] (or no evaluation when all hit). When there is
/// no caching (no directory, a non-git flake reference, not a git repository,
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
) -> Result<Result<(&'c Path, CacheKey, Guard), String>, Cancelled> {
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
                    Ok(Keyed { key, top, guard, .. })
                        if std::fs::canonicalize(&top).ok().as_deref() == flake.repo_top() =>
                    {
                        Ok((dir, key, guard))
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
    keyed: Option<(&Path, CacheKey, Guard)>,
) -> Result<(), Cancelled> {
    let mut misses = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for m in machines {
        if !seen.insert(m.name.as_str()) {
            continue;
        }
        let hit = match &keyed {
            Some((dir, key, _)) => lookup(dir, key, &m.name).await,
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
    let Some((_, key, guard)) = keyed else {
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
    finish_run(flake, cache, cancel, &tx, (&key, &guard), pending, finished).await
}

/// What follows an evaluation that ran to its end: the stores, then the held `Finished`.
///
/// Nothing at all when the run was cancelled or the receiver dropped meanwhile: a cancelled
/// call sends no `Finished` (see [`evaluate_cached`]), and that includes a run with nothing
/// to store, where [`store_checked`] is never reached to notice (review of PR #8, T3). The
/// window is the time between the evaluator's exit and this call, too short to hit on
/// purpose, so it is a function of its own that a test can call with the token already
/// cancelled.
async fn finish_run(
    flake: &FlakeRef,
    cache: &CacheParams<'_>,
    cancel: &CancellationToken,
    tx: &UnboundedSender<EvalItem>,
    (key, guard): (&CacheKey, &Guard),
    pending: Vec<(String, EvalSuccess)>,
    finished: Option<EvalItem>,
) -> Result<(), Cancelled> {
    if cancel.is_cancelled() || tx.is_closed() {
        return Err(Cancelled);
    }
    if !pending.is_empty() {
        store_checked(flake, cache, cancel, tx, (key, guard), pending).await?;
    }
    if let Some(finished) = finished {
        tx.send(finished).map_err(|_| Cancelled)?;
    }
    Ok(())
}

/// Publish only results whose before/after keys agree, and whose before/after guards agree:
/// nothing the key read was written, replaced or recreated in between, even if it was put
/// back (review finding N2).
async fn store_checked(
    flake: &FlakeRef,
    cache: &CacheParams<'_>,
    cancel: &CancellationToken,
    tx: &UnboundedSender<EvalItem>,
    (key, guard): (&CacheKey, &Guard),
    pending: Vec<(String, EvalSuccess)>,
) -> Result<(), Cancelled> {
    match cache_key_or_why(flake, cache, cancel, tx).await? {
        Ok((dir, after, after_guard)) if after == *key && after_guard == *guard => {
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Review of PR #8, T3 (mutation X10): a run cancelled after its evaluator exited sends no
    /// `Finished`, whether or not it had anything to store, and the receiver dropped likewise.
    #[tokio::test]
    async fn a_run_cancelled_after_the_evaluator_exited_sends_nothing() {
        let flake = FlakeRef::new("path:/x").unwrap();
        let cache = CacheParams {
            cache_dir: None,
            flake_dir: Path::new("/x"),
            git: GitParams::new(),
        };
        let key = cache_key(&CacheKeyInputs {
            head: "0123\n",
            flake_lock: None,
            tree: Tree::Clean,
        });
        let guard = Guard(Vec::new());
        let finished = || Some(EvalItem::Finished { error: None });

        let (tx, mut rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let r = finish_run(
            &flake,
            &cache,
            &cancel,
            &tx,
            (&key, &guard),
            vec![],
            finished(),
        )
        .await;
        assert!(r.is_err(), "a cancelled run is Cancelled");
        assert!(rx.try_recv().is_err(), "and sends no Finished");

        let (tx, rx) = unbounded_channel();
        drop(rx);
        let r = finish_run(
            &flake,
            &cache,
            &CancellationToken::new(),
            &tx,
            (&key, &guard),
            vec![],
            finished(),
        )
        .await;
        assert!(r.is_err(), "a dropped receiver is Cancelled");

        // Review of PR #37 (mutation R12): with results to store the same holds, and with no
        // `Finished` to fail on the send, only the check itself can say so: a closed receiver
        // or a cancel means nothing is stored and the call is Cancelled.
        let hash = "0123456789abcdfghijklmnpqrsvwxyz";
        let pending = || {
            vec![(
                "a".to_owned(),
                EvalSuccess {
                    attr: "a".to_owned(),
                    attr_path: vec!["a".to_owned()],
                    drv_path: auspex_domain::observe::StorePath::new(format!(
                        "/nix/store/{hash}-a.drv"
                    ))
                    .unwrap(),
                    out_path: auspex_domain::observe::StorePath::new(format!(
                        "/nix/store/{hash}-a"
                    ))
                    .unwrap(),
                    system: None,
                },
            )]
        };
        let (tx, rx) = unbounded_channel();
        drop(rx);
        let r = finish_run(
            &flake,
            &cache,
            &CancellationToken::new(),
            &tx,
            (&key, &guard),
            pending(),
            None,
        )
        .await;
        assert!(r.is_err(), "pending results and a dropped receiver");
        let (tx, _rx) = unbounded_channel();
        let r = finish_run(
            &flake,
            &cache,
            &cancel,
            &tx,
            (&key, &guard),
            pending(),
            None,
        )
        .await;
        assert!(r.is_err(), "pending results and a cancel");

        // The ordinary end: nothing to store, and the held Finished goes out.
        let (tx, mut rx) = unbounded_channel();
        let r = finish_run(
            &flake,
            &cache,
            &CancellationToken::new(),
            &tx,
            (&key, &guard),
            vec![],
            finished(),
        )
        .await;
        assert!(r.is_ok());
        assert_eq!(rx.try_recv().unwrap(), EvalItem::Finished { error: None });
    }

    /// R03/R28: each file fits, but their total does not. Inject the cap into the same
    /// digest used in production instead of hashing a gigabyte under a wall-clock deadline.
    #[test]
    fn the_worktree_cap_is_a_total() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.bin", "b.bin"] {
            std::fs::File::create(dir.path().join(name))
                .unwrap()
                .set_len(600 << 10)
                .unwrap();
        }
        let digest = |paths: &[&[u8]]| {
            worktree_digest::<{ 1 << 20 }>(
                dir.path(),
                Path::new(""),
                b"",
                paths,
                b"",
                &|| false,
                &mut Sha256::new(),
            )
        };
        assert!(digest(&[b"a.bin"]).is_ok());
        assert!(digest(&[b"b.bin"]).is_ok());
        let Stop::NoCache(why) = digest(&[b"a.bin", b"b.bin"]).unwrap_err() else {
            panic!("the digest stopped without checking the cap");
        };
        assert_eq!(why, "the tracked files are larger than 1 MiB");
    }

    /// An executable `sh` script in `dir`, written by a separate process (as the
    /// integration tests' helper does), so no thread's fork can inherit a write descriptor
    /// for it and make executing it fail with "Text file busy".
    fn script(dir: &Path, body: &str) -> PathBuf {
        let sh = which("sh");
        let path = dir.join("fake-git");
        let ok = std::process::Command::new(&sh)
            .arg("-c")
            .arg(r#"printf '#!%s\n%s\n' "$1" "$2" > "$3" && chmod 755 "$3""#)
            .arg("sh")
            .arg(&sh)
            .arg(body)
            .arg(&path)
            .status()
            .unwrap();
        assert!(ok.success());
        path
    }

    fn which(name: &str) -> PathBuf {
        let c = command(OsStr::new(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        PathBuf::from(c.as_std().get_program())
    }

    /// Blocks this thread (so the runtime polls nothing) until process `pid` is a zombie.
    fn block_until_zombie(pid_file: &Path) {
        let began = std::time::Instant::now();
        loop {
            assert!(
                began.elapsed() < Duration::from_secs(30),
                "the fake never exited"
            );
            if let Ok(pid) = std::fs::read_to_string(pid_file) {
                let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
                // `pid (comm) S ...`: the state follows the last ')'.
                if stat.is_ok_and(|s| {
                    s.rsplit(')')
                        .next()
                        .is_some_and(|r| r.trim_start().starts_with('Z'))
                }) {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Polls `fut` once: far enough for `run` to have spawned the child and found nothing
    /// to read yet.
    async fn poll_once<F: std::future::Future>(mut fut: std::pin::Pin<&mut F>) {
        std::future::poll_fn(|cx| {
            assert!(
                fut.as_mut().poll(cx).is_pending(),
                "finished on the first poll"
            );
            std::task::Poll::Ready(())
        })
        .await;
    }

    fn params(program: &Path, timeout: Duration) -> GitParams<'_> {
        GitParams {
            program: program.as_os_str(),
            timeout,
            read_timeout: Duration::from_secs(5),
        }
    }

    /// Case 1 of the CI failure on `ca2cc3f`, without depending on machine speed: git
    /// writes one byte more than the cap and exits, and the runtime sees the exit before
    /// reading any of it (the thread is blocked until git is a zombie), so every byte is
    /// read after the exit. That is still "printed more than", never the output cut to the
    /// cap.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn output_over_the_cap_read_after_the_exit_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let fake = script(
            dir.path(),
            &format!(
                "echo $$ > '{d}/pid'\nwhile [ ! -e '{d}/go' ]; do :; done\nprintf '%01001d' 0"
            ),
        );
        let p = params(&fake, Duration::from_secs(20));
        let cancel = CancellationToken::new();
        let fut = git(&p, dir.path(), &["ls-files"], 1000, &cancel);
        tokio::pin!(fut);
        poll_once(fut.as_mut()).await;
        std::fs::write(dir.path().join("go"), "").unwrap();
        block_until_zombie(&dir.path().join("pid"));
        let r = fut.await.unwrap();
        assert_eq!(
            r,
            Err("git ls-files printed more than 1000 bytes".to_owned())
        );
    }

    /// The same at exactly the cap: all of it, accepted.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn output_at_the_cap_read_after_the_exit_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let fake = script(
            dir.path(),
            &format!(
                "echo $$ > '{d}/pid'\nwhile [ ! -e '{d}/go' ]; do :; done\nprintf '%01000d' 0"
            ),
        );
        let p = params(&fake, Duration::from_secs(20));
        let cancel = CancellationToken::new();
        let fut = git(&p, dir.path(), &["ls-files"], 1000, &cancel);
        tokio::pin!(fut);
        poll_once(fut.as_mut()).await;
        std::fs::write(dir.path().join("go"), "").unwrap();
        block_until_zombie(&dir.path().join("pid"));
        assert_eq!(fut.await.unwrap(), Ok(vec![b'0'; 1000]));
    }

    /// Case 2: git exits 0 but its stdout is still open, held by a process that left its
    /// session (`setsid`), so the group kill at exit does not reach it. Whatever was read is
    /// not the whole output and must not be taken for it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn output_not_read_to_its_end_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let (sh, setsid, sleep) = (which("sh"), which("setsid"), which("sleep"));
        // The holder writes its pid only once it is in its own session; git exits only
        // after that, so the group kill at the exit can never reach it.
        let fake = script(
            dir.path(),
            &format!(
                "printf 'partial'\n'{}' '{}' -c 'echo $$ > \"$1\"; exec \"$2\" 30' sh '{d}/holder' '{}' &\nwhile [ ! -s '{d}/holder' ]; do :; done",
                setsid.display(),
                sh.display(),
                sleep.display()
            ),
        );
        let p = params(&fake, Duration::from_secs(1));
        let r = git(
            &p,
            dir.path(),
            &["ls-files"],
            1000,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        if let Ok(pid) = std::fs::read_to_string(dir.path().join("holder"))
            && let Some(pid) = pid
                .trim()
                .parse()
                .ok()
                .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
        let e = r.expect_err("a partial output was accepted as complete");
        assert!(e.contains("not read to the end"), "{e}");
    }

    /// What `OnExit::KillGroup` gives git: a descendant still in git's group is killed when
    /// git exits, so the pipe reaches EOF at once and the output is accepted.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_descendant_in_the_group_does_not_hold_the_output_open() {
        let dir = tempfile::tempdir().unwrap();
        let sleep = which("sleep");
        let fake = script(
            dir.path(),
            &format!("printf 'whole'\n'{}' 30 &", sleep.display()),
        );
        let p = params(&fake, Duration::from_secs(20));
        let start = std::time::Instant::now();
        let r = git(
            &p,
            dir.path(),
            &["ls-files"],
            1000,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(r, Ok(b"whole".to_vec()));
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "waited for the deadline: {:?}",
            start.elapsed()
        );
    }
}
