//! What the operator sees and presses between `u` and the confirmation, and what the shell
//! runs for it.
//!
//! Two halves. The dialog: driven through the keymap against the scripted demo with time
//! paused (refusals first, then warnings, then the question; the typed name for the demo's
//! public machine; snapshots at 40 and 120 columns). The adapters: the real [`Backend`]'s
//! checks against a fake `git` and a fake `nix`, pinning what is run and what is answered.
//!
//! Nothing here runs a real `clan`, `nix` or ssh, or touches a real fleet.

mod common;

use std::fs;
use std::time::Duration;

use auspex_core::effects::{PreflightRequest, RequestId};
use auspex_core::{ActionStatus, ConfirmKind, Mode, PreflightAnswer};
use auspex_domain::observe::StorePath;
use auspex_domain::preflight::{TreeState, Upstream};
use auspex_effects::CancellationToken;
use auspex_tui::backend::{Backend, Real};
use auspex_tui::demo::{Demo, PUBLIC_MACHINE};
use auspex_tui::keymap::{Key, Keymap};
use auspex_tui::ui::Ui;
use common::{
    Tools, expect_file, press, render, settled_demo, settled_demo_with, tempdir, tools, type_keys,
    type_keys_seen, until_asked, until_checked,
};
use crossterm::event::KeyCode;

fn dirty_and_unpushed() -> PreflightAnswer {
    PreflightAnswer {
        tree: Ok(TreeState {
            changed_files: 2,
            upstream: Upstream::Tracking {
                name: "origin/main".to_owned(),
                ahead: 1,
                behind: 0,
            },
        }),
        closure: Ok(vec![(StorePath::new(GLIBC).unwrap(), 2_469_606_195)]),
    }
}

fn act_on(shell: &std::sync::Arc<auspex_tui::shell::Shell>, ui: &mut Ui, km: &Keymap) {
    type_keys_seen(shell, ui, km, "Ay");
    assert_eq!(shell.view().mode, Mode::Act);
}

// ---------------------------------------------------------------------------------------
// The dialog
// ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn while_the_checks_run_the_dialog_says_so_and_any_key_cancels_them() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    act_on(&shell, &mut ui, &km);
    type_keys_seen(&shell, &mut ui, &km, "/luna\nu");
    let confirm = shell.view().confirm.expect("a dialog");
    assert_eq!(confirm.kind, ConfirmKind::Checking);
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    assert!(
        frame.contains("checking luna before the update") && frame.contains("any key cancels"),
        "{frame}"
    );
    expect_file("snapshots", "preflight_checking_120.txt", &frame);
    // `y` is not a yes to a question nobody has seen.
    type_keys(&shell, &mut ui, &km, "y");
    until_asked(&shell, &mut ui, &km).await;
    assert!(shell.view().confirm.is_none(), "cancelled");
    assert!(shell.view().action.is_none(), "nothing ran");
    // The demo's late answer for the cancelled check opens nothing.
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(shell.view().confirm.is_none());
}

