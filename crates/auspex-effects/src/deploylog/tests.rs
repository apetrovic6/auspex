//! Tests of the action log. Every one opens a database under its own temporary directory:
//! `Inner::open` panics in a test build if it is asked for the real state directory's log
//! (`forbid_default_location_in_tests`, itself tested below), so a test that forgot cannot
//! write to `~/.local/state/auspex`.
//!
//! The multi-process tests re-run this test binary on `child` (selected by environment
//! variables), as `tests/signals.rs` does for signals.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::*;

const HOST: &str = "testhost";

fn identity(host: &str) -> Identity {
    Identity {
        who: "operator".to_owned(),
        host: host.to_owned(),
        auspex_version: "9.9.9".to_owned(),
    }
}

fn dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

/// `<dir>/state/log.sqlite`: the `state` directory does not exist yet, so opening creates it.
fn db_in(dir: &Path) -> PathBuf {
    dir.join("state").join(FILE_NAME)
}

/// A fleet that no test directory is inside (it does not exist: `refuse_inside` resolves what
/// does not exist yet).
fn elsewhere() -> PathBuf {
    PathBuf::from("/nonexistent-auspex-test-fleet")
}

fn params(path: PathBuf, host: &str) -> OpenParams {
    OpenParams::new(path, identity(host), elsewhere())
}

async fn open_at(path: &Path, host: &str) -> ActionLog {
    ActionLog::open(params(path.to_owned(), host))
        .await
        .unwrap()
}

/// A call with all the time in the world, for the tests that call `Inner` directly.
fn call() -> Call {
    Call::within(Duration::from_secs(60))
}

async fn open_in(dir: &Path) -> ActionLog {
    open_at(&db_in(dir), HOST).await
}

fn new_action(kind: ActionKind, machine: &str) -> NewAction {
    NewAction {
        kind,
        machine: machine.to_owned(),
        fleet_commit: None,
        fleet_dirty: None,
    }
}

fn outcome(result: ActionResult) -> Outcome {
    Outcome {
        result,
        exit_code: None,
        detail: None,
    }
}

/// A plain connection to the file, for the tests that look at it or damage it.
fn raw(path: &Path) -> Connection {
    let c = Connection::open(path).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn ts(ms: i64) -> Timestamp {
    Timestamp::from_unix_millis(ms)
}

// ----- the basics -----

#[tokio::test]
async fn create_start_finish_query() {
    let d = dir();
    let log = open_in(d.path()).await;

    let id = log
        .start(NewAction {
            kind: ActionKind::Deploy,
            machine: "luna".to_owned(),
            fleet_commit: Some("0123abcd".to_owned()),
            fleet_dirty: Some(true),
        })
        .await
        .unwrap();

    // Started and not finished, by a process that is alive (this one): running.
    let rows = log.recent("luna", 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.id, id);
    assert_eq!(row.kind, ActionKind::Deploy);
    assert_eq!(row.machine, "luna");
    assert_eq!(row.fleet_commit.as_deref(), Some("0123abcd"));
    assert_eq!(row.fleet_dirty, Some(true));
    assert_eq!(
        (row.ended_at, row.result, row.exit_code),
        (None, None, None)
    );
    assert_eq!(
        (
            row.who.as_str(),
            row.host.as_str(),
            row.auspex_version.as_str()
        ),
        ("operator", HOST, "9.9.9")
    );

    log.finish(
        id,
        Outcome {
            result: ActionResult::Failed,
            exit_code: Some(1),
            detail: Some("error: build of 'x' failed".to_owned()),
        },
    )
    .await
    .unwrap();

    let rows = log.recent("luna", 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.result, Some(ActionResult::Failed));
    assert_eq!(row.exit_code, Some(1));
    assert_eq!(row.detail.as_deref(), Some("error: build of 'x' failed"));
    assert!(row.ended_at.unwrap() >= row.started_at);

    // Reopened by another handle, the row is there.
    let again = open_in(d.path()).await;
    assert_eq!(again.since(ts(0)).await.unwrap(), rows);
}

#[tokio::test]
async fn recent_and_since_order_and_bound_their_rows() {
    let d = dir();
    let log = open_in(d.path()).await;
    let i = &log.inner;
    // Times chosen by hand: the third action of "a" shares its millisecond with the second,
    // so the id breaks the tie.
    let a1 = i
        .start(&new_action(ActionKind::Deploy, "a"), ts(1_000), &call())
        .unwrap();
    let a2 = i
        .start(&new_action(ActionKind::Build, "a"), ts(2_000), &call())
        .unwrap();
    let a3 = i
        .start(&new_action(ActionKind::Reboot, "a"), ts(2_000), &call())
        .unwrap();
    let b1 = i
        .start(&new_action(ActionKind::Deploy, "b"), ts(3_000), &call())
        .unwrap();

    let ids = |rows: Vec<Action>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(ids(log.recent("a", 10).await.unwrap()), [a3, a2, a1]);
    assert_eq!(ids(log.recent("a", 2).await.unwrap()), [a3, a2]);
    assert_eq!(ids(log.recent("a", 0).await.unwrap()), []);
    assert_eq!(ids(log.recent("b", 10).await.unwrap()), [b1]);
    assert_eq!(ids(log.recent("nobody", 10).await.unwrap()), []);

    // `since` includes its own instant and is oldest first.
    assert_eq!(ids(log.since(ts(2_000)).await.unwrap()), [a2, a3, b1]);
    assert_eq!(ids(log.since(ts(2_001)).await.unwrap()), [b1]);
    assert_eq!(ids(log.since(ts(0)).await.unwrap()), [a1, a2, a3, b1]);
    assert_eq!(ids(log.since(ts(9_000)).await.unwrap()), []);
}

#[tokio::test]
async fn finish_completes_only_a_running_row_of_this_process() {
    let d = dir();
    let log = open_in(d.path()).await;
    let id = log.start(new_action(ActionKind::Build, "m")).await.unwrap();
    log.finish(id, outcome(ActionResult::Succeeded))
        .await
        .unwrap();

    // Again: already finished.
    let e = log
        .finish(id, outcome(ActionResult::Failed))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::NotRunning(i) if i == id), "{e}");
    // An id nobody has.
    let e = log
        .finish(ActionId(999), outcome(ActionResult::Failed))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::NotRunning(_)), "{e}");
    assert_eq!(
        log.recent("m", 1).await.unwrap()[0].result,
        Some(ActionResult::Succeeded)
    );

    // Another process's live row: not ours to complete, whatever its id.
    let theirs = log.start(new_action(ActionKind::Build, "m")).await.unwrap();
    raw(log.path())
        .execute("UPDATE actions SET pid = pid + 1 WHERE id = ?1", [theirs.0])
        .unwrap();
    let e = log
        .finish(theirs, outcome(ActionResult::Cancelled))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::NotRunning(_)), "{e}");
    let ended: Option<String> = raw(log.path())
        .query_row(
            "SELECT ended_at FROM actions WHERE id = ?1",
            [theirs.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ended, None);
}

// ----- the schema -----

/// The exact contents of the database. The digest reads this file, so a change here is a
/// change to a published contract: update the README's "The action log" in the same commit,
/// and bump `user_version` unless the change is a new nullable column.
#[tokio::test]
async fn the_schema_is_pinned() {
    let d = dir();
    let log = open_in(d.path()).await;
    let c = raw(log.path());

    let version: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1);
    let journal: String = c
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "wal");

    let mut stmt = c
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
        .unwrap();
    let got: Vec<(String, String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    let expected: [(&str, &str, &str, &str); 3] = [
        (
            "index",
            "actions_machine_started",
            "actions",
            "CREATE INDEX actions_machine_started ON actions (machine, started_at DESC, id DESC)",
        ),
        (
            "index",
            "actions_started",
            "actions",
            "CREATE INDEX actions_started ON actions (started_at, id)",
        ),
        (
            "table",
            "actions",
            "actions",
            "CREATE TABLE actions (
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
)",
        ),
    ];
    let expected: Vec<_> = expected
        .iter()
        .map(|(a, b, c, d)| {
            (
                (*a).to_owned(),
                (*b).to_owned(),
                (*c).to_owned(),
                (*d).to_owned(),
            )
        })
        .collect();
    assert_eq!(got, expected);

    // The column names and order, which is what a `SELECT *` in the digest would see.
    let columns: Vec<String> = c
        .prepare("SELECT name FROM pragma_table_info('actions') ORDER BY cid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        columns,
        [
            "id",
            "kind",
            "machine",
            "fleet_commit",
            "fleet_dirty",
            "started_at",
            "ended_at",
            "result",
            "exit_code",
            "who",
            "host",
            "detail",
            "auspex_version",
            "pid",
            "pid_start",
            "machine_id",
            "pid_ns"
        ]
    );
}

