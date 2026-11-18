//! Resolving `clan` with the real `nix` against local fixture flakes.
//!
//! The fleet is a flake with one input, `clan-core`, a local git repository whose
//! `packages.<system>.clan-cli` is a fake: a `builtin:buildenv` derivation (it needs no
//! builder, so it builds in the sandbox with nothing but nix) linking a source directory
//! with an executable `bin/clan` shell script. The fleet's `flake.lock` pins one commit of
//! that repository, and the tests add a later commit to show that the lock, not upstream,
//! decides what is built. The real `clan` is never run, and never needed.
//!
//! Skipped, with a message, when `nix` or `git` is not on `PATH`, which is the case inside
//! the `ci-pure` sandbox: there is no Nix daemon to build against there.

#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use auspex_effects::CancellationToken;
use auspex_effects::clan::{
    ClanBinary, ClanError, ClanLocator, ClanSource, ClanStep, PathClan, ResolveParams,
    current_system,
};
use auspex_effects::nix::FlakeRef;
use common::{Tools, find_on_path, tempdir, tools};

fn git(dir: &Path, args: &[&str]) -> String {
    let ok = Command::new("git")
        .current_dir(dir)
        // The developer's own config (commit signing, hooks) must not take part.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=auspex", "-c", "user.email=auspex@invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(ok.status.success(), "git {args:?}: {ok:?}");
    String::from_utf8(ok.stdout).unwrap().trim().to_owned()
}

fn nix(args: &[&str]) {
    let ok = Command::new("nix").args(args).output().unwrap();
    assert!(ok.status.success(), "nix {args:?}: {ok:?}");
}

struct Fixture {
    // Held for its Drop.
    _root: tempfile::TempDir,
    tools: Tools,
    core: PathBuf,
    fleet: PathBuf,
    /// A directory with no `clan` in it: the `PATH` the comparison searches by default.
    empty: PathBuf,
    /// A `nix` that logs its subcommand and runs the real one.
    nix: PathBuf,
}

/// `None` (after saying why) when the fixture cannot be built here.
fn fixture(core_has_package: bool) -> Option<Fixture> {
    for tool in ["nix", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH (as inside the ci-pure sandbox)");
            return None;
        }
    }
    let tools = tools(&["nix"])?;
    let root = tempdir();
    let (core, fleet, empty) = (
        root.path().join("core"),
        root.path().join("fleet"),
        root.path().join("empty"),
    );
    for d in [&core, &fleet, &empty] {
        fs::create_dir(d).unwrap();
    }
    write_core(&core, "A", core_has_package);
    git(&core, &["init", "-q"]);
    git(&core, &["add", "-A"]);
    git(&core, &["commit", "-q", "-m", "A"]);
    fs::write(
        fleet.join("flake.nix"),
        format!(
            "{{\n  inputs.clan-core.url = \"git+file://{}\";\n  outputs = _: {{}};\n}}\n",
            core.display()
        ),
    )
    .unwrap();
    lock(&fleet);
    let wrapper = tools.script(
        root.path(),
        "nix",
        "echo \"$1\" >> '@dir@/calls'\nexec @nix@ \"$@\"",
    );
    Some(Fixture {
        _root: root,
        tools,
        core,
        fleet,
        empty,
        nix: wrapper,
    })
}

fn lock(fleet: &Path) {
    nix(&["flake", "lock", &format!("path:{}", fleet.display())]);
}

/// The fake clan-core: `packages.<system>.clan-cli` links `pkg/`, whose `bin/clan` prints
/// its `tag`.
fn write_core(core: &Path, tag: &str, with_package: bool) {
    let system = current_system();
    let package = if with_package {
        format!(
            r#"packages.{system}.clan-cli = derivation {{
      name = "clan-cli"; system = "{system}";
      builder = "builtin:buildenv"; outputs = ["out"];
      manifest = builtins.toFile "manifest" "[]";
      derivations = [ "true" 5 1 src ];
    }};"#
        )
    } else {
        String::new()
    };
    fs::write(
        core.join("flake.nix"),
        format!("{{\n  outputs = _: let src = ./pkg; in {{\n    {package}\n  }};\n}}\n"),
    )
    .unwrap();
    let bin = core.join("pkg/bin");
    fs::create_dir_all(&bin).unwrap();
    let clan = bin.join("clan");
    fs::write(&clan, format!("#!/bin/sh\necho fake clan {tag}\n")).unwrap();
    fs::set_permissions(&clan, fs::Permissions::from_mode(0o755)).unwrap();
}

