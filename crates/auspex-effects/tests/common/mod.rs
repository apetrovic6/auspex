//! Helpers the integration tests share: finding tools, writing fake programs, and checking
//! that a process is gone.
//!
//! Fake programs are POSIX `sh` scripts written to a temporary directory, with the absolute
//! path of the `sh` found on `PATH` as their interpreter (the Nix sandbox has no `/bin/sh`
//! guarantee beyond what the builder provides). Tests kill only processes they spawned, by
//! pid, and never by name.

#![allow(dead_code, reason = "each test file uses a different subset")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    /// `@tool@` in the body is replaced by the tool's absolute path (and `@dir@` by
    /// `dir`), so the script needs nothing from `PATH`.
    ///
    /// The file is written by a separate `sh` process, not by this one: with tests on
    /// several threads, another thread's fork could otherwise inherit our write descriptor
    /// for an instant, and executing the script then fails with "Text file busy".
    pub fn script(&self, dir: &Path, name: &str, body: &str) -> PathBuf {
        let mut body = body.replace("@dir@", &dir.display().to_string());
        for (n, p) in &self.0 {
            body = body.replace(&format!("@{n}@"), &p.display().to_string());
        }
        let text = format!("#!{}\n{body}\n", self.get("sh").display());
        let src = dir.join(format!("{name}.src"));
        fs::write(&src, text).unwrap();
        let dst = dir.join(name);
        let status = std::process::Command::new(self.get("sh"))
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

/// A temporary directory with a short path (socket paths are short).
pub fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ax")
        .tempdir_in(std::env::temp_dir())
        .unwrap()
}

/// Waits up to 5 s for `path` to hold `lines` non-empty lines, and returns them as pids.
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
            start.elapsed() < Duration::from_secs(5),
            "{} never got {lines} pids",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether `pid` is alive (a zombie counts as gone: it has exited, and only its parent's
/// bookkeeping is left).
pub fn alive(pid: i32) -> bool {
    let stat = format!("/proc/{pid}/stat");
    if Path::new("/proc/self/stat").exists() {
        return fs::read_to_string(stat).is_ok_and(|s| {
            // The state follows the parenthesised command name.
            s.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z" && state != "X")
        });
    }
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
}

/// Waits up to 3 s for every pid to be gone; returns the ones still alive.
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

/// Kills pids this test spawned, so a failing assertion leaves nothing behind.
pub fn cleanup(pids: &[i32]) {
    // Only those still alive: a pid that has exited may have been reused since.
    for &p in pids.iter().filter(|&&p| alive(p)) {
        if let Some(pid) = rustix::process::Pid::from_raw(p) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}

/// Whether `pid` exists at all, zombies included: a child that has exited but was never
/// reaped still exists.
pub fn pid_exists(pid: i32) -> bool {
    if Path::new("/proc/self/stat").exists() {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
}

/// A private runtime directory for the machine-list files the evaluator tests create, so
/// they never touch the operator's own `$XDG_RUNTIME_DIR/auspex`. Short, for the socket-path
/// limit `ControlDir::prepare` checks; one per test process.
pub fn runtime_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(tempdir).path().join("rt")
}
