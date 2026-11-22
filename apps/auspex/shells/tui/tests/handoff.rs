//! `s`: the terminal lent to ssh and taken back, with the built binary on a pty and a fake
//! `ssh`.
//!
//! Each test is one rule from the brief, and fails when the rule is broken (the report lists
//! how each was checked):
//!
//! - the argv ssh gets is the domain's interactive one, and ssh has a tty, the operator's
//!   environment (askpass included) and the terminal's own signals;
//! - the terminal is the operator's while ssh runs (cooked, main screen, cursor shown, paste
//!   off) and auspex's again after, whatever ssh did (exit 0, exit 7, killed);
//! - keys, Ctrl-C and Ctrl-Z go to ssh, never to auspex;
//! - `SIGTERM` and `SIGHUP` end ssh and then auspex, with the terminal left as the operator's;
//! - refusals: observe mode, a machine with no usable target, the demo.
//!
//! Real processes: the binary, `setsid` (a controlling terminal for it, see `common/pty.rs`),
//! `sh` and `sleep`. Nothing is ever ssh'd to, and no `nix`, `clan` or fleet is real. Skipped
//! with a message when `setsid` is not on `PATH`; inside the Nix build, where a skip would be a
//! false green, that is a failure (`tools`).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::pty::{
    CURSOR_HIDE, CURSOR_SHOW, ENTER_ALT, LEAVE_ALT, PASTE_OFF, PASTE_ON, Pty, count, spawn, state,
};
use common::{Tools, alive, auspex, tempdir, tools, wait_for_pids};
use rustix::process::Signal;

const INVENTORY: &str = r#"{"terra":{"deploy":{"targetHost":"root@192.0.2.10"}},"luna":{"deploy":{"targetHost":"root@192.0.2.11"}}}"#;

fn store(name: &str) -> String {
    format!("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-{name}")
}

fn eval_ok(t: &Tools, dir: &Path) -> PathBuf {
    let line = |m: &str| {
        format!(
            r#"{{"attr":"{m}","attrPath":["{m}"],"drvPath":"{}.drv","outputs":{{"out":"{}"}}}}"#,
            store(&format!("nixos-system-{m}")),
            store(&format!("nixos-system-{m}"))
        )
    };
    t.script(
        dir,
        "nix-eval-jobs",
        &format!("printf '%s\\n' '{}' '{}'", line("terra"), line("luna")),
    )
}

/// An ssh that answers a probe (`BatchMode`, a script on stdin) as a healthy machine, and runs
/// `session` when it is not a probe: the operator's login. Every login records its argv, its
/// pid, and whether it has a tty and which askpass variables it was given.
fn ssh(t: &Tools, dir: &Path, session: &str) -> PathBuf {
    t.script(
        dir,
        "ssh",
        &format!(
            r#"case "$*" in
  *BatchMode=yes*)
    cat > /dev/null
    printf 'auspex_probe=1\nprobe=nixos\ncurrent_system={}\nsystem_state=running\nfailed_units=[]\nauspex_end=1\n'
    exit 0;;
esac
for a; do printf '%s\n' "$a"; done > '@dir@/ssh.argv'
echo $$ > '@dir@/ssh.pid'
if [ -t 0 ] && [ -t 1 ] && [ -t 2 ]; then echo tty > '@dir@/ssh.tty'; else echo notty > '@dir@/ssh.tty'; fi
printf '%s|%s\n' "${{SSH_ASKPASS-unset}}" "${{SSH_ASKPASS_REQUIRE-unset}}" > '@dir@/ssh.env'
{session}"#,
            store("nixos-system-terra"),
        ),
    )
}

struct Run {
    dir: tempfile::TempDir,
    pty: Pty,
}

impl Run {
    /// The binary on a pty with `ssh_session` as what a login runs, and the TUI up, luna selected.
    fn start(extra_config: &str, inventory: &str, ssh_session: &str) -> Option<Self> {
        Self::start_as(extra_config, inventory, ssh_session, false)
    }