impl Fixture {
    fn flake(&self) -> FlakeRef {
        FlakeRef::new(self.fleet.to_str().unwrap()).unwrap()
    }

    fn calls(&self, what: &str) -> usize {
        fs::read_to_string(self.nix.parent().unwrap().join("calls"))
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == what)
            .count()
    }

    async fn resolve(&self, locator: &ClanLocator, search: &Path) -> Result<ClanBinary, ClanError> {
        let mut p = ResolveParams::new();
        p.nix = self.nix.as_os_str();
        p.search_path = Some(search.as_os_str());
        p.lock_timeout = Duration::from_secs(120);
        p.build_timeout = Duration::from_secs(300);
        locator
            .resolve(&p, &self.flake(), &CancellationToken::new())
            .await
            .unwrap()
    }
}

fn output_of(program: &Path) -> String {
    let out = Command::new(program).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn the_locked_clan_core_is_built_and_reported() {
    let Some(f) = fixture(true) else { return };
    let rev = git(&f.core, &["rev-parse", "HEAD"]);
    let got = f
        .resolve(&ClanLocator::new(), &f.empty)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(got.path.starts_with("/nix/store/"), "{:?}", got.path);
    assert!(got.path.ends_with("bin/clan"), "{:?}", got.path);
    assert_eq!(output_of(&got.path), "fake clan A");
    assert_eq!(got.revision.as_deref(), Some(rev.as_str()));
    assert_eq!(
        got.source,
        ClanSource::FleetLock {
            revision: rev.clone()
        }
    );
    assert_eq!(got.source.to_string(), format!("fleet lock {}", &rev[..7]));
    assert_eq!(got.path_clan, PathClan::Absent);
    // Every step is on the record, in order.
    let trail = got.trail.join("\n");
    for (i, needle) in [
        "no explicit clan path",
        &rev,
        "nix build packages.",
        "no clan on PATH",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            trail.contains(needle),
            "step {i} ({needle}) missing from:\n{trail}"
        );
    }
}

#[tokio::test]
async fn what_is_built_is_what_the_lock_pins_not_what_upstream_has_now() {
    let Some(f) = fixture(true) else { return };
    let pinned = git(&f.core, &["rev-parse", "HEAD"]);
    // Upstream moves on: a later commit with a different clan. The fleet's lock does not.
    write_core(&f.core, "B", true);
    git(&f.core, &["add", "-A"]);
    git(&f.core, &["commit", "-q", "-m", "B"]);
    assert_ne!(git(&f.core, &["rev-parse", "HEAD"]), pinned);
    let got = f
        .resolve(&ClanLocator::new(), &f.empty)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(output_of(&got.path), "fake clan A");
    assert_eq!(got.revision.as_deref(), Some(pinned.as_str()));
}

#[tokio::test]
async fn the_result_is_cached_per_lock_and_a_new_lock_is_built_afresh() {
    let Some(f) = fixture(true) else { return };
    let locator = ClanLocator::new();
    let first = f.resolve(&locator, &f.empty).await.unwrap();
    let second = f.resolve(&locator, &f.empty).await.unwrap();
    assert_eq!(first.path, second.path);
    // The lock is read each time (it is the key), the build only once.
    assert_eq!(f.calls("flake"), 2);
    assert_eq!(f.calls("build"), 1);
    assert!(
        second.trail.iter().any(|l| l.contains("already built")),
        "{:?}",
        second.trail
    );

    // The fleet updates its lock: a different key, a different build.
    write_core(&f.core, "B", true);
    git(&f.core, &["add", "-A"]);
    git(&f.core, &["commit", "-q", "-m", "B"]);
    nix(&[
        "flake",
        "update",
        "--flake",
        &format!("path:{}", f.fleet.display()),
        "clan-core",
    ]);
    let third = f.resolve(&locator, &f.empty).await.unwrap();
    assert_eq!(f.calls("build"), 2);
    assert_ne!(third.revision, first.revision);
    assert_eq!(output_of(&third.path), "fake clan B");

    // The cache is the locator's, not the process's.
    let other = ClanLocator::new();
    f.resolve(&other, &f.empty).await.unwrap();
    assert_eq!(f.calls("build"), 3, "a new locator has an empty cache");
}

