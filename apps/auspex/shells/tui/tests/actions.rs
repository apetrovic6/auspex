//! `u` and `b` for real, with a fake `clan` and a fake `nix` standing in for the programs, over
//! the demo's scripted fleet: what is run, what the Log tab and the status bar get, the rows
//! the action log records (in a private temporary database), and what happens when the
//! operator quits, or a termination signal arrives, while a deploy is running.
//!
//! The clock is real here (processes and timers must not be fast-forwarded). The demo's
//! `alfrost` evaluates first, so it is the machine updated: it is selected from the start.
//!
//! Nothing here runs the real `clan` or `nix`, ssh, or touches a real fleet or state directory.

mod common;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use auspex_core::effects::{ActionKind, ActionRequest, RequestId};
use auspex_core::{ActionResult, ActionStatus, ConfirmKind, Event, ViewModel};
use auspex_domain::fleet::MachineClass;
use auspex_effects::CancellationToken;
use auspex_effects::deploylog::{
    ActionKind as LoggedKind, ActionLog, ActionResult as LoggedResult, Identity, OpenParams,
};
use auspex_tui::backend::{Backend, Real};
use auspex_tui::demo::{self, Demo};
use auspex_tui::log::Log;
use auspex_tui::output::Stream;
use auspex_tui::shell::Shell;
use common::{
    Tools, alive, cleanup, pid_exists, still_alive_after_wait, tempdir, tools, wait_for_pids,
};

/// What a test sets up: scripts and the log in `dir`, the fleet in `fleet`.
struct World {
    dir: tempfile::TempDir,
    fleet: tempfile::TempDir,
    shell: Arc<Shell>,
    db: PathBuf,
}

fn identity() -> Identity {
    Identity {
        who: "tester".to_owned(),
        host: "testhost".to_owned(),
        auspex_version: "0.0.0-test".to_owned(),
    }
}

/// A shell over the scripted fleet whose actions run `clan` and `nix` (fake programs), logging
/// to a database of its own.
fn world(t: &Tools, clan_body: &str, nix_body: &str) -> World {
    world_with(t, clan_body, nix_body, true)
}

/// [`world`], with no action log configured when `logged` is false (no usable state directory).
fn world_with(t: &Tools, clan_body: &str, nix_body: &str, logged: bool) -> World {
    let dir = tempdir();
    let fleet = tempdir();
    let clan = t.script(dir.path(), "clan", clan_body);
    let nix = t.script(dir.path(), "nix", nix_body);
    let real = Real::for_actions(nix.into_os_string(), Some(clan));
    let backend = Backend::Demo(Demo {
        real_actions: Some(Box::new(real)),
        ..Demo::default()
    });
    let shell = Shell::new(backend, Log::memory(), Duration::from_secs(120));
    let db = dir.path().join("state").join("log.sqlite");
    if logged {
        shell.set_action_log(OpenParams::new(
            db.clone(),
            identity(),
            fleet.path().to_path_buf(),
        ));
    }
    shell.send(Event::Start(auspex_core::Config {
        flake: fleet.path().to_str().unwrap().to_owned(),
        ..demo::config()
    }));
    World {
        dir,
        fleet,
        shell,
        db,
    }
}

