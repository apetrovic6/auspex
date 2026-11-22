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

/// How long a test waits for something that must happen, and the budget it gives a product
/// deadline whose length is not what the test is about.
///
/// Every bound in a test that depends on when the scheduler runs a process is a bet on the
/// machine: an idle laptop wins it, a CI box with the rest of a build beside it, or twelve
/// busy loops sharing the core at a better priority (AUS-45, AUS-47), does not, and the test
/// then fails with nothing wrong in the product. A wait that ends the moment its condition
/// holds costs nothing when the bound is long, so these are long: a condition that never comes
/// fails after this, which is a couple of minutes, not never.
///
/// A test whose subject *is* a short deadline cannot just lengthen it. It uses [`grown`] (the
/// deadline must expire only after the fake got as far as the thing under test), compares
/// against what the fake would do if the deadline were ignored (a sleeper that outlives this),
/// or scales its windows with [`scaled`].
pub const PATIENT: Duration = Duration::from_secs(120);

/// A sleeper that outlives [`PATIENT`]: a fake that must still be running when a test ends
/// (it is killed by pid) uses this instead of a number of seconds, so that "the product waited
/// for it" is told from "the product was slow" by a bound no load stretches.
pub const OUTLIVES_PATIENT: u64 = 150;

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

    /// A named pipe `name` in `dir` (the tool list must name `mkfifo`). A fake blocks on it
    /// with `read _ < '@dir@/name'` until something opens it for writing: waiting for a moment
    /// without spending the CPU it waits to be given. A `while [ ! -e flag ]; do :; done` loop
    /// does spend it, and under load starves the very process it is waiting for (AUS-47).
    pub fn fifo(&self, dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let status = std::process::Command::new(self.get("mkfifo"))
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo {}", path.display());
        path
    }
}

/// A temporary directory with a short path (socket paths are short).
pub fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ax")
        .tempdir_in(std::env::temp_dir())
        .unwrap()
}

/// Waits up to [`PATIENT`] for `path` to hold `lines` non-empty lines, and returns them as
/// pids.
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
            start.elapsed() < PATIENT,
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

/// How long [`still_alive_after_wait`] waits: 20 s, where it used to be 3. The fakes whose
/// children must be gone sleep 30 s (or 60), so a child still there after this was not killed,
/// and one that died of its own accord at 30 s is not mistaken for one that was; a longer wait
/// would stop telling them apart. A test that has its fake sleep for [`OUTLIVES_PATIENT`] uses
/// [`still_alive_within`] with [`PATIENT`] instead.
pub const GONE_WITHIN: Duration = Duration::from_secs(20);

/// [`still_alive_within`] [`GONE_WITHIN`]: the pids from `pids` still alive after 20 s.
pub fn still_alive_after_wait(pids: &[i32]) -> Vec<i32> {
    still_alive_within(pids, GONE_WITHIN)
}