#[tokio::test(start_paused = true)]
async fn refusals_come_first_then_warnings_then_the_question_and_nothing_proceeds() {
    // europa and io are the demo's two lab names that answer with one address: refused. The
    // shell also reports a dirty, unpushed tree: two warnings, shown with the refusal.
    let shell = settled_demo_with(Demo {
        preflight_answer: Some(dirty_and_unpushed()),
        ..Demo::default()
    })
    .await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    act_on(&shell, &mut ui, &km);
    type_keys_seen(&shell, &mut ui, &km, "/europa\nu");
    until_checked(&shell).await;
    // The dialog is open and has not been drawn: a key is discarded, not an answer to it.
    type_keys(&shell, &mut ui, &km, "y");
    assert!(
        shell.view().confirm.is_some(),
        "a key before the frame is not an answer"
    );
    common::frame(&shell, &mut ui, &km);
    common::elapse(auspex_tui::ui::QUESTION_DWELL);
    let confirm = shell.view().confirm.expect("a dialog");
    assert_eq!(confirm.kind, ConfirmKind::Refused);
    assert_eq!(confirm.refusals.len(), 1, "{:?}", confirm.refusals);
    assert!(
        confirm.refusals[0].contains("192.168.1.30")
            && confirm.refusals[0].contains("io and vulcan also resolve to"),
        "{:?}",
        confirm.refusals
    );
    assert_eq!(confirm.warnings.len(), 2, "{:?}", confirm.warnings);

    let wide = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    let at = |needle: &str| {
        wide.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{wide}"))
    };
    assert!(
        at("refused:") < at("warning:") && at("warning:") < at("will not be updated"),
        "refusals, then warnings, then the question:\n{wide}"
    );
    assert!(wide.contains("any key closes"), "{wide}");
    assert!(!wide.contains("y confirms"), "no way to proceed:\n{wide}");
    expect_file("snapshots", "preflight_refused_120.txt", &wide);
    let narrow = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 40, 30);
    assert!(
        narrow.contains("refused") && narrow.contains("warning") && narrow.contains("any key"),
        "{narrow}"
    );
    expect_file("snapshots", "preflight_refused_40.txt", &narrow);

    // `y` closes it and nothing runs.
    // It has been on screen for the dwell: `y` closes it.
    type_keys(&shell, &mut ui, &km, "y");
    type_keys(&shell, &mut ui, &km, "y");
    assert!(shell.view().confirm.is_none());
    assert!(shell.view().action.is_none());
}

#[tokio::test(start_paused = true)]
async fn warnings_alone_are_shown_with_the_question_and_y_still_confirms() {
    let shell = settled_demo_with(Demo {
        preflight_answer: Some(dirty_and_unpushed()),
        ..Demo::default()
    })
    .await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    act_on(&shell, &mut ui, &km);
    type_keys_seen(&shell, &mut ui, &km, "/luna\nu");
    until_asked(&shell, &mut ui, &km).await;
    let confirm = shell.view().confirm.expect("a dialog");
    assert_eq!(confirm.kind, ConfirmKind::Update);
    assert_eq!(confirm.refusals, Vec::<String>::new());
    assert_eq!(confirm.warnings.len(), 2);
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    assert!(
        frame.contains("warning: the fleet repository has uncommitted changes to 2 tracked")
            && frame.contains("y confirms"),
        "{frame}"
    );
    expect_file("snapshots", "preflight_warnings_120.txt", &frame);
    type_keys(&shell, &mut ui, &km, "y");
    assert!(shell.view().action.is_some(), "a warning does not stop it");
}

#[tokio::test(start_paused = true)]
async fn the_public_machine_asks_for_its_name_and_only_the_name_runs_the_update() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    act_on(&shell, &mut ui, &km);
    type_keys_seen(&shell, &mut ui, &km, &format!("/{PUBLIC_MACHINE}\nu"));
    until_asked(&shell, &mut ui, &km).await;
    let confirm = shell.view().confirm.expect("a dialog");
    assert_eq!(confirm.kind, ConfirmKind::UpdateTyped);

    // `y`, then Enter: the text is "y", which is not the name.
    type_keys(&shell, &mut ui, &km, "y\n");
    assert!(shell.view().action.is_none());
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    assert!(
        frame.contains(&format!("that is not {PUBLIC_MACHINE}")),
        "{frame}"
    );
    expect_file("snapshots", "preflight_typed_mismatch_120.txt", &frame);

    // Backspace deletes it (a key, not a character), and a prefix is still not the name.
    press(&shell, &mut ui, &km, Key::plain(KeyCode::Backspace));
    assert_eq!(shell.view().confirm.unwrap().typed.unwrap().text, "");
    let name_minus_one = &PUBLIC_MACHINE[..PUBLIC_MACHINE.len() - 1];
    type_keys(&shell, &mut ui, &km, name_minus_one);
    type_keys(&shell, &mut ui, &km, "\n");
    assert!(shell.view().action.is_none(), "a prefix is not the name");
    let frame = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 40, 24);
    expect_file("snapshots", "preflight_typed_mismatch_40.txt", &frame);
    // The last letter, then Enter.
    type_keys(
        &shell,
        &mut ui,
        &km,
        &PUBLIC_MACHINE[PUBLIC_MACHINE.len() - 1..],
    );
    let typing = render(&shell.view(), &mut ui, &km, &shell.log.lines(), 120, 30);
    assert!(typing.contains(&format!("> {PUBLIC_MACHINE}")), "{typing}");
    expect_file("snapshots", "preflight_typed_120.txt", &typing);
    type_keys(&shell, &mut ui, &km, "\n");
    let action = shell.view().action.expect("the update runs");
    assert_eq!(action.machine, PUBLIC_MACHINE);
    assert!(matches!(
        action.status,
        ActionStatus::Running | ActionStatus::Ended(_)
    ));
}

