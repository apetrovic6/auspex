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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use auspex_core::{ConfirmKind, Event, RollState, RollStatus, ViewModel};
use auspex_domain::roll::RollRule;
use auspex_domain::status::Badge;
use auspex_tui::app::{Leaving, wind_down};
use auspex_tui::backend::{Backend, Real};
use auspex_tui::demo::{self, Demo, Fault};
use auspex_tui::log::Log;
use auspex_tui::panics::{Hooks, Terminal};
use auspex_tui::shell::Shell;
use common::{PATIENT, Tools, tempdir, tools};

/// Tests that install the process-global panic hook run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The terminal's calls, counted.
#[derive(Default)]
struct Calls {
    restore: AtomicUsize,
    paste_off: AtomicUsize,
    /// What the hook said (the last resort's lines, a task panic that came first).
    said: Mutex<Vec<String>>,
}

impl Calls {
    fn said(&self) -> Vec<String> {
        self.said.lock().unwrap().clone()
    }

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

    fn say(&self, line: &str) {
        self.said.lock().unwrap().push(line.to_owned());
    }
}

/// The hook installed on this thread (the loop's), over a terminal that only counts; the old
/// hook is put back when this goes.
struct Installed {
    hooks: Arc<Hooks>,
    calls: Arc<Calls>,
    /// What the default hook was asked to print (the old hook still runs after: a failing
    /// assertion in these tests is shown as usual).
    printed: Arc<Mutex<Vec<String>>>,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for Installed {
    fn drop(&mut self) {
        self.hooks.uninstall();
    }
}

async fn install(shell: &Arc<Shell>) -> Installed {
    let serial = SERIAL.lock().await;
    let calls = Arc::new(Calls::default());
    let printed = Arc::new(Mutex::new(Vec::new()));
    let old: auspex_tui::panics::Previous = Arc::from(std::panic::take_hook());
    let record = Arc::clone(&printed);
    let hooks = Hooks::install_over(
        std::thread::current().id(),
        shell.cordons(),
        Arc::<Calls>::clone(&calls),
        Arc::new(move |info| {
            record.lock().unwrap().push(info.to_string());
            old(info);
        }),
    );
    // The loop-thread last resort has the product's 10 s for one `kubectl uncordon`: a fake that
    // a loaded machine does not run for 10 s leaves the node cordoned, and the test fails on the
    // machine (AUS-47). A hung uncordon still ends at this bound.
    hooks.set_last_resort_bound(PATIENT);
    Installed {
        hooks,
        calls,
        printed,
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

/// A cluster whose drain and uncordon run the given shell snippets.
fn kubectl_with(drain: &str, uncordon: &str) -> String {
    format!(
        "printf '%s\\n' \"$*\" >> '@dir@/kubectl.log'\n\
         case \" $* \" in\n\
         *\" config view \"*) {view}\n;;\n\
         *\" get leases \"*) {leases}\n;;\n\
         *\" get nodes \"*) {nodes}\n;;\n\
         *\" drain \"*) {drain}\n;;\n\
         *\" uncordon \"*) {uncordon}\n;;\n\
         *) echo 'ok (fake)';;\n\
         esac\n",
        view = cat(VIEW),
        leases = leases_alive(),
        nodes = cat(NODES_READY),
    )
}

/// What a world's fake cluster and fault injection do.
#[derive(Default)]
struct Opts {
    fault: Option<Fault>,
    /// The drain ends at once (it never ends, until it is killed, otherwise).
    quick_drain: bool,
    /// The drain ends after this many seconds (0: it does not).
    short_drain: u32,
    /// The uncordon takes this many seconds.
    slow_uncordon: u32,
}

fn world(t: &Tools) -> World {
    world_with(t, &Opts::default())
}

fn world_with(t: &Tools, opts: &Opts) -> World {
    let dir = tempdir();
    let clan = t.script(
        dir.path(),
        "clan",
        "for a in \"$@\"; do m=$a; done\nprintf 'start %s\\n' \"$m\" >> '@dir@/clan.log'\n@sleep@ 1",
    );
    let nix = t.script(dir.path(), "nix", "exit 0");
    let drain = if opts.short_drain > 0 {
        format!("@sleep@ {}; echo 'node drained (fake)'", opts.short_drain)
    } else if opts.quick_drain {
        "echo 'node drained (fake)'".to_owned()
    } else {
        "echo 'evicting (fake)'; @sleep@ 30".to_owned()
    };
    let uncordon = format!(
        "@sleep@ {}; echo 'node uncordoned (fake)'",
        opts.slow_uncordon
    );
    let kubectl = t.script(dir.path(), "kubectl", &kubectl_with(&drain, &uncordon));
    let real = Real::for_actions(nix.into_os_string(), Some(clan))
        .with_kubectl(kubectl.into_os_string(), dir.path().join("kubeconfig"));
    let shell = Shell::new(
        Backend::Demo(Demo {
            real_actions: Some(Box::new(real)),
            fault: opts.fault.clone(),
            ..Demo::default()
        }),
        Log::memory(),
        Duration::from_secs(120),
    );
    // The last resort a panic hook runs (and the one at the exit) has the product's 15 s for one
    // `kubectl uncordon`, which a fake that a loaded machine does not run for 15 s misses: the node
    // is then reported cordoned, and the test fails on the machine (AUS-47). A hung uncordon still
    // ends at this bound, and is said.
    shell.set_last_resort_bound(PATIENT);
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
    shell.set_loop_thread(std::thread::current().id());
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
        tokio::time::timeout(PATIENT, wait).await.is_ok(),
        "timed out waiting for {what}"
    );
}

/// `U` and `y` to the plan: the roll is started.
async fn start_roll(w: &World) {
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
}

/// The roll is started, and luna is (or comes to be) in a state `ready` accepts.
async fn draining_to(w: &World, ready: impl Fn(&RollState) -> bool) {
    start_roll(w).await;
    until(&w.shell, "luna to reach the state", |v| {
        v.roll.as_ref().is_some_and(|r| {
            r.machines
                .iter()
                .any(|m| m.name == "luna" && ready(&m.state))
        })
    })
    .await;
}

/// luna is being drained: cordoned by auspex, the drain running, its update not started.
async fn draining_luna(w: &World) {
    draining_to(w, |s| *s == RollState::Draining).await;
    // The core says "draining" a moment before the shell has registered the node (both happen on
    // the task that delivered the last answer): wait for the registration.
    for _ in 0..200 {
        if w.shell.cordons().held() == ["luna"] {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
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
    let message = tokio::time::timeout(PATIENT, installed.hooks.task_panicked())
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
    // Printed as text, by the default hook, on both threads while ssh has the terminal.
    let printed = installed.printed.lock().unwrap().join("\n");
    assert!(
        printed.contains("in a task, while ssh runs")
            && printed.contains("on the loop, while ssh runs"),
        "{printed}"
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
            fault: Some(Fault::ProbeOf("luna".to_owned())),
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
    let w = world_with(
        &t,
        &Opts {
            fault: Some(Fault::Drain),
            ..Opts::default()
        },
    );
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

// ---------------------------------------------------------------------------------------
// Fix round 1 (review of PR #41)
// ---------------------------------------------------------------------------------------

/// F1, the hook's half. A task panic inside the core poisons the model's lock, and the loop's next
/// look at the core is a panic on the loop thread; the task's own message must not be lost then.
/// The hook says it first. (And only the first of two task panics is kept: the second is most often
/// a consequence of it.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loop_thread_panic_after_a_task_panic_says_the_task_panic_first() {
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    let installed = install(&shell).await;
    let _ = tokio::spawn(async { panic!("the root cause") }).await;
    let _ = tokio::spawn(async { panic!("a consequence") }).await;
    let first = installed.hooks.task_panicked().await;
    assert!(first.contains("the root cause"), "{first}");
    assert!(
        installed
            .hooks
            .task_panic()
            .is_some_and(|m| m.contains("the root cause")),
        "the first one is kept"
    );

    let _ = std::panic::catch_unwind(|| panic!("Model RwLock was poisoned"));
    let said = installed.calls.said();
    assert!(
        said.iter()
            .any(|l| l.starts_with("a panic in a task came first: the root cause")),
        "{said:?}"
    );
    // The loop's own message went to the default hook, as before.
    assert!(
        installed
            .printed
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.contains("Model RwLock was poisoned")),
        "{:?}",
        installed.printed
    );
    assert_eq!(installed.calls.counts(), (1, 1));
}

/// F1, the shell's half. A panic inside a call into the core, on a worker (an adapter's answer
/// is delivered there), takes the core down. The loop does not look at the core again; the run ends
/// in order without it: the drained node is given back by auspex itself, the exit names the
/// panic and says the core could not stop the roll, and nothing waits out the 20 s bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_inside_the_core_in_a_task_still_ends_the_run_in_order() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world_with(
        &t,
        &Opts {
            short_drain: 2,
            ..Opts::default()
        },
    );
    // Installed first: it waits for the other tests that hold the process-wide hook, and the roll
    // must not have moved on by then.
    let installed = install(&w.shell).await;
    draining_luna(&w).await;

    // The next call into the core made by a task (here the free machine's update ending, or the
    // drain's answer) is the one that panics.
    w.shell.fault_next_core_call();
    let message = tokio::time::timeout(PATIENT, installed.hooks.task_panicked())
        .await
        .expect("the loop is told of the panic");
    assert!(message.contains("the core panicked"), "{message}");
    assert!(
        w.shell.core_failed(),
        "the core is marked failed, so that nothing looks at it again"
    );
    // What the loop does next must not panic: it reads the view.
    let _ = w.shell.view();
    assert_eq!(
        installed.calls.counts(),
        (0, 0),
        "the hook left the terminal"
    );

    let (code, lines) = wind_down(
        &w.shell,
        &installed.hooks,
        leaving(true),
        std::future::pending(),
    )
    .await;
    // The shell waits up to `TERMINATE_BOUND` (20 s) for a core that is still answering, and not
    // at all for one that has panicked: if it waited, it says so in the log when the bound ran
    // out. Asked of the log and not of the clock: how long the exit took is also how long a
    // loaded machine took to run the fake `kubectl` for the uncordon, and a bound of 15 s on that
    // failed it (AUS-47).
    assert!(
        !w.shell
            .log
            .lines()
            .iter()
            .any(|l| l.contains("stopping took longer than the bound")),
        "waited for a core that cannot answer: {:?}",
        w.shell.log.lines()
    );
    assert_ne!(code, 0);
    let said = lines.join("\n");
    assert!(
        said.contains("a panic in a task ended the run: ")
            && said.contains("the core panicked")
            && said.contains("could not stop the roll itself"),
        "{said}"
    );
    assert_eq!(
        w.uncordons_of_luna(),
        1,
        "auspex gave luna back itself: {:#?}",
        w.kubectl_calls()
    );
    assert_eq!(w.shell.cordons().held(), Vec::<String>::new());
    assert_eq!(installed.calls.counts(), (1, 1));
}

/// F2. A panic after `clan` was started: the machine may be half updated, so the node stays
/// cordoned and the exit names it with the command; it is never uncordoned. (As a refusal, the core
/// would give it back.) Also: the exit lines carry the report of nodes left cordoned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_after_clan_started_leaves_the_node_cordoned_and_named() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world_with(
        &t,
        &Opts {
            fault: Some(Fault::AfterClanStarted),
            quick_drain: true,
            ..Opts::default()
        },
    );
    let installed = {
        // Installed before the roll starts, as the loop's hook is.
        let i = install(&w.shell).await;
        draining_to(&w, |s| matches!(s, RollState::Failed(_))).await;
        i
    };
    let roll = w.shell.view().roll.expect("there was a roll");
    let luna = roll.machines.iter().find(|m| m.name == "luna").unwrap();
    assert!(
        matches!(&luna.state, RollState::Failed(why) if why.contains("internal error")),
        "failed, not refused: {:?}",
        luna.state
    );
    assert_eq!(
        w.shell.cordons().held(),
        ["luna"],
        "luna is on the record as left cordoned"
    );
    let message = tokio::time::timeout(PATIENT, installed.hooks.task_panicked())
        .await
        .expect("the panic was recorded");
    assert!(message.contains("after clan started"), "{message}");