/// Waits up to `bound` for every pid to be gone; returns the ones still alive. It returns the
/// moment they are all gone, so a long bound costs time only when something is left.
pub fn still_alive_within(pids: &[i32], bound: Duration) -> Vec<i32> {
    let start = Instant::now();
    loop {
        let left: Vec<i32> = pids.iter().copied().filter(|&p| alive(p)).collect();
        if left.is_empty() || start.elapsed() > bound {
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

/// A private runtime directory for evaluator tests, never the operator's own directory.
/// The thread-local guard removes it when the calling test thread exits.
pub fn runtime_dir() -> PathBuf {
    thread_local! {
        static DIR: tempfile::TempDir = tempdir();
    }
    DIR.with(|dir| dir.path().join("rt"))
}

/// `ResolveParams` for a test: with a private GC root directory, so that a test whose
/// resolution goes further than it should (a refusal that regressed) cannot create
/// `$XDG_STATE_HOME/auspex/gcroots` on the developer's machine. Every test builds its
/// parameters from this, never from `ResolveParams::new()` (a test in `clan_fake.rs` checks).
pub fn resolve_params() -> auspex_effects::clan::ResolveParams<'static> {
    thread_local! {
        static DIR: tempfile::TempDir = tempdir();
    }
    // The leaked path string is a few bytes; the directory goes with the thread's guard.
    let dir: &'static Path = Box::leak(DIR.with(|d| d.path().join("gcroots")).into_boxed_path());
    auspex_effects::clan::ResolveParams {
        gcroot_dir: Some(dir),
        ..auspex_effects::clan::ResolveParams::new()
    }
}

/// Runs `attempt` with `first`, then with four times that, and so on up to [`PATIENT`], until it
/// returns `Some`; panics if even [`PATIENT`] was not enough.
///
/// For a test whose subject is a short deadline: the deadline must expire only after a fake
/// program has got as far as the thing the test looks at (a pid written, a child started), and
/// no fixed short deadline can promise that on a machine that does not run the fake for seconds.
/// `attempt` builds everything it needs afresh (a new directory, a new fake), runs the call
/// under the deadline it is given, and returns `None` if the deadline won the race (the fake
/// was cut off before it got there). It must check nothing about the product before then: an
/// attempt that got far enough returns its observations, and the test asserts on those exactly
/// as before. A product that ignores the deadline fails the assertions of the first attempt that
/// got far enough, and one whose fake never gets there fails here, after at most the budget.
pub async fn grown<T>(first: Duration, mut attempt: impl AsyncFnMut(Duration) -> Option<T>) -> T {
    let mut deadline = first;
    loop {
        if let Some(seen) = attempt(deadline).await {
            return seen;
        }
        assert!(
            deadline < PATIENT,
            "the fake never got as far as the test needs, even with a deadline of {deadline:?}"
        );
        deadline = (deadline * 4).min(PATIENT);
    }
}

/// How many times slower than an idle machine this one starts a process right now, from 1 to
/// 10, measured once per test thread (the first call): the worst of three spawns of `sh -c :`,
/// per 100 ms. An idle machine, and one running the whole suite at once, answers 1.
fn measure_load_factor() -> u32 {
    let Some(sh) = find_on_path("sh") else {
        return 1;
    };
    let worst = (0..3)
        .map(|_| {
            let began = Instant::now();
            let _ = std::process::Command::new(&sh).args(["-c", ":"]).status();
            began.elapsed()
        })
        .max()
        .unwrap_or_default();
    u32::try_from(worst.as_millis() / 100)
        .unwrap_or(MAX_LOAD_FACTOR)
        .clamp(1, MAX_LOAD_FACTOR)
}

/// The largest [`scaled`] stretch. A test about a window that is stretched by this much and
/// still has the machine running behind it needs more time than a test is given.
const MAX_LOAD_FACTOR: u32 = 10;

fn load_factor() -> u32 {
    thread_local! {
        static FACTOR: u32 = measure_load_factor();
    }
    FACTOR.with(|f| *f)
}

/// `d` stretched by how loaded the machine is ([`measure_load_factor`]): for the windows of a
/// test that is *about* a window (an approval that must still fit at 2 s, a timeout that must
/// not fire at 1.5 s), where the margin that separates "right" from "wrong" is a few seconds
/// of scheduling and a load of seconds per process start eats it. Everything in such a test
/// that is a time (the product's windows, the fake's delays, the bounds) is scaled together, so
/// the ratios the test checks stay the same and only the time the run takes grows. On an
/// idle machine it is `d`.
pub fn scaled(d: Duration) -> Duration {
    d * load_factor()
}

/// Whether the machine is oversubscribed right now: more runnable threads than two per core
/// (`/proc/loadavg`, Linux only, for the other agents' builds and the CI neighbours that
/// [`measure_load_factor`]'s three process starts do not see), or a process start that takes
/// over 400 ms.
fn overloaded_now() -> bool {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let queued = fs::read_to_string("/proc/loadavg").ok().and_then(|s| {
        s.split_whitespace()
            .next()
            .and_then(|n| n.parse::<f64>().ok())
    });
    // Casting a core count to f64 is exact for any real machine.
    #[expect(clippy::cast_precision_loss, reason = "a core count")]
    let crowded = queued.is_some_and(|q| q > 2.0 * cores as f64);
    crowded || measure_load_factor() >= 4
}

/// For a test that is *about* a wall clock window (a timeout that must fire between two and four
/// seconds from the start, an approval that must still fit in its window): `true`, with a
/// message, when the machine is too busy for the window to tell the product's clock from the
/// scheduler's, so the test returns instead of failing; `false` on a machine where it can, and
/// always `false` inside a Nix build, where a skip would be a false green.
///
/// A window of a few seconds is a claim about the product's clock. A process start that takes
/// seconds (AUS-47: one core, twelve busy loops, the test at `nice 19`; or sixteen cores and a
/// load average of 160 from everybody's builds) is not a machine on which that claim can be
/// told from scheduling, however the numbers are stretched: they would have to be stretched by a
/// hundred and the test would take an hour. The tests that ask this are the ones that measure a
/// window; the ones about what is killed, reaped, cancelled or ordered do not, and run at any
/// load. Asked twice: before the test (so that it does not spend minutes on a window it cannot
/// read), and when a bound has been missed (the machine may have got busy since: a late answer
/// on a machine that is overloaded now is not evidence).
pub fn too_slow_for_windows(test: &str) -> bool {
    // `AUSPEX_TEST_WINDOWS_ALWAYS=1` runs them anyway, to see what they say on a busy machine.
    if in_nix_build()
        || std::env::var_os("AUSPEX_TEST_WINDOWS_ALWAYS").is_some()
        || !overloaded_now()
    {
        return false;
    }
    eprintln!(
        "SKIPPED: {test} measures a wall clock window, and this machine is too busy to read it"
    );
    true
}

/// The cache's git with [`PATIENT`] for each command and for the worktree read, for the tests
/// that run a real `git` and are not about a git deadline. The product's 2 s per command is
/// right for an operator and a loaded machine misses it ("git rev-parse timed out after 2s",
/// AUS-47). A test that is about a deadline sets `timeout` itself afterwards.
pub fn patient_git() -> auspex_effects::cache::GitParams<'static> {
    let mut git = auspex_effects::cache::GitParams::new();
    git.timeout = PATIENT;
    git.read_timeout = PATIENT;
    git
}