// ---------------------------------------------------------------------------------------
// What the real backend runs
// ---------------------------------------------------------------------------------------

const SYSTEM: &str = "/nix/store/2123456789abcdfghijklmnpqrsvwxyz-nixos-system-terra-25.11";
const GLIBC: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-glibc-2.40-66";

fn request(flake: &str) -> PreflightRequest {
    PreflightRequest {
        id: RequestId(1),
        machine: "luna".to_owned(),
        flake: flake.to_owned(),
        store_path: StorePath::new(SYSTEM).unwrap(),
        timeout_seconds: 20,
    }
}

fn backend(
    t: &Tools,
    dir: &std::path::Path,
    git_body: &str,
    nix_body: &str,
    cache: Option<&str>,
) -> Backend {
    let git = t.script(
        dir,
        "git",
        &format!("printf '%s\\n' \"$@\" > '@dir@/git-argv'\npwd > '@dir@/git-cwd'\n{git_body}"),
    );
    let nix = t.script(
        dir,
        "nix",
        &format!("printf '%s\\n' \"$@\" >> '@dir@/nix-argv'\n{nix_body}"),
    );
    Backend::Real(Box::new(
        Real::for_actions(nix.into_os_string(), None)
            .with_git_and_cache(git.into_os_string(), cache.map(str::to_owned)),
    ))
}

#[tokio::test]
async fn the_real_backend_answers_both_checks_from_git_and_nix_and_runs_exactly_these() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    let b = backend(
        &t,
        dir.path(),
        "printf '## main...origin/main [ahead 3]\\0 M flake.nix\\0'",
        &format!("printf '%s\\t%s\\n' '{GLIBC}' 1000 '{SYSTEM}' 2000"),
        None,
    );
    let answer = b
        .preflight(
            &request(fleet.path().to_str().unwrap()),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        answer,
        PreflightAnswer {
            tree: Ok(TreeState {
                changed_files: 1,
                upstream: Upstream::Tracking {
                    name: "origin/main".to_owned(),
                    ahead: 3,
                    behind: 0,
                },
            }),
            closure: Ok(vec![
                (StorePath::new(GLIBC).unwrap(), 1000),
                (StorePath::new(SYSTEM).unwrap(), 2000)
            ]),
        }
    );
    // git: one status, in the fleet directory.
    let argv = fs::read_to_string(dir.path().join("git-argv")).unwrap();
    assert!(
        argv.starts_with("-c\ncore.fsmonitor=false\n--no-optional-locks\n")
            && argv.contains("\nstatus\n--porcelain=v1\n-b\n-z\n"),
        "{argv}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("git-cwd"))
            .unwrap()
            .trim()
            .to_owned(),
        fleet.path().canonicalize().unwrap().to_str().unwrap()
    );
    // nix: the local size query, and nothing else (the answer was there).
    assert_eq!(
        fs::read_to_string(dir.path().join("nix-argv")).unwrap(),
        format!("path-info\n--recursive\n--size\n--\n{SYSTEM}\n")
    );
}