    /// As [`Run::start`], and with `job_control`, under a shell with job control (`set -m`) as
    /// an interactive shell would run it: auspex gets its own process group, and its parent is in
    /// the same session, so the group is not *orphaned*. That matters for exactly one signal:
    /// the kernel discards the stop of `SIGTSTP` for a process in an orphaned group, which is
    /// what every other test here has, so a test of Ctrl-Z without this could not fail.
    fn start_as(
        extra_config: &str,
        inventory: &str,
        ssh_session: &str,
        job_control: bool,
    ) -> Option<Self> {
        let t = tools(&["setsid", "sleep", "stty"])?;
        let dir = tempdir();
        let nix = t.script(dir.path(), "nix", &format!("printf '%s\\n' '{inventory}'"));
        let eval = eval_ok(&t, dir.path());
        let ssh = ssh(&t, dir.path(), ssh_session);
        let config = dir.path().join("auspex.toml");
        // The product's own deadlines (a 10 s probe, a 5 s ssh connect, a 120 s refresh) are right for
        // an operator, and a machine that does not run the fake `ssh` for ten seconds turns the row
        // "unreachable" and the test's `s` into a refusal, which no wait here can undo (AUS-47).
        // None of the tests here is about a deadline, so every one is long.
        let patient = "[timeouts]\nssh_connect = 60\nssh_approval = 600\nprobe = 600\neval = 600\nsettle = 900\ndiff = 600\n\n";
        std::fs::write(&config, format!("{patient}{extra_config}")).unwrap();
        let fleet = dir.path().join("fleet");
        std::fs::create_dir_all(&fleet).unwrap();
        let mut cmd = auspex(dir.path());
        cmd.arg("--config")
            .arg(&config)
            .arg("--flake")
            .arg(&fleet)
            .env("AUSPEX_NIX", &nix)
            .env("AUSPEX_NIX_EVAL_JOBS", &eval)
            .env("AUSPEX_SSH", &ssh)
            // The operator's own askpass: ssh must be given it, where a probe has it removed.
            .env("SSH_ASKPASS", "/operators/askpass")
            .env("TERM", "xterm-256color");
        if job_control {
            let mut wrapped = Command::new(t.get("sh"));
            wrapped
                .arg("-c")
                .arg("set -m\n\"$0\" \"$@\"")
                .arg(cmd.get_program())
                .args(cmd.get_args());
            for (k, v) in cmd.get_envs() {
                match v {
                    Some(v) => wrapped.env(k, v),
                    None => wrapped.env_remove(k),
                };
            }
            cmd = wrapped;
        }
        let pty = spawn(&cmd, t.get("setsid"));
        pty.wait_for_text("terra");
        pty.wait_for_text("luna");
        Some(Self { dir, pty })
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.file(name)).unwrap_or_default()
    }

    /// Waits for the fake login to say it is running.
    fn wait_for_login(&self) {
        wait_for_pids(&self.file("ssh.pid"), 1);
        common::pty::Pty::wait_until(&self.pty, "ssh to write ssh.ready", |_| {
            self.file("ssh.ready").exists()
        });
    }

    /// `A` and `y`: act mode on, answered once the question has been on screen long enough to
    /// be answered (the dwell is 300 ms).
    fn act(&mut self) {
        self.pty.send(b"A");
        self.pty.wait_for_text("act mode");
        std::thread::sleep(Duration::from_millis(500));
        self.pty.send(b"y");
        self.pty.wait_for_text("ACT");
    }

    /// The terminal is the TUI's: raw, alternate screen, paste on, cursor hidden by a frame.
    fn assert_tui_has_the_terminal(&self, when: &str) {
        let raw = self.pty.raw();
        assert!(self.pty.raw_mode(), "{when}: the terminal is raw");
        assert_eq!(
            state(&raw, ENTER_ALT, LEAVE_ALT),
            Some(true),
            "{when}: alt screen"
        );
        assert_eq!(
            state(&raw, PASTE_ON, PASTE_OFF),
            Some(true),
            "{when}: paste on"
        );
        assert_eq!(
            state(&raw, CURSOR_HIDE, CURSOR_SHOW),
            Some(true),
            "{when}: cursor hidden"
        );
    }

    /// The terminal is the operator's: cooked, main screen, paste off, cursor shown.
    fn assert_operator_has_the_terminal(&self, when: &str) {
        let raw = self.pty.raw();
        assert!(self.pty.cooked(), "{when}: the terminal is cooked");
        assert_eq!(
            state(&raw, ENTER_ALT, LEAVE_ALT),
            Some(false),
            "{when}: main screen"
        );
        assert_eq!(
            state(&raw, PASTE_ON, PASTE_OFF),
            Some(false),
            "{when}: paste off"
        );
        assert_eq!(
            state(&raw, CURSOR_HIDE, CURSOR_SHOW),
            Some(false),
            "{when}: cursor shown"
        );
    }
}