/// Waits (up to 30 s of real time) until `ready` holds of the view.
async fn until(shell: &Arc<Shell>, what: &str, ready: impl Fn(&ViewModel) -> bool) {
    let wait = async {
        while !ready(&shell.view()) {
            tokio::select! {
                () = shell.changed() => {}
                () = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .is_err()
    {
        panic!("timed out waiting for {what}");
    }
}

/// alfrost, the first row, is evaluated and the fleet has settled: a `y` while the evaluation
/// is still streaming is refused, so an update is asked for only once it has finished.
async fn ready(shell: &Arc<Shell>) {
    until(shell, "alfrost to evaluate and the fleet to settle", |v| {
        v.refresh.settled
            && v.report
                .machines
                .first()
                .is_some_and(|m| m.name == "alfrost" && m.pending_system.is_some())
    })
    .await;
}

fn act_on(shell: &Arc<Shell>) {
    shell.send(Event::ToggleAct);
    shell.send(Event::ConfirmAnswer(true));
}

async fn start_update(shell: &Arc<Shell>) {
    shell.send(Event::UpdateSelected);
    // The checks before the update run first (the demo answers them after a moment), then
    // the question opens.
    until(shell, "the checks before the update", |v| {
        v.confirm
            .as_ref()
            .is_some_and(|c| c.kind != ConfirmKind::Checking)
    })
    .await;
    assert_eq!(
        shell.view().confirm.expect("a question").kind,
        ConfirmKind::Update
    );
    shell.send(Event::ConfirmAnswer(true));
}

async fn ended(shell: &Arc<Shell>) -> auspex_core::ActionEnd {
    until(shell, "the action to end", |v| {
        v.action
            .as_ref()
            .is_some_and(|a| matches!(a.status, ActionStatus::Ended(_)))
    })
    .await;
    match shell.view().action.unwrap().status {
        ActionStatus::Ended(end) => end,
        _ => unreachable!(),
    }
}

/// Everything in the Log tab's buffer, as `stream: text`.
fn output(shell: &Shell) -> Vec<String> {
    shell
        .output()
        .window(0, 100_000)
        .into_iter()
        .map(|l| {
            let tag = match l.stream {
                Stream::Stdout => "out",
                Stream::Stderr => "err",
                Stream::Note => "note",
            };
            format!("{tag}: {}", l.text)
        })
        .collect()
}

/// The rows of the action log, read from a second connection after the first has finished.
async fn rows(db: &Path, fleet: &Path, machine: &str) -> Vec<auspex_effects::deploylog::Action> {
    let log = ActionLog::open(OpenParams::new(
        db.to_path_buf(),
        identity(),
        fleet.to_path_buf(),
    ))
    .await
    .expect("the log opens");
    let rows = log.recent(machine, 10).await.expect("the log reads");
    log.close().await;
    rows
}

fn recorded(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("argv"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

const RECORD: &str = "for a in \"$@\"; do printf '%s\\n' \"$a\" >> '@dir@/argv'; done\n";

// ---------------------------------------------------------------------------------------
// u
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn an_update_runs_the_exact_argv_streams_and_is_logged_succeeded() {
    let Some(t) = tools(&[]) else { return };
    let w = world(
        &t,
        &format!(
            "{RECORD}echo 'building'\necho 'a warning' >&2\necho \"cwd=$PWD\"\necho 'activating'\necho 'token=hunter2-from-an-activation-script'\nexit 0"
        ),
        "exit 99",
    );
    ready(&w.shell).await;
    act_on(&w.shell);
    start_update(&w.shell).await;
    let end = ended(&w.shell).await;
    assert_eq!(end.result, ActionResult::Succeeded);
    assert_eq!(end.exit_code, Some(0));
    assert_eq!(
        end.summary,
        ["exit 0, 5 lines"],
        "a success's summary is fixed text, not a line of what the script printed"
    );

    let fleet = fs::canonicalize(w.fleet.path()).unwrap();
    assert_eq!(
        recorded(w.dir.path()),
        [
            "machines",
            "update",
            "--host-key-check",
            "strict",
            "--flake",
            fleet.to_str().unwrap(),
            "--",
            "alfrost"
        ],
        "exactly one machine, after `--`"
    );

    // The tab: the command, then the output in order, stderr marked as such.
    let out = output(&w.shell);
    assert_eq!(out[0], "note: ── update alfrost ──");
    assert_eq!(
        out[1], "note: building on the target itself (no deploy.buildHost set)",
        "where it builds comes before clan's own words"
    );
    assert!(out[2].starts_with("note: resolving"), "{out:?}");
    assert!(out[3].starts_with("note: clan: "), "{out:?}");
    assert_eq!(
        out[4],
        format!(
            "note: $ clan machines update --host-key-check strict --flake {} -- alfrost",
            fleet.display()
        )
    );
    let streamed: Vec<&str> = out[5..out.len() - 1].iter().map(String::as_str).collect();
    assert!(streamed.contains(&"out: building"), "{out:?}");
    assert!(streamed.contains(&"err: a warning"), "{out:?}");
    assert!(
        streamed.iter().position(|l| *l == "out: building")
            < streamed.iter().position(|l| l.starts_with("out: cwd=")),
        "stdout keeps its order: {out:?}"
    );
    assert_eq!(out.last().unwrap(), "note: ── succeeded, exit 0 ──");

    w.shell.shutdown().await;
    let rows = rows(&w.db, w.fleet.path(), "alfrost").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.kind, LoggedKind::Deploy);
    assert_eq!(row.result, Some(LoggedResult::Succeeded));
    assert_eq!(row.exit_code, Some(0));
    assert_eq!(row.who, "tester");
    assert!(row.ended_at.is_some());
    assert_eq!(
        row.detail.as_deref(),
        Some("exit 0, 5 lines"),
        "the action log holds a summary, never a line of the output"
    );
    assert!(
        !format!("{row:?}").contains("hunter2"),
        "nothing the script printed is in the row: {row:?}"
    );
}

#[tokio::test]
async fn a_failed_update_is_logged_failed_with_a_summary_and_never_the_output() {
    let Some(t) = tools(&[]) else { return };
    let mut body = String::new();
    for i in 0..50 {
        let _ = writeln!(body, "echo 'build line {i}'");
    }
    body.push_str("echo 'first error line' >&2\necho 'root@10.0.0.1: Permission denied (publickey).' >&2\nexit 1");
    let w = world(&t, &body, "exit 99");
    ready(&w.shell).await;
    act_on(&w.shell);
    start_update(&w.shell).await;
    let end = ended(&w.shell).await;
    assert_eq!(end.result, ActionResult::Failed);
    assert_eq!(end.exit_code, Some(1));
    // Plainly: it failed at ssh, and what to do.
    let why = end.explanation.as_deref().expect("an ssh explanation");
    assert!(why.contains("failed at ssh"), "{why}");
    assert!(why.contains("SSH_AUTH_SOCK"), "{why}");
    assert!(why.contains("IdentityFile"), "{why}");

    w.shell.shutdown().await;
    let rows = rows(&w.db, w.fleet.path(), "alfrost").await;
    let row = &rows[0];
    assert_eq!(row.result, Some(LoggedResult::Failed));
    assert_eq!(row.exit_code, Some(1));
    let detail = row.detail.as_deref().unwrap();
    assert!(
        detail.contains("Permission denied (publickey)") && detail.contains("first error line"),
        "the last error lines: {detail}"
    );
    assert!(
        !detail.contains("build line"),
        "a summary, never the output: {detail}"
    );
}

#[tokio::test]
async fn a_nonzero_exit_that_is_not_ssh_is_not_blamed_on_ssh() {
    let Some(t) = tools(&[]) else { return };
    let w = world(&t, "echo \"error: builder failed\" >&2\nexit 2", "exit 99");
    ready(&w.shell).await;
    act_on(&w.shell);
    start_update(&w.shell).await;
    let end = ended(&w.shell).await;
    assert_eq!(end.exit_code, Some(2));
    assert!(end.explanation.is_none());
    w.shell.shutdown().await;
}

#[tokio::test]
async fn nothing_runs_without_act_mode_or_before_the_answer() {
    let Some(t) = tools(&[]) else { return };
    let w = world(&t, &format!("{RECORD}exit 0"), "exit 99");
    ready(&w.shell).await;
    // Observe mode.
    w.shell.send(Event::UpdateSelected);
    assert!(w.shell.view().action.is_none());
    // Act mode, question open, not answered.
    act_on(&w.shell);
    w.shell.send(Event::UpdateSelected);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(w.shell.view().action.is_none());
    // Answered no.
    w.shell.send(Event::ConfirmAnswer(false));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(w.shell.view().action.is_none());
    assert!(
        recorded(w.dir.path()).is_empty(),
        "clan was never started: {:?}",
        recorded(w.dir.path())
    );
    w.shell.shutdown().await;
    assert!(!w.db.exists(), "and no row was written for what never ran");
}

// ---------------------------------------------------------------------------------------
// The adapter's own refusals, with the core out of the way
// ---------------------------------------------------------------------------------------

fn request(kind: ActionKind, machine: &str, system: Option<&str>, flake: &str) -> ActionRequest {
    ActionRequest {
        id: RequestId(1),
        kind,
        machine: machine.to_owned(),
        class: MachineClass::Nixos,
        system: system.map(str::to_owned),
        flake: flake.to_owned(),
        build_host: None,
        target: None,
    }
}

#[tokio::test]
async fn the_adapter_refuses_what_the_core_would_never_ask_for() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let clan = t.script(
        dir.path(),
        "clan",
        &format!("{RECORD}echo ran >> '@dir@/ran'"),
    );
    let nix = t.script(
        dir.path(),
        "nix",
        &format!("{RECORD}echo ran >> '@dir@/ran'"),
    );
    let real = Real::for_actions(nix.into_os_string(), Some(clan));
    let fleet = fleet.path().to_str().unwrap();
    let sink = |_: Stream, _: &str| {};
    let cancel = CancellationToken::new();

    for name in ["", "-h", "--flake", "a b", "x;y"] {
        let answer = real
            .run_action(
                &request(ActionKind::Update, name, Some("x86_64-linux"), fleet),
                &cancel,
                &sink,
            )
            .await
            .unwrap();
        let err = answer.expect_err(&format!("update of {name:?}"));
        assert!(err.contains("refused"), "{name:?}: {err}");
    }
    let answer = real
        .run_action(
            &request(ActionKind::Build, "luna", Some("aarch64-darwin"), fleet),
            &cancel,
            &sink,
        )
        .await
        .unwrap();
    assert!(
        answer.unwrap_err().contains("another system's derivations"),
        "a build of another system"
    );
    let answer = real
        .run_action(
            &request(ActionKind::Build, "luna", None, fleet),
            &cancel,
            &sink,
        )
        .await
        .unwrap();
    assert!(answer.is_err());
    assert!(
        !dir.path().join("ran").exists(),
        "neither program was ever started"
    );
}

// ---------------------------------------------------------------------------------------
// b
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_build_runs_nix_build_streams_and_is_logged_as_a_build() {
    let Some(t) = tools(&[]) else { return };
    let w = world(
        &t,
        "exit 99",
        &format!("{RECORD}echo '/nix/store/0123456789abcdfghijklmnpqrsvwxyz-built'\nexit 0"),
    );
    ready(&w.shell).await;
    // No act mode: a build changes nothing on any machine.
    w.shell.send(Event::BuildSelected);
    let end = ended(&w.shell).await;
    assert_eq!(end.result, ActionResult::Succeeded);
    assert_eq!(end.summary, ["exit 0, 1 line"]);
    let argv = recorded(w.dir.path());
    assert_eq!(
        &argv[..4],
        [
            "build",
            "--no-link",
            "--print-out-paths",
            "--no-write-lock-file"
        ]
    );
    assert!(
        argv[4].ends_with("#nixosConfigurations.alfrost.config.system.build.toplevel"),
        "{argv:?}"
    );
    w.shell.shutdown().await;
    let rows = rows(&w.db, w.fleet.path(), "alfrost").await;
    assert_eq!(rows[0].kind, LoggedKind::Build);
    assert_eq!(rows[0].result, Some(LoggedResult::Succeeded));
}

// ---------------------------------------------------------------------------------------
// The log is a record, never part of the action
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_log_that_cannot_be_used_is_a_warning_and_the_update_still_succeeds() {
    let Some(t) = tools(&[]) else { return };
    let w = world(&t, "echo done\nexit 0", "exit 99");
    // A log inside the fleet is refused by the log; the update is not.
    w.shell.set_action_log(OpenParams::new(
        w.fleet.path().join("log.sqlite"),
        identity(),
        w.fleet.path().to_path_buf(),
    ));
    ready(&w.shell).await;
    act_on(&w.shell);
    start_update(&w.shell).await;
    let end = ended(&w.shell).await;
    assert_eq!(
        end.result,
        ActionResult::Succeeded,
        "the deploy did succeed"
    );
    w.shell.shutdown().await;
    let warning = w.shell.warning().expect("a warning for the status bar");
    assert!(warning.contains("action log"), "{warning}");
    assert!(
        !w.fleet.path().join("log.sqlite").exists(),
        "and nothing was written into the fleet"
    );
}

// ---------------------------------------------------------------------------------------
// Quitting during an update
// ---------------------------------------------------------------------------------------

/// A deploy that outlives `clan` itself: a child in the group, both pids recorded.
const SLOW_CLAN: &str =
    "@sleep@ 60 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\necho 'deploying'\nwait";

async fn running_update(w: &World) -> Vec<i32> {
    ready(&w.shell).await;
    act_on(&w.shell);
    start_update(&w.shell).await;
    let pids = tokio::task::spawn_blocking({
        let p = w.dir.path().join("pids");
        move || wait_for_pids(&p, 2)
    })
    .await
    .unwrap();
    until(&w.shell, "the first line", |_| {
        output(&w.shell).iter().any(|l| l == "out: deploying")
    })
    .await;
    pids
}

#[tokio::test]
async fn quitting_during_an_update_asks_then_stops_it_before_exit_and_logs_cancelled() {
    let Some(t) = tools(&["sleep"]) else { return };
    let w = world(&t, SLOW_CLAN, "exit 99");
    let pids = running_update(&w).await;

    w.shell.send(Event::Quit);
    let view = w.shell.view();
    assert_eq!(
        view.confirm.unwrap().question,
        "an update of alfrost is running; quit and stop it? (an activation already under way on the machine may complete) y/N"
    );
    assert!(!view.quit);
    assert!(
        pids.iter().all(|p| alive(*p)),
        "still running: N has not been said"
    );

    w.shell.send(Event::ConfirmAnswer(true));
    until(&w.shell, "quit", |v| v.quit).await;
    // The leader itself is reaped, not a zombie, by the time `quit` is true: no polling here,
    // `still_alive_after_wait` below is for the reparented grandchild, which init reaps.
    assert!(
        !pid_exists(pids[1]),
        "the leader {} is still in the process table when quit became true",
        pids[1]
    );
    // `quit` is true only once the process group is killed and reaped: nothing is left.
    let left = still_alive_after_wait(&pids);
    assert!(left.is_empty(), "orphans at the moment of exit: {left:?}");
    assert!(matches!(
        w.shell.view().action.unwrap().status,
        ActionStatus::Ended(ref e) if e.result == ActionResult::Cancelled
    ));

    // What main does next, and then exits.
    w.shell.terminate().await;
    cleanup(&pids);
    let rows = rows(&w.db, w.fleet.path(), "alfrost").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].result, Some(LoggedResult::Cancelled), "{rows:?}");
    assert!(rows[0].ended_at.is_some());
    assert_eq!(rows[0].exit_code, None);
}

#[tokio::test]
async fn a_termination_signal_does_the_same_without_a_question() {
    let Some(t) = tools(&["sleep"]) else { return };
    let w = world(&t, SLOW_CLAN, "exit 99");
    let pids = running_update(&w).await;

    // What main does when `Termination::recv` fires: stop, wait for the reaping and the row.
    tokio::time::timeout(Duration::from_secs(20), w.shell.terminate())
        .await
        .expect("terminate returns once everything is reaped");
    assert!(
        !pid_exists(pids[1]),
        "the leader {} is still in the process table after terminate returned",
        pids[1]
    );
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(left.is_empty(), "orphans after SIGTERM's path: {left:?}");
    assert!(w.shell.view().quit);
    let rows = rows(&w.db, w.fleet.path(), "alfrost").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].result, Some(LoggedResult::Cancelled), "{rows:?}");
    assert!(
        output(&w.shell).last().unwrap().contains("cancelled"),
        "{:?}",
        output(&w.shell)
    );
}

