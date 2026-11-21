//! What a panic does, by where it happens (AUS-46).
//!
//! - A panic in a **task** ends the run **in order**, as a signal does: the roll stops, the drained
//!   node is uncordoned once, the loop (not the hook) gives the terminal back, and the exit says a
//!   panic in a task ended the run, with its message, and is not a success.
//! - A panic on the **loop thread** gives the terminal back from the hook and runs the cordon last
//!   resort; while `s` has the terminal it runs the last resort and writes nothing to the terminal.
//! - A task that panicked **answers** its request, as a failure, so the core is not left waiting.
//!
//! The terminal is a test double ([`auspex_tui::panics::Terminal`]): there is no TTY under
//! `cargo test`, and what is asserted is which calls were made, and by whom. The panic hook is
//! process-global, so every test that installs it holds [`SERIAL`] and puts the old one back.
//!
//! Nothing here runs the real `clan`, `nix` or `kubectl`, ssh, or touches a real cluster.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use auspex_core::{ConfirmKind, Event, RollState, RollStatus, ViewModel};
use auspex_domain::roll::RollRule;
use auspex_domain::status::Badge;
use auspex_tui::app::{Leaving, wind_down};
use auspex_tui::backend::{Backend, Real};
use auspex_tui::demo::{self, Demo};
use auspex_tui::log::Log;
use auspex_tui::panics::{Hooks, Terminal};
use auspex_tui::shell::Shell;
use common::{Tools, tempdir, tools};

/// Tests that install the process-global panic hook run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The terminal's calls, counted.
#[derive(Default)]
struct Calls {
    restore: AtomicUsize,
    paste_off: AtomicUsize,
}

impl Calls {
    fn counts(&self) -> (usize, usize) {
        (
            self.restore.load(Ordering::SeqCst),
            self.paste_off.load(Ordering::SeqCst),
        )
    }
}

impl Terminal for Calls {
    fn restore(&self) {
        self.restore.fetch_add(1, Ordering::SeqCst);
    }

    fn paste_off(&self) {
        self.paste_off.fetch_add(1, Ordering::SeqCst);
    }
}

/// The hook installed on this thread (the loop's), over a terminal that only counts; the old
/// hook is put back when this goes.
struct Installed {
    hooks: Arc<Hooks>,
    calls: Arc<Calls>,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for Installed {
    fn drop(&mut self) {
        // Not while a failed assertion is unwinding: `set_hook` refuses then, and the abort would hide it.
        if !std::thread::panicking() {
            self.hooks.uninstall();
        }
    }
}

async fn install(shell: &Arc<Shell>) -> Installed {
    let serial = SERIAL.lock().await;
    let calls = Arc::new(Calls::default());
    let hooks = Hooks::install(
        std::thread::current().id(),
        shell.cordons(),
        Arc::<Calls>::clone(&calls),
    );
    Installed {
        hooks,
        calls,
        _serial: serial,
    }
}

// ---------------------------------------------------------------------------------------
// A world with a drain in progress (a fake `kubectl`), as `tests/roll.rs` has one.
// ---------------------------------------------------------------------------------------

struct World {
    dir: tempfile::TempDir,
    shell: Arc<Shell>,
}

impl World {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// What `kubectl` was called with, one call per line, without the lease reads.
    fn kubectl_calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("kubectl.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.contains(" get leases "))
            .map(str::to_owned)
            .collect()
    }

    fn uncordons_of_luna(&self) -> usize {
        self.kubectl_calls()
            .iter()
            .filter(|c| c.ends_with("uncordon luna"))
            .count()
    }

