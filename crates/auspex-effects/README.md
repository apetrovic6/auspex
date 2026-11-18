# `auspex-effects`

auspex's adapters: everything that touches the outside world. The ssh probe, the machine
list (`nix eval`), the evaluation (`nix-eval-jobs`), the eval cache, which `clan` binary
the fleet pins (`clan`: built from its lock, never taken from `PATH`) and running it, Prometheus, the narinfo
"cached" badge, DNS, and the local action log (`deploylog`, a SQLite database whose schema is a
documented contract: see "The action log" below). This crate is the only one in auspex with tokio.

**Admission rule:** a function belongs here when it *obtains* something: it runs a process,
reads or writes a file, asks a server, or resolves a name. It takes everything as arguments,
hands what it got to `auspex-domain` to be parsed and judged, and returns the domain's value.
Parsing and status rules stay in `auspex-domain`, and deciding what to ask stays in the Crux
core. This crate never depends on the core: the TUI shell maps each core `Effect` to a
function here.

Every function has a timeout and takes a `CancellationToken`. A cancelled call kills the
whole process group of anything it spawned, reaps it, and returns `Err(Cancelled)`. No
process is run through a shell, no program is found through a relative `PATH` entry, and
no name or path is spliced into argv or Nix source: the crate rustdoc and each module say
how.

**The shell must handle `SIGHUP`, `SIGTERM`, `SIGINT` and `SIGQUIT`**: create one
`auspex_effects::Termination` before the first child is spawned, keep it for the whole run
(never recreate it in a loop), and when its `recv` fires, cancel every token, drop the
runtime and exit. Children run in sessions of their own, so nothing else stops them if
auspex is killed by a signal.

Plain filesystem flake references are normalised to `git+file://<repository>?dir=<flake>`
inside Git repositories. Inventory and evaluation receive the same reference: the Nix CLI
and `builtins.getFlake` otherwise disagree about plain paths. Untracked, ignored and `.git`
files are excluded, as for `nix build .#…`: a new machine module must be `git add`ed before
auspex evaluates it. Explicit `path:` references retain path semantics and are not cached.

The cache hashes HEAD, index records (including assume-unchanged/skip-worktree flags),
tracked worktree contents and modes, the flake directory, and `flake.lock`. Results stream
immediately, but successful results are written only after a second key matches the first.
An edit during evaluation leaves that run uncached. This is a before/after consistency
check, not an immutable snapshot; edits that are reverted before the second key are not
detected.