#[tokio::test]
async fn clan_on_path_is_accepted_only_as_the_same_store_path() {
    let Some(f) = fixture(true) else { return };
    let locator = ClanLocator::new();
    let pinned = f.resolve(&locator, &f.empty).await.unwrap();

    // The same store path, seen through a profile-like symlink.
    let same = f.empty.parent().unwrap().join("same");
    fs::create_dir(&same).unwrap();
    symlink(&pinned.path, same.join("clan")).unwrap();
    let got = f.resolve(&locator, &same).await.unwrap();
    assert_eq!(got.path_clan, PathClan::Same);

    // Another clan-core: refused, with the message, and not used.
    let other = f.empty.parent().unwrap().join("other");
    fs::create_dir(&other).unwrap();
    let theirs = f.tools.script(&other, "clan", "echo someone else's clan");
    let got = f.resolve(&locator, &other).await.unwrap();
    let PathClan::Different(msg) = &got.path_clan else {
        panic!("{:?}", got.path_clan)
    };
    assert!(
        msg.contains("clan on PATH is a different clan-core than the fleet pins"),
        "{msg}"
    );
    assert!(msg.contains(&theirs.display().to_string()), "{msg}");
    assert_eq!(got.path, pinned.path, "the pinned clan is used, not PATH's");
    assert!(
        got.trail.iter().any(|l| l.contains("different clan-core")),
        "{:?}",
        got.trail
    );

    // A non-executable `clan` on PATH is not a `clan` at all.
    let dud = f.empty.parent().unwrap().join("dud");
    fs::create_dir(&dud).unwrap();
    fs::write(dud.join("clan"), "").unwrap();
    assert_eq!(
        f.resolve(&locator, &dud).await.unwrap().path_clan,
        PathClan::Absent
    );
}

#[tokio::test]
async fn a_fleet_whose_lock_has_no_clan_core_is_refused() {
    let Some(f) = fixture(true) else { return };
    fs::write(f.fleet.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    lock(&f.fleet);
    let e = f.resolve(&ClanLocator::new(), &f.empty).await.unwrap_err();
    assert_eq!(e.step, ClanStep::Lock);
    assert!(e.reason.contains("no input named clan-core"), "{e}");
    assert_eq!(f.calls("build"), 0);
}

#[tokio::test]
async fn a_lock_that_nix_would_have_to_extend_is_refused_not_silently_relocked() {
    let Some(f) = fixture(true) else { return };
    // The lock is made for a flake.nix with no inputs; flake.nix then gains `clan-core`, and
    // nobody re-locks. nix would lock it in memory, to whatever the URL says today: not a pin.
    let with_input = fs::read_to_string(f.fleet.join("flake.nix")).unwrap();
    // (An unrelated input, so that there is a lock file at all: nix writes none for no inputs.)
    let core = f.core.display();
    fs::write(
        f.fleet.join("flake.nix"),
        format!("{{ inputs.other.url = \"git+file://{core}\"; outputs = _: {{}}; }}\n"),
    )
    .unwrap();
    fs::remove_file(f.fleet.join("flake.lock")).unwrap();
    lock(&f.fleet);
    fs::write(f.fleet.join("flake.nix"), with_input).unwrap();
    let before = fs::read(f.fleet.join("flake.lock")).unwrap();
    let e = f.resolve(&ClanLocator::new(), &f.empty).await.unwrap_err();
    assert_eq!(e.step, ClanStep::Lock);
    assert!(e.reason.contains("not a pin"), "{e}");
    assert_eq!(
        fs::read(f.fleet.join("flake.lock")).unwrap(),
        before,
        "the lock was rewritten"
    );
    assert_eq!(f.calls("build"), 0);
}
#[tokio::test]
async fn a_clan_core_without_a_clan_cli_fails_the_build_step_with_nixs_reason() {
    let Some(f) = fixture(false) else { return };
    let e = f.resolve(&ClanLocator::new(), &f.empty).await.unwrap_err();
    assert_eq!(e.step, ClanStep::Build);
    assert!(e.reason.contains("exited 1"), "{e}");
    assert!(e.reason.contains("clan-cli"), "{e}");
}

#[tokio::test]
async fn a_system_that_is_not_a_system_name_is_refused_before_anything_runs() {
    let Some(f) = fixture(true) else { return };
    let mut p = ResolveParams::new();
    p.nix = f.nix.as_os_str();
    p.system = Some("x86_64-linux.evil\"");
    let e = ClanLocator::new()
        .resolve(&p, &f.flake(), &CancellationToken::new())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(e.step, ClanStep::Build);
    assert_eq!(f.calls("flake"), 0);
}