/// A login that says it is ready, reads one line, records it and exits with `code`.
fn exits_with(code: i32) -> String {
    format!(
        "touch '@dir@/ssh.ready'\nread line\nprintf '%s\\n' \"$line\" > '@dir@/ssh.line'\nexit {code}"
    )
}

// ---------------------------------------------------------------------------------------
// What ssh is given
// ---------------------------------------------------------------------------------------

#[test]
fn ssh_gets_the_interactive_argv_a_tty_and_the_operators_environment() {
    let Some(mut run) = Run::start("", INVENTORY, &exits_with(0)) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.wait_for_login();
    let control = run.file("run/auspex");
    assert_eq!(
        run.read("ssh.argv"),
        format!(
            "-o\nControlMaster=no\n-o\nControlPath={}/%C\n--\nroot@192.0.2.11\n",
            control.display()
        ),
        "the domain's interactive argv for luna: no -T, no BatchMode, the probes' socket"
    );
    assert_eq!(
        run.read("ssh.tty"),
        "tty\n",
        "stdin, stdout and stderr are the terminal"
    );
    assert_eq!(
        run.read("ssh.env"),
        "/operators/askpass|unset\n",
        "the operator's askpass, not removed and not forbidden"
    );
    run.pty.send(b"exit\n");
    run.pty.wait_for_text("ssh to luna exited (code 0)");
}

// ---------------------------------------------------------------------------------------
// The terminal, there and back
// ---------------------------------------------------------------------------------------

/// One login, ended as `ends`, and the terminal after it.
fn terminal_is_restored_after(session: &str, says: &str) {
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    run.assert_tui_has_the_terminal("before ssh");
    run.pty.send(b"s");
    run.wait_for_login();
    run.assert_operator_has_the_terminal("while ssh runs");
    let enters = count(&run.pty.raw(), ENTER_ALT);
    run.pty.send(b"go\n");
    run.pty.wait_for_text(says);
    run.pty.wait_until("the alternate screen again", |raw| {
        count(raw, ENTER_ALT) > enters
    });
    // The screen is cleared explicitly: not every terminal clears the alternate screen on entry,
    // and a diff against the new, empty buffers does not write blank cells over what was there.
    let raw = run.pty.raw();
    let from = common::pty::last(&raw, ENTER_ALT).expect("the alternate screen was entered");
    assert!(
        count(&raw[from..], b"\x1b[2J") > 0,
        "the screen is cleared when the TUI comes back"
    );
    run.assert_tui_has_the_terminal("after ssh");
    // And it still works: a key moves the selection, and q leaves, restoring the terminal.
    run.pty.send(b"q");
    assert_eq!(run.pty.wait_exit().code(), Some(0));
    run.assert_operator_has_the_terminal("after auspex quits");
}

#[test]
fn the_terminal_is_restored_after_a_normal_exit() {
    terminal_is_restored_after(&exits_with(0), "ssh to luna exited (code 0)");
}

#[test]
fn the_terminal_is_restored_after_a_non_zero_exit() {
    terminal_is_restored_after(&exits_with(7), "ssh to luna exited (code 7)");
}

#[test]
fn the_terminal_is_restored_after_ssh_is_killed() {
    terminal_is_restored_after(
        "touch '@dir@/ssh.ready'\nread line\nkill -9 $$",
        "ssh to luna exited (killed by signal 9)",
    );
}

// ---------------------------------------------------------------------------------------
// Keys and signals go to ssh
// ---------------------------------------------------------------------------------------