    /// An update was started for `machine` (the free machine's, cypramundi, is not the roll's).
    fn clan_started_for(&self, machine: &str) -> bool {
        fs::read_to_string(self.path("clan.log"))
            .unwrap_or_default()
            .lines()
            .any(|l| l == format!("start {machine}"))
    }
}

const NODES_READY: &str = r#"{"items":[
  {"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
  {"metadata":{"name":"mars"},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
  {"metadata":{"name":"sol"},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
  {"metadata":{"name":"terra"},"status":{"conditions":[{"type":"Ready","status":"True"}]}}
]}"#;

const VIEW: &str = r#"{"current-context":"rke2","contexts":[{"name":"rke2","context":{"cluster":"c1"}}],"clusters":[{"name":"c1","cluster":{"server":"https://10.0.0.1:6443"}}]}"#;

fn cat(json: &str) -> String {
    format!("printf '%s\\n' '{json}'")
}

/// Every kubelet renews at every read (the times are the kubelets' own: a fixed day plus a second
/// per read).
fn leases_alive() -> String {
    use std::fmt::Write as _;
    let nodes = ["luna", "mars", "sol", "terra"];
    let mut items = Vec::new();
    let mut args = Vec::new();
    let mut backs = String::new();
    for (i, node) in nodes.iter().enumerate() {
        let _ = write!(backs, "b{i}=0; ");
        items.push(format!(
            r#"{{"metadata":{{"name":"{node}"}},"spec":{{"renewTime":"%s"}}}}"#
        ));
        args.push(format!(
            r#""$(printf '2030-01-01T%02d:%02d:%02dZ' $(((n-b{i})/3600)) $(((n-b{i})/60%60)) $(((n-b{i})%60)))""#
        ));
    }
    format!(
        "f='@dir@/lease.reads'; r=$(@cat@ \"$f\" 2>/dev/null || echo 0); r=$((r+1)); \
         echo $r > \"$f\"; n=$((r+36000)); {backs}\
         printf '{{\"items\":[{}]}}\\n' {}",
        items.join(","),
        args.join(" ")
    )
}

/// A cluster whose **drain never ends** (until it is killed) and whose uncordon works.
fn kubectl_draining() -> String {
    format!(
        "printf '%s\\n' \"$*\" >> '@dir@/kubectl.log'\n\
         case \" $* \" in\n\
         *\" config view \"*) {view}\n;;\n\
         *\" get leases \"*) {leases}\n;;\n\
         *\" get nodes \"*) {nodes}\n;;\n\
         *\" drain \"*) echo 'evicting (fake)'; @sleep@ 30\n;;\n\
         *\" uncordon \"*) echo 'node uncordoned (fake)'\n;;\n\
         *) echo 'ok (fake)';;\n\
         esac\n",
        view = cat(VIEW),
        leases = leases_alive(),
        nodes = cat(NODES_READY),
    )
}

fn world(t: &Tools) -> World {
    world_with(t, false)
}

/// [`world`], where the call that drains a node panics when `panic_in_drain`.
fn world_with(t: &Tools, panic_in_drain: bool) -> World {
    let dir = tempdir();
    let clan = t.script(
        dir.path(),
        "clan",
        "for a in \"$@\"; do m=$a; done\nprintf 'start %s\\n' \"$m\" >> '@dir@/clan.log'\n@sleep@ 1",
    );
    let nix = t.script(dir.path(), "nix", "exit 0");
    let kubectl = t.script(dir.path(), "kubectl", &kubectl_draining());
    let real = Real::for_actions(nix.into_os_string(), Some(clan))
        .with_kubectl(kubectl.into_os_string(), dir.path().join("kubeconfig"));
    let shell = Shell::new(
        Backend::Demo(Demo {
            real_actions: Some(Box::new(real)),
            panic_in_drain,
            ..Demo::default()
        }),
        Log::memory(),
        Duration::from_secs(120),
    );
    let mut config = demo::config();
    dir.path().to_str().unwrap().clone_into(&mut config.flake);
    config.public_machines.clear();
    config.rolling.rules = [(
        "k8s-server".to_owned(),
        RollRule {
            kubernetes: true,
            drain: true,
            drain_timeout_seconds: 60,
            etcd: false,
        },
    )]
    .into();
    config.rolling.parallel = 2;
    config.rolling.ready_timeout_seconds = 20;
    shell.send(Event::Start(config));
    World { dir, shell }
}

async fn until(shell: &Arc<Shell>, what: &str, ready: impl Fn(&ViewModel) -> bool) {
    let wait = async {
        while !ready(&shell.view()) {
            tokio::select! {
                () = shell.changed() => {}
                () = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    };
    assert!(
        tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .is_ok(),
        "timed out waiting for {what}"
    );
}

/// luna is being drained: cordoned by auspex, the drain running, its update not started.
async fn draining_luna(w: &World) {
    until(&w.shell, "the fleet to settle", |v| {
        v.refresh.settled
            && ["luna", "sol", "cypramundi"].iter().all(|n| {
                v.report
                    .machines
                    .iter()
                    .any(|m| m.name == *n && m.pending_system.is_some())
            })
    })
    .await;
    w.shell.send(Event::ToggleAct);
    w.shell.send(Event::ConfirmAnswer(true));
    w.shell.send(Event::UpdateAll);
    until(&w.shell, "the plan to be asked about", |v| {
        v.confirm
            .as_ref()
            .is_some_and(|c| c.kind == ConfirmKind::UpdateAll)
    })
    .await;
    w.shell.send(Event::ConfirmAnswer(true));
    until(&w.shell, "luna to be draining", |v| {
        v.roll.as_ref().is_some_and(|r| {
            r.machines
                .iter()
                .any(|m| m.name == "luna" && m.state == RollState::Draining)
        })
    })
    .await;
    assert_eq!(w.shell.cordons().held(), ["luna"], "luna is registered");
}

const fn leaving(ended_by_panic: bool) -> Leaving {
    Leaving {
        code: 0,
        ended_by_panic,
        terminal_left: false,
        failure: None,
    }
}

// ---------------------------------------------------------------------------------------
// A panic in a task
// ---------------------------------------------------------------------------------------

/// A task panics while luna is being drained. The hook does not touch the terminal (the TUI goes on
/// drawing until the loop leaves), the loop is told, and the way out is the signal's: the drain is
/// cancelled, luna is uncordoned **once**, no update started, the roll stops, the terminal is
/// restored once, by the loop, and the exit names the panic, sanitised and bounded, and fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_panic_during_a_drain_ends_the_run_in_order() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world(&t);
    draining_luna(&w).await;
    let installed = install(&w.shell).await;

    // An escape sequence and a long message: both must be tamed in what the exit says.
    let long = "x".repeat(2000);
    let task = tokio::spawn(async move { panic!("boom \x1b[31mred\x1b[0m {long}") });
    let message = tokio::time::timeout(Duration::from_secs(10), installed.hooks.task_panicked())
        .await
        .expect("the loop is told of the panic");
    assert!(task.await.unwrap_err().is_panic());
    assert_eq!(
        installed.calls.counts(),
        (0, 0),
        "the hook left the terminal alone: the loop that is drawing on it is still running"
    );
    assert!(
        w.shell.cordons().held() == ["luna"] && w.uncordons_of_luna() == 0,
        "and uncordoned nothing by itself: the roll is still going"
    );

    let (code, lines) = wind_down(
        &w.shell,
        &installed.hooks,
        leaving(true),
        std::future::pending(),
    )
    .await;
    assert_ne!(code, 0, "{lines:?}");
    let said = lines.join("\n");
    assert!(
        said.contains("a panic in a task ended the run: boom"),
        "{said}"
    );
    assert!(!said.contains('\x1b'), "sanitised: {said:?}");
    assert!(
        message.chars().count() <= 241 && message.ends_with('…'),
        "bounded: {message}"
    );
    assert_eq!(
        installed.calls.counts(),
        (1, 1),
        "the terminal was given back once, by the loop"
    );

    assert_eq!(
        w.uncordons_of_luna(),
        1,
        "luna is given back once: {:#?}",
        w.kubectl_calls()
    );
    assert_eq!(w.shell.cordons().held(), Vec::<String>::new());
    assert!(
        !w.clan_started_for("luna") && !w.clan_started_for("sol"),
        "no update of the roll's machines was started"
    );
    let view = w.shell.view();
    let roll = view.roll.expect("there was a roll");
    assert_eq!(
        roll.machines
            .iter()
            .find(|m| m.name == "luna")
            .unwrap()
            .state,
        RollState::Cancelled
    );
    assert!(
        roll.machines
            .iter()
            .filter(|m| m.name != "luna")
            .all(|m| !matches!(m.state, RollState::Draining | RollState::Updating)),
        "nothing else started"
    );
    assert_eq!(
        w.shell.exit_report(),
        Vec::<String>::new(),
        "nothing is left cordoned"
    );
}

/// A task that panicked while a `q` or a signal was already ending the run is still said, and the
/// exit is not a success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_panic_while_a_quit_is_ending_the_run_is_still_said_and_fails() {
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    let installed = install(&shell).await;
    let _ = tokio::spawn(async { panic!("late") }).await;
    let (code, lines) = wind_down(
        &shell,
        &installed.hooks,
        leaving(false),
        std::future::pending(),
    )
    .await;
    assert_ne!(code, 0);
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("a task panicked during the run: late")),
        "{lines:?}"
    );
    drop(installed);
}