#[tokio::test]
async fn a_second_quit_forces_and_the_shutdown_still_reaps() {
    let Some(t) = tools(&["sleep"]) else { return };
    let w = world(&t, SLOW_CLAN, "exit 99");
    let pids = running_update(&w).await;
    w.shell.send(Event::Quit);
    w.shell.send(Event::ConfirmAnswer(true));
    w.shell.send(Event::Quit); // the second: forces, whether or not it has been reaped yet
    assert!(w.shell.view().quit);
    w.shell.terminate().await;
    assert!(
        !pid_exists(pids[1]),
        "the leader {} is still in the process table after the forced quit's shutdown",
        pids[1]
    );
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(left.is_empty(), "{left:?}");
}

/// With no action log (no usable state directory) nothing but the action's own task is left to
/// wait for, so the exit's wait for the reaping must not depend on the log task.
#[tokio::test]
async fn a_termination_signal_waits_for_the_reaping_with_no_action_log() {
    let Some(t) = tools(&["sleep"]) else { return };
    let w = world_with(&t, SLOW_CLAN, "exit 99", false);
    let pids = running_update(&w).await;

    tokio::time::timeout(Duration::from_secs(20), w.shell.terminate())
        .await
        .expect("terminate returns once everything is reaped");
    // At the instant terminate returns, which is when main exits: no polling.
    assert!(
        !pid_exists(pids[1]),
        "the leader {} is still in the process table: terminate did not wait for the reaping",
        pids[1]
    );
    let left = still_alive_after_wait(&pids);
    cleanup(&pids);
    assert!(left.is_empty(), "orphans after the signal's path: {left:?}");
    assert!(w.shell.view().quit);
    assert!(!w.db.exists(), "no log was configured, none was written");
}