#[test]
fn keys_typed_during_ssh_reach_ssh_and_not_auspex() {
    let Some(mut run) = Run::start("", INVENTORY, &exits_with(0)) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.wait_for_login();
    // `q` is auspex's quit key and `A` its act key: if auspex read these, it would be gone or
    // out of act mode. Typed as a line, because ssh is on a cooked terminal.
    run.pty.send(b"qAq\n");
    run.pty.wait_for_text("ssh to luna exited (code 0)");
    assert_eq!(run.read("ssh.line"), "qAq\n", "ssh read what was typed");
    assert!(
        run.pty.running(),
        "auspex was not quit by the q it was not meant to read"
    );
    assert!(
        run.pty.text().contains("ACT"),
        "and is still in act mode: it did not read the A"
    );
}

#[test]
fn ctrl_c_during_ssh_interrupts_ssh_and_not_auspex() {
    let session = "trap 'echo int > \"@dir@/ssh.int\"; exit 130' INT\ntouch '@dir@/ssh.ready'\n@sleep@ 30 &\nwait $!";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.wait_for_login();
    // The terminal turns the byte into SIGINT for its foreground group, ssh's and auspex's.
    run.pty.send(b"\x03");
    run.pty.wait_for_text("ssh to luna exited (code 130)");
    assert_eq!(run.read("ssh.int"), "int\n", "ssh got the SIGINT");
    assert!(
        run.pty.running(),
        "auspex did not quit on the SIGINT meant for ssh"
    );
    run.assert_tui_has_the_terminal("after the interrupted ssh");
}

#[test]
fn ctrl_z_during_ssh_reaches_ssh_and_does_not_stop_auspex() {
    // Under a shell with job control, where a SIGTSTP for auspex would stop it (see `start_as`).
    let session = "trap 'echo tstp > \"@dir@/ssh.tstp\"; exit 20' TSTP\ntouch '@dir@/ssh.ready'\n@sleep@ 30 &\nwait $!";
    let Some(mut run) = Run::start_as("", INVENTORY, session, true) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.wait_for_login();
    run.pty.send(b"\x1a");
    run.pty.wait_for_text("ssh to luna exited (code 20)");
    assert_eq!(run.read("ssh.tstp"), "tstp\n", "ssh got the SIGTSTP");
    assert!(run.pty.running(), "auspex was not stopped or ended by it");
    run.assert_tui_has_the_terminal("after ssh");
}

#[test]
fn a_stopped_ssh_is_continued_and_not_left_holding_the_terminal() {
    // ssh at a password prompt can stop itself on Ctrl-Z; auspex cannot bring it back, so it
    // continues it at once. Here ssh stops itself, and says so when it is running again.
    let session =
        "touch '@dir@/ssh.ready'\nkill -STOP $$\necho continued > '@dir@/ssh.continued'\nexit 5";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.pty.wait_for_text("ssh to luna exited (code 5)");
    assert_eq!(run.read("ssh.continued"), "continued\n");
    run.assert_tui_has_the_terminal("after the stop");
}

// ---------------------------------------------------------------------------------------
// auspex terminated while ssh runs
// ---------------------------------------------------------------------------------------

fn terminating_while_ssh_runs(signal: Signal, name: &str) {
    let session = "trap 'echo term > \"@dir@/ssh.term\"; exit 143' TERM\ntrap '' HUP\ntouch '@dir@/ssh.ready'\n@sleep@ 30 &\nwait $!\nwait $!";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.wait_for_login();
    let ssh_pid: i32 = run.read("ssh.pid").trim().parse().unwrap();
    let enters = count(&run.pty.raw(), ENTER_ALT);
    let leaves = count(&run.pty.raw(), LEAVE_ALT);
    run.pty.signal(signal);
    let status = run.pty.wait_exit();
    assert_eq!(
        status.code(),
        Some(128 + signal.as_raw()),
        "{name}: auspex exits with the signal's status"
    );
    assert!(!alive(ssh_pid), "{name}: ssh is gone");
    assert_eq!(
        run.read("ssh.term"),
        "term\n",
        "{name}: ssh was asked to end with SIGTERM"
    );
    // The terminal is the operator's, and auspex did not take it back for the exit.
    run.assert_operator_has_the_terminal(&format!("{name}: after auspex exits"));
    assert_eq!(
        count(&run.pty.raw(), ENTER_ALT),
        enters,
        "{name}: the alternate screen was not entered again on the way out"
    );
    assert_eq!(
        count(&run.pty.raw(), LEAVE_ALT),
        leaves,
        "{name}: the terminal was not 'restored' a second time on the way out"
    );
}