// ---------------------------------------------------------------------------------------
// A panic on the loop thread
// ---------------------------------------------------------------------------------------

/// A panic on the thread that runs the loop ends auspex: the hook gives the terminal back and runs
/// the last resort, so luna is uncordoned and the exit has nothing to add.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loop_thread_panic_restores_the_terminal_and_runs_the_last_resort() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world(&t);
    draining_luna(&w).await;
    let installed = install(&w.shell).await;

    let result = std::panic::catch_unwind(|| panic!("the loop itself"));
    assert!(result.is_err());

    assert_eq!(
        installed.calls.counts(),
        (1, 1),
        "the hook restored the terminal: it is the loop that panicked"
    );
    assert_eq!(
        w.uncordons_of_luna(),
        1,
        "the last resort gave luna back: {:#?}",
        w.kubectl_calls()
    );
    assert_eq!(w.shell.cordons().held(), Vec::<String>::new());
    assert_eq!(
        installed.hooks.task_panic(),
        None,
        "it is not a panic in a task"
    );
    // The drain's `kubectl` is still running (the process is not really ending): stop it.
    w.shell.terminate().await;
}

/// While `s` has the terminal, a loop-thread panic still runs the last resort (it used to be set
/// aside with the rest of the hooks) and writes nothing to the terminal; so does one in a task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_while_ssh_has_the_terminal_uncordons_and_writes_nothing() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world(&t);
    draining_luna(&w).await;
    let installed = install(&w.shell).await;
    installed.hooks.lend();

    let _ = tokio::spawn(async { panic!("in a task, while ssh runs") }).await;
    let _ = std::panic::catch_unwind(|| panic!("on the loop, while ssh runs"));

    assert_eq!(
        installed.calls.counts(),
        (0, 0),
        "no escape sequence reaches ssh's session"
    );
    assert_eq!(w.uncordons_of_luna(), 1, "{:#?}", w.kubectl_calls());
    assert!(
        installed
            .hooks
            .task_panic()
            .is_some_and(|m| m.contains("in a task, while ssh runs")),
        "the task's panic is recorded for when ssh ends"
    );

    installed.hooks.take_back();
    let _ = std::panic::catch_unwind(|| panic!("after ssh"));
    assert_eq!(
        installed.calls.counts(),
        (1, 1),
        "taken back: a loop panic restores again"
    );
    w.shell.terminate().await;
}

