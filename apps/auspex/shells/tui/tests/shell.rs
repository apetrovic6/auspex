//! The effect mapping's own rules, driven through the demo with time paused: the global
//! deadline, cancellation on refresh, and the two ways an evaluator can end that the shell
//! must not hide.

mod common;

use std::sync::Arc;
use std::time::Duration;

use auspex_core::{EVAL_MISSING, Event, FleetErrorKind, PROBE_TIMED_OUT, Phase};
use auspex_domain::status::Badge;
use auspex_tui::backend::Backend;
use auspex_tui::demo::{self, Demo};
use auspex_tui::log::Log;
use auspex_tui::shell::Shell;
use common::settled_demo;
use tokio::time::Instant;

async fn settle(shell: &Arc<Shell>) {
    while !shell.view().refresh.settled {
        shell.changed().await;
    }
}

fn row<'a>(
    view: &'a auspex_core::ViewModel,
    name: &str,
) -> &'a auspex_domain::report::MachineReport {
    view.report
        .machines
        .iter()
        .find(|m| m.name == name)
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn the_deadline_is_armed_when_a_refresh_starts_and_settles_the_fleet() {
    // luna's probe takes 4 s; the deadline is 2 s.
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(2),
    );
    let start = Instant::now();
    shell.send(Event::Start(demo::config()));
    settle(&shell).await;
    assert_eq!(
        start.elapsed(),
        Duration::from_secs(2),
        "settled by the deadline"
    );
    let view = shell.view();
    assert!(view.refresh.timed_out);
    assert!(!view.report.settled);
    assert_eq!(row(&view, "luna").status, Badge::Unreachable);
    assert_eq!(row(&view, "luna").reason.as_deref(), Some(PROBE_TIMED_OUT));
    // The others answered in time and are untouched.
    assert_eq!(row(&view, "terra").status, Badge::InSync);
    // The cancelled requests were dropped: no token is left.
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(shell.in_flight(), 0);

    // A refresh arms a deadline of its own, for its own generation.
    let refreshed = Instant::now();
    shell.send(Event::Refresh);
    settle(&shell).await;
    assert_eq!(refreshed.elapsed(), Duration::from_secs(2));
    assert!(shell.view().refresh.timed_out);
}

#[tokio::test(start_paused = true)]
async fn a_refresh_cancels_what_is_in_flight_and_the_late_answers_change_nothing() {
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    shell.send(Event::Start(demo::config()));
    // Mid-scan: the list is in, probes and the evaluation are out.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(shell.view().refresh.phase, Phase::Scanning);
    let before = shell.in_flight();
    assert!(before > 3, "{before} requests in flight");
    shell.send(Event::Refresh);
    // Every old token is cancelled and forgotten; only the new list request is out.
    assert_eq!(shell.in_flight(), 1);
    // And the cancelled calls really stopped: without the cancel, luna's probe alone would
    // run on for another 3.5 s. Left: the new list request and the new deadline timer (the
    // old one is cancelled, so holding `r` does not pile timers up).
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(shell.tasks(), 2, "only the new refresh's work is running");
    settle(&shell).await;
    let view = shell.view();
    assert!(view.report.settled, "the second scan completes cleanly");
    assert_eq!(view.report.machines.len(), 18);
    assert_eq!(shell.in_flight(), 0);
    // Same report as an undisturbed scan.
    let clean = settled_demo().await;
    assert_eq!(view.report, clean.view().report);
}

#[tokio::test(start_paused = true)]
async fn a_machine_the_evaluator_finishes_without_is_an_error_on_its_row() {
    let backend = Backend::Demo(Demo {
        omit_from_eval: vec!["terra".to_owned()],
        ..Demo::default()
    });
    let shell = Shell::new(backend, Log::memory(), Duration::from_secs(120));
    let start = Instant::now();
    shell.send(Event::Start(demo::config()));
    settle(&shell).await;
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "not left to the deadline"
    );
    let view = shell.view();
    assert_eq!(row(&view, "terra").status, Badge::EvalError);
    assert_eq!(row(&view, "terra").reason.as_deref(), Some(EVAL_MISSING));
    assert!(!view.refresh.timed_out);
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_ends_without_finished_still_finishes() {
    let backend = Backend::Demo(Demo {
        end_without_finished: true,
        omit_from_eval: vec!["terra".to_owned()],
    });
    let shell = Shell::new(backend, Log::memory(), Duration::from_secs(120));
    let start = Instant::now();
    shell.send(Event::Start(demo::config()));
    settle(&shell).await;
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "not left to the deadline"
    );
    let view = shell.view();
    assert!(!view.refresh.timed_out);
    assert_eq!(row(&view, "terra").status, Badge::EvalError);
    assert!(
        view.report
            .errors
            .iter()
            .any(|e| e.kind == FleetErrorKind::EvaluationFailed
                && e.reason.contains("without a final answer")),
        "{:?}",
        view.report.errors
    );
}

