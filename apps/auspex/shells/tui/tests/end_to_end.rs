//! The built binary, `--json`, against real `nix` and `nix-eval-jobs` and a fixture flake
//! with no inputs (`tests/fixtures/flake`), with a fake `ssh` (`AUSPEX_SSH`).
//!
//! Skipped, with a message, when `nix`, `nix-eval-jobs`, `git` or `prlimit` is not on `PATH`,
//! as inside the `ci-pure` sandbox, where there is no Nix daemon to evaluate against.
//!
//! The fixture is committed into a fresh git repository and named by a plain path, which the
//! adapters turn into a `git+file` reference: so the eval cache applies, and the second run
//! must read every successful evaluation from it.

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use auspex_domain::fleet::TargetSource;
use auspex_domain::report::FleetReport;
use auspex_domain::status::Badge;
use common::{auspex, crate_dir, find_on_path, run_within, tempdir, tools};

const LIMIT: Duration = Duration::from_secs(300);

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(dir)
        // The developer's own config (signing, hooks) must not take part.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=auspex", "-c", "user.email=auspex@invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(ok.status.success(), "git {args:?}: {ok:?}");
}

#[test]
fn the_fixture_fleet_end_to_end() {
    for tool in ["nix", "nix-eval-jobs", "git", "prlimit"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH");
            return;
        }
    }
    let Some(t) = tools(&[]) else { return };
    let dir = tempdir();
    let fleet = dir.path().join("fleet");
    std::fs::create_dir_all(&fleet).unwrap();
    std::fs::copy(
        crate_dir().join("tests/fixtures/flake/flake.nix"),
        fleet.join("flake.nix"),
    )
    .unwrap();
    git(&fleet, &["init", "-q"]);
    git(&fleet, &["add", "flake.nix"]);
    git(&fleet, &["commit", "-q", "-m", "fixture"]);

    // luna's ssh fails as ssh does; every other host answers, darwin or NixOS by the script.
    let ssh = t.script(
        dir.path(),
        "ssh",
        r#"for a; do case "$prev" in --) dest=$a;; esac; prev=$a; done
case "$dest" in
  *192.0.2.11*) echo 'ssh: connect to host 192.0.2.11 port 2222: Connection refused' >&2; exit 255;;
esac
script=$(cat)
case "$script" in
  *"kv probe darwin"*) printf 'auspex_probe=1\nprobe=darwin\ncurrent_system=/nix/store/0123456789abcdfghijklmnpqrsvwxyz-darwin-system-genetor\nauspex_end=1\n';;
  *) printf 'auspex_probe=1\nprobe=nixos\ncurrent_system=/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-old\nsystem_state=running\nfailed_units=[]\nauspex_end=1\n';;
esac"#,
    );
    let config = dir.path().join("auspex.toml");
    std::fs::write(&config, "[machines.terra]\ntarget = \"root@192.0.2.99\"\n").unwrap();

    let run = || {
        let mut cmd = auspex(dir.path());
        cmd.args(["--json", "--config"])
            .arg(&config)
            .arg("--flake")
            .arg(&fleet)
            .env("AUSPEX_SSH", &ssh);
        run_within(&mut cmd, LIMIT)
    };

    let first = run();
    let stderr = String::from_utf8_lossy(&first.stderr).into_owned();
    assert_eq!(first.status.code(), Some(0), "{stderr}");
    let report: FleetReport = serde_json::from_slice(&first.stdout).unwrap();
    let m = |name: &str| {
        report
            .machines
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("{name} missing: {report:#?}"))
    };
    let names: Vec<&str> = report.machines.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["1odd", "broken", "genetor", "luna", "terra"]);

    assert_eq!(m("terra").target.as_deref(), Some("root@192.0.2.99"));
    assert_eq!(m("terra").target_source, TargetSource::Override);
    assert_eq!(m("luna").target.as_deref(), Some("root@192.0.2.11:2222"));
    assert_eq!(m("luna").target_source, TargetSource::Inventory);
    assert_eq!(m("genetor").target.as_deref(), Some("admin@192.0.2.20"));
    assert_eq!(m("broken").target.as_deref(), Some("broken"));
    assert_eq!(m("broken").target_source, TargetSource::MachineName);

    assert_eq!(m("broken").status, Badge::EvalError);
    let reason = m("broken").reason.as_deref().unwrap();
    assert!(
        reason.contains("broken fails to evaluate, on purpose"),
        "{reason}"
    );

    assert_eq!(m("luna").status, Badge::Unreachable);
    let reason = m("luna").reason.as_deref().unwrap();
    assert!(reason.contains("Connection refused"), "{reason}");

    for name in ["terra", "1odd", "genetor"] {
        assert!(m(name).pending_system.is_some(), "{name} evaluated");
        assert_eq!(m(name).status, Badge::UpdatePending, "{name}");
    }
    assert!(!stderr.contains("eval cache hit"), "a cold cache: {stderr}");

    // A second run on the clean tree reads every successful evaluation from the cache, and
    // the log says so. (`broken` failed, and errors are never cached.)
    let second = run();
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert_eq!(second.status.code(), Some(0), "{stderr}");
    for name in ["1odd", "genetor", "luna", "terra"] {
        assert!(
            stderr.contains(&format!("eval cache hit: {name}")),
            "{name} from the cache:\n{stderr}"
        );
    }
    assert!(!stderr.contains("eval cache hit: broken"), "{stderr}");
    let again: FleetReport = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(again, report, "the cache changes nothing in the report");
}