// ---------------------------------------------------------------------------------------
// The checks before an update, cancelled (PR #26 review 8, M14)
// ---------------------------------------------------------------------------------------

/// A shell whose checks before an update run for real, with a fake `git` and a fake `nix`
/// that each start a `sleep` and wait for it, recording their pids.
fn world_checking(t: &Tools) -> World {
    let dir = tempdir();
    let fleet = tempdir();
    let slow = "@sleep@ 60 &\necho $! >> '@dir@/pids'\necho $$ >> '@dir@/pids'\nwait";
    // git is asked twice (the config listing, then the status): only the status hangs.
    let git = t.script(
        dir.path(),
        "git",
        &format!("case \" $* \" in *\" config \"*) exit 0;; esac\n{slow}"),
    );
    let nix = t.script(dir.path(), "nix", slow);
    let clan = t.script(dir.path(), "clan", "exit 99");
    let real = Real::for_actions(nix.into_os_string(), Some(clan))
        .with_git_and_cache(git.into_os_string(), None);
    let backend = Backend::Demo(Demo {
        real_actions: Some(Box::new(real)),
        real_preflight: true,
        ..Demo::default()
    });
    let shell = Shell::new(backend, Log::memory(), Duration::from_secs(120));
    shell.send(Event::Start(auspex_core::Config {
        flake: fleet.path().to_str().unwrap().to_owned(),
        ..demo::config()
    }));
    World {
        db: dir.path().join("state").join("log.sqlite"),
        dir,
        fleet,
        shell,
    }
}