#[tokio::test(start_paused = true)]
async fn cache_hits_and_the_adapters_notes_reach_the_log() {
    let shell = settled_demo().await;
    let log = shell.log.lines();
    assert!(log.iter().any(|l| l == "eval cache hit: terra"), "{log:?}");
    assert!(
        !log.iter().any(|l| l == "eval cache hit: luna"),
        "luna was evaluated: {log:?}"
    );
    assert!(
        log.iter().any(|l| l.starts_with("eval results stored")),
        "{log:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn quitting_cancels_everything_in_flight() {
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    shell.send(Event::Start(demo::config()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(shell.in_flight() > 0);
    shell.send(Event::Quit);
    assert_eq!(shell.in_flight(), 0);
    assert!(shell.view().quit);
    shell.shutdown().await;
}

fn connection_status(shell: &Shell, ui: &mut auspex_tui::ui::Ui) -> String {
    use auspex_tui::keymap::Keymap;
    use auspex_tui::theme::Theme;
    use auspex_tui::ui::Context;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut terminal = Terminal::new(TestBackend::new(160, 24)).unwrap();
    let ctx = Context {
        theme: &Theme::Ansi,
        keymap: &Keymap::defaults(),
        flake: "fake",
        log: &[],
    };
    terminal.draw(|frame| shell.draw(frame, ui, &ctx)).unwrap();
    (0..160)
        .map(|x| terminal.backend().buffer()[(x, 23)].symbol())
        .collect()
}

#[test]
fn approval_count_tracks_real_connection_and_reaches_the_frame() {
    use std::fs;
    use std::process::Command;

    let Some(t) = common::tools(&["sleep"]) else {
        return;
    };
    let dir = common::tempdir();
    fs::create_dir(dir.path().join("run")).unwrap();
    let nix = t.script(
        dir.path(),
        "nix",
        r#"printf '%s\n' '{"terra":{"deploy":{"targetHost":"root@192.0.2.10"}}}'"#,
    );
    let eval = t.script(dir.path(), "eval", r#"printf '%s\n' '{"attr":"terra","attrPath":["terra"],"drvPath":"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-system.drv","outputs":{"out":"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-system"}}'"#);
    let ssh = t.script(dir.path(), "ssh", r#"for arg; do
case "$arg" in
  -O) exit 1;;
  true)
    while [ ! -f '@dir@/release' ]; do @sleep@ 0.01; done
    exit 0;;
esac
done
printf 'auspex_probe=1\nprobe=nixos\ncurrent_system=/nix/store/0123456789abcdfghijklmnpqrsvwxyz-system\nauspex_end=1\n'"#);
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "approval_count_tracks_real_connection_inner",
        "--nocapture",
    ])
    .env("AUSPEX_TEST_APPROVAL_DIR", dir.path())
    .env("AUSPEX_NIX", nix)
    .env("AUSPEX_NIX_EVAL_JOBS", eval)
    .env("AUSPEX_SSH", ssh)
    .env("HOME", dir.path())
    .env("XDG_CACHE_HOME", dir.path().join("cache"))
    .env("XDG_RUNTIME_DIR", dir.path().join("run"));
    let out = common::run_within(&mut cmd, Duration::from_secs(30));
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"));
}

#[tokio::test]
async fn approval_count_tracks_real_connection_inner() {
    use auspex_tui::app::core_config;
    use auspex_tui::backend::Real;
    use auspex_tui::config::{FlakeSource, Settings};
    use auspex_tui::ui::Ui;

    let Some(dir) = std::env::var_os("AUSPEX_TEST_APPROVAL_DIR").map(std::path::PathBuf::from)
    else {
        return;
    };
    let settings = Settings::default();
    let shell = Shell::new(
        Backend::Real(Box::new(Real::new(&settings, FlakeSource::Flag))),
        Log::memory(),
        Duration::from_secs(20),
    );
    assert_eq!(shell.approvals(), 0);
    let mut ui = Ui::default();
    assert!(!connection_status(&shell, &mut ui).contains("ssh: connecting"));
    shell.send(Event::Start(core_config(
        &settings,
        format!("path:{}", dir.display()),
    )));
    let waiting = tokio::time::timeout(Duration::from_secs(10), async {
        while shell.approvals() != 1 {
            shell.changed().await;
        }
    })
    .await
    .is_ok();
    let active_status = connection_status(&shell, &mut ui);
    std::fs::write(dir.join("release"), "").unwrap();
    let settled = tokio::time::timeout(Duration::from_secs(10), async {
        while !shell.view().refresh.settled {
            shell.changed().await;
        }
    })
    .await
    .is_ok();
    let final_count = shell.approvals();
    let final_status = connection_status(&shell, &mut ui);
    shell.shutdown().await;
    assert!(waiting, "never entered connection phase");
    assert!(
        active_status.contains("ssh: connecting 1 host"),
        "{active_status}"
    );
    assert!(!active_status.contains("1 hosts"), "{active_status}");
    assert!(settled, "scan did not settle");
    assert_eq!(final_count, 0);
    assert!(!final_status.contains("ssh: connecting"), "{final_status}");
}

#[tokio::test(start_paused = true)]
async fn the_global_deadline_does_not_cut_a_preview_short() {
    // sol's two closure queries are requested at 690 ms (the list takes 150 ms, the probe 420,
    // the cache lookup 120) and the first answers (with a
    // failure) 300 ms later, at 990 ms; the deadline is 800 ms, so it passes with both in flight.
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_millis(800),
    );
    let start = Instant::now();
    shell.send(Event::Start(demo::config()));
    settle(&shell).await;
    assert_eq!(
        start.elapsed(),
        Duration::from_millis(990),
        "settled when sol's preview failed, not at the 800 ms deadline"
    );
    let view = shell.view();
    // luna's probe (4 s) was cut by the deadline, which is what timed the scan out; the
    // preview did not.
    assert!(view.refresh.timed_out);
    assert_eq!(row(&view, "luna").status, Badge::Unreachable);
    let sol = row(&view, "sol");
    assert_eq!(
        sol.status,
        Badge::UpdatePending,
        "the preview changed nothing else"
    );
    let diff = sol.package_diff.as_ref().unwrap();
    assert_eq!(diff.state, auspex_domain::diff::DiffState::Failed);
    assert!(sol.settled);
    // The failed side cancelled the other, and nothing is left running.
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(shell.in_flight(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_refresh_during_a_preview_cancels_it_and_the_next_scan_makes_its_own() {
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    shell.send(Event::Start(demo::config()));
    // 700 ms: sol's closure queries are in flight.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(shell.view().refresh.diffs_pending > 0);
    shell.send(Event::Refresh);
    assert_eq!(shell.view().refresh.diffs_pending, 0, "the rows are gone");
    settle(&shell).await;
    let view = shell.view();
    assert_eq!(
        row(&view, "luna").package_diff.as_ref().unwrap().state,
        auspex_domain::diff::DiffState::Computed
    );
    assert_eq!(
        row(&view, "sol").package_diff.as_ref().unwrap().state,
        auspex_domain::diff::DiffState::Failed
    );
    assert_eq!(view.refresh.diffs_pending, 0);
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        shell.in_flight(),
        0,
        "no token of the cancelled queries is left"
    );
}

#[tokio::test(start_paused = true)]
async fn the_shell_arms_the_cores_backstop_at_settle_plus_diff() {
    // sol's preview would fail by itself at 990 ms; the deadline is 800 ms and the backstop
    // 50 ms after it, so the core fails the preview first, at 850 ms, with its own reason.
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_millis(800),
    );
    shell.set_preview_grace(Duration::from_millis(50));
    let start = Instant::now();
    shell.send(Event::Start(demo::config()));
    settle(&shell).await;
    assert_eq!(start.elapsed(), Duration::from_millis(850));
    let view = shell.view();
    let sol = row(&view, "sol");
    assert_eq!(
        sol.package_diff.as_ref().unwrap().reason.as_deref(),
        Some(auspex_core::PREVIEW_TIMED_OUT)
    );
    assert_eq!(sol.status, Badge::UpdatePending);
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(shell.in_flight(), 0, "the cancelled queries left no token");

    // Without the backstop the same run settles when the preview answers itself.
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_millis(800),
    );
    let start = Instant::now();
    shell.send(Event::Start(demo::config()));
    settle(&shell).await;
    assert_eq!(start.elapsed(), Duration::from_millis(990));
}

#[tokio::test(start_paused = true)]
async fn previews_have_a_permit_pool_of_their_own() {
    use auspex_tui::shell::MAX_PREVIEW_QUERIES;
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    shell.send(Event::Start(demo::config()));
    // 800 ms: sol's two queries run (and luna's probe, a per-machine call, is out).
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(shell.running_previews() > 0);
    assert!(shell.running_previews() <= MAX_PREVIEW_QUERIES);
    assert!(
        shell.running_per_machine() <= 2,
        "previews take nothing from the shared pool: {}",
        shell.running_per_machine()
    );
}