/// The two documented questions use the two indexes and sort nothing.
#[tokio::test]
async fn the_indexes_serve_the_two_documented_queries() {
    let d = dir();
    let log = open_in(d.path()).await;
    let c = raw(log.path());
    let plan = |sql: &str| -> String {
        c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let recent = plan(
        "SELECT id FROM actions WHERE machine = 'm' ORDER BY started_at DESC, id DESC LIMIT 5",
    );
    assert!(recent.contains("actions_machine_started"), "{recent}");
    assert!(!recent.contains("TEMP B-TREE"), "{recent}");
    let since = plan(
        "SELECT id FROM actions WHERE started_at >= '2026-01-01T00:00:00.000Z' \
         ORDER BY started_at, id",
    );
    assert!(since.contains("actions_started"), "{since}");
    assert!(!since.contains("TEMP B-TREE"), "{since}");
}

#[tokio::test]
async fn the_database_refuses_what_the_contract_forbids() {
    let d = dir();
    let log = open_in(d.path()).await;
    let c = raw(log.path());
    let insert = |kind: &str, commit: &str, ended: &str, result: &str, code: &str, detail: &str| {
        c.execute(
            &format!(
                "INSERT INTO actions (kind, machine, fleet_commit, started_at, ended_at, result, \
                 exit_code, who, host, detail, auspex_version, pid) VALUES ('{kind}', 'm', {commit}, \
                 '2026-01-01T00:00:00.000Z', {ended}, {result}, {code}, 'w', 'h', {detail}, 'v', 1)"
            ),
            [],
        )
    };
    let none = "NULL";
    let end = "'2026-01-01T00:00:01.000Z'";
    // A good row goes in…
    insert("deploy", "'abcd'", end, "'failed'", "1", none).unwrap();
    insert("freeze", none, none, none, none, none).unwrap();
    // …and each rule refuses its violation.
    assert!(
        insert("nuke", none, none, none, none, none).is_err(),
        "kind"
    );
    assert!(
        insert("deploy", none, end, "'weird'", none, none).is_err(),
        "result"
    );
    assert!(
        insert("deploy", none, end, none, none, none).is_err(),
        "end without result"
    );
    assert!(
        insert("deploy", none, none, "'failed'", none, none).is_err(),
        "result without end"
    );
    assert!(
        insert("deploy", none, none, none, "1", none).is_err(),
        "exit code while running"
    );
    assert!(
        insert("deploy", "'ABCD'", none, none, none, none).is_err(),
        "upper-case commit"
    );
    assert!(
        insert("deploy", "'abc'", none, none, none, none).is_err(),
        "short commit"
    );
    assert!(
        insert("deploy", "'zzzz'", none, none, none, none).is_err(),
        "non-hex commit"
    );
    assert!(
        insert("deploy", none, "'yesterday'", "'failed'", none, none).is_err(),
        "end time form"
    );
    let big = format!("'{}'", "x".repeat(DETAIL_CAP + 1));
    assert!(
        insert("deploy", none, none, none, none, &big).is_err(),
        "detail cap"
    );
    let fits = format!("'{}'", "x".repeat(DETAIL_CAP));
    insert("deploy", none, none, none, none, &fits).unwrap();
}

// ----- versions and foreign files -----

#[tokio::test]
async fn a_newer_schema_version_refuses_and_leaves_the_file_alone() {
    let d = dir();
    let path = db_in(d.path());
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    std::fs::set_permissions(path.parent().unwrap(), PermissionsExt::from_mode(0o700)).unwrap();
    {
        let c = raw(&path);
        c.execute_batch("CREATE TABLE actions (future TEXT); PRAGMA user_version = 2;")
            .unwrap();
    }
    std::fs::set_permissions(&path, PermissionsExt::from_mode(0o600)).unwrap();
    let before = std::fs::read(&path).unwrap();

    let e = ActionLog::open(params(path.clone(), HOST))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::NewerSchema { found: 2, .. }), "{e}");
    let text = e.to_string();
    assert!(text.contains("version 2") && text.contains("newer") && text.contains("update auspex"));

    // Not rewritten, not downgraded, not switched to WAL, and no side files.
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let c = raw(&path);
    let version: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
    let journal: String = c
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "delete");
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );
}

#[tokio::test]
async fn a_database_that_is_not_ours_refuses() {
    let d = dir();
    let path = db_in(d.path());
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    std::fs::set_permissions(path.parent().unwrap(), PermissionsExt::from_mode(0o700)).unwrap();
    // Tables, but `user_version` 0: somebody else's.
    raw(&path)
        .execute_batch("CREATE TABLE notes (t TEXT)")
        .unwrap();
    std::fs::set_permissions(&path, PermissionsExt::from_mode(0o600)).unwrap();
    let before = std::fs::read(&path).unwrap();
    let e = ActionLog::open(params(path.clone(), HOST))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::NotOurs(_)), "{e}");
    // Refused before anything was done to it: not rewritten, not switched to WAL, no side
    // files.
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let journal: String = raw(&path)
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "delete");
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );

    // Not a database at all.
    std::fs::write(
        &path,
        b"this is not sqlite, it is a text file of some length\n",
    )
    .unwrap();
    assert!(ActionLog::open(params(path.clone(), HOST)).await.is_err());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"this is not sqlite, it is a text file of some length\n"
    );
}

#[tokio::test]
async fn an_empty_file_becomes_version_one_and_reopening_keeps_it() {
    let d = dir();
    let path = db_in(d.path());
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    std::fs::set_permissions(path.parent().unwrap(), PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::write(&path, b"").unwrap();
    std::fs::set_permissions(&path, PermissionsExt::from_mode(0o600)).unwrap();

    let log = open_at(&path, HOST).await;
    let id = log
        .start(new_action(ActionKind::Freeze, "m"))
        .await
        .unwrap();
    log.finish(id, outcome(ActionResult::Succeeded))
        .await
        .unwrap();
    drop(log);
    let log = open_at(&path, HOST).await;
    assert_eq!(log.since(ts(0)).await.unwrap().len(), 1);
    let version: i64 = raw(&path)
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

// ----- where it lives, and who may touch it -----

#[test]
fn the_default_location() {
    let p =
        |x: Option<&str>, h: Option<&str>| default_path_from(x.map(OsStr::new), h.map(OsStr::new));
    assert_eq!(
        p(Some("/state"), Some("/home/u")),
        Some(PathBuf::from("/state/auspex/log.sqlite"))
    );
    // A relative XDG_STATE_HOME is ignored, as the XDG specification says.
    assert_eq!(
        p(Some("state"), Some("/home/u")),
        Some(PathBuf::from("/home/u/.local/state/auspex/log.sqlite"))
    );
    assert_eq!(
        p(None, Some("/home/u")),
        Some(PathBuf::from("/home/u/.local/state/auspex/log.sqlite"))
    );
    assert_eq!(p(Some("state"), Some("home")), None);
    assert_eq!(p(None, None), None);
}

#[test]
fn a_test_that_opens_the_real_location_is_stopped() {
    let real = Path::new("/home/u/.local/state/auspex/log.sqlite");
    let caught = std::panic::catch_unwind(|| {
        forbid_default_location_in_tests(real, Some(real));
    });
    assert!(caught.is_err(), "the real location was not refused");
    // Reached through `..` or a symlinked parent, too.
    let d = dir();
    let state = d.path().join("auspex");
    std::fs::create_dir(&state).unwrap();
    let link = d.path().join("link");
    symlink(&state, &link).unwrap();
    let default = state.join(FILE_NAME);
    let caught = std::panic::catch_unwind(|| {
        forbid_default_location_in_tests(&link.join(FILE_NAME), Some(&default));
    });
    assert!(
        caught.is_err(),
        "the real location through a symlink was not refused"
    );
    // Any other path is fine.
    forbid_default_location_in_tests(&d.path().join("other.sqlite"), Some(&default));
    forbid_default_location_in_tests(&default, None);
}

#[tokio::test]
async fn the_directory_is_private_and_the_files_are_0600() {
    let d = dir();
    let log = open_in(d.path()).await;
    let state = db_in(d.path()).parent().unwrap().to_owned();
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(log.path()), 0o600);
    // A write makes the WAL and its shared-memory file; SQLite gives them the database's mode.
    log.start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", log.path().display()));
        assert_eq!(mode(&side), 0o600, "{}", side.display());
    }
}

#[tokio::test]
async fn a_directory_or_file_with_the_wrong_mode_is_refused() {
    let d = dir();
    let log = open_in(d.path()).await;
    let path = log.path().to_owned();
    drop(log);
    let state = path.parent().unwrap();

    std::fs::set_permissions(&path, PermissionsExt::from_mode(0o644)).unwrap();
    let e = ActionLog::open(params(path.clone(), HOST))
        .await
        .unwrap_err();
    assert!(
        matches!(e, Error::Unusable(_)) && e.to_string().contains("0644"),
        "{e}"
    );
    std::fs::set_permissions(&path, PermissionsExt::from_mode(0o600)).unwrap();

    std::fs::set_permissions(state, PermissionsExt::from_mode(0o755)).unwrap();
    let e = ActionLog::open(params(path.clone(), HOST))
        .await
        .unwrap_err();
    assert!(
        matches!(e, Error::Unusable(_)) && e.to_string().contains("0755"),
        "{e}"
    );
    // The refusal changed nothing: still 0755, not quietly tightened.
    assert_eq!(mode(state), 0o755);
}

#[tokio::test]
async fn a_symlinked_directory_is_refused() {
    let d = dir();
    let real = d.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, PermissionsExt::from_mode(0o700)).unwrap();
    let link = d.path().join("link");
    symlink(&real, &link).unwrap();
    let e = ActionLog::open(params(link.join(FILE_NAME), HOST))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Unusable(m) if m.contains("symlink")),
        "{e}"
    );
    assert!(!real.join(FILE_NAME).exists());
}

#[tokio::test]
async fn a_symlinked_database_file_is_refused_and_not_created_through() {
    let d = dir();
    let state = d.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, PermissionsExt::from_mode(0o700)).unwrap();
    let elsewhere = d.path().join("elsewhere.sqlite");

    // Dangling: opening must not create the target.
    symlink(&elsewhere, state.join(FILE_NAME)).unwrap();
    let e = ActionLog::open(params(state.join(FILE_NAME), HOST))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Unusable(m) if m.contains("symlink")),
        "{e}"
    );
    assert!(!elsewhere.exists());

    // To a real, private file: refused all the same, and not touched.
    std::fs::write(&elsewhere, b"").unwrap();
    std::fs::set_permissions(&elsewhere, PermissionsExt::from_mode(0o600)).unwrap();
    let e = ActionLog::open(params(state.join(FILE_NAME), HOST))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Unusable(m) if m.contains("symlink")),
        "{e}"
    );
    assert_eq!(std::fs::metadata(&elsewhere).unwrap().len(), 0);
}

