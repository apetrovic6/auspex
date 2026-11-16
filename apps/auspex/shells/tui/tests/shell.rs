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
    // run on for another 3.5 s. Left: the new list request and the two deadline timers.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(shell.tasks(), 3, "only the new refresh's work is running");
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