#[test]
fn sigterm_during_ssh_ends_ssh_restores_the_terminal_and_exits() {
    terminating_while_ssh_runs(Signal::TERM, "SIGTERM");
}

#[test]
fn sighup_during_ssh_ends_ssh_restores_the_terminal_and_exits() {
    terminating_while_ssh_runs(Signal::HUP, "SIGHUP");
}

#[test]
fn an_ssh_that_ignores_sigterm_is_killed() {
    let session = "trap '' TERM\ntouch '@dir@/ssh.ready'\nwhile :; do @sleep@ 1; done";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.wait_for_login();
    let ssh_pid: i32 = run.read("ssh.pid").trim().parse().unwrap();
    run.pty.signal(Signal::TERM);
    assert_eq!(run.pty.wait_exit().code(), Some(143));
    assert!(!alive(ssh_pid), "ssh did not outlive auspex");
}

// ---------------------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------------------

#[test]
fn s_in_observe_mode_says_so_and_runs_nothing() {
    let Some(mut run) = Run::start("", INVENTORY, &exits_with(0)) else {
        return;
    };
    run.pty.send(b"s");
    run.pty.wait_for_text("act mode is off (A)");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!run.file("ssh.pid").exists(), "ssh was not started");
    run.assert_tui_has_the_terminal("after the refusal");
}

#[test]
fn s_for_a_machine_with_no_usable_target_says_so_and_runs_nothing() {
    // The inventory's target is not one auspex will give ssh.
    let inventory = r#"{"luna":{"deploy":{"targetHost":"-oProxyCommand=sh"}},"terra":{"deploy":{"targetHost":"root@192.0.2.10"}}}"#;
    let Some(mut run) = Run::start("", inventory, &exits_with(0)) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.pty.wait_for_text("luna has no usable ssh target");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!run.file("ssh.pid").exists(), "ssh was not started");
    run.assert_tui_has_the_terminal("after the refusal");
}