#[tokio::test]
async fn a_log_inside_the_fleet_is_refused() {
    let d = dir();
    let fleet = d.path().join("fleet");
    std::fs::create_dir(&fleet).unwrap();
    let mut p = params(fleet.join("sub").join(FILE_NAME), HOST);
    p.fleet = fleet.clone();
    let e = ActionLog::open(p).await.unwrap_err();
    assert!(
        matches!(&e, Error::Unusable(m) if m.contains("inside the fleet")),
        "{e}"
    );
    assert!(!fleet.join("sub").join(FILE_NAME).exists());
    // Refused before anything was created: no directory appeared in the fleet.
    assert!(!fleet.join("sub").exists());
    let mut p = params(fleet.join("a").join("b").join(FILE_NAME), HOST);
    p.fleet = fleet.clone();
    assert!(ActionLog::open(p).await.is_err());
    assert_eq!(
        std::fs::read_dir(&fleet).unwrap().count(),
        0,
        "the fleet was touched"
    );

    // Through a symlink to it, too.
    let alias = d.path().join("alias");
    symlink(&fleet, &alias).unwrap();
    let mut p = params(alias.join("sub").join(FILE_NAME), HOST);
    p.fleet = fleet.clone();
    assert!(ActionLog::open(p).await.is_err());

    // Outside it, fine.
    let mut p = params(db_in(d.path()), HOST);
    p.fleet = fleet;
    ActionLog::open(p).await.unwrap();
}

// ----- untrusted text -----

#[tokio::test]
async fn names_and_details_are_sanitised() {
    let d = dir();
    let log = open_in(d.path()).await;
    // Escape sequences, a NUL, a bidi override and a zero-width space in the machine name.
    let id = log
        .start(new_action(
            ActionKind::Deploy,
            "\u{1b}[31mlu\0na\u{202e}\u{200b}\u{1b}[0m\n",
        ))
        .await
        .unwrap();
    log.finish(
        id,
        Outcome {
            result: ActionResult::Failed,
            exit_code: Some(255),
            detail: Some(
                "noise\nline 2\u{1b}[1m bold\u{1b}[0m\u{7}\n\nssh: \u{202e}connect to host\0 failed\n"
                    .to_owned(),
            ),
        },
    )
    .await
    .unwrap();
    // The same raw text finds it: the query is cleaned the way the write was.
    let rows = log.recent("\u{1b}[31mlu\0na\u{202e}", 5).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].machine, "luna");
    assert_eq!(
        rows[0].detail.as_deref(),
        Some("noise\nline 2 bold\nssh: connect to host failed")
    );
    // What is in the file, read without going through our code.
    let stored: [String; 2] = raw(log.path())
        .query_row("SELECT machine, detail FROM actions", [], |r| {
            Ok([r.get(0)?, r.get(1)?])
        })
        .unwrap();
    for text in stored {
        assert!(
            !text
                .chars()
                .any(|c| c.is_control() && c != '\n' || c == '\u{202e}' || c == '\u{200b}'),
            "{text:?}"
        );
    }

    // Nothing left of the detail: no detail, not an empty one.
    let id = log
        .start(new_action(ActionKind::Build, "luna"))
        .await
        .unwrap();
    log.finish(
        id,
        Outcome {
            result: ActionResult::Succeeded,
            exit_code: Some(0),
            detail: Some("\u{1b}[0m \n\t\n".to_owned()),
        },
    )
    .await
    .unwrap();
    assert_eq!(log.recent("luna", 1).await.unwrap()[0].detail, None);
}

#[tokio::test]
async fn values_are_capped_on_a_character_boundary() {
    let d = dir();
    let log = open_in(d.path()).await;

    let long_ascii = "m".repeat(300);
    // 'é' is two bytes: 200 of them are 400 bytes, and 255 - 3 is odd, so the cut falls
    // between the two bytes of one character unless it is moved.
    let long_wide = "é".repeat(200);
    for name in [&long_ascii, &long_wide] {
        let id = log
            .start(new_action(ActionKind::Build, name))
            .await
            .unwrap();
        log.finish(id, outcome(ActionResult::Succeeded))
            .await
            .unwrap();
        let rows = log.recent(name, 5).await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the capped name is found by the same long name"
        );
        assert!(rows[0].machine.len() <= NAME_CAP);
        assert!(rows[0].machine.ends_with('…'));
    }

    let id = log
        .start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    let huge = format!("{}\n", "é".repeat(10_000));
    log.finish(
        id,
        Outcome {
            result: ActionResult::Failed,
            exit_code: Some(1),
            detail: Some(huge),
        },
    )
    .await
    .unwrap();
    let detail = log.recent("m", 1).await.unwrap()[0].detail.clone().unwrap();
    assert!(
        detail.len() <= DETAIL_CAP && detail.len() > DETAIL_CAP - 4,
        "{}",
        detail.len()
    );
    // The end is kept, so the cut is at the front.
    assert!(detail.starts_with('…') && !detail.ends_with('…'));

    // Text that fits is not touched, to the byte.
    assert_eq!(cap(&"x".repeat(DETAIL_CAP), DETAIL_CAP).len(), DETAIL_CAP);
    assert_eq!(cap("abc", 3), "abc");
    assert_eq!(cap("abcd", 4), "abcd");
    assert_eq!(cap("abcde", 4), "a…");
    assert_eq!(cap_end("abcde", 4), "…e");
    assert_eq!(cap_end("abcd", 4), "abcd");
}