    let (code, lines) = wind_down(
        &w.shell,
        &installed.hooks,
        leaving(true),
        std::future::pending(),
    )
    .await;
    assert_ne!(code, 0);
    assert_eq!(
        w.uncordons_of_luna(),
        0,
        "never uncordoned: {:#?}",
        w.kubectl_calls()
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("node luna was left cordoned") && l.contains("uncordon luna")),
        "named, with the command, in what the exit says: {lines:?}"
    );
    drop(installed);
}

/// F3 (A2). The terminal is the shell's already (ssh was stopped by a signal): the loop does not
/// restore it again, but bracketed paste is still switched off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_that_is_the_shells_already_is_not_restored_again() {
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    let installed = install(&shell).await;
    let (code, lines) = wind_down(
        &shell,
        &installed.hooks,
        Leaving {
            code: 143,
            ended_by_panic: false,
            terminal_left: true,
            failure: Some("cannot take the terminal back from ssh: x".to_owned()),
        },
        std::future::pending(),
    )
    .await;
    assert_eq!(installed.calls.counts(), (0, 1));
    assert_eq!(code, 143, "a signal's code is kept");
    assert_eq!(lines, ["cannot take the terminal back from ssh: x"]);
}

/// F3 (A5). A panic that happens while the run is being wound down is said, and fails the exit:
/// the record is read after the wait, not before. The `interrupt` here is a task that panics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_panic_during_the_wind_down_is_said() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world_with(
        &t,
        &Opts {
            slow_uncordon: 2,
            ..Opts::default()
        },
    );
    draining_luna(&w).await;
    let installed = install(&w.shell).await;
    assert_eq!(installed.hooks.task_panic(), None, "nothing yet");
    let (code, lines) = wind_down(&w.shell, &installed.hooks, leaving(false), async {
        let _ = tokio::spawn(async { panic!("during the wind-down") }).await;
    })
    .await;
    assert_ne!(code, 0);
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("a task panicked during the run: during the wind-down")),
        "{lines:?}"
    );
    drop(installed);
}