#[test]
fn the_demo_has_nothing_to_ssh_to_and_runs_nothing() {
    let Some(t) = tools(&["setsid"]) else { return };
    let dir = tempdir();
    // An `ssh` first on PATH: if the demo ran ssh by name, this is what it would find.
    let marker = dir.path().join("ssh.ran");
    t.script(
        dir.path(),
        "ssh",
        &format!("touch '{}'\nexit 0", marker.display()),
    );
    let mut cmd = auspex(dir.path());
    cmd.arg("--demo")
        .env("AUSPEX_SSH", dir.path().join("ssh"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("TERM", "xterm-256color");
    let mut pty = spawn(&cmd, t.get("setsid"));
    pty.wait_for_text("luna");
    pty.send(b"A");
    pty.wait_for_text("act mode");
    std::thread::sleep(Duration::from_millis(500));
    pty.send(b"y");
    pty.wait_for_text("ACT");
    pty.send(b"s");
    pty.wait_for_text("did not start: this is the demo");
    assert!(!marker.exists(), "no ssh was run");
    assert!(pty.running());
    // The terminal never left the TUI.
    let raw = pty.raw();
    assert_eq!(state(&raw, ENTER_ALT, LEAVE_ALT), Some(true));
    assert!(pty.raw_mode());
}

// ---------------------------------------------------------------------------------------
// A race that is real
// ---------------------------------------------------------------------------------------

#[test]
fn a_sigint_that_reaches_auspex_as_ssh_ends_does_not_quit_it() {
    // Ctrl-C reaches auspex and ssh together. When ssh is gone first, auspex's copy used to be
    // read by the loop afterwards and quit it (one `s` in twenty, under load). The kernel's
    // delivery cannot be delayed from a test, so ssh's background child sends auspex the SIGINT
    // 30 ms after ssh has exited, which lands in the same place: after the wait is over and before
    // the loop reads signals again. Eight sessions in a row, so a drain that only sometimes works
    // is seen.
    let session = "code=$(( $(cat '@dir@/rounds' 2>/dev/null || echo 0) + 1 ))\nparent=$PPID\n( @sleep@ 0.03; kill -INT $parent; : > \"@dir@/killed-$code\" ) &\nexit $code";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    for round in 1..=8 {
        std::fs::write(run.file("rounds"), (round - 1).to_string()).unwrap();
        run.pty.send(b"s");
        run.pty
            .wait_for_text(&format!("ssh to luna exited (code {round})"));
        assert!(
            run.pty.running(),
            "round {round}: auspex quit on the SIGINT"
        );
        // The signal is sent by a background child of the fake ssh, 30 ms after it has exited, and
        // on a loaded machine that child runs when the scheduler gets to it: a round's signal sent
        // during the next round's session is not the race this test makes (AUS-47). So the next
        // round starts only once this round's signal has been sent.
        common::pty::Pty::wait_until(&run.pty, "the round's SIGINT to be sent", |_| {
            run.file(&format!("killed-{round}")).exists()
        });
        // Let any late signal be read, and then the next `s` is a fresh session.
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            run.pty.running(),
            "round {round}: auspex quit on the SIGINT, late"
        );
        run.assert_tui_has_the_terminal(&format!("after round {round}"));
    }
}

// ---------------------------------------------------------------------------------------
// What ssh leaves behind
// ---------------------------------------------------------------------------------------

/// After `q` the operator's shell has the terminal as it had it before auspex, whatever ssh did
/// to it on the way.
fn assert_terminal_is_the_operators_after_quit(run: &mut Run, when: &str) {
    run.pty.send(b"q");
    assert_eq!(run.pty.wait_exit().code(), Some(0), "{when}");
    run.assert_operator_has_the_terminal(when);
}

#[test]
fn a_killed_ssh_that_left_the_terminal_raw_does_not_leave_it_raw_after_auspex() {
    // A real ssh puts the tty in raw mode and restores it only on a clean exit. Killed, it
    // cannot; auspex must put back what was there before ssh, or crossterm takes ssh's raw mode
    // for the original and the operator's shell gets it back (no echo, no Ctrl-C) when auspex quits.
    let session = "stty raw -echo\ntouch '@dir@/ssh.ready'\nread line\nkill -9 $$";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.wait_for_login();
    run.pty.send(b"go\n");
    run.pty
        .wait_for_text("ssh to luna exited (killed by signal 9)");
    run.assert_tui_has_the_terminal("after the killed ssh");
    assert_terminal_is_the_operators_after_quit(&mut run, "after quit");
}

#[test]
fn an_ssh_that_exits_nonzero_without_restoring_the_terminal_does_not_leave_it_raw() {
    let session = "stty raw -echo\ntouch '@dir@/ssh.ready'\nread line\nexit 7";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.wait_for_login();
    run.pty.send(b"go\n");
    run.pty.wait_for_text("ssh to luna exited (code 7)");
    assert_terminal_is_the_operators_after_quit(&mut run, "after quit");
}

#[test]
fn sigterm_with_an_ssh_that_must_be_killed_and_left_the_terminal_raw_leaves_it_cooked() {
    // The path where auspex exits without taking the terminal back (`stopped_by`), with an ssh
    // that ignores SIGTERM, is killed after `KILL_AFTER`, and was in raw mode.
    let session =
        "trap '' TERM\nstty raw -echo\ntouch '@dir@/ssh.ready'\nwhile :; do @sleep@ 1; done";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.wait_for_login();
    let ssh_pid: i32 = run.read("ssh.pid").trim().parse().unwrap();
    run.pty.signal(Signal::TERM);
    assert_eq!(run.pty.wait_exit().code(), Some(143));
    assert!(!alive(ssh_pid));
    run.assert_operator_has_the_terminal("after auspex exits");
}

// ---------------------------------------------------------------------------------------
// Keys and signals, the ones not covered above
// ---------------------------------------------------------------------------------------

#[test]
fn ctrl_backslash_during_ssh_reaches_ssh_and_does_not_quit_auspex() {
    // SIGQUIT is taken by auspex's `Termination` like SIGINT, and is dropped while ssh runs.
    let session = "trap 'echo quit > \"@dir@/ssh.quit\"; exit 131' QUIT\ntouch '@dir@/ssh.ready'\n@sleep@ 30 &\nwait $!";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.wait_for_login();
    run.pty.send(b"\x1c");
    run.pty.wait_for_text("ssh to luna exited (code 131)");
    assert_eq!(run.read("ssh.quit"), "quit\n", "ssh got the SIGQUIT");
    assert!(
        run.pty.running(),
        "auspex did not quit on the SIGQUIT meant for ssh"
    );
    run.assert_tui_has_the_terminal("after ssh");
}

#[test]
fn keys_typed_to_an_ssh_that_never_read_them_are_not_read_by_auspex_afterwards() {
    // ssh ignores its input and exits; what was typed meanwhile is for a dead ssh. Read by
    // auspex it would be a command, and the key here is `q`, which has an effect nothing can
    // overwrite: it quits auspex. (This test used to type `b`, whose only effect was to clear the
    // status note, and the note is replaced by "ssh to luna exited" a moment later whether or not
    // `b` was read, so it passed with the `tcflush` in `enter_terminal` removed: review of PR
    // #30, R1. The long-burst test below caught that mutation alone.) The line is typed whole,
    // so it is waiting in the terminal's input queue and becomes readable when auspex puts the
    // terminal in raw mode again, which is what `tcflush` is for.
    let session = "touch '@dir@/ssh.ready'\n@sleep@ 1\nexit 4";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    run.pty.send(b"s");
    run.wait_for_login();
    run.pty.send(b"q\n");
    run.pty.wait_for_text("ssh to luna exited (code 4)");
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        run.pty.running(),
        "a q typed to the dead ssh was read by auspex, which quit:\n{}",
        run.pty.text()
    );
    assert!(
        run.pty.text().contains("ssh to luna exited (code 4)"),
        "the screen after ssh:\n{}",
        run.pty.text()
    );
    run.assert_tui_has_the_terminal("after ssh");
}