#[tokio::test]
async fn values_that_cannot_be_stored_are_refused() {
    let d = dir();
    let log = open_in(d.path()).await;
    for machine in ["", "  ", "\u{1b}[0m\0"] {
        let e = log
            .start(new_action(ActionKind::Deploy, machine))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Invalid(_)), "{machine:?}: {e}");
    }
    for commit in ["", "abc", "ABCDEF", "not hex!", &"a".repeat(65), "abc\0d"] {
        let e = log
            .start(NewAction {
                fleet_commit: Some(commit.to_owned()),
                ..new_action(ActionKind::Deploy, "m")
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Invalid(_)), "{commit:?}: {e}");
    }
    assert_eq!(log.since(ts(0)).await.unwrap().len(), 0);
    // A full sha1 and a full sha256 are fine.
    for commit in ["a".repeat(40), "0".repeat(64)] {
        log.start(NewAction {
            fleet_commit: Some(commit),
            ..new_action(ActionKind::Deploy, "m")
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn sql_in_text_is_data() {
    let d = dir();
    let log = open_in(d.path()).await;
    let nasty = "x'); DROP TABLE actions; --\"quoted\" \\ %_ ?1 :name $v";
    let id = log
        .start(new_action(ActionKind::Deploy, nasty))
        .await
        .unwrap();
    log.finish(
        id,
        Outcome {
            result: ActionResult::Failed,
            exit_code: Some(1),
            detail: Some("it's \"bad\"'); DELETE FROM actions; --".to_owned()),
        },
    )
    .await
    .unwrap();

    let rows = log.recent(nasty, 5).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].machine, nasty);
    assert_eq!(
        rows[0].detail.as_deref(),
        Some("it's \"bad\"'); DELETE FROM actions; --")
    );
    // The wildcard characters of LIKE are not wildcards here.
    assert_eq!(log.recent("%", 5).await.unwrap().len(), 0);
    assert_eq!(log.since(ts(0)).await.unwrap().len(), 1);
}

// ----- time -----

#[test]
fn timestamps_are_fixed_width_utc_with_milliseconds() {
    assert_eq!(ts(0).to_string(), "1970-01-01T00:00:00.000Z");
    assert_eq!(
        ts(1_700_000_000_123).to_string(),
        "2023-11-14T22:13:20.123Z"
    );
    assert_eq!(ts(951_782_400_000).to_string(), "2000-02-29T00:00:00.000Z");
    assert_eq!(
        ts(1_709_164_800_000).to_string(),
        "2024-02-29T00:00:00.000Z"
    );
    assert_eq!(ts(MAX_MILLIS).to_string(), "9999-12-31T23:59:59.999Z");
    // Out of range clamps rather than overflowing the fixed width.
    assert_eq!(ts(-5).to_string(), "1970-01-01T00:00:00.000Z");
    assert_eq!(ts(i64::MAX).to_string(), "9999-12-31T23:59:59.999Z");

    let mut previous = String::new();
    let mut at = 0_i64;
    while at < 4_000_000_000_000 {
        let t = ts(at);
        let text = t.to_string();
        assert_eq!(text.len(), 24);
        assert_eq!(Timestamp::parse(&text), Some(t), "{text}");
        assert!(
            text > previous,
            "text order is time order: {previous} < {text}"
        );
        previous = text;
        at += 86_399_777_123; // odd step: lands on every kind of day and time
    }

    for bad in [
        "",
        "2026-11-18T09:30:00Z",
        "2026-11-18 09:30:00.000Z",
        "2026-11-18T09:30:00.000+00:00",
        "2026-02-30T09:30:00.000Z",
        "2026-13-01T09:30:00.000Z",
        "2026-11-18T24:00:00.000Z",
        "2026-11-18T09:60:00.000Z",
        "2026-11-18T09:30:00.00éZ",
        "2026-11-18T09:30:00.000z",
    ] {
        assert_eq!(Timestamp::parse(bad), None, "{bad:?}");
    }
    assert_eq!(ts(10 * DAY_MILLIS).minus_days(7), ts(3 * DAY_MILLIS));
    assert_eq!(ts(DAY_MILLIS).minus_days(7), ts(0));
}

#[test]
fn proc_stat_is_read_from_the_last_parenthesis() {
    // Field 2 is a command name in parentheses and can hold spaces and parentheses; field 3
    // is the state and field 22 the start time.
    let line = "4242 (evil) S (name) R 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 \
                1000 100 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
    assert_eq!(parse_stat(line), Some(('R', 987_654)));
    assert_eq!(parse_stat("1 (x) Z 0"), None);
    assert_eq!(parse_stat("garbage"), None);
}

// ----- interrupted rows -----

#[tokio::test]
async fn a_row_whose_writer_is_this_live_process_is_running_not_interrupted() {
    let d = dir();
    let log = open_in(d.path()).await;
    log.start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    let row = &log.recent("m", 1).await.unwrap()[0];
    assert_eq!(row.result, None);
    // Nothing was written back either way.
    let stored: Option<String> = raw(log.path())
        .query_row("SELECT result FROM actions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, None);
}

#[tokio::test]
async fn a_recycled_pid_is_not_the_writer() {
    let d = dir();
    let log = open_in(d.path()).await;
    log.start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    // Our own pid, but a start time (and boot) that is not ours: some other process that
    // came to have this pid.
    raw(log.path())
        .execute("UPDATE actions SET pid_start = 'another-boot/1'", [])
        .unwrap();
    let row = &log.recent("m", 1).await.unwrap()[0];
    assert_eq!(
        (row.result, row.ended_at),
        (Some(ActionResult::Interrupted), None)
    );
    // It is a judgement at read time: the row itself is untouched.
    let stored: Option<String> = raw(log.path())
        .query_row("SELECT result FROM actions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, None);
}

#[tokio::test]
async fn a_row_from_another_host_is_never_judged() {
    let d = dir();
    let path = db_in(d.path());
    let there = open_at(&path, "elsewhere").await;
    there
        .start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    // Its writer's pid means nothing here, and is not even a process (this one's pid is,
    // but the start time says it is not).
    raw(&path)
        .execute("UPDATE actions SET pid = 4000000, pid_start = 'gone/1'", [])
        .unwrap();
    let here = open_at(&path, HOST).await;
    assert_eq!(here.recent("m", 1).await.unwrap()[0].result, None);
    // On its own host, the same row is a dead writer's.
    assert_eq!(
        there.recent("m", 1).await.unwrap()[0].result,
        Some(ActionResult::Interrupted)
    );
}

const MODE: &str = "AUSPEX_TEST_DEPLOYLOG_MODE";
const WORK: &str = "AUSPEX_TEST_DEPLOYLOG_DIR";
const WHO: &str = "AUSPEX_TEST_DEPLOYLOG_WHO";
const ROUNDS: &str = "AUSPEX_TEST_DEPLOYLOG_ROUNDS";

fn child_command(mode: &str, dir: &Path, who: usize) -> Command {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args([
        "--exact",
        "deploylog::tests::child",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(MODE, mode)
    .env(WORK, dir)
    .env(WHO, who.to_string())
    .stdin(Stdio::null())
    .stdout(Stdio::null());
    c
}

fn wait_for(path: &Path) {
    let end = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(
            Instant::now() < end,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The other side of the multi-process tests: this binary, run again with the mode in its
/// environment. Run on its own, it does nothing.
#[tokio::test]
async fn child() {
    let (Ok(mode), Some(dir), Ok(who)) = (
        std::env::var(MODE),
        std::env::var_os(WORK).map(PathBuf::from),
        std::env::var(WHO),
    ) else {
        return;
    };
    let path = dir.join("state").join(FILE_NAME);
    let touch = |name: String| std::fs::write(dir.join(name), "").unwrap();
    match mode.as_str() {
        // Wait at the barrier, then all open the (not yet existing) log at the same instant
        // and write as fast as possible. Every error is reported, none is fatal.
        "hammer" => {
            touch(format!("ready-{who}"));
            wait_for(&dir.join("go"));
            let mut errors = Vec::new();
            match ActionLog::open(params(path, HOST)).await {
                Err(e) => errors.push(format!("open: {e}")),
                Ok(log) => {
                    let rounds = std::env::var(ROUNDS)
                        .ok()
                        .and_then(|r| r.parse().ok())
                        .unwrap_or(HAMMER_ROUNDS);
                    for n in 0..rounds {
                        let id = match log
                            .start(new_action(ActionKind::Deploy, &format!("m{who}")))
                            .await
                        {
                            Ok(id) => id,
                            Err(e) => {
                                errors.push(format!("start {n}: {e}"));
                                continue;
                            }
                        };
                        if let Err(e) = log.recent(&format!("m{who}"), 5).await {
                            errors.push(format!("recent {n}: {e}"));
                        }
                        if let Err(e) = log.finish(id, outcome(ActionResult::Succeeded)).await {
                            errors.push(format!("finish {n}: {e}"));
                        }
                    }
                }
            }
            std::fs::write(dir.join(format!("errors-{who}")), errors.join("\n")).unwrap();
        }
        // Start an action, say so, and either die at once (no destructor runs, no finish) or
        // wait to be told to.
        "crash" | "live" => {
            let log = open_at(&path, HOST).await;
            log.start(new_action(ActionKind::Deploy, "m"))
                .await
                .unwrap();
            touch("started".to_owned());
            if mode == "live" {
                wait_for(&dir.join("stop"));
            }
            std::process::exit(0);
        }
        // Open, start an action and read it back, all in this one live process: what does it
        // say about its own row?
        "own" => {
            let log = open_at(&path, HOST).await;
            log.start(new_action(ActionKind::Deploy, "m"))
                .await
                .unwrap();
            let row = log.recent("m", 1).await.unwrap().remove(0);
            std::fs::write(dir.join("own-result"), format!("{:?}", row.result)).unwrap();
        }
        other => panic!("unknown mode {other}"),
    }
}

const HAMMER_ROUNDS: usize = 150;
const HAMMERS: usize = 5;

#[tokio::test]
async fn several_processes_write_at_once_without_locking_each_other_out() {
    let d = dir();
    let work = d.path();
    let mut children: Vec<Child> = (0..HAMMERS)
        .map(|n| child_command("hammer", work, n).spawn().unwrap())
        .collect();
    for n in 0..HAMMERS {
        wait_for(&work.join(format!("ready-{n}")));
    }
    std::fs::write(work.join("go"), "").unwrap();
    for c in &mut children {
        assert!(c.wait().unwrap().success());
    }

    for n in 0..HAMMERS {
        let errors = std::fs::read_to_string(work.join(format!("errors-{n}"))).unwrap();
        assert_eq!(errors, "", "child {n} had errors");
    }
    let log = open_in(work).await;
    let rows = log.since(ts(0)).await.unwrap();
    assert_eq!(rows.len(), HAMMERS * HAMMER_ROUNDS);
    assert!(
        rows.iter()
            .all(|r| r.result == Some(ActionResult::Succeeded))
    );
    let journal: String = raw(log.path())
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    // Ids are unique, and every process's rows are all there.
    let mut ids: Vec<_> = rows.iter().map(|r| r.id.get()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), rows.len());
    for n in 0..HAMMERS {
        assert_eq!(
            log.recent(&format!("m{n}"), 1000).await.unwrap().len(),
            HAMMER_ROUNDS
        );
    }
}

#[tokio::test]
async fn an_unfinished_row_of_a_dead_process_reads_as_interrupted() {
    let d = dir();
    let work = d.path();

    // A process that dies without finishing.
    let mut crashed = child_command("crash", work, 0).spawn().unwrap();
    wait_for(&work.join("started"));
    assert!(crashed.wait().unwrap().success()); // reaped: gone from /proc
    let log = open_in(work).await;
    let row = log.recent("m", 1).await.unwrap().remove(0);
    assert_eq!(
        (row.result, row.ended_at),
        (Some(ActionResult::Interrupted), None)
    );
    assert_eq!(row.kind, ActionKind::Deploy);
    // Read, not rewritten.
    let stored: Option<String> = raw(log.path())
        .query_row("SELECT result FROM actions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, None);
}

#[tokio::test]
async fn an_unfinished_row_of_a_live_process_is_running_until_it_dies() {
    let d = dir();
    let work = d.path();
    let mut live = child_command("live", work, 0).spawn().unwrap();
    wait_for(&work.join("started"));
    let log = open_in(work).await;

    // Alive: running, and a reader in another process does not touch the row.
    let row = log.recent("m", 1).await.unwrap().remove(0);
    assert_eq!(row.result, None);
    assert_eq!(row.ended_at, None);
    // Nor can this process finish it.
    let e = log
        .finish(row.id, outcome(ActionResult::Failed))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::NotRunning(_)), "{e}");
    assert_eq!(log.recent("m", 1).await.unwrap()[0].result, None);

    // It exits. Until `wait` reaps it, it is a zombie: in /proc, but it can write nothing.
    std::fs::write(work.join("stop"), "").unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let row = log.recent("m", 1).await.unwrap().remove(0);
        if row.result == Some(ActionResult::Interrupted) {
            break;
        }
        assert!(Instant::now() < end, "never read as interrupted: {row:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(live.wait().unwrap().success());
    assert_eq!(
        log.recent("m", 1).await.unwrap()[0].result,
        Some(ActionResult::Interrupted)
    );
}

// ----- nothing blocks the runtime -----

/// Runs `fut` on a current-thread runtime while another thread holds the connection's lock
/// for 300 ms, with a ticker task on the same runtime. If the call did its SQLite work on
/// the runtime's thread it would sit on the lock for those 300 ms and the ticker would not
/// run once; on the blocking pool the runtime keeps ticking.
async fn ticks_while_locked<T>(log: &ActionLog, fut: impl Future<Output = T>) -> (T, usize) {
    static TICKS: AtomicUsize = AtomicUsize::new(0);
    TICKS.store(0, Ordering::SeqCst);
    let ticker = tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            TICKS.fetch_add(1, Ordering::SeqCst);
        }
    });
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let inner = Arc::clone(&log.inner);
    let holder = std::thread::spawn(move || {
        let _guard = inner.conn.lock().unwrap();
        held_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
    });
    held_rx.recv().unwrap();
    let out = fut.await;
    let ticks = TICKS.load(Ordering::SeqCst);
    ticker.abort();
    holder.join().unwrap();
    (out, ticks)
}

#[tokio::test(flavor = "current_thread")]
async fn no_call_does_its_database_work_on_the_runtime() {
    let d = dir();
    let log = open_in(d.path()).await;
    let id = log.start(new_action(ActionKind::Build, "m")).await.unwrap();

    let (r, ticks) = ticks_while_locked(&log, log.start(new_action(ActionKind::Build, "n"))).await;
    r.unwrap();
    assert!(
        ticks >= 10,
        "start blocked the runtime ({ticks} ticks in 300 ms)"
    );

    let (r, ticks) =
        ticks_while_locked(&log, log.finish(id, outcome(ActionResult::Succeeded))).await;
    r.unwrap();
    assert!(ticks >= 10, "finish blocked the runtime ({ticks} ticks)");

    let (r, ticks) = ticks_while_locked(&log, log.recent("m", 5)).await;
    assert_eq!(r.unwrap().len(), 1);
    assert!(ticks >= 10, "recent blocked the runtime ({ticks} ticks)");

    let (r, ticks) = ticks_while_locked(&log, log.since(ts(0))).await;
    assert_eq!(r.unwrap().len(), 2);
    assert!(ticks >= 10, "since blocked the runtime ({ticks} ticks)");
}

/// The guard is wired into opening, not just correct on its own: with the "real" location
/// stood in for by a path in a temporary directory, opening it panics before touching the
/// file system.
#[test]
fn opening_the_default_location_panics_in_a_test_build() {
    let d = dir();
    let stand_in = db_in(d.path());
    let caught = std::panic::catch_unwind(|| {
        let _ = Inner::open_guarded(params(stand_in.clone(), HOST), Some(&stand_in));
    });
    assert!(
        caught.is_err(),
        "opening the default location did not panic"
    );
    assert!(!stand_in.exists() && !stand_in.parent().unwrap().exists());
}

/// Starts `count` hammer processes on `work`, releases them together, and returns every error
/// they reported.
fn run_hammers(work: &Path, count: usize, rounds: usize) -> Vec<String> {
    let mut children: Vec<Child> = (0..count)
        .map(|n| {
            child_command("hammer", work, n)
                .env(ROUNDS, rounds.to_string())
                .spawn()
                .unwrap()
        })
        .collect();
    for n in 0..count {
        wait_for(&work.join(format!("ready-{n}")));
    }
    std::fs::write(work.join("go"), "").unwrap();
    for c in &mut children {
        assert!(c.wait().unwrap().success());
    }
    (0..count)
        .map(|n| std::fs::read_to_string(work.join(format!("errors-{n}"))).unwrap())
        .filter(|e| !e.is_empty())
        .collect()
}

/// The race on a database that does not exist yet: the TUI and a `--json` run started
/// together. One round is rarely enough to lose it, so it is run many times, each on a
/// fresh directory. (Before the schema was created under `BEGIN IMMEDIATE` and the busy
/// failures retried, this failed about one run in five.)
#[tokio::test]
async fn processes_that_create_the_database_together_do_not_fail() {
    for round in 0..40 {
        let d = dir();
        let errors = run_hammers(d.path(), 6, 1);
        assert!(errors.is_empty(), "round {round}: {errors:?}");
        let log = open_in(d.path()).await;
        assert_eq!(log.since(ts(0)).await.unwrap().len(), 6, "round {round}");
        let version: i64 = raw(log.path())
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}

// =======================================================================================
// Fix round 1 (review of PR #18)
// =======================================================================================

/// Another connection takes the write lock for `how_long`, then lets go. Returns once it
/// holds it.
fn hold_write_lock(path: &Path, how_long: Duration) -> std::thread::JoinHandle<()> {
    let path = path.to_owned();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let c = raw(&path);
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        ready_tx.send(()).unwrap();
        std::thread::sleep(how_long);
        c.execute_batch("ROLLBACK").unwrap();
    });
    ready_rx.recv().unwrap();
    holder
}

fn params_with(path: PathBuf, busy: Duration, call: Duration) -> OpenParams {
    OpenParams {
        busy_timeout: busy,
        call_timeout: call,
        ..params(path, HOST)
    }
}

// ----- F1: `detail` keeps the cause -----

/// The review's first reproduction: seven long lines of context, then the cause. The cause is
/// last and must survive; the front is what goes.
#[tokio::test]
async fn a_long_detail_keeps_its_end_where_the_cause_is() {
    let d = dir();
    let log = open_in(d.path()).await;
    let id = log
        .start(new_action(ActionKind::Deploy, "d"))
        .await
        .unwrap();
    let mut text = String::new();
    for _ in 0..7 {
        text.push_str(&"x".repeat(900));
        text.push('\n');
    }
    text.push_str("error: activation failed on d: THE REAL CAUSE");
    log.finish(
        id,
        Outcome {
            result: ActionResult::Failed,
            exit_code: Some(4),
            detail: Some(text),
        },
    )
    .await
    .unwrap();
    let detail = log.recent("d", 1).await.unwrap()[0].detail.clone().unwrap();
    assert_eq!(detail.len(), DETAIL_CAP, "cut to exactly the cap");
    assert!(detail.starts_with('…'), "{:?}", &detail[..10]);
    assert!(
        detail.ends_with("error: activation failed on d: THE REAL CAUSE"),
        "{:?}",
        &detail[detail.len() - 50..]
    );
}

/// The second: a summary of twelve lines is a summary, and nothing takes its tail by lines.
#[tokio::test]
async fn a_summary_of_many_lines_keeps_all_of_them() {
    let d = dir();
    let log = open_in(d.path()).await;
    let id = log
        .start(new_action(ActionKind::Deploy, "d"))
        .await
        .unwrap();
    let lines: Vec<String> = (1..=12).map(|n| format!("summary line {n}")).collect();
    log.finish(
        id,
        Outcome {
            result: ActionResult::Failed,
            exit_code: Some(1),
            detail: Some(lines.join("\n")),
        },
    )
    .await
    .unwrap();
    let detail = log.recent("d", 1).await.unwrap()[0].detail.clone().unwrap();
    assert_eq!(detail, lines.join("\n"));
}

#[test]
fn the_cut_never_splits_a_multibyte_character() {
    // Three-byte characters, so that the cut offset is inside one for two of every three
    // positions: whatever the cap, the result is valid, within the cap, and the end of the
    // text.
    let text = "€".repeat(2000) + "end";
    for max in [4096, 4095, 4094, 101, 100, 99, 7, 6] {
        let cut = cap_end(&text, max);
        assert!(cut.len() <= max, "{max}: {}", cut.len());
        assert!(cut.starts_with('…') && cut.ends_with("end"), "{max}");
    }
    // The same through the whole pipeline, with a character straddling the cut.
    let piped = clean_detail(&format!("{}\n{}", "€".repeat(2000), "last line")).unwrap();
    assert!(piped.len() <= DETAIL_CAP && piped.starts_with('…') && piped.ends_with("last line"));
    // What fits is untouched, and empty lines and escapes are gone.
    assert_eq!(
        clean_detail("a\n\n\u{1b}[31mb\u{1b}[0m\n").as_deref(),
        Some("a\nb")
    );
    assert_eq!(clean_detail("\n \n"), None);
}

// ----- F2: a call has one deadline -----

/// The review's reproduction: busy timeout 1 s, another process holding the write lock, five
/// `start`s at once (one per machine, as `U` does). Each used to wait its own busy timeout in
/// turn, five seconds in all; now all five are over within one call deadline.
#[tokio::test]
async fn queued_calls_share_one_deadline() {
    let d = dir();
    let path = db_in(d.path());
    // A busy timeout much longer than the deadline: if it were not cut to what a call has
    // left, the call holding the connection would wait it out and overrun its deadline.
    let log = ActionLog::open(params_with(
        path.clone(),
        Duration::from_secs(2),
        Duration::from_millis(400),
    ))
    .await
    .unwrap();
    let holder = hold_write_lock(&path, Duration::from_secs(4));

    let began = Instant::now();
    let tasks: Vec<_> = (0..5)
        .map(|n| {
            let log = log.clone();
            tokio::spawn(async move {
                let t = Instant::now();
                let r = log
                    .start(new_action(ActionKind::Deploy, &format!("m{n}")))
                    .await;
                (t.elapsed(), r)
            })
        })
        .collect();
    for t in tasks {
        let (took, r) = t.await.unwrap();
        assert!(matches!(r, Err(Error::Timeout)), "{r:?}");
        // Every call, the one that held the connection included, within its own deadline
        // plus a margin for scheduling.
        assert!(
            took < Duration::from_millis(400 + 450),
            "one call took {took:?} with a deadline of 400 ms"
        );
    }
    assert!(
        began.elapsed() < Duration::from_millis(1500),
        "five calls took {:?}, not one deadline",
        began.elapsed()
    );
    holder.join().unwrap();
    // They wrote nothing, and the log works again once the lock is gone.
    assert_eq!(log.since(ts(0)).await.unwrap().len(), 0);
    log.start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
}

/// `OpenParams::busy_timeout` is what a statement waits for another process's lock, when the
/// call's own deadline allows it.
#[tokio::test]
async fn the_busy_timeout_bounds_a_statement() {
    let d = dir();
    let path = db_in(d.path());
    let log = ActionLog::open(params_with(
        path.clone(),
        Duration::from_millis(300),
        Duration::from_secs(30),
    ))
    .await
    .unwrap();
    let holder = hold_write_lock(&path, Duration::from_secs(4));
    let t = Instant::now();
    let r = log.start(new_action(ActionKind::Deploy, "m")).await;
    let took = t.elapsed();
    assert!(matches!(r, Err(Error::Timeout)), "{r:?}");
    assert!(
        took >= Duration::from_millis(250) && took < Duration::from_millis(1500),
        "waited {took:?} for a busy timeout of 300 ms"
    );
    holder.join().unwrap();
}

/// The deadline covers the wait for the shared connection too, not only SQLite's lock.
#[tokio::test]
async fn the_deadline_covers_the_wait_for_the_connection() {
    let d = dir();
    let log = ActionLog::open(params_with(
        db_in(d.path()),
        Duration::from_secs(5),
        Duration::from_millis(300),
    ))
    .await
    .unwrap();
    let inner = Arc::clone(&log.inner);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = inner.conn.lock().unwrap();
        held_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
    });
    held_rx.recv().unwrap();
    let t = Instant::now();
    let r = log.recent("m", 5).await;
    let took = t.elapsed();
    assert!(matches!(r, Err(Error::Timeout)), "{r:?}");
    assert!(took < Duration::from_millis(1000), "{took:?}");
    holder.join().unwrap();
}