/// F5. A task that panics once the loop has put the terminal back and read the record (a task
/// that outlived the reap grace, or one that dies while the runtime goes down) is printed as any
/// panic is, and `main` fails the exit. Before that, the hook prints nothing for a task panic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_panic_after_the_wind_down_is_printed_and_fails_the_exit() {
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    let installed = install(&shell).await;
    let _ = tokio::spawn(async { panic!("while the loop runs") }).await;
    assert!(
        installed.printed.lock().unwrap().is_empty() && !auspex_tui::panics::late_task_panic(),
        "a task panic is not printed over the TUI, and is not late"
    );
    let (code, _) = wind_down(
        &shell,
        &installed.hooks,
        leaving(true),
        std::future::pending(),
    )
    .await;
    assert_ne!(code, 0);

    let _ = tokio::spawn(async { panic!("after the exit report") }).await;
    assert!(
        auspex_tui::panics::late_task_panic(),
        "main is told to fail the exit"
    );
    assert!(
        installed
            .printed
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.contains("after the exit report")),
        "and the panic is printed: {:?}",
        installed.printed
    );
    assert_eq!(
        installed.calls.counts(),
        (1, 1),
        "the hook did not restore the terminal again"
    );
}

// ---------------------------------------------------------------------------------------
// Fix round 2 (re-review of PR #41)
// ---------------------------------------------------------------------------------------