#[tokio::test]
async fn a_failing_check_is_one_line_in_its_own_half_of_the_answer_and_never_the_other() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let fleet = tempdir();
    // git fails; nix has the closure in the cache only.
    let b = backend(
        &t,
        dir.path(),
        "printf 'fatal: not a git repository\\n' >&2\nexit 128",
        &format!("case \"$*\" in *--store*) printf '%s\\t%s\\n' '{SYSTEM}' 5;; *) exit 1;; esac"),
        Some("https://cache.example"),
    );
    let answer = b
        .preflight(
            &request(fleet.path().to_str().unwrap()),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let why = answer.tree.unwrap_err();
    assert!(why.contains("not a git repository"), "{why}");
    assert_eq!(
        answer.closure,
        Ok(vec![(StorePath::new(SYSTEM).unwrap(), 5)])
    );
    let argv = fs::read_to_string(dir.path().join("nix-argv")).unwrap();
    assert!(
        argv.contains("--store\nhttps://cache.example/\n--\n"),
        "the cache is asked when the local store does not have it:\n{argv}"
    );

    // A fleet that is not a local directory: no git is run, and that is said.
    let dir = tempdir();
    let b = backend(&t, dir.path(), "printf '## main\\0'", "exit 1", None);
    let answer = b
        .preflight(&request("github:example/fleet"), &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        answer.tree.unwrap_err().contains("not a local directory"),
        "no working tree to check"
    );
    assert!(!dir.path().join("git-argv").exists());
    assert!(
        answer
            .closure
            .unwrap_err()
            .contains("no binary cache is configured")
    );
}

#[tokio::test]
async fn a_cancelled_check_answers_nothing() {
    let Some(t) = tools(&["cat", "sleep"]) else {
        return;
    };
    let dir = tempdir();
    let fleet = tempdir();
    let b = backend(&t, dir.path(), "@sleep@ 30", "@sleep@ 30", None);
    let cancel = CancellationToken::new();
    let canceller = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.cancel();
        })
    };
    let started = std::time::Instant::now();
    let answer = b
        .preflight(&request(fleet.path().to_str().unwrap()), &cancel)
        .await;
    canceller.await.unwrap();
    assert!(answer.is_err(), "cancelled, not answered");
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// A paste is a no to a drawn question, and the machine's name pasted into a typed
/// confirmation is not typing it: it cancels (the typed name is a deliberate act, and
/// bracketed paste is how the terminal says "this was not typed").
#[tokio::test(start_paused = true)]
async fn a_paste_answers_the_checks_dialogs_no_and_never_yes() {
    let shell = settled_demo().await;
    let mut ui = Ui::default();
    let km = Keymap::defaults();
    act_on(&shell, &mut ui, &km);
    // The typed confirmation: pasting the exact name, newline and all, deploys nothing.
    type_keys_seen(&shell, &mut ui, &km, &format!("/{PUBLIC_MACHINE}\nu"));
    until_asked(&shell, &mut ui, &km).await;
    assert_eq!(
        shell.view().confirm.map(|c| c.kind),
        Some(ConfirmKind::UpdateTyped)
    );
    common::paste(&shell, &mut ui, &km, &format!("{PUBLIC_MACHINE}\n"));
    assert!(shell.view().action.is_none());
    assert!(shell.view().confirm.is_none(), "answered no");
    // A warning dialog: a pasted `y` is a no.
    type_keys_seen(&shell, &mut ui, &km, "/luna\nu");
    until_asked(&shell, &mut ui, &km).await;
    common::paste(&shell, &mut ui, &km, "y");
    assert!(shell.view().action.is_none());
    assert!(shell.view().confirm.is_none());
}