/// Dropping the future of a call that has not begun its statement means it never begins it:
/// the write does not land later, after the caller stopped listening.
#[tokio::test]
async fn a_dropped_call_that_has_not_begun_writes_nothing() {
    let d = dir();
    let log = open_in(d.path()).await;
    let inner = Arc::clone(&log.inner);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _guard = inner.conn.lock().unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.recv().unwrap();

    // The call is queued behind the held connection; the caller gives up after 100 ms.
    let gave_up = tokio::time::timeout(
        Duration::from_millis(100),
        log.start(new_action(ActionKind::Deploy, "m")),
    )
    .await;
    assert!(gave_up.is_err(), "the call should still have been waiting");
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        log.since(ts(0)).await.unwrap().len(),
        0,
        "a call dropped before it began wrote its row later"
    );
}

// ----- F3: whose process is it? -----

/// A running row of ours whose recorded start time says its writer is gone: dead, if this
/// process is the one to judge. (Shared by the tests below, which each change one more thing.)
async fn dead_writer_row(d: &Path) -> ActionLog {
    let log = open_in(d).await;
    log.start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    raw(log.path())
        .execute("UPDATE actions SET pid_start = 'gone/1'", [])
        .unwrap();
    log
}

#[tokio::test]
async fn a_writer_in_another_pid_namespace_is_never_called_dead() {
    let d = dir();
    let log = dead_writer_row(d.path()).await;
    // The control: with our own namespace the row reads as interrupted.
    assert_eq!(
        log.recent("m", 1).await.unwrap()[0].result,
        Some(ActionResult::Interrupted)
    );
    // Its writer was in another pid namespace (a container): its pid means something else
    // here, so the row is left as running.
    raw(log.path())
        .execute("UPDATE actions SET pid_ns = 'pid:[1]'", [])
        .unwrap();
    assert_eq!(log.recent("m", 1).await.unwrap()[0].result, None);
    // Nothing known about it (a row from before the column): left alone too.
    raw(log.path())
        .execute("UPDATE actions SET pid_ns = NULL", [])
        .unwrap();
    assert_eq!(log.recent("m", 1).await.unwrap()[0].result, None);
}

