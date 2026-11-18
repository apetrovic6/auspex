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
    ClanBinary, ClanError, ClanLocator, ClanSource, ClanStep, PathClan, current_system,
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

/// A revision a tarball input claims (nix takes `rev` on a tarball URL as given).
const TARBALL_REV: &str = "ae4bb5d7ae4bb5d7ae4bb5d7ae4bb5d7ae4bb5d7";

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
    /// Where the GC root links go: never the operator's own state directory.
    gcroots: PathBuf,
}

/// `None` (after saying why) when the fixture cannot be built here.
fn fixture(core_has_package: bool) -> Option<Fixture> {
    fixture_as(core_has_package, false)
}

/// With `tarball`, the fleet pins clan-core as a tarball with a `rev`, the shape of the real
/// fleet's lock entry (`{type: tarball, rev, narHash}`), instead of a git input.
fn fixture_as(core_has_package: bool, tarball: bool) -> Option<Fixture> {
    fixture_hook(core_has_package, tarball, |_, _| {})
}

/// As [`fixture_as`], with `hook(core, root)` run on the clan-core checkout before its first
/// commit.
fn fixture_hook(
    core_has_package: bool,
    tarball: bool,
    hook: impl FnOnce(&Path, &Path),
) -> Option<Fixture> {
    for tool in ["nix", "git"] {
        if find_on_path(tool).is_none() {
            eprintln!("SKIPPED: {tool} is not on PATH (as inside the ci-pure sandbox)");
            return None;
        }
    }
    let tools = tools(&["nix"])?;
    let root = tempdir();
    let (core, fleet, empty, gcroots) = (
        root.path().join("core"),
        root.path().join("fleet"),
        root.path().join("empty"),
        root.path().join("gcroots"),
    );
    for d in [&core, &fleet, &empty] {
        fs::create_dir(d).unwrap();
    }
    write_core(&core, "A", core_has_package);
    hook(&core, root.path());
    git(&core, &["init", "-q"]);
    git(&core, &["add", "-A"]);
    git(&core, &["commit", "-q", "-m", "A"]);
    let url = if tarball {
        let archive = root.path().join("core.tar.gz");
        git(
            &core,
            &[
                "archive",
                "--format=tar.gz",
                "-o",
                archive.to_str().unwrap(),
                "HEAD",
            ],
        );
        format!("tarball+file://{}?rev={TARBALL_REV}", archive.display())
    } else {
        format!("git+file://{}", core.display())
    };
    fs::write(
        fleet.join("flake.nix"),
        format!("{{\n  inputs.clan-core.url = \"{url}\";\n  outputs = _: {{}};\n}}\n"),
    )
    .unwrap();
    lock(&fleet);
    let wrapper = tools.script(root.path(), "nix", &wrapper_body(""));
    Some(Fixture {
        _root: root,
        tools,
        core,
        fleet,
        empty,
        nix: wrapper,
        gcroots,
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

/// A `nix` that logs its subcommand (`calls`), logs what `nix build` printed (`outs`), runs
/// `before` (shell) first, and then runs the real one.
fn wrapper_body(before: &str) -> String {
    format!(
        "echo \"$1\" >> '@dir@/calls'\n{before}\nif [ \"$1\" = build ]; then\n  out=$(@nix@ \"$@\") || exit $?\n  printf '%s\\n' \"$out\" >> '@dir@/outs'\n  printf '%s\\n' \"$out\"\nelse\n  exec @nix@ \"$@\"\nfi"
    )
}

impl Fixture {
    /// Another wrapper, in the same directory, that runs `before` first.
    fn wrapper(&self, name: &str, before: &str) -> PathBuf {
        self.tools
            .script(self.nix.parent().unwrap(), name, &wrapper_body(before))
    }

    async fn resolve_flake(
        &self,
        flake: &FlakeRef,
        locator: &ClanLocator,
        search: &Path,
    ) -> Result<ClanBinary, ClanError> {
        self.resolve_via(&self.nix, flake, locator, search).await
    }

    async fn resolve_via(
        &self,
        nix: &Path,
        flake: &FlakeRef,
        locator: &ClanLocator,
        search: &Path,
    ) -> Result<ClanBinary, ClanError> {
        let mut p = common::resolve_params();
        p.nix = nix.as_os_str();
        p.search_path = Some(search.as_os_str());
        p.gcroot_dir = Some(&self.gcroots);
        p.lock_timeout = Duration::from_secs(120);
        p.build_timeout = Duration::from_secs(300);
        locator
            .resolve(&p, flake, &CancellationToken::new())
            .await
            .unwrap()
    }

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
        self.resolve_flake(&self.flake(), locator, search).await
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
    // The canonical path is what is reported, not `<out>/bin/clan` (a symlink here).
    assert_eq!(got.path, fs::canonicalize(&got.path).unwrap());
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
        msg.contains(
            "clan on PATH is a different clan-cli derivation than the fleet's locked clan-core builds"
        ),
        "{msg}"
    );
    assert!(msg.contains(&theirs.display().to_string()), "{msg}");
    assert_eq!(got.path, pinned.path, "the pinned clan is used, not PATH's");
    assert!(
        got.trail
            .iter()
            .any(|l| l.contains("different clan-cli derivation")),
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
    // nix itself refuses (`--no-update-lock-file` without `--no-write-lock-file`); the disk check
    // behind it would say "not a pin" for a nix that only warned.
    assert!(e.reason.contains("requires lock file changes"), "{e}");
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
    let mut p = common::resolve_params();
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

#[tokio::test]
async fn a_shallow_clone_of_a_git_fleet_resolves() {
    let Some(f) = fixture(true) else { return };
    // The fleet as a git repository with two commits, cloned with --depth 1: nix refuses
    // `nix flake metadata` on such a clone ("is a shallow Git repository, so 'revCount' is
    // not available") unless the reference says `shallow=1`.
    git(&f.fleet, &["init", "-q"]);
    git(&f.fleet, &["add", "flake.nix", "flake.lock"]);
    git(&f.fleet, &["commit", "-q", "-m", "one"]);
    fs::write(f.fleet.join("notes"), "x").unwrap();
    git(&f.fleet, &["add", "notes"]);
    git(&f.fleet, &["commit", "-q", "-m", "two"]);
    let shallow = f.empty.parent().unwrap().join("shallow");
    git(
        &f.empty,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", f.fleet.display()),
            shallow.to_str().unwrap(),
        ],
    );
    assert!(
        shallow.join(".git/shallow").exists(),
        "the clone is not shallow"
    );
    let flake = FlakeRef::new(shallow.to_str().unwrap()).unwrap();
    assert!(
        flake.as_str().starts_with("git+file://"),
        "{}",
        flake.as_str()
    );
    let rev = git(&f.core, &["rev-parse", "HEAD"]);
    let got = f
        .resolve_flake(&flake, &ClanLocator::new(), &f.empty)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(got.revision.as_deref(), Some(rev.as_str()));
    assert_eq!(output_of(&got.path), "fake clan A");
}

#[tokio::test]
async fn a_tarball_clan_core_with_a_rev_is_pinned_and_built() {
    let Some(f) = fixture_as(true, true) else {
        return;
    };
    // The fixture is what it claims to be: the lock entry is a tarball that carries a rev.
    let lock: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(f.fleet.join("flake.lock")).unwrap()).unwrap();
    let locked = &lock["nodes"]["clan-core"]["locked"];
    assert_eq!(locked["type"], "tarball", "{locked}");
    assert_eq!(locked["rev"], TARBALL_REV, "{locked}");
    assert!(locked["narHash"].is_string(), "{locked}");

    let got = f
        .resolve(&ClanLocator::new(), &f.empty)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(got.revision.as_deref(), Some(TARBALL_REV));
    assert_eq!(
        got.source,
        ClanSource::FleetLock {
            revision: TARBALL_REV.to_owned()
        }
    );
    assert_eq!(got.source.to_string(), "fleet lock ae4bb5d");
    assert_eq!(output_of(&got.path), "fake clan A");
}

// --- Fix round 2 ---

/// Commits a later clan-core ("B"), locks the fleet to it, and returns (lock A, lock B,
/// rev A, rev B), with lock A restored on disk.
fn lock_a_and_b(f: &Fixture) -> (Vec<u8>, Vec<u8>, String, String) {
    let lock_file = f.fleet.join("flake.lock");
    let (lock_a, rev_a) = (
        fs::read(&lock_file).unwrap(),
        git(&f.core, &["rev-parse", "HEAD"]),
    );
    write_core(&f.core, "B", true);
    git(&f.core, &["add", "-A"]);
    git(&f.core, &["commit", "-q", "-m", "B"]);
    let rev_b = git(&f.core, &["rev-parse", "HEAD"]);
    nix(&[
        "flake",
        "update",
        "--flake",
        &format!("path:{}", f.fleet.display()),
        "clan-core",
    ]);
    let lock_b = fs::read(&lock_file).unwrap();
    fs::write(&lock_file, &lock_a).unwrap();
    assert_ne!(lock_a, lock_b);
    (lock_a, lock_b, rev_a, rev_b)
}

/// F1 (the review's R1): the lock changes while `nix build` is starting. What is reported
/// and cached for lock A must be A's binary, never B's under A's name.
#[tokio::test]
async fn a_lock_swapped_just_before_the_build_is_never_built_as_the_old_one() {
    let Some(f) = fixture(true) else { return };
    let (lock_a, lock_b, rev_a, rev_b) = lock_a_and_b(&f);
    let root = f.nix.parent().unwrap();
    fs::write(root.join("lockB"), &lock_b).unwrap();
    // Once, just before `nix build`, a `git pull` or `nix flake update` lands lock B.
    let swap = f.wrapper(
        "nix-swap",
        &format!(
            "if [ \"$1\" = build ] && [ ! -e '@dir@/swapped' ]; then\n  cat '@dir@/lockB' > '{}'\n  echo x > '@dir@/swapped'\nfi",
            f.fleet.join("flake.lock").display()
        ),
    );
    let locator = ClanLocator::new();
    let first = f
        .resolve_via(&swap, &f.flake(), &locator, &f.empty)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(root.join("swapped").exists(), "the swap never happened");
    // What it says it is, it is: A, labelled A.
    assert_eq!(first.revision.as_deref(), Some(rev_a.as_str()));
    assert_eq!(output_of(&first.path), "fake clan A");

    // Lock B is on disk now: B is resolved as B.
    let second = f
        .resolve_via(&swap, &f.flake(), &locator, &f.empty)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(second.revision.as_deref(), Some(rev_b.as_str()));
    assert_eq!(output_of(&second.path), "fake clan B");

    // And back to A: the cache entry for lock A holds A's binary.
    fs::write(f.fleet.join("flake.lock"), &lock_a).unwrap();
    let third = f
        .resolve_via(&swap, &f.flake(), &locator, &f.empty)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(third.revision.as_deref(), Some(rev_a.as_str()));
    assert_eq!(output_of(&third.path), "fake clan A");
}

/// F1, the registry experiment: when the fleet has no `clan-core` input at the moment of the
/// build, `--inputs-from` registered nothing and `clan-core` fell through to the registry.
/// A registry entry named `clan-core` must never be used.
#[tokio::test]
async fn a_registry_entry_named_clan_core_is_never_used() {
    let Some(f) = fixture(true) else { return };
    let root = f.nix.parent().unwrap().to_owned();
    let evil = root.join("evil");
    fs::create_dir(&evil).unwrap();
    write_core(&evil, "EVIL", true);
    fs::write(
        root.join("registry.json"),
        format!(
            r#"{{"version":2,"flakes":[{{"from":{{"type":"indirect","id":"clan-core"}},"to":{{"type":"path","path":"{}"}}}}]}}"#,
            evil.display()
        ),
    )
    .unwrap();
    let fleet = f.fleet.display();
    // The registry applies to every nix call, and the fleet loses its input just before the
    // build.
    let wrapper = f.wrapper(
        "nix-reg",
        &format!(
            "export NIX_CONFIG=\"${{NIX_CONFIG-}}\nflake-registry = @dir@/registry.json\"\nif [ \"$1\" = build ]; then\n  printf '{{ outputs = _: {{}}; }}\\n' > '{fleet}/flake.nix'\n  printf '{{\"nodes\":{{\"root\":{{}}}},\"root\":\"root\",\"version\":7}}\\n' > '{fleet}/flake.lock'\nfi"
        ),
    );
    let rev = git(&f.core, &["rev-parse", "HEAD"]);
    let got = f
        .resolve_via(&wrapper, &f.flake(), &ClanLocator::new(), &f.empty)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(got.revision.as_deref(), Some(rev.as_str()));
    assert_eq!(
        output_of(&got.path),
        "fake clan A",
        "the registry's clan-core was used"
    );
}

/// F3 (R2): `bin/clan` that is a symlink out of the store is not a store path.
#[tokio::test]
async fn a_bin_clan_that_leaves_the_store_is_refused() {
    let Some(f) = fixture_hook(true, false, |core, root| {
        let outside = root.join("outside-clan");
        fs::write(&outside, "#!/bin/sh\necho mutable file outside the store\n").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
        let clan = core.join("pkg/bin/clan");
        fs::remove_file(&clan).unwrap();
        symlink(&outside, &clan).unwrap();
    }) else {
        return;
    };
    let e = f.resolve(&ClanLocator::new(), &f.empty).await.unwrap_err();
    assert_eq!(e.step, ClanStep::Binary, "{e}");
    assert!(e.reason.contains("outside the nix store"), "{e}");
}

/// The fleet's `flake.nix` names a different `clan-core` than its lock does. nix itself must
/// refuse (`--no-update-lock-file` alone), and must not rewrite the lock.
async fn nix_refuses_a_stale_lock(f: &Fixture, flake: &FlakeRef, lock_file: &Path) {
    let before = fs::read(lock_file).unwrap();
    let e = f
        .resolve_flake(flake, &ClanLocator::new(), &f.empty)
        .await
        .unwrap_err();
    assert_eq!(e.step, ClanStep::Lock, "{e}");
    // The error is nix's own, not auspex's after the fact (that is `check_lock_file`'s).
    assert!(e.reason.contains("nix flake metadata exited"), "{e}");
    assert_eq!(
        fs::read(lock_file).unwrap(),
        before,
        "the lock was rewritten"
    );
    assert_eq!(f.calls("build"), 0);
}

fn point_flake_nix_elsewhere(f: &Fixture, dir: &Path) {
    // A second clan-core, not the locked one.
    let other = f.fleet.parent().unwrap().join("other-core");
    fs::create_dir(&other).unwrap();
    write_core(&other, "OTHER", true);
    git(&other, &["init", "-q"]);
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-q", "-m", "other"]);
    let text = fs::read_to_string(dir.join("flake.nix")).unwrap();
    fs::write(
        dir.join("flake.nix"),
        text.replace(
            &format!("file://{}", f.core.display()),
            &format!("file://{}", other.display()),
        ),
    )
    .unwrap();
}

#[tokio::test]
async fn a_path_fleet_whose_flake_nix_disagrees_with_its_lock_is_refused_by_nix() {
    let Some(f) = fixture(true) else { return };
    point_flake_nix_elsewhere(&f, &f.fleet);
    let flake = f.flake();
    assert!(flake.as_str().starts_with("path:"), "{}", flake.as_str());
    nix_refuses_a_stale_lock(&f, &flake, &f.fleet.join("flake.lock")).await;
}

#[tokio::test]
async fn a_git_fleet_whose_flake_nix_disagrees_with_its_lock_is_refused_by_nix() {
    let Some(f) = fixture(true) else { return };
    git(&f.fleet, &["init", "-q"]);
    git(&f.fleet, &["add", "flake.nix", "flake.lock"]);
    git(&f.fleet, &["commit", "-q", "-m", "one"]);
    point_flake_nix_elsewhere(&f, &f.fleet);
    git(&f.fleet, &["commit", "-q", "-am", "two"]);
    let flake = f.flake();
    assert!(
        flake.as_str().starts_with("git+file:"),
        "{}",
        flake.as_str()
    );
    nix_refuses_a_stale_lock(&f, &flake, &f.fleet.join("flake.lock")).await;
}

/// F5 (R3): the cache key has the system and the nix program in it.
#[tokio::test]
async fn the_cache_key_has_the_system_and_the_nix_program_in_it() {
    let Some(f) = fixture(true) else { return };
    let locator = ClanLocator::new();
    f.resolve(&locator, &f.empty).await.unwrap();
    assert_eq!(f.calls("build"), 1);

    // Another system is another build. The fake core has only this machine's, so it fails:
    // it must not be answered with this machine's binary from the cache.
    let mut p = common::resolve_params();
    p.nix = f.nix.as_os_str();
    p.search_path = Some(f.empty.as_os_str());
    p.gcroot_dir = Some(&f.gcroots);
    p.system = Some("aarch64-plan9");
    let other = locator
        .resolve(&p, &f.flake(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(other.is_err(), "answered from the cache: {other:?}");
    assert_eq!(f.calls("build"), 2);

    // Another nix program is another build too.
    let nix2 = f.wrapper("nix2", "");
    let again = f
        .resolve_via(&nix2, &f.flake(), &locator, &f.empty)
        .await
        .unwrap();
    assert_eq!(f.calls("build"), 3, "{:?}", again.trail);
}

/// Survivor M3: a cached build that has since been garbage collected is built again, and
/// `run` is never handed a path that is gone.
#[tokio::test]
async fn a_garbage_collected_cache_entry_is_built_again() {
    let unique = format!("GC-{}", std::process::id());
    let Some(f) = fixture_hook(true, false, |core, _| {
        // A derivation of its own, so that deleting it cannot touch another test's.
        // (its source directory too: a comment line in the script changes nothing it prints)
        let clan = core.join("pkg/bin/clan");
        let script = fs::read_to_string(&clan).unwrap();
        fs::write(&clan, format!("{script}# {unique}\n")).unwrap();
        let flake = fs::read_to_string(core.join("flake.nix")).unwrap();
        fs::write(
            core.join("flake.nix"),
            flake.replace(
                "name = \"clan-cli\"",
                &format!("name = \"clan-cli-{unique}\""),
            ),
        )
        .unwrap();
    }) else {
        return;
    };
    let locator = ClanLocator::new();
    let first = f.resolve(&locator, &f.empty).await.unwrap();
    assert_eq!(f.calls("build"), 1);
    // Unroot it first: the build is held by its link in the GC root directory.
    let links: Vec<_> = fs::read_dir(&f.gcroots)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(links.len(), 1, "{links:?}");
    fs::remove_file(&links[0]).unwrap();
    // Collect it: the build's output, and the store path its bin/clan lives in.
    let outs = fs::read_to_string(f.nix.parent().unwrap().join("outs")).unwrap();
    let out = outs.lines().next().unwrap().to_owned();
    let canonical = fs::canonicalize(&first.path).unwrap();
    let home: PathBuf = canonical.components().take(4).collect();
    // The derivation refers to its source too, so it goes with them.
    let drv = Command::new("nix")
        .args(["path-info", "--derivation", &out])
        .output()
        .unwrap();
    let drv = String::from_utf8(drv.stdout).unwrap().trim().to_owned();
    nix(&["store", "delete", &out, home.to_str().unwrap(), &drv]);
    assert!(!first.path.exists(), "the store path is still there");
    let second = f.resolve(&locator, &f.empty).await.unwrap();
    assert_eq!(f.calls("build"), 2, "the vanished build was trusted");
    assert_eq!(output_of(&second.path), "fake clan A");
}

/// F9: each build is rooted by a link in the GC root directory, one per (lock, system), so a
/// garbage collection between `resolve` and `run` cannot take the binary away.
#[tokio::test]
async fn a_built_clan_is_rooted_by_one_link_per_lock_and_system() {
    let Some(f) = fixture(true) else { return };
    let locator = ClanLocator::new();
    let first = f.resolve(&locator, &f.empty).await.unwrap();
    let outs = fs::read_to_string(f.nix.parent().unwrap().join("outs")).unwrap();
    let out = outs.lines().next().unwrap().to_owned();

    let links = |dir: &Path| -> Vec<PathBuf> {
        let mut v: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        v.sort();
        v
    };
    let one = links(&f.gcroots);
    assert_eq!(one.len(), 1, "{one:?}");
    let name = one[0].file_name().unwrap().to_str().unwrap().to_owned();
    assert!(
        name.ends_with(&format!("-{}", current_system())) && name.len() > 20,
        "{name}"
    );
    // A symlink to what was built, in a directory only we can use, outside the fleet.
    assert_eq!(fs::read_link(&one[0]).unwrap(), Path::new(&out));
    assert_eq!(
        fs::metadata(&f.gcroots).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(!f.gcroots.starts_with(&f.fleet));
    assert!(
        first.trail.iter().any(|l| l.contains(&name)),
        "{:?}",
        first.trail
    );

    // The same lock again, with an empty cache: the same link, replaced, not a second one.
    f.resolve(&ClanLocator::new(), &f.empty).await.unwrap();
    assert_eq!(links(&f.gcroots), one);
    assert_eq!(fs::read_link(&one[0]).unwrap(), Path::new(&out));
}

/// D: the root is named after the clan-core node, so a lock change that is not clan-core's
/// keeps the link; a new clan-core gets a new link and the older ones of ours go, and nothing
/// that is not ours does.
#[tokio::test]
async fn a_new_clan_core_replaces_the_old_root_and_only_ours_go() {
    let Some(f) = fixture(true) else { return };
    let locator = ClanLocator::new();
    f.resolve(&locator, &f.empty).await.unwrap();
    let outs = fs::read_to_string(f.nix.parent().unwrap().join("outs")).unwrap();
    let out = outs.lines().next().unwrap().to_owned();
    let links = |dir: &Path| -> Vec<PathBuf> {
        let mut v: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        v.sort();
        v
    };
    let one = links(&f.gcroots);
    assert_eq!(one.len(), 1, "{one:?}");

    // The fleet gains an unrelated input: the lock changes, the clan-core node does not, so it
    // is the same link (and the same build), not a second root.
    let unrelated = f.fleet.parent().unwrap().join("unrelated");
    fs::create_dir(&unrelated).unwrap();
    fs::write(unrelated.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    git(&unrelated, &["init", "-q"]);
    git(&unrelated, &["add", "-A"]);
    git(&unrelated, &["commit", "-q", "-m", "x"]);
    let flake_nix = fs::read_to_string(f.fleet.join("flake.nix")).unwrap();
    fs::write(
        f.fleet.join("flake.nix"),
        flake_nix.replace(
            "outputs",
            &format!(
                "inputs.unrelated.url = \"git+file://{}\";\n  outputs",
                unrelated.display()
            ),
        ),
    )
    .unwrap();
    lock(&f.fleet);
    f.resolve(&locator, &f.empty).await.unwrap();
    assert_eq!(
        links(&f.gcroots),
        one,
        "a lock change that is not clan-core's made a link"
    );

    // Things in the directory that are not auspex's links, and links that are.
    let system = current_system();
    let decoys: Vec<PathBuf> = [
        "notes.txt".to_owned(),
        format!("0123456789abcdef-{system}-dev"),
        "0123456789abcdef-aarch64-plan9".to_owned(),
    ]
    .iter()
    .map(|n| f.gcroots.join(n))
    .collect();
    fs::write(&decoys[0], "mine").unwrap();
    symlink("/nowhere", &decoys[1]).unwrap();
    symlink("/nowhere", &decoys[2]).unwrap();
    // A regular file with exactly the name of a link is not one.
    let regular = f.gcroots.join(format!("1111111111111111-{system}"));
    fs::write(&regular, "mine").unwrap();
    // An older build of ours for this system, left by an earlier clan-core.
    let older = f.gcroots.join(format!("2222222222222222-{system}"));
    symlink(&out, &older).unwrap();

    // Another clan-core: a new link, and the links of the builds before it are gone.
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
    let second = f.resolve(&locator, &f.empty).await.unwrap();
    let now = links(&f.gcroots);
    let newest: Vec<_> = now
        .iter()
        .filter(|p| {
            let name = p.file_name().unwrap().to_str().unwrap();
            name.ends_with(&format!("-{system}"))
                && name.len() == 16 + 1 + system.len()
                && p.symlink_metadata().unwrap().file_type().is_symlink()
        })
        .collect();
    assert_eq!(newest.len(), 1, "{now:?}");
    assert_ne!(newest[0], &one[0], "the link of the old clan-core was kept");
    assert!(!one[0].exists() && one[0].symlink_metadata().is_err());
    assert!(
        older.symlink_metadata().is_err(),
        "an older link of ours was kept"
    );
    for survivor in decoys.iter().chain([&regular]) {
        assert!(
            survivor.symlink_metadata().is_ok(),
            "{survivor:?} was removed"
        );
    }
    assert!(
        second.trail.iter().any(|l| l.contains("removed 2 GC root")),
        "{:?}",
        second.trail
    );
    assert_eq!(output_of(&second.path), "fake clan B");
}

/// B: with another input of the fleet unlocked (a `git+file` input whose tree is dirty, as when
/// an input is checked out for development), `--no-update-lock-file` is not a refusal: nix warns,
/// exits 0, and has relocked `clan-core` in memory to whatever `flake.nix` now names. The check
/// of the lock file on disk is then the only thing between auspex and a clan-core that is not
/// the pinned one.
#[tokio::test]
async fn with_an_unlocked_input_the_disk_check_is_what_refuses_a_stale_lock() {
    let Some(f) = fixture(true) else { return };
    // The fleet is locked (by the fixture) with `clan-core` only. Then `flake.nix` gains a second
    // input, a git repository whose tree is dirty, that the lock does not have: nix locks it in
    // memory, as an unlocked input.
    let dirty = f.fleet.parent().unwrap().join("dirty-input");
    fs::create_dir(&dirty).unwrap();
    fs::write(dirty.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    fs::write(dirty.join("data"), "committed\n").unwrap();
    git(&dirty, &["init", "-q"]);
    git(&dirty, &["add", "-A"]);
    git(&dirty, &["commit", "-q", "-m", "x"]);
    let flake_nix = fs::read_to_string(f.fleet.join("flake.nix")).unwrap();
    fs::write(
        f.fleet.join("flake.nix"),
        flake_nix.replace(
            "outputs",
            &format!(
                "inputs.dirty.url = \"git+file://{}\";\n  outputs",
                dirty.display()
            ),
        ),
    )
    .unwrap();
    let lock_file = f.fleet.join("flake.lock");
    let before = fs::read(&lock_file).unwrap();
    // And `flake.nix` names another clan-core than the lock does.
    fs::write(dirty.join("data"), "edited, not committed\n").unwrap();
    point_flake_nix_elsewhere(&f, &f.fleet);

    // The premise: nix itself does not refuse here. It exits 0, with clan-core relocked.
    let metadata = Command::new("nix")
        .args([
            "flake",
            "metadata",
            "--json",
            "--no-update-lock-file",
            &format!("path:{}", f.fleet.display()),
        ])
        .output()
        .unwrap();
    assert!(
        metadata.status.success(),
        "nix refused after all (a different nix?): {}",
        String::from_utf8_lossy(&metadata.stderr)
    );

    // auspex does, by the lock file on disk, not by nix's own error.
    let e = f.resolve(&ClanLocator::new(), &f.empty).await.unwrap_err();
    assert_eq!(e.step, ClanStep::Lock, "{e}");
    assert!(e.reason.contains("out of date"), "{e}");
    assert!(!e.reason.contains("nix flake metadata exited"), "{e}");
    assert_eq!(
        fs::read(&lock_file).unwrap(),
        before,
        "the lock was rewritten"
    );
    assert_eq!(f.calls("build"), 0);
}
