//! Helpers the integration tests share: the demo driven to its end, a rendered frame as
//! text, snapshot files, fake programs, and running the built binary in an environment of
//! its own.
//!
//! Every run of the binary gets fresh `HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME` and
//! `XDG_RUNTIME_DIR`, so no test reads the real config, the real eval cache or `~/.ssh`.
//! Fake programs are POSIX `sh` scripts with the absolute path of the `sh` on `PATH` as
//! their interpreter (copied from `crates/auspex-effects/tests/common`, as is `alive`). Tests
//! signal and kill only processes they spawned, by pid.

#![allow(dead_code, reason = "each test file uses a different subset")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use auspex_core::{Event, ViewModel};
use auspex_tui::backend::Backend;
use auspex_tui::demo::{self, Demo};
use auspex_tui::keymap::{Key, Keymap};
use auspex_tui::log::Log;
use auspex_tui::output::Output as ActionOutput;
use auspex_tui::shell::Shell;
use auspex_tui::theme::Theme;
use auspex_tui::ui::{self, Context, Ui};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// The demo, started. Call from a test with paused time.
pub fn started_demo() -> Arc<Shell> {
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    shell.send(Event::Start(demo::config()));
    shell
}

/// The demo, run to its settled end. Call from a test with paused time.
pub async fn settled_demo() -> Arc<Shell> {
    let shell = Shell::new(
        Backend::Demo(Demo::default()),
        Log::memory(),
        Duration::from_secs(120),
    );
    shell.send(Event::Start(demo::config()));
    while !shell.view().refresh.settled {
        shell.changed().await;
    }
    shell
}

/// Types `keys` (each char a key; `\n` is Enter, `\x1b` Esc) into `ui` against `shell`, with no
/// frame drawn between them: typeahead, all in one burst.
pub fn type_keys(shell: &Arc<Shell>, ui: &mut Ui, keymap: &Keymap, keys: &str) {
    for c in keys.chars() {
        let key = match c {
            '\n' => Key::plain(crossterm::event::KeyCode::Enter),
            '\t' => Key::plain(crossterm::event::KeyCode::Tab),
            '\x1b' => Key::plain(crossterm::event::KeyCode::Esc),
            c => Key::char(c),
        };
        press(shell, ui, keymap, key);
    }
}

thread_local! {
    static CLOCK: (Instant, std::cell::Cell<Duration>) = (Instant::now(), std::cell::Cell::new(Duration::ZERO));
}

/// The test's own clock: it moves only when [`elapse`] says so, so "the question has been on
/// screen for 0.3 s" is not a race with the machine's load. One per test thread.
pub fn now() -> Instant {
    CLOCK.with(|(base, offset)| *base + offset.get())
}

/// Lets `d` pass on the test's clock.
pub fn elapse(d: Duration) {
    CLOCK.with(|(_, offset)| offset.set(offset.get() + d));
}

/// One key, as the real loop sends it: through `Ui::key`, which discards a key that answers a
/// question that has not been on screen, whole, for `QUESTION_DWELL` (on [`now`]'s clock).
pub fn press(shell: &Arc<Shell>, ui: &mut Ui, keymap: &Keymap, key: Key) {
    let view = shell.view();
    // Long after any refresh started: a test that needs "just started" builds its Screen.
    for e in ui.key(key, &view, Duration::from_secs(3600), keymap, now()) {
        shell.send(e);
    }
}

/// A paste, as the real loop sends it: through `Ui::paste`.
pub fn paste(shell: &Arc<Shell>, ui: &mut Ui, keymap: &Keymap, text: &str) {
    let view = shell.view();
    for e in ui.paste(text, &view, Duration::from_secs(3600), keymap, now()) {
        shell.send(e);
    }
}

/// One frame of `shell` is drawn (and so a question open at that moment has been shown).
pub fn frame(shell: &Arc<Shell>, ui: &mut Ui, keymap: &Keymap) {
    draw(
        &shell.view(),
        ui,
        keymap,
        &shell.log.lines(),
        shell.output(),
        120,
        40,
    );
}