#[tokio::test]
async fn a_writer_on_another_machine_with_our_host_name_is_never_called_dead() {
    let d = dir();
    let log = dead_writer_row(d.path()).await;
    assert_eq!(
        log.recent("m", 1).await.unwrap()[0].result,
        Some(ActionResult::Interrupted)
    );
    raw(log.path())
        .execute("UPDATE actions SET machine_id = 'some-other-machine'", [])
        .unwrap();
    assert_eq!(log.recent("m", 1).await.unwrap()[0].result, None);
}

/// The two halves of a process's stamp, each alone. (Changed together, as the recycled-pid
/// test does, a mistake in one half hides behind the other.)
#[tokio::test]
async fn a_reboot_alone_and_a_new_start_time_alone_each_make_the_writer_dead() {
    let d = dir();
    let log = open_in(d.path()).await;
    log.start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    let ours = process_stamp("self").unwrap().1;
    let (boot, ticks) = ours.split_once('/').unwrap();
    let state = |stamp: &str| {
        raw(log.path())
            .execute("UPDATE actions SET pid_start = ?1", [stamp])
            .unwrap();
    };
    // Our own stamp: alive.
    state(&ours);
    assert_eq!(log.recent("m", 1).await.unwrap()[0].result, None);
    // Another boot, the same start time: the machine rebooted since.
    state(&format!("another-boot/{ticks}"));
    assert_eq!(
        log.recent("m", 1).await.unwrap()[0].result,
        Some(ActionResult::Interrupted)
    );
    // The same boot, another start time: another process with the pid.
    let later: u64 = ticks.parse::<u64>().unwrap() + 1;
    state(&format!("{boot}/{later}"));
    assert_eq!(
        log.recent("m", 1).await.unwrap()[0].result,
        Some(ActionResult::Interrupted)
    );
}

/// The review's scenario for real: a live writer in a pid namespace of its own (`unshare -p`).
/// Skipped where unprivileged user namespaces are not available.
#[tokio::test]
async fn a_live_writer_in_another_pid_namespace_stays_running() {
    let probe = Command::new("unshare")
        .args(["-r", "-p", "-f", "--mount-proc", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !probe.is_ok_and(|s| s.success()) {
        eprintln!("skipped: no unprivileged user and pid namespaces here");
        return;
    }
    let d = dir();
    let work = d.path();
    let mut live = Command::new("unshare");
    live.args(["-r", "-p", "-f", "--mount-proc"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "deploylog::tests::child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(MODE, "live")
        .env(WORK, work)
        .env(WHO, "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let mut live = live.spawn().unwrap();
    wait_for(&work.join("started"));
    let log = open_in(work).await;

    let row = log.recent("m", 1).await.unwrap().remove(0);
    assert_eq!(
        row.result, None,
        "a live writer in another pid namespace was called dead"
    );
    std::fs::write(work.join("stop"), "").unwrap();
    assert!(live.wait().unwrap().success());
    // After it has gone nobody here can tell either way: still left alone.
    assert_eq!(log.recent("m", 1).await.unwrap()[0].result, None);
}

// ----- F4: local file systems only -----

#[test]
fn network_and_userspace_file_systems_are_named() {
    // `linux/magic.h`.
    for (magic, name) in [
        (0x6969, "NFS"),
        (0x517B, "SMB"),
        (0xFF53_4D42, "CIFS/SMB"),
        (0xFE53_4D42, "CIFS/SMB"),
        (0x6573_5546, "FUSE"),
        (0x0102_1997, "9p"),
    ] {
        assert_eq!(network_fs_name(magic), Some(name), "{magic:#x}");
        // A 32-bit `f_type` read as signed is the same number.
        assert_eq!(
            network_fs_name(i128::from(i32::from_ne_bytes(
                u32::try_from(magic).unwrap().to_ne_bytes()
            ))),
            Some(name),
            "{magic:#x} as i32"
        );
    }
    // ext4, tmpfs (one off from 9p), btrfs, xfs, overlayfs, zfs, 0.
    for local in [
        0xEF53,
        0x0102_1994,
        0x9123_683E,
        0x5846_5342,
        0x794C_7630,
        0x2FC1_2FC1,
        0,
    ] {
        assert_eq!(network_fs_name(local), None, "{local:#x}");
    }
}

#[tokio::test]
async fn a_log_on_a_network_file_system_is_refused_before_anything_is_created() {
    let d = dir();
    let path = db_in(d.path());
    tests_hook::pretend(&path, tests_hook::Pretend::Network);
    let e = ActionLog::open(params(path.clone(), HOST))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Unusable(m) if m.contains("NFS") && m.contains("local file system")),
        "{e}"
    );
    assert!(
        !path.exists(),
        "the file was created on the refused file system"
    );
    // Nor the directory: an NFS home is not left with an empty `state/` behind.
    assert!(
        !path.parent().unwrap().exists(),
        "the directory was created on the refused file system"
    );
    // A real, local directory is not refused: every other test opens one.
}

#[test]
fn a_journal_mode_other_than_wal_is_refused() {
    let p = Path::new("/x/log.sqlite");
    assert!(require_wal("wal", p).is_ok());
    assert!(require_wal("WAL", p).is_ok());
    for mode in ["delete", "memory", "truncate", "persist", "off", ""] {
        let e = require_wal(mode, p).unwrap_err();
        assert!(
            matches!(&e, Error::Unusable(m) if m.contains("write-ahead")),
            "{mode}: {e}"
        );
    }
}

// ----- F5: the connection is closed off the runtime -----

#[tokio::test(flavor = "current_thread")]
async fn dropping_the_log_closes_the_connection_on_the_blocking_pool() {
    let d = dir();
    let log = open_in(d.path()).await;
    let path = log.path().to_str().unwrap().to_owned();
    log.start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    let here = std::thread::current().id();
    drop(log);
    let end = Instant::now() + Duration::from_secs(10);
    let closed_on = loop {
        let found = tests_hook::CLOSED_ON
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, t)| *t);
        if let Some(t) = found {
            break t;
        }
        assert!(Instant::now() < end, "the connection was never closed");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_ne!(
        closed_on, here,
        "the connection was closed on the runtime's thread"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn close_waits_for_the_close_and_does_it_on_the_blocking_pool() {
    let d = dir();
    let log = open_in(d.path()).await;
    let path = log.path().to_str().unwrap().to_owned();
    let other_handle = log.clone();
    let here = std::thread::current().id();
    // Not the last handle: nothing is closed yet.
    log.close().await;
    assert!(
        !tests_hook::CLOSED_ON
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _)| *p == path),
        "closed while another handle was alive"
    );
    other_handle.close().await;
    let closed = tests_hook::CLOSED_ON.lock().unwrap().clone();
    let (_, thread) = closed
        .iter()
        .find(|(p, _)| *p == path)
        .expect("closed by close()");
    assert_ne!(*thread, here);
    // The WAL is gone: it was checkpointed and deleted by that close.
    assert!(!PathBuf::from(format!("{path}-wal")).exists());
}

// ----- F8: the README's query -----

/// The digest query, from `tests/fixtures/digest-week.sql`: the file is what the test runs, and
/// the README's first `sql` code block must be the same text. (The README is not in the Nix
/// build's source tree, which holds only a crate's sources and `tests/fixtures`: there the
/// comparison is skipped, and `just ci` and `cargo test` make it.)
fn readme_query() -> String {
    let query = include_str!("../../tests/fixtures/digest-week.sql");
    let readme_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    let Ok(readme) = std::fs::read_to_string(&readme_path) else {
        eprintln!("skipped: the README is not in this source tree");
        return query.to_owned();
    };
    let section = readme.split("## The action log").nth(1).unwrap();
    let start = section.find("```sql\n").unwrap() + "```sql\n".len();
    let end = section[start..].find("```").unwrap();
    assert_eq!(
        section[start..start + end].trim(),
        query.trim(),
        "the README's query is not tests/fixtures/digest-week.sql"
    );
    query.to_owned()
}

#[tokio::test]
async fn the_readmes_digest_query_runs_and_counts_what_it_says() {
    let d = dir();
    let log = open_in(d.path()).await;
    let c = raw(log.path());
    let row = |machine: &str, kind: &str, ago: &str, result: Option<&str>| {
        let ended =
            result.map(|_| format!("strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '{ago}', '+1 minute')"));
        c.execute(
            &format!(
                "INSERT INTO actions (kind, machine, started_at, ended_at, result, who, host, \
                 auspex_version, pid) VALUES ('{kind}', '{machine}', \
                 strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '{ago}'), {}, {}, 'w', 'h', 'v', 1)",
                ended.unwrap_or_else(|| "NULL".to_owned()),
                result.map_or_else(|| "NULL".to_owned(), |r| format!("'{r}'")),
            ),
            [],
        )
        .unwrap();
    };
    row("a", "deploy", "-1 hours", Some("succeeded"));
    row("a", "deploy", "-2 hours", Some("failed"));
    row("a", "build", "-3 hours", Some("timed_out"));
    row("a", "deploy", "-30 minutes", None); // running, started recently
    row("a", "deploy", "-48 hours", None); // unfinished for two days: interrupted
    row("a", "reboot", "-4 hours", Some("interrupted"));
    row("b", "deploy", "-10 minutes", None); // only a running row
    row("c", "deploy", "-10 days", Some("succeeded")); // outside the week

    let sql = readme_query();
    let mut stmt = c.prepare(&sql).unwrap();
    let names: Vec<String> = stmt.column_names().map(str::to_owned).collect();
    assert_eq!(
        names,
        [
            "machine",
            "actions",
            "deploys_ok",
            "failures",
            "interrupted"
        ]
    );
    // Integers, never NULL: `sum` over nothing is NULL, which is what the old query returned.
    let rows: Vec<(String, i64, i64, i64, i64)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        rows,
        [("a".to_owned(), 6, 1, 2, 2), ("b".to_owned(), 1, 0, 0, 0),]
    );
}

// ----- F9 and the review's surviving mutants -----

#[test]
fn what_makes_a_database_file_unusable() {
    let ok = |is_file, owner, mode, links| check_file_facts("f", is_file, owner, 1000, mode, links);
    assert!(ok(true, 1000, 0o600, 1).is_ok());
    assert!(
        ok(true, 1000, 0o100_600, 1).is_ok(),
        "file type bits are not permission bits"
    );
    assert!(ok(true, 1000, 0o400, 1).is_ok());
    // Not a regular file (a FIFO would block SQLite's read; a directory is nonsense).
    let e = ok(false, 1000, 0o600, 1).unwrap_err();
    assert!(e.to_string().contains("not a regular file"), "{e}");
    // Somebody else's.
    let e = ok(true, 0, 0o600, 1).unwrap_err();
    assert!(e.to_string().contains("owned by uid 0"), "{e}");
    // Every group and other bit counts, each alone.
    for bit in [0o040, 0o020, 0o010, 0o004, 0o002, 0o001] {
        let e = ok(true, 1000, 0o600 | bit, 1).unwrap_err();
        assert!(e.to_string().contains("expected 0600"), "{bit:o}: {e}");
    }
    // A second name for the file.
    let e = ok(true, 1000, 0o600, 2).unwrap_err();
    assert!(e.to_string().contains("2 hard links"), "{e}");
}

#[tokio::test]
async fn a_hard_linked_database_is_refused() {
    let d = dir();
    let log = open_in(d.path()).await;
    let path = log.path().to_owned();
    log.close().await;
    std::fs::hard_link(&path, path.with_file_name("other-name.sqlite")).unwrap();
    let e = ActionLog::open(params(path, HOST)).await.unwrap_err();
    assert!(
        matches!(&e, Error::Unusable(m) if m.contains("hard links")),
        "{e}"
    );
}

#[tokio::test]
async fn a_database_that_is_a_fifo_is_refused_and_does_not_hang() {
    let d = dir();
    let path = db_in(d.path());
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    std::fs::set_permissions(path.parent().unwrap(), PermissionsExt::from_mode(0o700)).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &path,
        rustix::fs::FileType::Fifo,
        Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    let e = tokio::time::timeout(Duration::from_secs(10), ActionLog::open(params(path, HOST)))
        .await
        .expect("opening a FIFO hung")
        .unwrap_err();
    assert!(
        matches!(&e, Error::Unusable(m) if m.contains("not a regular file")),
        "{e}"
    );
}

#[tokio::test]
async fn any_group_or_other_permission_on_the_file_is_refused() {
    let d = dir();
    let log = open_in(d.path()).await;
    let path = log.path().to_owned();
    log.close().await;
    for bad in [0o640, 0o604, 0o660, 0o606, 0o620, 0o602] {
        std::fs::set_permissions(&path, PermissionsExt::from_mode(bad)).unwrap();
        let e = ActionLog::open(params(path.clone(), HOST))
            .await
            .unwrap_err();
        assert!(
            matches!(&e, Error::Unusable(m) if m.contains("expected 0600")),
            "{bad:o}: {e}"
        );
    }
    std::fs::set_permissions(&path, PermissionsExt::from_mode(0o600)).unwrap();
    open_at(&path, HOST).await;
}

#[tokio::test]
async fn finish_needs_the_host_and_the_start_time_of_the_row_as_well_as_its_pid() {
    let d = dir();
    let log = open_in(d.path()).await;
    for (what, sql) in [
        (
            "another host",
            "UPDATE actions SET host = 'elsewhere' WHERE id = ?1",
        ),
        (
            "another start time",
            "UPDATE actions SET pid_start = 'x/1' WHERE id = ?1",
        ),
        (
            "no start time",
            "UPDATE actions SET pid_start = NULL WHERE id = ?1",
        ),
    ] {
        let id = log
            .start(new_action(ActionKind::Deploy, "m"))
            .await
            .unwrap();
        raw(log.path()).execute(sql, [id.get()]).unwrap();
        let e = log
            .finish(id, outcome(ActionResult::Failed))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::NotRunning(_)), "{what}: {e}");
        let ended: Option<String> = raw(log.path())
            .query_row(
                "SELECT ended_at FROM actions WHERE id = ?1",
                [id.get()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ended, None, "{what}: the row was finished");
    }
}

