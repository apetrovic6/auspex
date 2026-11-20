//! `f`: the key, what the screen shows, the state file that keeps a freeze across runs, the
//! rows in the action log, and `--json`.
//!
//! Three kinds of test. The keys and the snapshots drive the scripted demo with time paused
//! (a freeze there lives in memory, as it does with no state file). The persistence tests run
//! the shell over a real [`FreezeStore`] in a temporary directory, twice, as a restart is. The
//! `--json` tests run the built binary with fake `nix`, `nix-eval-jobs` and `ssh`, in a `HOME`
//! of its own, so the real state directory is never read or written.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use auspex_core::{Config, ConfirmKind, Event, FreezeConfig, Mode, SavedFreezes, ViewModel};
use auspex_effects::deploylog::{
    ActionKind as LoggedKind, ActionLog, ActionResult as LoggedResult, Identity, OpenParams,
};
use auspex_effects::freeze::{self, FreezeStore};
use auspex_tui::app::{load_freezes, setup_freezes};
use auspex_tui::backend::Backend;
use auspex_tui::demo::{self, Demo};
use auspex_tui::input::{Input, Screen};
use auspex_tui::keymap::{Action, Key, Keymap};
use auspex_tui::log::Log;
use auspex_tui::shell::Shell;
use auspex_tui::ui::Ui;
use common::{
    Tools, auspex, expect_file, render, run_within, settled_demo_config, tempdir, tools, type_keys,
    type_keys_seen,
};

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn demo_config(declared: &[&str], saved: SavedFreezes) -> Config {
    Config {
        freeze: FreezeConfig {
            declared: set(declared),
            saved,
        },
        ..demo::config()
    }
}

fn notice(shell: &Shell) -> String {
    shell.view().notice.expect("a notice")
}

fn row_frozen(view: &ViewModel, name: &str) -> bool {
    view.report
        .machines
        .iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("no row for {name}"))
        .frozen
}

