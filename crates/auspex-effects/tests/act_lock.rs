//! `b` in observe mode must not touch the fleet's repository.
//!
//! nix locks a local flake before it evaluates anything, and when `flake.lock` disagrees with
//! `flake.nix` it writes the lock file into the working tree and, for a `git+file` flake,
//! stages it. `b` runs without act mode because it "changes nothing", so a stale lock must
//! leave the lock file and git's index byte for byte as they were.
//!
//! Uses the real `nix` and `git` against a toy fleet in a private temporary directory, and
//! asks nix for `--dry-run` through a wrapper so nothing is built (the lock is written in
//! the same code path before evaluation either way). Skipped, with a message, when either is
//! not on `PATH`, which is the case inside the `ci-pure` sandbox (no Nix daemon there).

#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use auspex_domain::fleet::MachineClass;
use auspex_effects::CancellationToken;
use auspex_effects::act::build;
use auspex_effects::clan::{RunParams, current_system};
use auspex_effects::nix::FlakeRef;
use common::{find_on_path, tempdir, tools};
use tokio::sync::mpsc::unbounded_channel;

fn git(dir: &Path, args: &[&str]) -> String {
    let ok = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=auspex", "-c", "user.email=auspex@invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(ok.status.success(), "git {args:?}: {ok:?}");
    String::from_utf8(ok.stdout).unwrap()
}

#[tokio::test]
async fn b_with_a_stale_lock_leaves_the_lock_file_and_the_git_index_alone() {
    for tool in ["nix", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH (as inside the ci-pure sandbox)");
            return;
        }
    }
    let Some(t) = tools(&["nix"]) else { return };
    let root = tempdir();
    let (fleet, dep, dep2) = (
        root.path().join("fleet"),
        root.path().join("dep"),
        root.path().join("dep2"),
    );
    fs::create_dir(&fleet).unwrap();
    for d in [&dep, &dep2] {
        fs::create_dir(d).unwrap();
        fs::write(d.join("data"), "data\n").unwrap();
    }
    let system = current_system();
    let outputs = format!(
        "outputs = {{ self, ... }}: {{ nixosConfigurations.m.config.system.build.toplevel = \
         derivation {{ name = \"t\"; system = \"{system}\"; builder = \"/bin/sh\"; }}; }};"
    );
    let input = |name: &str, dir: &Path| {
        format!(
            "inputs.{name} = {{ url = \"path:{}\"; flake = false; }};\n  ",
            dir.display()
        )
    };
    // A locked fleet with one input ...
    fs::write(
        fleet.join("flake.nix"),
        format!("{{ {}{outputs} }}\n", input("dep", &dep)),
    )
    .unwrap();
    git(&fleet, &["init", "-q"]);
    git(&fleet, &["add", "flake.nix"]);
    let lock = Command::new("nix")
        .args(["flake", "lock"])
        .arg(format!("git+file://{}", fleet.display()))
        .output()
        .unwrap();
    assert!(lock.status.success(), "{lock:?}");
    git(&fleet, &["add", "flake.lock"]);
    git(&fleet, &["commit", "-q", "-m", "locked"]);
    // ... that then gains a second input the lock does not have, committed: a clean tree and a
    // stale lock.
    fs::write(
        fleet.join("flake.nix"),
        format!(
            "{{ {}{}{outputs} }}\n",
            input("dep", &dep),
            input("dep2", &dep2)
        ),
    )
    .unwrap();
    git(&fleet, &["commit", "-q", "-am", "an input"]);
    let lock_before = fs::read(fleet.join("flake.lock")).unwrap();
    let index_before = fs::read(fleet.join(".git/index")).unwrap();
    let status_before = git(&fleet, &["status", "--porcelain"]);
    assert_eq!(status_before, "", "the fixture's tree is clean");

    // The real nix, asked not to build.
    let nix = t.script(
        root.path(),
        "nix",
        "exec @nix@ \"$@\" --dry-run --extra-experimental-features 'nix-command flakes'",
    );
    let flake = FlakeRef::new(&format!("git+file://{}", fleet.display())).unwrap();
    let (tx, mut rx) = unbounded_channel();
    let end = build(
        nix.as_os_str(),
        &flake,
        "m",
        MachineClass::Nixos,
        Some(&system),
        &system,
        &RunParams::new(),
        &CancellationToken::new(),
        tx,
    )
    .await
    .unwrap()
    .unwrap();
    let mut seen = Vec::new();
    while let Ok(l) = rx.try_recv() {
        seen.push(l);
    }
    assert!(end.end.success(), "nix failed: {seen:?}");

    // `assert!`, not `assert_eq!`: a failure would otherwise print both files as byte arrays.
    assert!(
        fs::read(fleet.join("flake.lock")).unwrap() == lock_before,
        "b rewrote the fleet's flake.lock"
    );
    assert!(
        fs::read(fleet.join(".git/index")).unwrap() == index_before,
        "b changed the fleet's git index"
    );
    assert_eq!(git(&fleet, &["status", "--porcelain"]), status_before);
}
