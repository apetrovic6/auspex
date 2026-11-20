//! Tests of the freeze state file. Every one works under its own temporary directory: a test
//! build panics if it is asked for the real state file (`forbid_default_location_in_tests`,
//! itself tested below), so a test that forgot cannot write to `~/.local/state/auspex`.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use tempfile::TempDir;

use super::*;

const FLEET: &str = "/fleets/home";

fn dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

/// `<dir>/state/state.toml`: the `state` directory does not exist yet, so the first write
/// creates it.
fn file_in(dir: &Path) -> PathBuf {
    dir.join("state").join(FILE_NAME)
}

fn store(dir: &Path) -> FreezeStore {
    FreezeStore::new(file_in(dir), FLEET.to_owned())
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

/// A state file with `body`, 0600, in a directory 0700.
fn write_file(dir: &Path, body: &[u8]) -> PathBuf {
    let path = file_in(dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn read_text(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

// ---------------------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------------------

#[test]
fn a_missing_file_is_the_empty_set_and_loading_creates_nothing() {
    let d = dir();
    assert_eq!(store(d.path()).load_blocking().unwrap(), set(&[]));
    assert!(
        !d.path().join("state").exists(),
        "reading must not create the directory"
    );
}

#[test]
fn freezing_creates_a_private_directory_and_file_and_survives_a_reload() {
    let d = dir();
    let s = store(d.path());
    assert_eq!(s.set_blocking("luna", true).unwrap(), set(&["luna"]));
    assert_eq!(s.set_blocking("sol", true).unwrap(), set(&["luna", "sol"]));
    let path = file_in(d.path());
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    assert_eq!(mode(&path), 0o600);
    // A new store over the same file, as a restart is.
    assert_eq!(
        store(d.path()).load_blocking().unwrap(),
        set(&["luna", "sol"])
    );
    let text = read_text(&path);
    assert!(text.contains("version = 1"), "{text}");
    assert!(text.contains(r#"frozen = ["luna", "sol"]"#), "{text}");
}

#[test]
fn unfreezing_removes_only_that_machine() {
    let d = dir();
    let s = store(d.path());
    s.set_blocking("luna", true).unwrap();
    s.set_blocking("sol", true).unwrap();
    assert_eq!(s.set_blocking("luna", false).unwrap(), set(&["sol"]));
    assert_eq!(s.load_blocking().unwrap(), set(&["sol"]));
    // Unfreezing what is not frozen is not an error, and changes nothing.
    assert_eq!(s.set_blocking("terra", false).unwrap(), set(&["sol"]));
    assert_eq!(s.set_blocking("sol", false).unwrap(), set(&[]));
    assert_eq!(s.load_blocking().unwrap(), set(&[]));
}

#[test]
fn two_fleets_in_one_file_do_not_share_freezes() {
    let d = dir();
    let home = FreezeStore::new(file_in(d.path()), "/fleets/home".to_owned());
    let work = FreezeStore::new(file_in(d.path()), "/fleets/work".to_owned());
    home.set_blocking("luna", true).unwrap();
    assert_eq!(work.load_blocking().unwrap(), set(&[]));
    assert_eq!(work.set_blocking("sol", true).unwrap(), set(&["sol"]));
    assert_eq!(home.load_blocking().unwrap(), set(&["luna"]));
    // Unfreezing in one leaves the other.
    work.set_blocking("sol", false).unwrap();
    work.set_blocking("luna", false).unwrap();
    assert_eq!(home.load_blocking().unwrap(), set(&["luna"]));
}

#[test]
fn a_write_keeps_the_keys_this_build_does_not_know() {
    let d = dir();
    let path = write_file(
        d.path(),
        b"version = 1\nfuture = \"yes\"\n[fleets.\"/other\"]\nfrozen = [\"mars\"]\nnote = 3\n\
          [fleets.\"/fleets/home\"]\nfrozen = [\"luna\"]\nsince = \"2026\"\n",
    );
    store(d.path()).set_blocking("sol", true).unwrap();
    let text = read_text(&path);
    assert!(text.contains("future = \"yes\""), "{text}");
    assert!(text.contains("note = 3"), "{text}");
    assert!(text.contains("since = \"2026\""), "{text}");
    let other = FreezeStore::new(path, "/other".to_owned());
    assert_eq!(other.load_blocking().unwrap(), set(&["mars"]));
    assert_eq!(
        store(d.path()).load_blocking().unwrap(),
        set(&["luna", "sol"])
    );
}

#[test]
fn a_name_that_is_not_plain_is_refused_before_anything_is_written() {
    let d = dir();
    for bad in ["", "-rf", "a b", "a/b", "..", "luna\n", &"x".repeat(64)] {
        let e = store(d.path()).set_blocking(bad, true).unwrap_err();
        assert!(matches!(e, Error::BadName(_)), "{bad:?}: {e}");
    }
    assert!(!d.path().join("state").exists());
}

// ---------------------------------------------------------------------------------------
// Atomic writes and concurrent writers
// ---------------------------------------------------------------------------------------

#[test]
fn a_write_leaves_no_temporary_file_behind() {
    let d = dir();
    let s = store(d.path());
    for i in 0..20 {
        s.set_blocking("luna", i % 2 == 0).unwrap();
    }
    let names: Vec<String> = std::fs::read_dir(d.path().join("state"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [FILE_NAME]);
}

/// The file is replaced by a rename, so a reader running while it is rewritten sees the old
/// file or the new one: never an empty or half-written file, which would read as corrupt.
#[test]
fn a_reader_never_sees_half_a_file_while_it_is_rewritten() {
    let d = dir();
    let s = store(d.path());
    // Many names, so the file is big enough for a torn write to be possible.
    let names: Vec<String> = (0..200).map(|i| format!("machine-{i:03}")).collect();
    for n in &names {
        s.set_blocking(n, true).unwrap();
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let (s, stop) = (s.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut reads = 0_u32;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let got = s.load_blocking();
                assert!(got.is_ok(), "a read saw a bad file: {got:?}");
                reads += 1;
            }
            reads
        })
    };
    for round in 0..300 {
        s.set_blocking("machine-000", round % 2 == 0).unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(reader.join().unwrap() > 0, "the reader never ran");
}

#[test]
fn a_failed_write_leaves_the_old_file_whole() {
    // As root a read-only directory is writable all the same.
    if rustix::process::getuid().is_root() {
        return;
    }
    let d = dir();
    let s = store(d.path());
    s.set_blocking("luna", true).unwrap();
    let before = read_text(&file_in(d.path()));
    let state = d.path().join("state");
    // Not writable, so the temporary file cannot be made: the rename never happens.
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o500)).unwrap();
    let e = s.set_blocking("sol", true);
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    // The private-directory rule refuses a 0500 directory before the write is tried; either
    // way nothing changed.
    assert!(e.is_err());
    assert_eq!(read_text(&file_in(d.path())), before);
    assert_eq!(s.load_blocking().unwrap(), set(&["luna"]));
}

#[test]
fn concurrent_writers_do_not_lose_each_others_freezes() {
    let d = dir();
    let n = 24;
    let barrier = Arc::new(Barrier::new(n));
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let s = store(d.path());
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                s.set_blocking(&format!("m{i:02}"), true).unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let all: BTreeSet<String> = (0..n).map(|i| format!("m{i:02}")).collect();
    assert_eq!(store(d.path()).load_blocking().unwrap(), all);
}

#[test]
fn a_writer_that_cannot_get_the_lock_says_busy_and_writes_nothing() {
    let d = dir();
    let s = store(d.path());
    s.set_blocking("luna", true).unwrap();
    let before = read_text(&file_in(d.path()));
    let (dir_fd, _) = s.prepare_dir().unwrap();
    let held = s.lock(&dir_fd).unwrap();
    let started = Instant::now();
    let e = s.set_blocking("sol", true).unwrap_err();
    assert!(matches!(e, Error::Busy(_)), "{e}");
    assert!(started.elapsed() >= LOCK_WAIT, "it waited its time first");
    drop(held);
    assert_eq!(read_text(&file_in(d.path())), before);
    assert_eq!(s.set_blocking("sol", true).unwrap(), set(&["luna", "sol"]));
}

// ---------------------------------------------------------------------------------------
// Corrupt, newer, and files that are not ours to use
// ---------------------------------------------------------------------------------------

#[test]
fn a_corrupt_file_is_refused_by_load_and_by_set_and_is_not_changed() {
    let bad: &[(&str, &[u8])] = &[
        ("not toml", b"this is { not toml\n"),
        ("empty", b""),
        (
            "no version",
            b"[fleets.\"/fleets/home\"]\nfrozen = [\"luna\"]\n",
        ),
        ("version a string", b"version = \"1\"\n"),
        ("version zero", b"version = 0\n"),
        ("fleets not a table", b"version = 1\nfleets = 3\n"),
        (
            "fleet not a table",
            b"version = 1\n[fleets]\n\"/fleets/home\" = 3\n",
        ),
        (
            "frozen not a list",
            b"version = 1\n[fleets.\"/fleets/home\"]\nfrozen = \"luna\"\n",
        ),
        (
            "frozen holds a number",
            b"version = 1\n[fleets.\"/fleets/home\"]\nfrozen = [1]\n",
        ),
        (
            "a bad machine name",
            b"version = 1\n[fleets.\"/fleets/home\"]\nfrozen = [\"-rf\"]\n",
        ),
        ("not utf-8", b"version = 1\n\xff\xfe\n"),
    ];
    for (what, body) in bad {
        let d = dir();
        let path = write_file(d.path(), body);
        let e = store(d.path()).load_blocking().unwrap_err();
        assert!(matches!(e, Error::Corrupt { .. }), "{what}: {e}");
        let e = store(d.path()).set_blocking("luna", true).unwrap_err();
        assert!(matches!(e, Error::Corrupt { .. }), "{what}: {e}");
        assert_eq!(std::fs::read(&path).unwrap(), *body, "{what}: file changed");
        // The note says what to do.
        assert!(e.to_string().contains("Fix or delete"), "{what}: {e}");
    }
}

#[test]
fn a_file_over_the_size_cap_is_corrupt() {
    let d = dir();
    let mut body = b"version = 1\n# ".to_vec();
    body.resize(usize::try_from(MAX_BYTES).unwrap() + 10, b'x');
    write_file(d.path(), &body);
    let e = store(d.path()).load_blocking().unwrap_err();
    assert!(matches!(e, Error::Corrupt { .. }), "{e}");
}

#[test]
fn a_newer_file_is_refused_and_never_written_over() {
    let d = dir();
    let body = b"version = 2\n[fleets.\"/fleets/home\"]\nfrozen = [\"luna\"]\nbrand_new = true\n";
    let path = write_file(d.path(), body);
    let e = store(d.path()).load_blocking().unwrap_err();
    assert!(matches!(e, Error::Newer { found: 2, .. }), "{e}");
    assert!(e.to_string().contains("newer auspex"), "{e}");
    let e = store(d.path()).set_blocking("sol", true).unwrap_err();
    assert!(matches!(e, Error::Newer { found: 2, .. }), "{e}");
    assert_eq!(std::fs::read(&path).unwrap(), body);
}

#[test]
fn a_file_others_can_read_is_refused() {
    let d = dir();
    let path = write_file(d.path(), b"version = 1\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let e = store(d.path()).load_blocking().unwrap_err();
    assert!(
        matches!(e, Error::Unusable(_)) && e.to_string().contains("0644"),
        "{e}"
    );
    let e = store(d.path()).set_blocking("luna", true).unwrap_err();
    assert!(matches!(e, Error::Unusable(_)), "{e}");
}

#[test]
fn a_symlinked_file_is_refused_not_followed() {
    let d = dir();
    let real = d.path().join("elsewhere.toml");
    std::fs::write(&real, "version = 1\n").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
    let path = file_in(d.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    symlink(&real, &path).unwrap();
    let e = store(d.path()).load_blocking().unwrap_err();
    assert!(e.to_string().contains("symlink"), "{e}");
    let e = store(d.path()).set_blocking("luna", true).unwrap_err();
    assert!(e.to_string().contains("symlink"), "{e}");
    assert_eq!(
        read_text(&real),
        "version = 1\n",
        "the target was not written"
    );
}

#[test]
fn a_file_with_two_names_is_refused() {
    let d = dir();
    let path = write_file(d.path(), b"version = 1\n");
    std::fs::hard_link(&path, d.path().join("other-name")).unwrap();
    let e = store(d.path()).load_blocking().unwrap_err();
    assert!(e.to_string().contains("hard links"), "{e}");
}

#[test]
fn a_directory_in_place_of_the_file_is_refused() {
    let d = dir();
    let path = file_in(d.path());
    std::fs::create_dir_all(&path).unwrap();
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let e = store(d.path()).load_blocking().unwrap_err();
    assert!(e.to_string().contains("not a regular file"), "{e}");
}

#[test]
fn a_state_directory_others_can_open_is_refused() {
    let d = dir();
    let path = write_file(d.path(), b"version = 1\n");
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let e = store(d.path()).load_blocking().unwrap_err();
    assert!(e.to_string().contains("0755"), "{e}");
    let e = store(d.path()).set_blocking("luna", true).unwrap_err();
    assert!(e.to_string().contains("0755"), "{e}");
}

// ---------------------------------------------------------------------------------------
// Where it lives, and which fleet
// ---------------------------------------------------------------------------------------

#[test]
fn the_default_path_follows_xdg_state_home_then_home_and_ignores_a_relative_one() {
    let p =
        |x: Option<&str>, h: Option<&str>| default_path_from(x.map(OsStr::new), h.map(OsStr::new));
    assert_eq!(
        p(Some("/xdg"), Some("/home/me")),
        Some(PathBuf::from("/xdg/auspex/state.toml"))
    );
    assert_eq!(
        p(None, Some("/home/me")),
        Some(PathBuf::from("/home/me/.local/state/auspex/state.toml"))
    );
    assert_eq!(
        p(Some("relative/state"), Some("/home/me")),
        Some(PathBuf::from("/home/me/.local/state/auspex/state.toml"))
    );
    assert_eq!(p(Some("relative"), Some("relative")), None);
    assert_eq!(p(None, None), None);
}

#[test]
fn a_fleet_is_keyed_by_its_canonical_path() {
    let d = dir();
    let real = d.path().join("fleet");
    std::fs::create_dir(&real).unwrap();
    let link = d.path().join("link");
    symlink(&real, &link).unwrap();
    let canonical = std::fs::canonicalize(&real)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let dotted = format!("{}/../fleet", real.display());
    for spelling in [
        real.to_string_lossy().into_owned(),
        link.to_string_lossy().into_owned(),
        dotted,
        format!("path:{}", real.display()),
    ] {
        assert_eq!(fleet_key(&spelling), canonical, "{spelling}");
    }
    // Not a local path: the reference as written.
    assert_eq!(fleet_key("github:me/fleet"), "github:me/fleet");
    assert_eq!(
        fleet_key("/nonexistent-auspex-fleet"),
        "/nonexistent-auspex-fleet"
    );
}

#[test]
fn the_guard_refuses_the_default_location_and_nothing_else() {
    // A pretend "real" file under a temporary directory: the real one is never touched.
    let d = dir();
    let pretend = file_in(d.path());
    let refused = std::panic::catch_unwind(|| {
        forbid_default_location_in_tests(&file_in(d.path()), Some(&pretend));
    });
    assert!(
        refused.is_err(),
        "the default location must panic in a test build"
    );
    // Another file in the same directory, and another directory, are fine.
    forbid_default_location_in_tests(&d.path().join("state").join("other.toml"), Some(&pretend));
    forbid_default_location_in_tests(&d.path().join("elsewhere").join(FILE_NAME), Some(&pretend));
    forbid_default_location_in_tests(&pretend, None);
}

// ---------------------------------------------------------------------------------------
// Fix round 1
// ---------------------------------------------------------------------------------------

#[test]
fn the_fleet_key_is_the_same_for_every_spelling_of_a_plain_local_fleet() {
    let d = dir();
    let real = d.path().join("fleet");
    std::fs::create_dir(&real).unwrap();
    let canonical = std::fs::canonicalize(&real)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let r = real.display();
    for spelling in [
        format!("git+file://{r}"),
        format!("git+file:{r}"),
        format!("path:{r}"),
        r.to_string(),
        format!("{r}/../fleet"),
    ] {
        assert_eq!(fleet_key(&spelling), canonical, "{spelling}");
    }
}

#[test]
fn a_reference_with_a_query_or_a_remote_host_is_not_taken_for_the_plain_path() {
    let d = dir();
    let real = d.path().join("fleet");
    std::fs::create_dir(&real).unwrap();
    let r = real.display();
    // `?dir=sub` names another flake than the repository at the path before it.
    for spelling in [
        format!("git+file://{r}?dir=sub"),
        format!("git+file://{r}?ref=main"),
        format!("path:{r}?dir=sub"),
        "git+file://otherhost/fleet".to_owned(),
        "github:me/fleet".to_owned(),
    ] {
        assert_eq!(fleet_key(&spelling), spelling, "{spelling}");
    }
}

#[test]
fn a_moved_fleet_is_another_fleet_and_its_freezes_stay_in_the_file() {
    let d = dir();
    let old = FreezeStore::new(file_in(d.path()), "/old/place/fleet".to_owned());
    let new = FreezeStore::new(file_in(d.path()), "/new/place/fleet".to_owned());
    old.set_blocking("luna", true).unwrap();
    assert_eq!(
        new.load_blocking().unwrap(),
        set(&[]),
        "not found under the new path"
    );
    new.set_blocking("sol", true).unwrap();
    assert_eq!(old.load_blocking().unwrap(), set(&["luna"]), "and not lost");
}

#[test]
fn a_write_can_carry_the_freezes_an_earlier_save_failed_to_keep() {
    let d = dir();
    let s = store(d.path());
    let also = set(&["luna", "terra"]);
    let got = s.set_with_blocking("sol", true, &also).unwrap();
    assert_eq!(got, set(&["luna", "sol", "terra"]));
    assert_eq!(s.load_blocking().unwrap(), got);
    // An unfreeze carries them too, and removes only its own machine.
    let got = s.set_with_blocking("sol", false, &set(&["mars"])).unwrap();
    assert_eq!(got, set(&["luna", "mars", "terra"]));
}

#[test]
fn a_name_that_is_not_plain_among_the_carried_ones_refuses_the_whole_write() {
    let d = dir();
    let s = store(d.path());
    s.set_blocking("luna", true).unwrap();
    let before = read_text(&file_in(d.path()));
    let e = s
        .set_with_blocking("sol", true, &set(&["fine", "-rf"]))
        .unwrap_err();
    assert!(matches!(e, Error::BadName(n) if n == "-rf"));
    assert_eq!(read_text(&file_in(d.path())), before, "nothing was written");
}

// The rules for a file that may be used, as a function of the numbers (as the action log's are),
// so that each is tested without another user's file.

fn facts(kind: FileType, owner: u32, mode: u32, links: u64) -> Result<()> {
    check_file_facts("/x/state.toml", kind, owner, 1000, mode, links)
}

#[test]
fn a_file_with_no_group_or_other_bit_that_is_ours_and_has_one_name_is_usable() {
    for mode in [0o600, 0o400, 0o200, 0o100_600] {
        assert!(
            facts(FileType::RegularFile, 1000, mode, 1).is_ok(),
            "{mode:o}"
        );
    }
    // A name that was just replaced under the reader has none left: not a second name.
    assert!(facts(FileType::RegularFile, 1000, 0o600, 0).is_ok());
}

#[test]
fn any_group_or_other_permission_bit_refuses_the_file() {
    for mode in [
        0o640, 0o660, 0o604, 0o620, 0o602, 0o601, 0o610, 0o644, 0o666, 0o700_077,
    ] {
        let e = facts(FileType::RegularFile, 1000, mode, 1).unwrap_err();
        assert!(e.to_string().contains("expected 0600"), "{mode:o}: {e}");
    }
}

#[test]
fn another_users_file_is_refused() {
    let e = facts(FileType::RegularFile, 1001, 0o600, 1).unwrap_err();
    assert!(e.to_string().contains("owned by uid 1001"), "{e}");
    let e = facts(FileType::RegularFile, 0, 0o600, 1).unwrap_err();
    assert!(e.to_string().contains("not by this user"), "{e}");
}

#[test]
fn a_symlink_a_directory_and_a_second_name_are_refused() {
    let e = facts(FileType::Symlink, 1000, 0o600, 1).unwrap_err();
    assert!(e.to_string().contains("symlink"), "{e}");
    let e = facts(FileType::Directory, 1000, 0o600, 1).unwrap_err();
    assert!(e.to_string().contains("not a regular file"), "{e}");
    let e = facts(FileType::RegularFile, 1000, 0o600, 2).unwrap_err();
    assert!(e.to_string().contains("2 hard links"), "{e}");
}

// Fix round 2, N1: `Stat`'s field widths differ by target (`st_mode` is a u32 on Linux and a u16
// on darwin, `st_nlink` a u64 on x86_64 Linux, a u32 on aarch64 Linux and a u16 on darwin), and
// the code that reads them must build on all of them. Another target's std is not available
// here, so the rules take any width that converts: this test is the guard, and stops
// compiling if the signature goes back to one fixed width.

#[test]
fn the_rules_take_every_width_the_stat_fields_have_on_any_target() {
    // darwin
    assert!(
        check_file_facts(
            "/x",
            FileType::RegularFile,
            1000,
            1000,
            0o100_600_u16,
            1_u16
        )
        .is_ok()
    );
    assert!(
        check_file_facts(
            "/x",
            FileType::RegularFile,
            1000,
            1000,
            0o100_640_u16,
            1_u16
        )
        .is_err()
    );
    assert!(
        check_file_facts(
            "/x",
            FileType::RegularFile,
            1000,
            1000,
            0o100_600_u16,
            2_u16
        )
        .is_err()
    );
    // aarch64 Linux
    assert!(
        check_file_facts(
            "/x",
            FileType::RegularFile,
            1000,
            1000,
            0o100_600_u32,
            1_u32
        )
        .is_ok()
    );
    assert!(
        check_file_facts(
            "/x",
            FileType::RegularFile,
            1000,
            1000,
            0o100_600_u32,
            2_u32
        )
        .is_err()
    );
    // x86_64 Linux
    assert!(
        check_file_facts(
            "/x",
            FileType::RegularFile,
            1000,
            1000,
            0o100_600_u32,
            1_u64
        )
        .is_ok()
    );
    assert!(
        check_file_facts(
            "/x",
            FileType::RegularFile,
            1000,
            1000,
            0o100_600_u32,
            2_u64
        )
        .is_err()
    );
}