/// Waits (time is paused or real: both work) until no save is out.
async fn saved(shell: &Arc<Shell>) {
    let wait = async {
        while shell.view().freeze.saving {
            tokio::select! {
                () = shell.changed() => {}
                () = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("the save to finish");
}

// ---------------------------------------------------------------------------------------
// The key
// ---------------------------------------------------------------------------------------

/// No events.
const NOTHING: [String; 0] = [];

const SCREEN: Screen = Screen {
    help_open: false,
    page: 20,
    matches: 3,
    listing_for: None,
    rows: 18,
    act: false,
    confirm: None,
};

fn feed(input: &mut Input, keymap: &Keymap, screen: Screen, keys: &str) -> Vec<String> {
    keys.chars()
        .flat_map(|c| input.key(Key::char(c), screen, keymap, ""))
        .map(|e| format!("{e:?}"))
        .collect()
}

#[test]
fn f_is_the_freeze_key_and_needs_no_act_mode() {
    let km = Keymap::defaults();
    assert_eq!(km.keys_for(Action::Freeze), ["f"]);
    let mut input = Input::default();
    // Observe mode: freezing only prevents changes.
    assert_eq!(feed(&mut input, &km, SCREEN, "f"), ["ToggleFreeze"]);
    let act = Screen {
        act: true,
        ..SCREEN
    };
    assert_eq!(feed(&mut input, &km, act, "f"), ["ToggleFreeze"]);
}

#[test]
fn f_can_be_rebound_and_unbound() {
    let mut warnings = Vec::new();
    let km = Keymap::with_overrides(
        &BTreeMap::from([("freeze".to_owned(), vec!["F".to_owned()])]),
        &mut warnings,
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut input = Input::default();
    assert_eq!(feed(&mut input, &km, SCREEN, "F"), ["ToggleFreeze"]);
    assert_eq!(feed(&mut input, &km, SCREEN, "f"), NOTHING);
    let km = Keymap::with_overrides(
        &BTreeMap::from([("freeze".to_owned(), Vec::new())]),
        &mut warnings,
    )
    .unwrap();
    assert!(feed(&mut input, &km, SCREEN, "f").is_empty(), "unbound");
}

#[test]
fn f_does_nothing_behind_an_overlay_or_a_question_it_does_not_answer() {
    let km = Keymap::defaults();
    // The help overlay covers the list: a key that froze the selection would act unseen.
    let mut input = Input::default();
    let help = Screen {
        help_open: true,
        ..SCREEN
    };
    assert_eq!(feed(&mut input, &km, help, "f"), NOTHING);
    // The diff and the Log overlays too.
    let mut input = Input {
        diff_open: true,
        ..Input::default()
    };
    assert_eq!(feed(&mut input, &km, SCREEN, "f"), NOTHING);
    let mut input = Input {
        log_open: true,
        ..Input::default()
    };
    assert_eq!(feed(&mut input, &km, SCREEN, "f"), NOTHING);
    // With a question open every key answers it, and `f` is a no.
    let mut input = Input::default();
    let asking = Screen {
        confirm: Some(ConfirmKind::Unfreeze),
        ..SCREEN
    };
    assert_eq!(feed(&mut input, &km, asking, "f"), ["ConfirmAnswer(false)"]);
}

#[test]
fn help_lists_the_freeze_key() {
    let lines = auspex_tui::ui::help_lines(&Keymap::defaults());
    assert!(
        lines.iter().any(|(k, d)| k == "f" && d.contains("freeze")),
        "{lines:?}"
    );
}

// ---------------------------------------------------------------------------------------
// What the operator sees (the demo, time paused)
// ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn f_freezes_in_observe_mode_and_the_row_shows_the_snowflake() {
    let shell = settled_demo_config(demo_config(&[], SavedFreezes::default())).await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/luna\n");
    assert_eq!(shell.view().mode, Mode::Observe);
    assert!(!row_frozen(&shell.view(), "luna"));
    type_keys_seen(&shell, &mut ui, &km, "f");
    saved(&shell).await;
    let view = shell.view();
    assert!(row_frozen(&view, "luna"));
    assert!(view.confirm.is_none(), "freezing asks nothing");
    assert_eq!(view.mode, Mode::Observe, "and needs no act mode");
    let frame = render(&view, &mut ui, &km, &[], 120, 30);
    let luna = frame.lines().find(|l| l.contains("luna  ")).unwrap();
    assert!(luna.contains('❄'), "{luna}");
    let others = frame
        .lines()
        .filter(|l| l.chars().take(53).any(|c| c == '❄'))
        .count();
    assert_eq!(others, 1, "only luna's row has it:\n{frame}");
    assert!(frame.contains("frozen"), "the detail says so:\n{frame}");
}

#[tokio::test(start_paused = true)]
async fn unfreezing_asks_by_name_and_a_y_unfreezes() {
    let shell = settled_demo_config(demo_config(&[], SavedFreezes::Known(set(&["luna"])))).await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/luna\n");
    type_keys_seen(&shell, &mut ui, &km, "f");
    let confirm = shell.view().confirm.expect("a question");
    assert_eq!(confirm.kind, ConfirmKind::Unfreeze);
    assert!(
        confirm.question.contains("unfreeze luna"),
        "{}",
        confirm.question
    );
    let frame = render(&shell.view(), &mut ui, &km, &[], 120, 30);
    expect_file("snapshots", "confirm_unfreeze_120.txt", &frame);
    assert!(
        row_frozen(&shell.view(), "luna"),
        "nothing changed before the y"
    );
    // The drawn-question rules of `u` hold for this question too: a `y` typed before the
    // question was on screen is discarded, so the machine stays frozen.
    type_keys_seen(&shell, &mut ui, &km, "y");
    saved(&shell).await;
    assert!(!row_frozen(&shell.view(), "luna"));
    assert!(shell.view().confirm.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_y_typed_behind_the_f_is_not_an_answer() {
    let shell = settled_demo_config(demo_config(&[], SavedFreezes::Known(set(&["luna"])))).await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/luna\n");
    // `fy` in one burst: the question has not been drawn when the `y` arrives.
    type_keys(&shell, &mut ui, &km, "fy");
    assert!(shell.view().confirm.is_some(), "the question is still open");
    assert!(row_frozen(&shell.view(), "luna"));
    assert!(
        !shell.view().freeze.saving,
        "nothing was asked of the shell"
    );
}

#[tokio::test(start_paused = true)]
async fn any_other_key_to_the_unfreeze_question_leaves_the_machine_frozen() {
    let shell = settled_demo_config(demo_config(&[], SavedFreezes::Known(set(&["luna"])))).await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/luna\n");
    type_keys_seen(&shell, &mut ui, &km, "fn");
    assert!(shell.view().confirm.is_none());
    assert!(row_frozen(&shell.view(), "luna"));
    assert!(!shell.view().freeze.saving);
}

#[tokio::test(start_paused = true)]
async fn u_on_a_frozen_machine_says_it_is_frozen() {
    let shell = settled_demo_config(demo_config(&[], SavedFreezes::Known(set(&["luna"])))).await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/luna\n");
    type_keys_seen(&shell, &mut ui, &km, "Ay");
    assert_eq!(shell.view().mode, Mode::Act);
    type_keys_seen(&shell, &mut ui, &km, "u");
    assert_eq!(notice(&shell), "luna is frozen; f to unfreeze");
    assert!(shell.view().confirm.is_none(), "no question, nothing runs");
    assert!(shell.view().action.is_none());
    let frame = render(&shell.view(), &mut ui, &km, &[], 120, 30);
    assert!(frame.contains("luna is frozen; f to unfreeze"), "{frame}");
}

#[tokio::test(start_paused = true)]
async fn a_config_freeze_cannot_be_undone_with_f_and_says_so() {
    let shell = settled_demo_config(demo_config(&["luna"], SavedFreezes::default())).await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/luna\n");
    type_keys_seen(&shell, &mut ui, &km, "f");
    let n = notice(&shell);
    assert!(
        n.contains("config.toml") && n.contains("cannot unfreeze"),
        "{n}"
    );
    assert!(shell.view().confirm.is_none());
    assert!(row_frozen(&shell.view(), "luna"));
    let frame = render(&shell.view(), &mut ui, &km, &[], 120, 30);
    assert!(
        frame.contains("yes, in config.toml (f cannot unfreeze it)"),
        "the detail panel says why:\n{frame}"
    );
}

#[tokio::test(start_paused = true)]
async fn snapshot_of_frozen_machines_at_40_columns() {
    // luna frozen with f, cypramundi in the config: both show ❄ in the list.
    let shell = settled_demo_config(demo_config(
        &["cypramundi"],
        SavedFreezes::Known(set(&["luna"])),
    ))
    .await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    let frame = render(&shell.view(), &mut ui, &km, &[], 40, 24);
    assert_eq!(
        frame.lines().filter(|l| l.contains('❄')).count(),
        2,
        "{frame}"
    );
    expect_file("snapshots", "freeze_list_40.txt", &frame);
    // The narrow layout's detail of the one the config froze.
    type_keys(&shell, &mut ui, &km, "/cypramundi\n\n");
    let frame = render(&shell.view(), &mut ui, &km, &[], 40, 24);
    assert!(frame.contains("frozen"), "{frame}");
    expect_file("snapshots", "freeze_declared_detail_40.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn snapshot_of_a_frozen_machine_at_120_columns() {
    let shell = settled_demo_config(demo_config(
        &["cypramundi"],
        SavedFreezes::Known(set(&["luna"])),
    ))
    .await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/luna\n");
    let frame = render(&shell.view(), &mut ui, &km, &[], 120, 30);
    assert!(frame.contains("yes (f to unfreeze)"), "{frame}");
    expect_file("snapshots", "freeze_120.txt", &frame);
}

#[tokio::test(start_paused = true)]
async fn when_it_is_unknown_which_machines_are_frozen_every_screen_says_so() {
    let shell = settled_demo_config(demo_config(
        &["cypramundi"],
        SavedFreezes::Unknown("the state file /x/state.toml is not valid (not TOML)".to_owned()),
    ))
    .await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    type_keys(&shell, &mut ui, &km, "/luna\n");
    let frame = render(&shell.view(), &mut ui, &km, &[], 120, 30);
    assert!(
        frame.contains("freezes unknown: u and f refused"),
        "the status bar says what it costs:\n{frame}"
    );
    assert!(
        !frame.contains("fleet error: freezes made with f"),
        "the report's error is for --json readers, not the status bar:\n{frame}"
    );
    assert!(frame.contains("unknown: the state file"), "{frame}");
    expect_file("snapshots", "freeze_unknown_120.txt", &frame);
    // `u` and `f` are refused, in plain words.
    type_keys_seen(&shell, &mut ui, &km, "Ay");
    type_keys_seen(&shell, &mut ui, &km, "u");
    assert!(notice(&shell).contains("unknown"), "{}", notice(&shell));
    type_keys_seen(&shell, &mut ui, &km, "f");
    assert!(notice(&shell).contains("unknown"), "{}", notice(&shell));
    assert!(!shell.view().freeze.saving, "nothing was written");
}

// ---------------------------------------------------------------------------------------
// The state file: two runs, two fleets, damage
// ---------------------------------------------------------------------------------------

fn identity() -> Identity {
    Identity {
        who: "tester".to_owned(),
        host: "testhost".to_owned(),
        auspex_version: "0.0.0-test".to_owned(),
    }
}

/// One run: the demo over a real state file, as `main` builds it.
struct Run {
    shell: Arc<Shell>,
}

async fn run(state: &Path, fleet: &str, declared: &[&str]) -> Run {
    run_logged(state, fleet, declared, None).await
}

async fn run_logged(state: &Path, fleet: &str, declared: &[&str], log: Option<OpenParams>) -> Run {
    let store = FreezeStore::new(state.to_path_buf(), fleet.to_owned());
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    if let Some(log) = log {
        shell.set_action_log(log);
    }
    // The wiring `main` does for a real run: read what was saved, say where to keep the next.
    let mut config = demo_config(declared, SavedFreezes::default());
    setup_freezes(&shell, &mut config, Some(store)).await;
    shell.send(Event::Start(config));
    // The rows are there once the list has been answered; the scan need not be over.
    let wait = async {
        while shell.view().report.machines.is_empty() {
            shell.changed().await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("the machine list");
    Run { shell }
}

impl Run {
    fn select(&self, name: &str) {
        self.shell.send(Event::Search(name.to_owned()));
    }

    /// `f` on `name`, answered `y` when it asks, and waited for.
    async fn toggle(&self, name: &str, yes: bool) {
        self.select(name);
        self.shell.send(Event::ToggleFreeze);
        if self.shell.view().confirm.is_some() {
            self.shell.send(Event::ConfirmAnswer(yes));
        }
        saved(&self.shell).await;
    }

    fn view(&self) -> ViewModel {
        self.shell.view()
    }

    async fn quit(self) {
        self.shell.shutdown().await;
    }
}

fn state_file(dir: &Path) -> PathBuf {
    dir.join("state").join(freeze::FILE_NAME)
}

fn damage(path: &Path, body: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test]
async fn a_freeze_survives_a_restart_and_so_does_an_unfreeze() {
    let dir = tempdir();
    let state = state_file(dir.path());

    let first = run(&state, "/fleet", &[]).await;
    assert!(!row_frozen(&first.view(), "luna"));
    first.toggle("luna", true).await;
    assert!(row_frozen(&first.view(), "luna"));
    first.quit().await;
    assert!(state.exists(), "the freeze was written");
    assert_eq!(
        fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // A second run on the same state file: frozen before anything is pressed.
    let second = run(&state, "/fleet", &[]).await;
    assert!(row_frozen(&second.view(), "luna"), "frozen after a restart");
    assert!(!row_frozen(&second.view(), "sol"));
    assert_eq!(second.view().freeze.saved, set(&["luna"]));
    // And `u` there is refused, in act mode, with the reason.
    second.select("luna");
    second.shell.send(Event::ToggleAct);
    second.shell.send(Event::ConfirmAnswer(true));
    assert_eq!(second.view().mode, Mode::Act);
    second.shell.send(Event::UpdateSelected);
    assert_eq!(notice(&second.shell), "luna is frozen; f to unfreeze");
    assert!(second.view().confirm.is_none() && second.view().action.is_none());
    // Unfreeze it: that survives too.
    second.toggle("luna", true).await;
    assert!(!row_frozen(&second.view(), "luna"));
    second.quit().await;

    let third = run(&state, "/fleet", &[]).await;
    assert!(
        !row_frozen(&third.view(), "luna"),
        "unfrozen after a restart"
    );
    third.quit().await;
}

#[tokio::test]
async fn a_no_to_the_unfreeze_question_changes_nothing_in_the_file() {
    let dir = tempdir();
    let state = state_file(dir.path());
    let first = run(&state, "/fleet", &[]).await;
    first.toggle("luna", true).await;
    first.quit().await;
    let before = fs::read(&state).unwrap();
    let second = run(&state, "/fleet", &[]).await;
    second.toggle("luna", false).await;
    assert!(row_frozen(&second.view(), "luna"));
    second.quit().await;
    assert_eq!(fs::read(&state).unwrap(), before);
}

#[tokio::test]
async fn two_fleets_do_not_share_freezes() {
    let dir = tempdir();
    let state = state_file(dir.path());
    let a = run(&state, "/fleets/a", &[]).await;
    a.toggle("luna", true).await;
    a.quit().await;

    // The same file, another fleet: luna is not frozen there, and freezing sol does not touch
    // the first fleet's entry.
    let b = run(&state, "/fleets/b", &[]).await;
    assert!(!row_frozen(&b.view(), "luna"));
    b.toggle("sol", true).await;
    b.quit().await;

    let a = run(&state, "/fleets/a", &[]).await;
    assert!(row_frozen(&a.view(), "luna") && !row_frozen(&a.view(), "sol"));
    a.quit().await;
    let b = run(&state, "/fleets/b", &[]).await;
    assert!(row_frozen(&b.view(), "sol") && !row_frozen(&b.view(), "luna"));
    b.quit().await;
}

#[tokio::test]
async fn a_corrupt_state_file_makes_freezes_unknown_and_nothing_can_be_updated() {
    let dir = tempdir();
    let state = state_file(dir.path());
    damage(&state, b"this is { not toml\n");
    let r = run(&state, "/fleet", &["mars"]).await;
    let view = r.view();
    let why = view.freeze.unknown.clone().expect("unknown, not empty");
    assert!(
        why.contains("not valid") && why.contains("state.toml"),
        "{why}"
    );
    assert!(row_frozen(&view, "mars"), "a declared freeze needs no file");
    r.shell.send(Event::ToggleAct);
    r.shell.send(Event::ConfirmAnswer(true));
    for m in ["alfrost", "luna"] {
        r.select(m);
        r.shell.send(Event::UpdateSelected);
        assert!(
            notice(&r.shell).contains("unknown"),
            "{m}: {}",
            notice(&r.shell)
        );
        assert!(
            r.view().confirm.is_none() && r.view().action.is_none(),
            "{m}"
        );
    }
    // `f` does not write over it.
    r.select("luna");
    r.shell.send(Event::ToggleFreeze);
    assert!(notice(&r.shell).contains("unknown"));
    r.quit().await;
    assert_eq!(fs::read(&state).unwrap(), b"this is { not toml\n");
}

#[tokio::test]
async fn a_newer_state_file_makes_freezes_unknown_and_is_never_written_over() {
    let dir = tempdir();
    let state = state_file(dir.path());
    let body = b"version = 2\n[fleets.\"/fleet\"]\nfrozen = [\"luna\"]\nnew_thing = 1\n";
    damage(&state, body);
    let r = run(&state, "/fleet", &[]).await;
    let why = r.view().freeze.unknown.expect("unknown");
    assert!(why.contains("newer auspex"), "{why}");
    r.shell.send(Event::ToggleAct);
    r.shell.send(Event::ConfirmAnswer(true));
    r.select("luna");
    r.shell.send(Event::UpdateSelected);
    assert!(notice(&r.shell).contains("unknown"));
    r.shell.send(Event::ToggleFreeze);
    assert!(notice(&r.shell).contains("unknown"));
    r.quit().await;
    assert_eq!(fs::read(&state).unwrap(), body);
}

#[tokio::test]
async fn no_usable_state_directory_is_unknown_not_empty() {
    let SavedFreezes::Unknown(why) = load_freezes(None).await else {
        panic!("no state directory must not read as nothing frozen");
    };
    assert!(why.contains("no usable state directory"), "{why}");
}

#[tokio::test]
async fn a_save_that_fails_leaves_the_machine_frozen_for_the_run_and_says_so() {
    let dir = tempdir();
    let state = state_file(dir.path());
    let r = run(&state, "/fleet", &[]).await;
    // The file goes bad after the run started (another program, a bad edit).
    damage(&state, b"garbage {\n");
    r.toggle("luna", true).await;
    assert!(row_frozen(&r.view(), "luna"), "frozen for this run");
    let n = notice(&r.shell);
    assert!(
        n.contains("this run only") && n.contains("not valid"),
        "{n}"
    );
    r.quit().await;
    assert_eq!(
        fs::read(&state).unwrap(),
        b"garbage {\n",
        "and the file is as it was"
    );
}

#[tokio::test]
async fn a_config_freeze_is_never_written_to_the_state_file() {
    let dir = tempdir();
    let state = state_file(dir.path());
    let r = run(&state, "/fleet", &["luna"]).await;
    assert!(row_frozen(&r.view(), "luna"));
    r.toggle("luna", true).await;
    assert!(row_frozen(&r.view(), "luna"), "f cannot undo it");
    r.toggle("sol", true).await;
    r.quit().await;
    let store = FreezeStore::new(state, "/fleet".to_owned());
    assert_eq!(
        store.load().await.unwrap(),
        set(&["sol"]),
        "only f's own freeze"
    );
}

// ---------------------------------------------------------------------------------------
// The action log
// ---------------------------------------------------------------------------------------

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

#[tokio::test]
async fn every_freeze_and_unfreeze_is_a_row_in_the_action_log() {
    let dir = tempdir();
    let fleet = tempdir();
    let state = state_file(dir.path());
    let db = dir.path().join("logs").join("log.sqlite");
    let log = || OpenParams::new(db.clone(), identity(), fleet.path().to_path_buf());

    let r = run_logged(&state, "/fleet", &[], Some(log())).await;
    r.toggle("luna", true).await; // freeze
    r.toggle("luna", true).await; // unfreeze (asks, y)
    r.toggle("sol", true).await; // another machine
    r.quit().await;

    let luna = rows(&db, fleet.path(), "luna").await;
    // Not by order: two rows written within one millisecond of two quick presses may sort
    // either way, and which is newer is not what is tested.
    let rows_of: BTreeSet<_> = luna
        .iter()
        .map(|a| {
            (
                a.kind.as_str(),
                a.result.map(LoggedResult::as_str),
                a.detail.clone().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        rows_of,
        BTreeSet::from([
            ("freeze", Some("succeeded"), "frozen".to_owned()),
            ("unfreeze", Some("succeeded"), "unfrozen".to_owned()),
        ])
    );
    assert!(
        luna.iter()
            .all(|a| a.who == "tester" && a.machine == "luna")
    );
    let sol = rows(&db, fleet.path(), "sol").await;
    assert_eq!(sol.len(), 1);
    assert_eq!(sol[0].kind, LoggedKind::Freeze);
}

#[tokio::test]
async fn a_freeze_that_could_not_be_saved_is_logged_as_failed_with_the_reason() {
    let dir = tempdir();
    let fleet = tempdir();
    let state = state_file(dir.path());
    let db = dir.path().join("logs").join("log.sqlite");
    let r = run_logged(
        &state,
        "/fleet",
        &[],
        Some(OpenParams::new(
            db.clone(),
            identity(),
            fleet.path().to_path_buf(),
        )),
    )
    .await;
    damage(&state, b"garbage {\n");
    r.toggle("luna", true).await;
    r.quit().await;
    let luna = rows(&db, fleet.path(), "luna").await;
    assert_eq!(luna.len(), 1);
    assert_eq!(luna[0].kind, LoggedKind::Freeze);
    assert_eq!(luna[0].result, Some(LoggedResult::Failed));
    assert!(
        luna[0]
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("not valid")),
        "{:?}",
        luna[0].detail
    );
}

#[tokio::test]
async fn a_refused_f_is_not_logged() {
    let dir = tempdir();
    let fleet = tempdir();
    let state = state_file(dir.path());
    let db = dir.path().join("logs").join("log.sqlite");
    let r = run_logged(
        &state,
        "/fleet",
        &["luna"],
        Some(OpenParams::new(
            db.clone(),
            identity(),
            fleet.path().to_path_buf(),
        )),
    )
    .await;
    r.select("luna");
    r.shell.send(Event::ToggleFreeze); // refused: frozen in the config
    r.quit().await;
    assert!(
        !db.exists() || rows(&db, fleet.path(), "luna").await.is_empty(),
        "nothing happened, so nothing is recorded"
    );
}

/// The rule in `deploylog`'s documentation: a freeze never waits for, or fails because of, the
/// log. A log that is refused (inside the fleet) is a warning; the freeze is made and kept.
#[tokio::test]
async fn a_log_that_cannot_be_used_is_a_warning_and_the_freeze_is_kept() {
    let dir = tempdir();
    let fleet = tempdir();
    let state = state_file(dir.path());
    let r = run_logged(
        &state,
        "/fleet",
        &[],
        Some(OpenParams::new(
            fleet.path().join("log.sqlite"),
            identity(),
            fleet.path().to_path_buf(),
        )),
    )
    .await;
    r.toggle("luna", true).await;
    assert!(row_frozen(&r.view(), "luna"));
    let shell = Arc::clone(&r.shell);
    r.quit().await;
    let warning = shell.warning().expect("a warning for the status bar");
    assert!(warning.contains("action log"), "{warning}");
    let store = FreezeStore::new(state, "/fleet".to_owned());
    assert_eq!(
        store.load().await.unwrap(),
        set(&["luna"]),
        "the freeze was kept"
    );
    assert!(
        !fleet.path().join("log.sqlite").exists(),
        "nothing written into the fleet"
    );
}

// ---------------------------------------------------------------------------------------
// --json
// ---------------------------------------------------------------------------------------

const LIMIT: Duration = Duration::from_secs(60);

const INVENTORY: &str = r#"{"terra":{"deploy":{"targetHost":"root@192.0.2.10"}},"luna":{"deploy":{"targetHost":"root@192.0.2.11"}},"sol":{"deploy":{"targetHost":"root@192.0.2.12"}}}"#;

fn store_path(name: &str) -> String {
    format!("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-{name}")
}

struct Fleet {
    dir: tempfile::TempDir,
    flake: PathBuf,
    nix: PathBuf,
    eval: PathBuf,
    ssh: PathBuf,
}

fn fleet(t: &Tools) -> Fleet {
    let dir = tempdir();
    let flake = dir.path().join("fleet");
    fs::create_dir_all(&flake).unwrap();
    let nix = t.script(dir.path(), "nix", &format!("printf '%s\\n' '{INVENTORY}'"));
    let line = |m: &str| {
        format!(
            r#"{{"attr":"{m}","attrPath":["{m}"],"drvPath":"{}.drv","outputs":{{"out":"{}"}}}}"#,
            store_path(&format!("nixos-system-{m}")),
            store_path(&format!("nixos-system-{m}"))
        )
    };
    let eval = t.script(
        dir.path(),
        "nix-eval-jobs",
        &format!(
            "printf '%s\\n' '{}' '{}' '{}'",
            line("terra"),
            line("luna"),
            line("sol")
        ),
    );
    let ssh = t.script(
        dir.path(),
        "ssh",
        &format!(
            "cat > /dev/null\nprintf 'auspex_probe=1\\nprobe=nixos\\ncurrent_system={}\\nsystem_state=running\\nfailed_units=[]\\nauspex_end=1\\n'",
            store_path("nixos-system-old")
        ),
    );
    Fleet {
        dir,
        flake,
        nix,
        eval,
        ssh,
    }
}

impl Fleet {
    /// Where the binary, with this `HOME`, keeps its state file.
    fn state(&self) -> PathBuf {
        self.dir
            .path()
            .join("state")
            .join("auspex")
            .join(freeze::FILE_NAME)
    }

    fn key(&self) -> String {
        freeze::fleet_key(self.flake.to_str().unwrap())
    }

    fn plain(&self, config: &str) -> std::process::Output {
        let cfg = self.dir.path().join("auspex.toml");
        fs::write(&cfg, config).unwrap();
        let mut cmd = auspex(self.dir.path());
        cmd.args(["--plain", "--config"])
            .arg(&cfg)
            .arg("--flake")
            .arg(&self.flake)
            .env("AUSPEX_NIX", &self.nix)
            .env("AUSPEX_NIX_EVAL_JOBS", &self.eval)
            .env("AUSPEX_SSH", &self.ssh);
        run_within(&mut cmd, LIMIT)
    }

    fn json(&self, config: &str) -> (std::process::Output, serde_json::Value) {
        let cfg = self.dir.path().join("auspex.toml");
        fs::write(&cfg, config).unwrap();
        let mut cmd = auspex(self.dir.path());
        cmd.args(["--json", "--config"])
            .arg(&cfg)
            .arg("--flake")
            .arg(&self.flake)
            .env("AUSPEX_NIX", &self.nix)
            .env("AUSPEX_NIX_EVAL_JOBS", &self.eval)
            .env("AUSPEX_SSH", &self.ssh);
        let out = run_within(&mut cmd, LIMIT);
        let json = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out, json)
    }
}

fn frozen_flags(json: &serde_json::Value) -> BTreeMap<String, bool> {
    json["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["name"].as_str().unwrap().to_owned(),
                m["frozen"].as_bool().expect("every machine has `frozen`"),
            )
        })
        .collect()
}

#[test]
fn json_has_a_frozen_field_on_every_machine_and_it_is_false_by_default() {
    let Some(t) = tools(&[]) else { return };
    let f = fleet(&t);
    let (out, json) = f.json("");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(json["version"], 1, "the contract's version does not change");
    assert_eq!(
        frozen_flags(&json),
        BTreeMap::from([
            ("luna".to_owned(), false),
            ("sol".to_owned(), false),
            ("terra".to_owned(), false)
        ])
    );
    assert!(!f.state().exists(), "a read-only run creates no state file");
}

#[test]
fn json_reports_both_kinds_of_freeze() {
    let Some(t) = tools(&[]) else { return };
    let f = fleet(&t);
    // luna frozen with `f` in an earlier run; terra declared in the config.
    FreezeStore::new(f.state(), f.key())
        .set_blocking("luna", true)
        .unwrap();
    let (out, json) = f.json("[machines.terra]\nfrozen = true\n");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        frozen_flags(&json),
        BTreeMap::from([
            ("luna".to_owned(), true),
            ("sol".to_owned(), false),
            ("terra".to_owned(), true)
        ])
    );
    // The badge list says it too, so a program need not read two places.
    let badges = |name: &str| {
        json["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .unwrap()["badges"]
            .clone()
    };
    assert!(
        badges("luna")
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b == "frozen")
    );
    assert!(
        !badges("sol")
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b == "frozen")
    );
}

#[test]
fn json_does_not_report_another_fleets_freeze() {
    let Some(t) = tools(&[]) else { return };
    let f = fleet(&t);
    FreezeStore::new(f.state(), "/some/other/fleet".to_owned())
        .set_blocking("luna", true)
        .unwrap();
    let (_, json) = f.json("");
    assert!(!frozen_flags(&json)["luna"]);
}

#[test]
fn json_with_a_corrupt_state_file_says_so_in_band_and_in_the_exit_status() {
    let Some(t) = tools(&[]) else { return };
    let f = fleet(&t);
    damage(&f.state(), b"this is { not toml\n");
    let (out, json) = f.json("[machines.terra]\nfrozen = true\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The report cannot be trusted for `frozen`, so the run says so where a script looks: an error
    // in the report, and exit 1. A silent `frozen: false` would have a script update the machine
    // the operator froze.
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    let errors = json["errors"].as_array().expect("errors");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["kind"], "freezes_unknown");
    let reason = errors[0]["reason"].as_str().unwrap();
    assert!(
        reason.contains("not valid") && reason.contains("unknown"),
        "{reason}"
    );
    assert_eq!(
        json["settled"], false,
        "a report with an error is not a settled one"
    );
    assert!(
        stderr.contains("freezes made with f are unknown") && stderr.contains("not valid"),
        "{stderr}"
    );
    // What cannot be known is not guessed; what the config says still holds.
    assert_eq!(
        frozen_flags(&json),
        BTreeMap::from([
            ("luna".to_owned(), false),
            ("sol".to_owned(), false),
            ("terra".to_owned(), true)
        ])
    );
    assert_eq!(
        fs::read(f.state()).unwrap(),
        b"this is { not toml\n",
        "not changed"
    );
}

#[test]
fn a_frozen_key_of_the_wrong_type_is_an_error_naming_it() {
    let e = auspex_tui::config::parse("[machines.terra]\nfrozen = \"yes\"\n").unwrap_err();
    assert!(e.contains("`machines.terra.frozen`"), "{e}");
    let (s, w) = auspex_tui::config::parse("[machines.terra]\nfrozen = true\n[machines.luna]\nfrozen = false\n[machines.sol]\ntarget = \"root@10.0.0.1\"\n").unwrap();
    assert!(w.is_empty(), "{w:?}");
    assert!(s.machines["terra"].frozen);
    assert!(
        !s.machines["luna"].frozen && !s.machines["sol"].frozen,
        "false is the default"
    );
    let core = auspex_tui::app::core_config(&s, "/fleet".to_owned());
    assert_eq!(core.freeze.declared, set(&["terra"]));
    assert_eq!(core.freeze.saved, SavedFreezes::default());
}

/// The rule in `deploylog`'s documentation, tested where it can fail: with the log's write lock
/// held by someone else for seconds, a freeze is still made, kept and answered at once. The row
/// waits (and is a warning if it gives up), never the freeze.
#[tokio::test]
async fn a_freeze_does_not_wait_for_a_busy_log() {
    let dir = tempdir();
    let fleet = tempdir();
    let state = state_file(dir.path());
    let db = dir.path().join("logs").join("log.sqlite");
    let params = || OpenParams::new(db.clone(), identity(), fleet.path().to_path_buf());
    // Create the log (schema and all), then hold its write lock from a second connection.
    ActionLog::open(params())
        .await
        .expect("the log opens")
        .close()
        .await;
    let holder = rusqlite::Connection::open(&db).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let r = run_logged(&state, "/fleet", &[], Some(params())).await;
    let started = std::time::Instant::now();
    r.toggle("luna", true).await;
    let took = started.elapsed();
    assert!(row_frozen(&r.view(), "luna"));
    assert!(
        took < Duration::from_millis(1_000),
        "the freeze waited {took:?} for a log whose write lock is held"
    );
    let store = FreezeStore::new(state, "/fleet".to_owned());
    assert_eq!(store.load().await.unwrap(), set(&["luna"]), "kept at once");
    // The row gives up (the log's own call deadline) and says so; the freeze stands.
    let shell = Arc::clone(&r.shell);
    r.quit().await;
    drop(holder);
    let warning = shell.warning().expect("the log's failure is a warning");
    assert!(warning.contains("action log"), "{warning}");
}

#[tokio::test]
async fn without_a_state_directory_the_setup_marks_freezes_unknown_and_says_so_in_the_log() {
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    let mut config = demo_config(&[], SavedFreezes::default());
    setup_freezes(&shell, &mut config, None).await;
    assert!(matches!(config.freeze.saved, SavedFreezes::Unknown(_)));
    assert!(
        shell
            .log
            .lines()
            .iter()
            .any(|l| l.contains("freezes made with f are unknown")),
        "{:?}",
        shell.log.lines()
    );
}

#[test]
fn json_with_readable_freezes_has_no_freeze_error_and_exits_0() {
    let Some(t) = tools(&[]) else { return };
    let f = fleet(&t);
    FreezeStore::new(f.state(), f.key())
        .set_blocking("luna", true)
        .unwrap();
    let (out, json) = f.json("");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(json["errors"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------------------
// Fix round 1: F1 (run-only freezes), F2 (the last look at the file)
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_freeze_whose_save_failed_is_kept_in_the_next_save_that_works() {
    let dir = tempdir();
    let state = state_file(dir.path());
    let r = run(&state, "/fleet", &[]).await;
    // The file is bad when luna is frozen, so that save fails: frozen for this run only.
    damage(&state, b"garbage {\n");
    r.toggle("luna", true).await;
    assert!(
        notice(&r.shell).contains("this run only"),
        "{}",
        notice(&r.shell)
    );
    // The file is repaired (deleted); the next freeze works, and carries luna into the file.
    fs::remove_file(&state).unwrap();
    r.toggle("sol", true).await;
    assert!(
        row_frozen(&r.view(), "luna"),
        "still frozen: nothing lifted it"
    );
    assert!(row_frozen(&r.view(), "sol"));
    assert!(r.view().freeze.run_only.is_empty(), "the file holds it now");
    r.quit().await;
    let store = FreezeStore::new(state, "/fleet".to_owned());
    assert_eq!(store.load().await.unwrap(), set(&["luna", "sol"]));
}

/// Waits until the update question is open (the checks have answered).
async fn until_update_question(shell: &Arc<Shell>) {
    let wait = async {
        while !shell
            .view()
            .confirm
            .as_ref()
            .is_some_and(|c| c.kind == ConfirmKind::Update)
        {
            tokio::select! {
                () = shell.changed() => {}
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("the question about the update");
}

/// The demo is scanned and alfrost, the first row, is evaluated.
async fn ready(r: &Run) {
    let wait = async {
        while !(r.view().refresh.settled
            && r.view()
                .report
                .machines
                .first()
                .is_some_and(|m| m.name == "alfrost" && m.pending_system.is_some()))
        {
            tokio::select! {
                () = r.shell.changed() => {}
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("the scan to settle");
}

fn act_on(r: &Run) {
    r.shell.send(Event::ToggleAct);
    r.shell.send(Event::ConfirmAnswer(true));
    assert_eq!(r.view().mode, Mode::Act);
}

#[tokio::test]
async fn a_freeze_made_by_another_auspex_is_honoured_at_the_y() {
    let dir = tempdir();
    let state = state_file(dir.path());
    // Two running auspex processes on one fleet; both read an empty file at start-up.
    let a = run(&state, "/fleet", &[]).await;
    let b = run(&state, "/fleet", &[]).await;
    ready(&a).await;
    act_on(&a);
    a.select("alfrost");
    a.shell.send(Event::UpdateSelected);
    until_update_question(&a.shell).await;
    // The operator freezes alfrost in the other terminal while the question is open here.
    b.toggle("alfrost", true).await;
    a.shell.send(Event::ConfirmAnswer(true));
    let wait = async {
        while a.view().notice.is_none() {
            a.shell.changed().await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("an answer to the y");
    assert_eq!(notice(&a.shell), "alfrost is frozen; f to unfreeze");
    assert!(a.view().action.is_none(), "nothing was started");
    assert!(
        row_frozen(&a.view(), "alfrost"),
        "and this process shows the ❄"
    );
    a.quit().await;
    b.quit().await;
}

#[tokio::test]
async fn a_state_file_that_cannot_be_read_at_the_y_refuses_the_update() {
    let dir = tempdir();
    let state = state_file(dir.path());
    let a = run(&state, "/fleet", &[]).await;
    ready(&a).await;
    act_on(&a);
    a.select("alfrost");
    a.shell.send(Event::UpdateSelected);
    until_update_question(&a.shell).await;
    damage(&state, b"garbage {\n");
    a.shell.send(Event::ConfirmAnswer(true));
    let wait = async {
        while a.view().notice.is_none() {
            a.shell.changed().await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("an answer to the y");
    let n = notice(&a.shell);
    assert!(
        n.contains("could not read which machines are frozen") && n.contains("not valid"),
        "{n}"
    );
    assert!(a.view().action.is_none(), "nothing was started");
    a.quit().await;
}

#[tokio::test]
async fn the_gate_before_the_spawn_refuses_a_frozen_machine_and_an_unreadable_file() {
    let dir = tempdir();
    let state = state_file(dir.path());
    let store = FreezeStore::new(state.clone(), "/fleet".to_owned());
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    shell.set_freeze_store(store.clone());
    // Nothing frozen (no file at all): through.
    assert_eq!(shell.freeze_gate("luna").await, Ok(()));
    store.set("sol", true).await.unwrap();
    assert_eq!(
        shell.freeze_gate("luna").await,
        Ok(()),
        "another machine's freeze"
    );
    let why = shell.freeze_gate("sol").await.unwrap_err();
    assert!(why.contains("sol is frozen"), "{why}");
    damage(&state, b"garbage {\n");
    let why = shell.freeze_gate("luna").await.unwrap_err();
    assert!(
        why.contains("could not read which machines are frozen") && why.contains("not valid"),
        "{why}"
    );
}

/// The shell looks again right before `clan` is started, after the core's own look at the `y`:
/// two reads for an update that goes through (and `clan` really ran).
#[tokio::test]
async fn an_update_reads_the_state_file_at_the_y_and_again_before_clan_starts() {
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let clan = t.script(dir.path(), "clan", "echo deploying\nexit 0");
    let nix = t.script(dir.path(), "nix", "exit 99");
    let real = auspex_tui::backend::Real::for_actions(nix.into_os_string(), Some(clan));
    let shell = Shell::new(
        Backend::Demo(Demo {
            real_actions: Some(Box::new(real)),
            ..Demo::default()
        }),
        Log::memory(),
        Duration::from_secs(120),
    );
    let state = state_file(dir.path());
    let mut config = Config {
        flake: fleet.path().to_str().unwrap().to_owned(),
        ..demo::config()
    };
    setup_freezes(
        &shell,
        &mut config,
        Some(FreezeStore::new(state, "/fleet".to_owned())),
    )
    .await;
    shell.send(Event::Start(config));
    let r = Run {
        shell: Arc::clone(&shell),
    };
    ready(&r).await;
    act_on(&r);
    assert_eq!(shell.freeze_reads(), 0, "reading is for updates");
    shell.send(Event::UpdateSelected);
    until_update_question(&shell).await;
    shell.send(Event::ConfirmAnswer(true));
    let wait = async {
        while !shell
            .view()
            .action
            .as_ref()
            .is_some_and(|a| matches!(a.status, auspex_core::ActionStatus::Ended(_)))
        {
            shell.changed().await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("the update to end");
    assert_eq!(
        shell.freeze_reads(),
        2,
        "once for the core, once for the spawn"
    );
    shell.shutdown().await;
}

#[test]
fn plain_with_a_corrupt_state_file_prints_the_freeze_error() {
    let Some(t) = tools(&[]) else { return };
    let f = fleet(&t);
    damage(&f.state(), b"this is { not toml\n");
    let out = f.plain("");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    let line = stdout
        .lines()
        .find(|l| l.starts_with("error: freezes_unknown"))
        .unwrap_or_else(|| panic!("no freeze error in:\n{stdout}"));
    assert!(line.contains("not valid"), "{line}");
}

// ---------------------------------------------------------------------------------------
// Fix round 2: N4 (the gate before the spawn, through `run_action`), N5 (the status note)
// ---------------------------------------------------------------------------------------

async fn run_op(
    shell: &Arc<Shell>,
    op: auspex_core::effects::ActionRequest,
) -> auspex_core::ActionEnd {
    let (tx, rx) = tokio::sync::oneshot::channel();
    shell.start_action(op, move |_, end| {
        let _ = tx.send(end);
    });
    tokio::time::timeout(Duration::from_secs(30), rx)
        .await
        .expect("the action to end")
        .expect("an answer")
}

/// A stale core can ask for the update of a machine that is frozen in the file by now. The shell
/// reads the file once more before it starts `clan`, and a frozen machine, or a file that cannot
/// be read, ends the action there: `clan` is never started and no row says a deploy was tried.
#[tokio::test]
async fn the_shell_never_starts_clan_for_a_machine_frozen_in_the_file() {
    use auspex_core::ActionResult;
    use auspex_core::effects::{ActionKind, ActionRequest, RequestId};
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let clan = t.script(dir.path(), "clan", "echo ran >> '@dir@/clan-ran'\nexit 0");
    let nix = t.script(dir.path(), "nix", "exit 99");
    let real = auspex_tui::backend::Real::for_actions(nix.into_os_string(), Some(clan));
    let shell = Shell::new(
        Backend::Demo(Demo {
            real_actions: Some(Box::new(real)),
            ..Demo::default()
        }),
        Log::memory(),
        Duration::from_secs(120),
    );
    let state = state_file(dir.path());
    let store = FreezeStore::new(state.clone(), "/fleet".to_owned());
    shell.set_freeze_store(store.clone());
    let db = dir.path().join("logs").join("log.sqlite");
    shell.set_action_log(OpenParams::new(
        db.clone(),
        identity(),
        fleet.path().to_path_buf(),
    ));
    let ran = dir.path().join("clan-ran");
    let op = |id| ActionRequest {
        id: RequestId(id),
        kind: ActionKind::Update,
        machine: "alfrost".to_owned(),
        class: auspex_domain::fleet::MachineClass::Nixos,
        system: Some("x86_64-linux".to_owned()),
        flake: fleet.path().to_str().unwrap().to_owned(),
        build_host: None,
        target: None,
        log: None,
    };

    // Control: nothing frozen, so it goes through.
    let end = run_op(&shell, op(1)).await;
    assert_eq!(end.result, ActionResult::Succeeded, "{end:?}");
    assert!(ran.exists(), "clan ran");
    fs::remove_file(&ran).unwrap();

    // Frozen in the file: refused here, `clan` not started.
    store.set("alfrost", true).await.unwrap();
    let end = run_op(&shell, op(2)).await;
    assert_eq!(end.result, ActionResult::Refused);
    assert!(
        end.summary.iter().any(|l| l.contains("alfrost is frozen")),
        "{end:?}"
    );
    assert!(!ran.exists(), "clan was not started for a frozen machine");

    // A file that cannot be read: refused too.
    damage(&state, b"garbage {\n");
    let end = run_op(&shell, op(3)).await;
    assert_eq!(end.result, ActionResult::Refused);
    assert!(
        end.summary
            .iter()
            .any(|l| l.contains("could not read which machines are frozen")),
        "{end:?}"
    );
    assert!(!ran.exists());

    shell.shutdown().await;
    // Only the control run is a deploy row: a refused one is not a deploy that was tried.
    let rows = rows(&db, fleet.path(), "alfrost").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].kind, LoggedKind::Deploy);
}