/// As [`type_keys`], with a frame drawn after each key and a second let pass: an operator who
/// reads what is on the screen before pressing the next key, which is what a question asks of
/// them.
pub fn type_keys_seen(shell: &Arc<Shell>, ui: &mut Ui, keymap: &Keymap, keys: &str) {
    frame(shell, ui, keymap);
    for c in keys.chars() {
        type_keys(shell, ui, keymap, &c.to_string());
        frame(shell, ui, keymap);
        elapse(Duration::from_secs(1));
    }
}

/// One frame at `width` × `height`, as text: one line per row, trailing spaces trimmed.
pub fn render(
    view: &ViewModel,
    ui: &mut Ui,
    keymap: &Keymap,
    log: &[String],
    width: u16,
    height: u16,
) -> String {
    render_with(
        view,
        ui,
        keymap,
        log,
        &ActionOutput::default(),
        width,
        height,
    )
}

/// [`render`] with the streamed action output the Log tab draws.
pub fn render_with(
    view: &ViewModel,
    ui: &mut Ui,
    keymap: &Keymap,
    log: &[String],
    output: &ActionOutput,
    width: u16,
    height: u16,
) -> String {
    let buffer = draw(view, ui, keymap, log, output, width, height);
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// One frame as ratatui's buffer, for tests that look at styles as well as symbols.
pub fn draw(
    view: &ViewModel,
    ui: &mut Ui,
    keymap: &Keymap,
    log: &[String],
    output: &ActionOutput,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let ctx = Context {
        theme: &Theme::Ansi,
        keymap,
        flake: demo::FLAKE,
        log,
        output,
        now: now(),
    };
    terminal.draw(|f| ui::draw(f, view, ui, &ctx)).unwrap();
    terminal.backend().buffer().clone()
}

/// The crate's directory.
pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Compares `actual` with `tests/fixtures/<dir>/<name>`. With `AUSPEX_UPDATE_SNAPSHOTS=1` (and never
/// inside a Nix build) the file is written instead.
pub fn expect_file(dir: &str, name: &str, actual: &str) {
    // Under `tests/fixtures/`: the only part of `tests/` beside the sources that the
    // pipeline puts into the `ci-pure` sandbox (nix/pipeline/pipeline.nix, "Golden fixtures").
    let path = crate_dir().join("tests/fixtures").join(dir).join(name);
    if std::env::var_os("AUSPEX_UPDATE_SNAPSHOTS").is_some() && !in_nix_build() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; run with AUSPEX_UPDATE_SNAPSHOTS=1 to write it\nactual:\n{actual}",
            path.display()
        )
    });
    assert!(
        expected == actual,
        "{} differs; run with AUSPEX_UPDATE_SNAPSHOTS=1 to accept\n--- expected\n{expected}\n--- actual\n{actual}",
        path.display()
    );
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// Inside a Nix build (`ci-pure`), where a skip would be a false green.
pub fn in_nix_build() -> bool {
    std::env::var_os("NIX_BUILD_TOP").is_some()
}

/// `sh` and the named tools, as absolute paths, or `None` after printing why the test is
/// skipped. Inside a Nix build a missing tool fails the test instead.
pub fn tools(names: &[&str]) -> Option<Tools> {
    let mut found = Vec::new();
    for n in std::iter::once(&"sh").chain(names) {
        if let Some(p) = find_on_path(n) {
            found.push(((*n).to_owned(), p));
        } else {
            assert!(
                !in_nix_build(),
                "{n} is not on PATH inside the Nix build; the test would be a false green"
            );
            eprintln!("SKIPPED: {n} is not on PATH");
            return None;
        }
    }
    Some(Tools(found))
}

pub struct Tools(Vec<(String, PathBuf)>);

impl Tools {
    pub fn get(&self, name: &str) -> &Path {
        &self.0.iter().find(|(n, _)| n == name).expect("asked for").1
    }