// ---------------------------------------------------------------------------------------
// The lost answer
// ---------------------------------------------------------------------------------------

/// The probe of luna panics. Its request is answered as a failure (`internal error`), at once, and
/// not left to the global deadline: the row says so and the scan is not "timed out".
#[tokio::test(start_paused = true)]
async fn a_panicked_effect_task_answers_its_request_as_a_failure() {
    let _serial = SERIAL.lock().await;
    let shell = Shell::new(
        Backend::Demo(Demo {
            panic_in_probe_of: Some("luna".to_owned()),
            ..Demo::default()
        }),
        Log::memory(),
        Duration::from_secs(120),
    );
    let start = tokio::time::Instant::now();
    shell.send(Event::Start(demo::config()));
    while !shell.view().refresh.settled {
        shell.changed().await;
    }
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "not left to the deadline: {:?}",
        start.elapsed()
    );
    let view = shell.view();
    assert!(!view.refresh.timed_out);
    let luna = view
        .report
        .machines
        .iter()
        .find(|m| m.name == "luna")
        .unwrap();
    assert_eq!(luna.status, Badge::Unreachable);
    assert!(
        luna.reason
            .as_deref()
            .is_some_and(|r| r.contains("internal error")),
        "{:?}",
        luna.reason
    );
    // The others were not touched.
    let terra = view
        .report
        .machines
        .iter()
        .find(|m| m.name == "terra")
        .unwrap();
    assert_eq!(terra.status, Badge::InSync);
    assert_eq!(shell.in_flight(), 0, "no token is left");
}

/// The call that drains luna panics: its node is registered as cordoned, and nothing but its
/// answer lets the core give it back. The task answers its request as a failure, so the core ends
/// luna as failed, uncordons the node **once** and stops the group (sol is never touched); the
/// request is not left waiting for a quit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicked_drain_is_answered_so_the_core_uncordons_the_node() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let w = world_with(&t, true);
    until(&w.shell, "the fleet to settle", |v| {
        v.refresh.settled
            && ["luna", "sol", "cypramundi"].iter().all(|n| {
                v.report
                    .machines
                    .iter()
                    .any(|m| m.name == *n && m.pending_system.is_some())
            })
    })
    .await;
    w.shell.send(Event::ToggleAct);
    w.shell.send(Event::ConfirmAnswer(true));
    w.shell.send(Event::UpdateAll);
    until(&w.shell, "the plan to be asked about", |v| {
        v.confirm
            .as_ref()
            .is_some_and(|c| c.kind == ConfirmKind::UpdateAll)
    })
    .await;
    w.shell.send(Event::ConfirmAnswer(true));
    // Without a quit, a signal or anything else: the answer alone ends the roll.
    until(&w.shell, "the roll to end", |v| {
        v.roll
            .as_ref()
            .is_some_and(|r| r.status == RollStatus::Ended)
    })
    .await;
    let roll = w.shell.view().roll.expect("there was a roll");
    let luna = roll.machines.iter().find(|m| m.name == "luna").unwrap();
    assert!(
        matches!(&luna.state, RollState::Failed(why) if why.contains("internal error")),
        "{:?}",
        luna.state
    );
    assert_eq!(
        w.uncordons_of_luna(),
        1,
        "luna was given back: {:#?}",
        w.kubectl_calls()
    );
    assert_eq!(w.shell.cordons().held(), Vec::<String>::new());
    assert!(!w.clan_started_for("luna") && !w.clan_started_for("sol"));
    assert!(
        w.kubectl_calls().iter().all(|c| !c.contains("drain sol")),
        "the group stopped: {:#?}",
        w.kubectl_calls()
    );
}
