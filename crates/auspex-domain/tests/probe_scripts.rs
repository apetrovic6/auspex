//! Runs the real probe scripts under a local `sh` and feeds what they print to the real
//! parser.
//!
//! The golden fixtures are written by hand, so on their own nothing checks that the scripts
//! print the keys the parser reads: a renamed key, a dropped `--no-pager` or a lost PATH
//! extension would show every machine as "not installed" with every other test green. This
//! file closes that gap. It is the one place in the crate that spawns a process; the library
//! itself never does.
//!
//! How it stays off the real system:
//!
//! - Every absolute path the scripts read (`/run/…`, `/proc/…`, `/nix/var/…`) is rewritten
//!   to the same path under a fresh temporary root, and the test first asserts the scripts
//!   name no other absolute path except `/dev/null` and the `/` that `df` is asked about.
//!   The rewrite is the only change to the script text.
//! - The fake root holds a fake store: `run/current-system` is a symlink into
//!   `<root>/nix/store/<hash>-…`, whose `sw/bin` holds the shims a real system would have
//!   there (`systemctl`, `nixos-version`, `darwin-version`), so the PATH extension is what
//!   finds them. `<root>` is stripped from the output before parsing, which turns those
//!   targets back into `/nix/store/…` paths.
//! - `PATH` is exactly a shim directory (`df`, `sysctl`, `date`) and a directory of symlinks
//!   to the few real tools the scripts use (`tr`, `cut`, `awk`, `sed`, `readlink`); the
//!   environment is otherwise empty. No real `systemctl` or `df` can be reached.
//! - Each shim checks its arguments and prints nothing on a mismatch, so a changed command
//!   line shows up as a missing value.
//!
//! Skipped, with a message, where `sh` or one of those tools is missing, except inside a
//! Nix build (`NIX_BUILD_TOP` set), where `ci-pure` runs it and a skip would be a false
//! green.

#![cfg(unix)]

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use auspex_domain::fleet::MachineClass;
use auspex_domain::observe::{
    DiskUsage, Load, Observation, ProbeExit, ProbeOutcome, SshHealth, StorePath, parse_probe,
    probe_script,
};

const TOOLS: &[&str] = &["tr", "cut", "awk", "sed", "readlink"];