/// R2a. A panic inside a **real** `update` (the core's `fault-injection` switch) poisons crux's model
/// lock, as a bug in the core reached from a task would. Nothing looks at the core again: `view()`
/// and `send` return at once on the failed core. Without `core_call`'s early return, `view()` here
/// panics on the poisoned lock, on the loop's thread. (`Shell::fault_next_core_call` panics before
/// the lock is taken, so it cannot show this.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_in_a_real_update_poisons_the_lock_and_the_failed_core_is_never_called_again() {
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    shell.set_loop_thread(std::thread::current().id());
    let installed = install(&shell).await;
    // The loop's own updates are spared; the first answer a task delivers is not.
    let mut config = demo::config();
    config.flake = "poison-the-core-of-this-test".to_owned();
    auspex_core::fault::arm_except_this_thread(&config.flake);
    shell.send(Event::Start(config));
    let message = tokio::time::timeout(PATIENT, installed.hooks.task_panicked())
        .await
        .expect("the loop is told of the panic");
    assert!(message.contains("the core's update panics"), "{message}");
    assert!(shell.core_failed());
    // Let the unwind finish: the lock is poisoned for good from here.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // What the loop does on every turn and in `terminate_with`: none of it may touch crux.
    let _ = shell.view();
    shell.send(Event::Refresh);
    assert_eq!(
        installed.calls.counts(),
        (0, 0),
        "no loop-thread panic: the terminal was left alone"
    );
    let (code, lines) = wind_down(
        &shell,
        &installed.hooks,
        leaving(true),
        std::future::pending(),
    )
    .await;
    assert_ne!(code, 0);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("took the core down") || l.contains("the core's update panics")),
        "{lines:?}"
    );
    assert_eq!(installed.calls.counts(), (1, 1));
}