#[tokio::test]
async fn cancelling_the_checks_kills_and_reaps_the_shells_git_and_nix() {
    let Some(t) = tools(&["sleep"]) else { return };
    let w = world_checking(&t);
    ready(&w.shell).await;
    act_on(&w.shell);
    w.shell.send(Event::UpdateSelected);
    assert_eq!(
        w.shell.view().confirm.expect("checking").kind,
        ConfirmKind::Checking
    );
    // git's status and nix's size query are both running (a shell and its sleep each).
    let pids = tokio::task::spawn_blocking({
        let p = w.dir.path().join("pids");
        move || wait_for_pids(&p, 4)
    })
    .await
    .unwrap();
    assert!(pids.iter().all(|p| alive(*p)));
    // Any key cancels the checks: the processes are gone, long before `timeouts.diff`.
    w.shell.send(Event::ConfirmAnswer(false));
    // The wait blocks, so it runs off the runtime: the shell's own tasks do the killing.
    let left = tokio::task::spawn_blocking({
        let pids = pids.clone();
        move || still_alive_after_wait(&pids)
    })
    .await
    .unwrap();
    assert!(left.is_empty(), "left running after the cancel: {left:?}");
    assert!(w.shell.view().confirm.is_none());
    assert!(w.shell.view().action.is_none());
    cleanup(&pids);
    w.shell.shutdown().await;
    let _ = (&w.fleet, &w.db);
}