    /// Writes an executable script `name` in `dir` running `body` under `sh`. Every
    /// `@tool@` in the body is replaced by the tool's absolute path (and `@dir@` by `dir`).
    ///
    /// Written by a separate `sh` process: with tests on several threads, another thread's
    /// fork could otherwise inherit our write descriptor, and executing the script then fails
    /// with "Text file busy".
    pub fn script(&self, dir: &Path, name: &str, body: &str) -> PathBuf {
        let mut body = body.replace("@dir@", &dir.display().to_string());
        for (n, p) in &self.0 {
            body = body.replace(&format!("@{n}@"), &p.display().to_string());
        }
        let text = format!("#!{}\n{body}\n", self.get("sh").display());
        let src = dir.join(format!("{name}.src"));
        fs::write(&src, text).unwrap();
        let dst = dir.join(name);
        let status = Command::new(self.get("sh"))
            .arg("-c")
            .arg(
                r#"while IFS= read -r l || [ -n "$l" ]; do printf '%s\n' "$l"; done < "$1" > "$2""#,
            )
            .arg("sh")
            .arg(&src)
            .arg(&dst)
            .status()
            .unwrap();
        assert!(status.success());
        fs::set_permissions(&dst, fs::Permissions::from_mode(0o755)).unwrap();
        dst
    }
}

/// A temporary directory with a short path (ssh's socket paths are short).
pub fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ax")
        .tempdir_in(std::env::temp_dir())
        .unwrap()
}

/// The built binary, in a sandbox of its own under `home`: fresh home, config, cache and
/// runtime directories, and `CLAN_DIR` unset.
pub fn auspex(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_auspex-tui"));
    for d in ["home", "config", "cache", "run"] {
        let p = home.join(d);
        fs::create_dir_all(&p).unwrap();
        if d == "run" {
            fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    cmd.env("HOME", home.join("home"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env_remove("CLAN_DIR")
        .env_remove("AUSPEX_SSH")
        .env_remove("AUSPEX_NIX")
        .env_remove("AUSPEX_NIX_STORE")
        .env_remove("AUSPEX_NIX_EVAL_JOBS")
        .stdin(Stdio::null());
    cmd
}

/// Runs `cmd`, failing the test if it takes longer than `limit`.
pub fn run_within(cmd: &mut Command, limit: Duration) -> Output {
    let child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = i32::try_from(child.id()).unwrap();
    // Read the pipes while waiting (a report is more than a pipe buffer holds).
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let Ok(out) = rx.recv_timeout(limit) else {
        cleanup(&[pid]);
        panic!("auspex took longer than {limit:?}");
    };
    out.unwrap()
}

/// Waits up to 10 s for `path` to hold `lines` non-empty lines, and returns them as pids.
pub fn wait_for_pids(path: &Path, lines: usize) -> Vec<i32> {
    let start = Instant::now();
    loop {
        if let Ok(s) = fs::read_to_string(path) {
            let pids: Vec<i32> = s.lines().filter_map(|l| l.trim().parse().ok()).collect();
            if pids.len() >= lines {
                return pids;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{} never got {lines} pids",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether `pid` is alive (a zombie counts as gone).
pub fn alive(pid: i32) -> bool {
    let stat = format!("/proc/{pid}/stat");
    if Path::new("/proc/self/stat").exists() {
        return fs::read_to_string(stat).is_ok_and(|s| {
            s.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z" && state != "X")
        });
    }
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
}

/// Whether `pid` exists at all, zombies included: a child that has exited but was never
/// reaped still exists. What "the leader was reaped" is checked with: `alive` counts a zombie
/// as gone.
pub fn pid_exists(pid: i32) -> bool {
    if Path::new("/proc/self/stat").exists() {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
}

/// Kills pids this test spawned that are still alive, so a failing assertion leaves
/// nothing behind.
pub fn cleanup(pids: &[i32]) {
    for &p in pids.iter().filter(|&&p| alive(p)) {
        if let Some(pid) = rustix::process::Pid::from_raw(p) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}

/// Waits up to 3 s for every pid to be gone; returns the ones still alive. A process killed
/// with SIGKILL is gone within moments, but not at the instant its group leader is reaped.
pub fn still_alive_after_wait(pids: &[i32]) -> Vec<i32> {
    let start = Instant::now();
    loop {
        let left: Vec<i32> = pids.iter().copied().filter(|&p| alive(p)).collect();
        if left.is_empty() || start.elapsed() > Duration::from_secs(3) {
            return left;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