const SYSTEM: &str = "0123456789abcdfghijklmnpqrsvwxyz-nixos-system-terra-25.05";
const BOOTED: &str = "1123456789abcdfghijklmnpqrsvwxyz-nixos-system-terra-25.05-old";
const KERNEL_NEW: &str = "2123456789abcdfghijklmnpqrsvwxyz-linux-6.12.4";
const KERNEL_OLD: &str = "3123456789abcdfghijklmnpqrsvwxyz-linux-6.12.1";
const INITRD: &str = "4123456789abcdfghijklmnpqrsvwxyz-initrd-linux-6.12";
const DARWIN: &str = "5123456789abcdfghijklmnpqrsvwxyz-darwin-system-25.05";
const GONE: &str = "6123456789abcdfghijklmnpqrsvwxyz-nixos-system-gone";

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// `sh` and the real tools, or `None` (with the reason) when the test cannot run here.
fn host_tools() -> Option<(PathBuf, Vec<(&'static str, PathBuf)>)> {
    let found = || -> Result<_, String> {
        let sh = find_on_path("sh").ok_or("no sh on PATH")?;
        let tools = TOOLS
            .iter()
            .map(|t| {
                find_on_path(t)
                    .map(|p| (*t, p))
                    .ok_or_else(|| format!("no {t} on PATH"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((sh, tools))
    };
    match found() {
        Ok(t) => Some(t),
        Err(why) => {
            assert!(
                std::env::var_os("NIX_BUILD_TOP").is_none(),
                "{why}, inside a Nix build: the probe scripts must be run there"
            );
            eprintln!("skipping the probe-script test: {why}");
            None
        }
    }
}

/// A fresh, empty directory under the target's temporary directory.
fn fresh_dir(case: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let base = option_env!("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let dir = base.join(format!(
        "auspex-probe-{case}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_exec(path: &Path, body: &str, sh: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!{}\n{body}", sh.display())).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A shim that prints `out` when called with exactly `args`, and nothing (exit 2) otherwise.
fn shim(path: &Path, sh: &Path, cases: &[(&str, &str)]) {
    let mut body = String::from("case \"$*\" in\n");
    for (args, out) in cases {
        // Single-quoted printf format: the outputs below contain no single quote.
        assert!(!out.contains('\''));
        let _ = writeln!(body, "  '{args}') printf '%s\\n' '{out}' ;;");
    }
    body += "  *) exit 2 ;;\nesac\n";
    write_exec(path, &body, sh);
}

struct Root {
    dir: PathBuf,
    sh: PathBuf,
}

impl Root {
    fn new(case: &str, sh: &Path, tools: &[(&str, PathBuf)]) -> Self {
        let dir = fresh_dir(case);
        let s = dir.to_str().unwrap();
        assert!(
            !s.contains(char::is_whitespace)
                && !["/run/", "/proc/", "/nix/var/"]
                    .iter()
                    .any(|p| s.contains(p)),
            "unusable temporary directory {s:?}"
        );
        for (name, real) in tools {
            fs::create_dir_all(dir.join("tools")).unwrap();
            symlink(real, dir.join("tools").join(name)).unwrap();
        }
        fs::create_dir_all(dir.join("shims")).unwrap();
        fs::create_dir_all(dir.join("run")).unwrap();
        fs::create_dir_all(dir.join("proc")).unwrap();
        fs::create_dir_all(dir.join("nix/var/nix/profiles/default/bin")).unwrap();
        Self {
            dir,
            sh: sh.to_owned(),
        }
    }

    fn store(&self, name: &str) -> PathBuf {
        self.dir.join("nix/store").join(name)
    }

    fn file(&self, rel: &str, contents: &str) {
        let p = self.dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    }

    fn link(&self, rel: &str, target: &Path) {
        let p = self.dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        symlink(target, p).unwrap();
    }

    /// A system profile in the fake store with its kernel, initrd and `sw/bin` shims.
    fn system(&self, name: &str, kernel: &str, bins: &[(&str, &[(&str, &str)])]) -> PathBuf {
        let sys = self.store(name);
        fs::create_dir_all(&sys).unwrap();
        self.file(&format!("nix/store/{kernel}/bzImage"), "kernel");
        self.file(&format!("nix/store/{INITRD}/initrd"), "initrd");
        symlink(self.store(kernel).join("bzImage"), sys.join("kernel")).unwrap();
        symlink(self.store(INITRD).join("initrd"), sys.join("initrd")).unwrap();
        for (bin, cases) in bins {
            shim(&sys.join("sw/bin").join(bin), &self.sh, cases);
        }
        sys
    }

    fn shim(&self, name: &str, cases: &[(&str, &str)]) {
        shim(&self.dir.join("shims").join(name), &self.sh, cases);
    }

    /// Runs `script`, relocated under this root, and parses what it prints.
    fn run(&self, script: &str) -> ProbeOutcome {
        let root = self.dir.to_str().unwrap();
        let relocated = relocate(script, root);
        let out = Command::new(&self.sh)
            .arg("-s")
            .env_clear()
            .env("PATH", format!("{root}/shims:{root}/tools"))
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write as _;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(relocated.as_bytes())?;
                child.wait_with_output()
            })
            .unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap().replace(root, "");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.is_empty(), "the script wrote to stderr: {stderr}");
        let code = out.status.code().unwrap();
        let outcome = parse_probe(&stdout, &stderr, ProbeExit::Code(code));
        assert!(
            matches!(outcome, ProbeOutcome::Observed(_)),
            "{outcome:?}\n--- stdout ---\n{stdout}"
        );
        outcome
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The absolute paths a script names, other than inside the sed expression.
fn absolute_paths(script: &str) -> Vec<&str> {
    script
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-')))
        .filter(|w| w.starts_with('/') && w.len() > 2)
        .collect()
}

fn relocate(script: &str, root: &str) -> String {
    for p in absolute_paths(script) {
        assert!(
            p == "/dev/null"
                || ["/run/", "/proc/", "/nix/var/"]
                    .iter()
                    .any(|r| p.starts_with(r)),
            "the script reads {p}, which the test does not relocate"
        );
    }
    let out = script
        .replace("/run/", &format!("{root}/run/"))
        .replace("/proc/", &format!("{root}/proc/"))
        .replace("/nix/var/", &format!("{root}/nix/var/"));
    assert!(
        !absolute_paths(&out)
            .iter()
            .any(|p| { !p.starts_with(root) && *p != "/dev/null" })
    );
    out
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "fills the Option fields of Observation"
)]
fn sp(name: &str, rest: &str) -> Option<StorePath> {
    Some(StorePath::new(format!("/nix/store/{name}{rest}")).unwrap())
}

fn observed(o: ProbeOutcome) -> Observation {
    match o {
        ProbeOutcome::Observed(o) => *o,
        other => panic!("{other:?}"),
    }
}

/// What systemctl prints for `--failed --output=json`, spread over several lines so that
/// `kv`'s newline folding matters.
const FAILED_JSON: &str = "[\n{\"unit\":\"k3s.service\",\"load\":\"loaded\",\"active\":\"failed\"},\n{\"unit\":\"dev-disk-by\\\\x2duuid.mount\"}\n]";

fn nixos_bins(state: &'static str) -> Vec<(&'static str, &'static [(&'static str, &'static str)])> {
    let systemctl: &'static [(&str, &str)] = match state {
        "degraded" => &[
            ("is-system-running", "degraded"),
            ("--failed --output=json --no-pager", FAILED_JSON),
        ],
        _ => &[
            ("is-system-running", "running"),
            ("--failed --output=json --no-pager", "[]"),
        ],
    };
    vec![
        ("systemctl", systemctl),
        (
            "nixos-version",
            &[(
                "--configuration-revision",
                "c68e00a1b2c3d4e5f60718293a4b5c6d7e8f9012",
            )],
        ),
    ]
}

fn nixos_root(case: &str, sh: &Path, tools: &[(&str, PathBuf)]) -> Root {
    let r = Root::new(case, sh, tools);
    r.file("proc/uptime", "864123.45 1728000.10\n");
    r.file("proc/loadavg", "0.15 0.22 0.30 1/234 5678\n");
    r.shim(
        "df",
        &[(
            "-Pk /",
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 488281250 122070312 341796875 27% /",
        )],
    );
    r
}

const DISK: DiskUsage = DiskUsage {
    total_bytes: 488_281_250 * 1024,
    used_bytes: 122_070_312 * 1024,
    available_bytes: 341_796_875 * 1024,
};

#[test]
fn nixos_probe_runs_and_parses() {
    let Some((sh, tools)) = host_tools() else {
        return;
    };
    let script = probe_script(MachineClass::Nixos);

    // Installed, degraded, booted into an older kernel.
    let r = nixos_root("nixos", &sh, &tools);
    let sys = r.system(SYSTEM, KERNEL_NEW, &nixos_bins("degraded"));
    let booted = r.system(BOOTED, KERNEL_OLD, &[]);
    r.link("run/current-system", &sys);
    r.link("run/booted-system", &booted);
    let o = observed(r.run(script));
    assert_eq!(
        o,
        Observation {
            class: Some(MachineClass::Nixos),
            current_system: sp(SYSTEM, ""),
            booted_kernel: sp(KERNEL_OLD, "/bzImage"),
            booted_initrd: sp(INITRD, "/initrd"),
            current_kernel: sp(KERNEL_NEW, "/bzImage"),
            current_initrd: sp(INITRD, "/initrd"),
            configuration_revision: Some("c68e00a1b2c3d4e5f60718293a4b5c6d7e8f9012".to_owned()),
            system_state: Some("degraded".to_owned()),
            failed_units: Some(vec![
                "k3s.service".to_owned(),
                r"dev-disk-by\x2duuid.mount".to_owned()
            ]),
            ssh_health: SshHealth {
                uptime_seconds: Some(864_123),
                root_disk: Some(DISK),
                load: Some(Load {
                    one: 0.15,
                    five: 0.22,
                    fifteen: 0.3
                }),
            },
        }
    );
    assert!(o.reboot_needed());

    // Healthy, booted into what is current.
    let r = nixos_root("nixos-healthy", &sh, &tools);
    let sys = r.system(SYSTEM, KERNEL_NEW, &nixos_bins("running"));
    r.link("run/current-system", &sys);
    r.link("run/booted-system", &sys);
    let o = observed(r.run(script));
    assert_eq!(o.system_state.as_deref(), Some("running"));
    assert_eq!(o.failed_units, Some(vec![]));
    assert!(!o.reboot_needed());

    // Not installed: no /run/current-system, so no sw/bin, so no systemctl either.
    let r = nixos_root("nixos-missing", &sh, &tools);
    let o = observed(r.run(script));
    assert_eq!(o.current_system, None);
    assert_eq!(o.current_kernel, None);
    assert_eq!(o.booted_kernel, None);
    assert_eq!(o.configuration_revision, None);
    assert_eq!(o.system_state, None);
    assert_eq!(o.failed_units, None);
    assert_eq!(o.ssh_health.uptime_seconds, Some(864_123));

    // F6: a dangling /run/current-system is not installed, and neither are its kernel and
    // initrd links (which do not exist through it).
    let r = nixos_root("nixos-dangling", &sh, &tools);
    r.link("run/current-system", &r.store(GONE));
    r.link("run/booted-system", &r.store(GONE));
    let o = observed(r.run(script));
    assert_eq!(o.current_system, None);
    assert_eq!(o.current_kernel, None);
    assert_eq!(o.booted_kernel, None);
    assert_eq!(o.booted_initrd, None);
}

#[test]
fn darwin_probe_runs_and_parses() {
    let Some((sh, tools)) = host_tools() else {
        return;
    };
    let script = probe_script(MachineClass::Darwin);
    let setup = |case: &str| {
        let r = Root::new(case, &sh, &tools);
        r.shim(
            "sysctl",
            &[
                (
                    "-n kern.boottime",
                    "{ sec = 1700000000, usec = 12345 } Tue Nov 14 22:13:20 2023",
                ),
                ("-n vm.loadavg", "{ 1.84 2.01 2.10 }"),
            ],
        );
        r.shim("date", &[("+%s", "1700302400")]);
        r.shim(
            "df",
            &[(
                "-Pk /",
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s1s1 971350180 412113220 559236960 43% /",
            )],
        );
        r
    };

    let r = setup("darwin");
    let sys = r.store(DARWIN);
    shim(
        &sys.join("sw/bin/darwin-version"),
        &sh,
        &[("--configuration-revision", "null")],
    );
    r.link("run/current-system", &sys);
    let o = observed(r.run(script));
    assert_eq!(
        o,
        Observation {
            class: Some(MachineClass::Darwin),
            current_system: sp(DARWIN, ""),
            booted_kernel: None,
            booted_initrd: None,
            current_kernel: None,
            current_initrd: None,
            configuration_revision: None,
            system_state: None,
            failed_units: None,
            ssh_health: SshHealth {
                uptime_seconds: Some(302_400),
                root_disk: Some(DiskUsage {
                    total_bytes: 971_350_180 * 1024,
                    used_bytes: 412_113_220 * 1024,
                    available_bytes: 559_236_960 * 1024,
                }),
                load: Some(Load {
                    one: 1.84,
                    five: 2.01,
                    fifteen: 2.1
                }),
            },
        }
    );

    // The revision, once the module is imported, comes through darwin-version on the
    // extended PATH.
    let r = setup("darwin-rev");
    let sys = r.store(DARWIN);
    shim(
        &sys.join("sw/bin/darwin-version"),
        &sh,
        &[("--configuration-revision", "abc1234")],
    );
    r.link("run/current-system", &sys);
    assert_eq!(
        observed(r.run(script)).configuration_revision.as_deref(),
        Some("abc1234")
    );

    // F6 on darwin: a dangling link after an incomplete uninstall.
    let r = setup("darwin-dangling");
    r.link("run/current-system", &r.store(GONE));
    assert_eq!(observed(r.run(script)).current_system, None);
}