/// RR3. A panic on the loop thread **while the run is being wound down** (here in the `interrupt`
/// future, which `terminate_with` polls on the loop) still gives the terminal back: the hook is not
/// told the loop is done until the wait is over. With `finish()` before the wait it would skip the
/// restore and leave raw mode and the alternate screen on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loop_thread_panic_during_the_wind_down_still_restores_the_terminal() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let w = world_with(
        &t,
        &Opts {
            slow_uncordon: 2,
            ..Opts::default()
        },
    );
    let installed = install(&w.shell).await;
    draining_luna(&w).await;
    let (_, _) = wind_down(&w.shell, &installed.hooks, leaving(false), async {
        let _ = std::panic::catch_unwind(|| panic!("in the interrupt future"));
    })
    .await;
    assert_eq!(
        installed.calls.counts(),
        (2, 2),
        "the hook restored (1, 1) during the wait, and the loop restored again on the way out"
    );
    assert!(
        installed
            .calls
            .said()
            .iter()
            .all(|l| !l.contains("a panic in a task")),
        "a loop-thread panic is not a task panic"
    );
    drop(installed);
}

/// R2c. A panic while reading the freeze file for the last look before an update refuses the
/// update (a freeze holds, and a file that cannot be read means the machine may be frozen). Mapped
/// to "go ahead", `clan` would be started here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_in_the_freeze_gate_refuses_the_update() {
    let Some(t) = tools(&["sleep", "cat", "date"]) else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let w = world_with(
        &t,
        &Opts {
            quick_drain: true,
            fault: Some(Fault::FreezeGate),
            ..Opts::default()
        },
    );
    start_roll(&w).await;
    until(&w.shell, "the roll to end", |v| {
        v.roll
            .as_ref()
            .is_some_and(|r| r.status == RollStatus::Ended)
    })
    .await;
    let roll = w.shell.view().roll.expect("there was a roll");
    for name in ["luna", "cypramundi"] {
        let m = roll.machines.iter().find(|m| m.name == name).unwrap();
        assert!(
            matches!(&m.state, RollState::Refused(why) if why.contains("internal error")),
            "{name}: {:?}",
            m.state
        );
    }
    for name in ["luna", "sol", "cypramundi"] {
        assert!(
            !w.clan_started_for(name),
            "clan was started for {name} although the freeze file could not be read"
        );
    }
    assert_eq!(w.shell.cordons().held(), Vec::<String>::new());
}