On Linux, git runs through util-linux `prlimit`, which sets the 512 MiB address-space limit
**before exec**; the dev shell, pure CI and the installed package supply it (the package
appends it to `PATH`, so the operator's own tools keep precedence). Without it, or on
another platform, there is no caching. A hostile `.git/` costs at most that limit and a
2-second git timeout per refresh. The index listing is capped at 64 MiB.

Evaluator machine-list files require a private directory (0700, owned by this user, not
a symlink), but have no SSH socket-path limit. SSH control directories still have the
45-byte limit.

Tests: `tests/ssh.rs` (fake `ssh` scripts, control-directory checks), `tests/nix_fake.rs`
(fake `nix` and `nix-eval-jobs`), `tests/nix_fixture.rs` (the real tools against
`tests/fixtures/flake`, skipped where `nix` or `nix-eval-jobs` is missing),
`tests/cache.rs` (temporary git repositories), `tests/cache_key.rs` (what git may read, and
what the key must tell apart), `tests/cache_hostile.rs` (a fleet repository,
cache directory or `flake.lock` prepared by someone else), `tests/signals.rs` (auspex itself
signalled), and `tests/http.rs` (a server on 127.0.0.1, and TLS with an untrusted
certificate from `tests/fixtures/tls`).
None contacts a machine or the network beyond loopback. The action log's tests are unit tests
(`src/deploylog/tests.rs`, which re-run the test binary for the multi-process cases); each opens a
database in its own temporary directory, and a test build panics if one asks for the real state
directory.

## The action log

`deploylog` records everything auspex does to a machine (deploy, build, reboot, rollback,
freeze, unfreeze) in one SQLite database. **The weekly homelab digest reads that file
directly, so what follows is a stable, versioned contract**, like the `--json` output, and
`the_schema_is_pinned` (in `src/deploylog/tests.rs`) fails when the schema changes.

**Location.** `$XDG_STATE_HOME/auspex/log.sqlite`, else `~/.local/state/auspex/log.sqlite`
(a relative `XDG_STATE_HOME` is ignored). The directory is 0700 and owned by the user, the
file 0600 with no hard link besides its own; a symlink in place of either is refused. Never
inside the fleet repository (checked before any directory is created). The `-wal` and `-shm`
files auspex creates take the database's mode from SQLite; an existing one is not checked. The
file is in WAL mode: read it with a read-only connection (`file:…/log.sqlite?mode=ro`), never
write to it, and never `VACUUM` or change its journal mode.

**Local file systems only.** WAL keeps an index in shared memory, which does not work across
hosts: two machines writing one log over NFS can corrupt it. auspex refuses a directory on NFS,
SMB/CIFS, FUSE or 9p. Do not put the file on one, and do not share it between machines. On a
read-only file system it opens read-only: history is shown, `start` and `finish` fail.

**Version.** `PRAGMA user_version` is the schema version, currently **1**. An empty file is
created at 1. auspex refuses (and does not touch) a file with a higher version, and a file with
tables but version 0. The rule for changes: a **new nullable column** is additive and keeps
version 1 (a reader must name the columns it wants, never `SELECT *` by position); **anything
else** (renaming, removing or retyping a column, changing a value set, a new `NOT NULL`
column) bumps the version, and ships with a migration that runs on open. Rows are never
deleted by auspex. *Known gap:* the version is checked when a process opens the file, not on
every write, so an older auspex that is still running when a newer one migrates the file keeps
inserting rows in the old shape. Before the first migration exists, writes must check the
version (or the migration must keep old-shape inserts valid); until then, stop every auspex
before upgrading one.

**Table `actions`**, one row per action:

| Column | Type | Meaning |
|---|---|---|
| `id` | integer, primary key | Row id; increases with time of insertion. |
| `kind` | text, not null | `deploy` (`clan machines update`), `build` (build only), `reboot`, `rollback`, `freeze`, `unfreeze`. |
| `machine` | text, not null | The machine's name from the inventory, sanitised (no control or format characters), at most 255 bytes. |
| `fleet_commit` | text | The fleet repository's commit when the action started: lowercase hex, 4 to 64 characters. NULL when unknown. |
| `fleet_dirty` | integer | 1 if the fleet tree had uncommitted changes, 0 if clean, NULL when unknown. |
| `started_at` | text, not null | UTC RFC 3339 with milliseconds, fixed width: `2026-11-18T09:30:00.123Z`. Text order is time order. |
| `ended_at` | text | Same form. NULL while running, and for a row nobody finished. |
| `result` | text | `succeeded`, `failed`, `cancelled`, `timed_out` or `interrupted` (auspex stopped before it knew). NULL while running or unfinished. NULL exactly when `ended_at` is. |
| `exit_code` | integer | The process's exit code, when there was one (never set before `ended_at`). |
| `who` | text, not null | The operator (`$USER`). It records, it does not authenticate. |
| `host` | text, not null | The host auspex ran on. |
| `detail` | text | A short summary of the outcome, sanitised line by line (control characters removed, empty lines dropped), at most 4096 bytes. **If it was longer, its end was kept** and it starts with `…`: the cause of a failure is last. **Never clan's output**: the caller passes a summary, because output can hold whatever a machine prints. |
| `auspex_version` | text, not null | The auspex that wrote the row, sanitised, at most 255 bytes. |
| `pid`, `pid_start`, `machine_id`, `pid_ns` | integer, text, text, text | The writing process, for auspex's own interrupted-row check: its pid; `<boot id>/<start time>`; `/etc/machine-id` (the host name on macOS); the pid-namespace link (`pid:[<inode>]`). **Ignore them**: not part of the contract's meaning. |

**An unfinished row.** `result IS NULL` means *running*, or *auspex died before it
finished*. auspex's own queries tell the two apart (a row whose writing process is gone is
reported `interrupted`) and write nothing back. They judge only a row whose writer they can
see: same `host`, same `machine_id`, same `pid_ns`. A writer in a container (another pid
namespace) or on another machine that shares the host name stays *running* for ever, never
dead. SQL cannot look at processes, so **the digest reads a row with `result IS NULL` and
`started_at` more than 24 hours old as interrupted**, and a younger one as running.

**Indexes.** `actions_machine_started (machine, started_at DESC, id DESC)` answers "the last N
actions of machine M" without a sort; `actions_started (started_at, id)` answers "everything in
the last 7 days" with a range scan. The log gains a handful of rows a day; nothing else is
indexed.

**The digest's week**, for example (`the_readmes_digest_query_runs_and_counts_what_it_says`
runs it against a fixture):

```sql
SELECT machine,
       count(*)                                                       AS actions,
       coalesce(sum(kind = 'deploy' AND result = 'succeeded'), 0)     AS deploys_ok,
       coalesce(sum(result IN ('failed', 'timed_out')), 0)            AS failures,
       coalesce(sum(result = 'interrupted'
                    OR (result IS NULL
                        AND started_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-24 hours'))), 0)
                                                                      AS interrupted
FROM actions
WHERE started_at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-7 days')
GROUP BY machine
ORDER BY machine;
```

**Concurrency and failure.** Two auspex processes (the TUI and a `--json` run) may write at
once: WAL, a 5-second busy timeout, one short statement per transaction, all on tokio's
blocking pool. Every call has **one total deadline, 2 seconds by default**, covering its wait
for the shared connection, for the lock and the statement, so a burst of calls (one per machine
for `U`) waits 2 seconds in all, not once per call. A call that misses it fails with `Timeout`
and writes nothing. A call whose future is dropped before it begins its statement never
begins it; one already in its statement completes atomically.

**The log must never hold up or decide a deploy.** Callers do not await a log call on a deploy's
critical path (start the deploy, and write the row beside it), and never turn a log error into
the action's result: a full disk, a locked file, a read-only file system or a timeout fail the
log call, and the deploy that ran still succeeded or failed on its own.