#[tokio::test]
async fn the_row_limit_and_the_ordering_do_not_depend_on_ids() {
    let d = dir();
    let log = open_in(d.path()).await;
    // Times that do not follow the ids (a clock that stepped back): ordering is by time,
    // the id only breaks a tie.
    let i = &log.inner;
    let first = i
        .start(&new_action(ActionKind::Deploy, "m"), ts(3_000), &call())
        .unwrap();
    let second = i
        .start(&new_action(ActionKind::Deploy, "m"), ts(1_000), &call())
        .unwrap();
    let third = i
        .start(&new_action(ActionKind::Deploy, "m"), ts(2_000), &call())
        .unwrap();
    let ids = |rows: Vec<Action>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(ids(log.since(ts(0)).await.unwrap()), [second, third, first]);
    assert_eq!(
        ids(log.recent("m", 10).await.unwrap()),
        [first, third, second]
    );

    // More rows than the cap: a caller asking for more than `RECENT_CAP` gets `RECENT_CAP`.
    raw(log.path())
        .execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1005) \
             INSERT INTO actions (kind, machine, started_at, who, host, auspex_version, pid) \
             SELECT 'build', 'many', strftime('%Y-%m-%dT%H:%M:%fZ', 1700000000 + i, 'unixepoch'), \
             'w', 'h', 'v', 1 FROM n",
        )
        .unwrap();
    assert_eq!(log.recent("many", 5000).await.unwrap().len(), RECENT_CAP);
    assert_eq!(
        log.recent("many", usize::MAX).await.unwrap().len(),
        RECENT_CAP
    );
    assert_eq!(log.recent("many", 7).await.unwrap().len(), 7);
}

#[tokio::test]
async fn who_host_and_version_are_sanitised_when_the_log_is_opened() {
    let d = dir();
    let path = db_in(d.path());
    let log = ActionLog::open(OpenParams::new(
        path.clone(),
        Identity {
            who: "\u{1b}[31mal\0ice\u{202e}".to_owned(),
            host: "ho\u{200b}st\n".to_owned(),
            auspex_version: format!("1.2.3\u{1b}[0m{}", "v".repeat(400)),
        },
        elsewhere(),
    ))
    .await
    .unwrap();
    log.start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    let row = log.recent("m", 1).await.unwrap().remove(0);
    assert_eq!((row.who.as_str(), row.host.as_str()), ("alice", "host"));
    assert!(row.auspex_version.starts_with("1.2.3vvv") && row.auspex_version.len() <= NAME_CAP);
    assert!(row.auspex_version.ends_with('…'));
    // Nothing is left of an identity that is all escapes.
    let e = ActionLog::open(OpenParams::new(
        db_in(dir().path()),
        Identity {
            who: "\u{1b}[0m".to_owned(),
            host: "h".to_owned(),
            auspex_version: "1".to_owned(),
        },
        elsewhere(),
    ))
    .await
    .unwrap_err();
    assert!(matches!(e, Error::Invalid(_)), "{e}");
}

// ----- F9: a read-only file system -----

#[tokio::test]
async fn a_read_only_file_system_still_shows_the_history() {
    let d = dir();
    let path = db_in(d.path());
    let log = open_at(&path, HOST).await;
    let id = log
        .start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    log.finish(id, outcome(ActionResult::Succeeded))
        .await
        .unwrap();
    log.close().await;

    tests_hook::pretend(&path, tests_hook::Pretend::ReadOnly);
    let log = open_at(&path, HOST).await;
    assert!(log.is_read_only());
    let rows = log.recent("m", 5).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].result, Some(ActionResult::Succeeded));
    assert_eq!(log.since(ts(0)).await.unwrap().len(), 1);
    // Writes fail, say why, and change nothing.
    let e = log
        .start(new_action(ActionKind::Build, "m"))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::ReadOnly), "{e}");
    let e = log
        .finish(id, outcome(ActionResult::Failed))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::ReadOnly), "{e}");
    assert_eq!(log.since(ts(0)).await.unwrap().len(), 1);
    // A newer schema is refused read-only too, and nothing there is made.
    let empty = db_in(dir().path());
    tests_hook::pretend(&empty, tests_hook::Pretend::ReadOnly);
    assert!(ActionLog::open(params(empty.clone(), HOST)).await.is_err());
}