#[test]
fn keys_typed_in_the_same_burst_as_s_are_not_replayed_when_auspex_is_back() {
    // `s` and what follows arrive together. Either the keys are handled before ssh starts or
    // they are dropped; none may be replayed after it, where the status note would be cleared.
    // Eight sessions, because which of the two happens is a race.
    let session = "@sleep@ 0.3\nexit $(( $(cat '@dir@/rounds' 2>/dev/null || echo 0) + 1 ))";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    for round in 1..=8 {
        std::fs::write(run.file("rounds"), (round - 1).to_string()).unwrap();
        run.pty.send(b"sbbbb");
        run.pty
            .wait_for_text(&format!("ssh to luna exited (code {round})"));
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            run.pty
                .text()
                .contains(&format!("ssh to luna exited (code {round})")),
            "round {round}: a key from the burst was replayed after ssh:\n{}",
            run.pty.text()
        );
    }
}

#[test]
fn a_long_burst_of_typeahead_behind_s_is_not_replayed_when_auspex_is_back() {
    // Two thousand `b` behind the `s`, in one write (under the pty's input limit, so all of it
    // arrives before ssh starts). crossterm reads a kilobyte at a time and parses every event in
    // it at once, so when auspex stops reading there are still hundreds parsed and not yet
    // returned; if they stayed they would be read by the next reader, and the first one clears the
    // status bar's note.
    let session = "@sleep@ 0.5\nexit 4";
    let Some(mut run) = Run::start("", INVENTORY, session) else {
        return;
    };
    run.act();
    let mut burst = b"s".to_vec();
    burst.extend(std::iter::repeat_n(b'b', 2000));
    run.pty.send(&burst);
    run.pty.wait_for_text("ssh to luna exited (code 4)");
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        run.pty.text().contains("ssh to luna exited (code 4)"),
        "keys from the burst were replayed after ssh:\n{}",
        run.pty.text()
    );
}