/// SQLite cannot read a WAL database in a directory it cannot write (no `-shm`). The open then
/// fails and says why: no `immutable` fallback, which would show stale and torn history
/// while another process writes the file (the re-review's reproduction).
#[tokio::test]
async fn a_database_in_a_directory_that_cannot_be_written_shows_no_history_instead_of_a_torn_one() {
    let d = dir();
    let log = open_in(d.path()).await;
    let id = log
        .start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    log.finish(id, outcome(ActionResult::Succeeded))
        .await
        .unwrap();
    let path = log.path().to_owned();
    log.close().await;
    let state = path.parent().unwrap().to_owned();
    std::fs::set_permissions(&state, PermissionsExt::from_mode(0o500)).unwrap();

    let opened = open_read_only(&path, Duration::from_secs(1));
    std::fs::set_permissions(&state, PermissionsExt::from_mode(0o700)).unwrap();
    if rustix::process::geteuid().is_root() {
        // Root writes into a 0500 directory: nothing here is read-only.
        eprintln!("skipped: running as root");
        return;
    }
    let e = opened.unwrap_err();
    assert!(
        matches!(&e, Error::Unusable(m) if m.contains("-shm") && m.contains("no history")),
        "{e}"
    );
}

/// A read-only open of a newer schema is refused too (the review's survivor: the version check
/// of the read-only path was never exercised, because the file there did not exist).
#[tokio::test]
async fn a_newer_schema_is_refused_read_only_too() {
    let d = dir();
    let path = db_in(d.path());
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    std::fs::set_permissions(path.parent().unwrap(), PermissionsExt::from_mode(0o700)).unwrap();
    raw(&path)
        .execute_batch("CREATE TABLE actions (future TEXT); PRAGMA user_version = 2;")
        .unwrap();
    std::fs::set_permissions(&path, PermissionsExt::from_mode(0o600)).unwrap();
    tests_hook::pretend(&path, tests_hook::Pretend::ReadOnly);
    let e = ActionLog::open(params(path, HOST)).await.unwrap_err();
    assert!(matches!(e, Error::NewerSchema { found: 2, .. }), "{e}");
}

/// A file that does not exist on a read-only file system cannot be created: say that.
#[tokio::test]
async fn a_missing_log_on_a_read_only_file_system_says_why() {
    let d = dir();
    let path = db_in(d.path());
    tests_hook::pretend(&path, tests_hook::Pretend::ReadOnly);
    let e = ActionLog::open(params(path, HOST)).await.unwrap_err();
    assert!(
        matches!(&e, Error::Unusable(m) if m.contains("read-only") && m.contains("cannot be created")),
        "{e}"
    );
}

/// A poisoned connection lock (a panic while it was held) does not turn into refusing every
/// later call.
#[tokio::test]
async fn a_poisoned_connection_lock_is_recovered_from() {
    let d = dir();
    let log = open_in(d.path()).await;
    let inner = Arc::clone(&log.inner);
    let panicked = std::thread::spawn(move || {
        let _guard = inner.conn.lock().unwrap();
        panic!("a panic while the connection is locked");
    })
    .join();
    assert!(panicked.is_err());
    assert!(log.inner.conn.is_poisoned());
    let id = log
        .start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap();
    log.finish(id, outcome(ActionResult::Succeeded))
        .await
        .unwrap();
    assert_eq!(log.recent("m", 5).await.unwrap().len(), 1);
    // And closing it, which takes the connection out of a poisoned lock.
    log.close().await;
}

/// A deadline of nothing is a deadline that has passed: the call fails even though the
/// database is free, and writes nothing.
#[tokio::test]
async fn a_call_with_no_time_left_fails_without_writing() {
    let d = dir();
    let log = ActionLog::open(params_with(
        db_in(d.path()),
        Duration::from_secs(5),
        Duration::ZERO,
    ))
    .await
    .unwrap();
    let e = log
        .start(new_action(ActionKind::Deploy, "m"))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Timeout), "{e}");
    let rows: i64 = raw(log.path())
        .query_row("SELECT count(*) FROM actions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn a_path_that_does_not_exist_yet_is_resolved_through_what_does() {
    let d = dir();
    let real = d.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = d.path().join("link");
    symlink(&real, &link).unwrap();
    let canonical = real.canonicalize().unwrap();
    // The existing prefix is resolved through the symlink; the rest is appended as it is.
    assert_eq!(
        resolve_through_existing(&link.join("a").join("b")).unwrap(),
        canonical.join("a").join("b")
    );
    // A relative path is made absolute.
    let relative = resolve_through_existing(Path::new("auspex-no-such-dir/x")).unwrap();
    assert!(relative.is_absolute() && relative.starts_with(std::env::current_dir().unwrap()));
    // Where a `..` in the part that does not exist points cannot be said.
    assert!(resolve_through_existing(&link.join("a").join("..").join("b")).is_err());
}

// ----- Fix round 2 -----

fn unshare_works(extra: &[&str]) -> bool {
    Command::new("unshare")
        .args(["-r", "-p", "-f"])
        .args(extra)
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// What a process says about its own live row, run as `unshare [options] <this binary>`.
fn own_row_result(unshare: Option<&[&str]>) -> String {
    let d = dir();
    let work = d.path();
    let exe = std::env::current_exe().unwrap();
    let mut cmd = match unshare {
        Some(options) => {
            let mut c = Command::new("unshare");
            c.args(options).arg(exe);
            c
        }
        None => Command::new(exe),
    };
    cmd.args([
        "--exact",
        "deploylog::tests::child",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(MODE, "own")
    .env(WORK, work)
    .env(WHO, "0")
    .stdin(Stdio::null())
    .stdout(Stdio::null());
    assert!(cmd.status().unwrap().success());
    std::fs::read_to_string(work.join("own-result")).unwrap()
}

/// The re-review's R3: in a pid namespace that did not remount `/proc`, `getpid()` is 1 but
/// `/proc/self` is the process's pid in the namespace `/proc` belongs to. The row used to be
/// written under `getpid()` and judged by looking `/proc/1` up, which is somebody else: a
/// process called its own live row interrupted.
#[test]
fn a_process_never_calls_its_own_live_row_interrupted() {
    assert_eq!(own_row_result(None), "None", "outside any namespace");
    if !unshare_works(&[]) {
        eprintln!("skipped: no unprivileged user and pid namespaces here");
        return;
    }
    // New pid namespace, /proc not remounted: `/proc` is the parent namespace's.
    assert_eq!(own_row_result(Some(&["-r", "-p", "-f"])), "None");
    // With its own /proc.
    if unshare_works(&["--mount-proc"]) {
        assert_eq!(
            own_row_result(Some(&["-r", "-p", "-f", "--mount-proc"])),
            "None"
        );
    }
}

#[test]
fn the_pid_is_the_one_proc_knows() {
    // Outside a namespace the two are the same; the pid recorded is what `/proc/self` says.
    let from_proc: i64 = std::fs::read_link("/proc/self")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(pid_in_proc(), from_proc);
    assert_eq!(pid_in_proc(), i64::from(std::process::id()));
}

// ----- R4: a file from before the nullable columns -----

/// The version-1 schema exactly as `a07fe0e` created it, before `machine_id` and `pid_ns`.
const SCHEMA_V1_A07FE0E: &str = include_str!("../../tests/fixtures/schema-v1-a07fe0e.sql");

/// An action log as `a07fe0e` made it, with one finished row, in `dir`'s `state/`.
fn make_old_log(dir: &Path) -> PathBuf {
    let path = db_in(dir);
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    std::fs::set_permissions(path.parent().unwrap(), PermissionsExt::from_mode(0o700)).unwrap();
    {
        let c = raw(&path);
        c.execute_batch(SCHEMA_V1_A07FE0E).unwrap();
        c.execute_batch(
            "INSERT INTO actions (kind, machine, started_at, ended_at, result, exit_code, who, \
             host, auspex_version, pid, pid_start) VALUES ('deploy', 'old', \
             '2026-01-01T00:00:00.000Z', '2026-01-01T00:01:00.000Z', 'succeeded', 0, 'w', 'h', \
             'v', 1, 'b/1'); PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    std::fs::set_permissions(&path, PermissionsExt::from_mode(0o600)).unwrap();
    path
}

fn column_names(path: &Path) -> Vec<String> {
    raw(path)
        .prepare("SELECT name FROM pragma_table_info('actions') ORDER BY cid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[tokio::test]
async fn a_version_one_file_without_the_new_columns_gets_them_on_open() {
    let d = dir();
    let path = make_old_log(d.path());
    assert!(!column_names(&path).contains(&"machine_id".to_owned()));

    let log = open_at(&path, HOST).await;
    // Version 1 still, and the columns are those of a fresh file, in the same order.
    let version: i64 = raw(&path)
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let fresh_dir = dir();
    let fresh = open_in(fresh_dir.path()).await;
    assert_eq!(column_names(&path), column_names(fresh.path()));
    // The old row is still there (and, its writer unknown, left unjudged); new calls work.
    let old = log.recent("old", 5).await.unwrap();
    assert_eq!(old.len(), 1);
    assert_eq!(old[0].result, Some(ActionResult::Succeeded));
    let id = log
        .start(new_action(ActionKind::Deploy, "new"))
        .await
        .unwrap();
    log.finish(id, outcome(ActionResult::Succeeded))
        .await
        .unwrap();
    assert_eq!(log.since(ts(0)).await.unwrap().len(), 2);
    // A second open finds nothing to add.
    log.close().await;
    let again = open_at(&path, HOST).await;
    assert_eq!(again.since(ts(0)).await.unwrap().len(), 2);
}

#[tokio::test]
async fn processes_that_open_an_old_file_together_add_each_column_once() {
    for round in 0..10 {
        let d = dir();
        make_old_log(d.path());
        let errors = run_hammers(d.path(), 6, 1);
        assert!(errors.is_empty(), "round {round}: {errors:?}");
        let log = open_in(d.path()).await;
        assert_eq!(log.since(ts(0)).await.unwrap().len(), 7, "round {round}");
        assert!(column_names(log.path()).contains(&"pid_ns".to_owned()));
    }
}

#[tokio::test]
async fn an_old_file_on_a_read_only_file_system_cannot_be_upgraded_and_says_so() {
    let d = dir();
    let path = make_old_log(d.path());
    tests_hook::pretend(&path, tests_hook::Pretend::ReadOnly);
    let e = ActionLog::open(params(path, HOST)).await.unwrap_err();
    assert!(
        matches!(&e, Error::Unusable(m) if m.contains("predates columns")),
        "{e}"
    );
}
