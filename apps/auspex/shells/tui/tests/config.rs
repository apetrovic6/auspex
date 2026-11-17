//! The config file: the home-manager module's contract fixture, unknown keys, wrong types,
//! a missing default file, a missing explicit file, and the flake path's precedence.

mod common;

use std::collections::BTreeMap;

use auspex_tui::app::core_config;
use auspex_tui::config::{self, FlakeSource, MachineSettings, PrometheusSettings, resolve_flake};
use auspex_tui::keymap::{Action, Keymap};
use common::{auspex, crate_dir, run_within, tempdir};

/// `nix/auspex/expected.toml` is what the home-manager module writes for its full set of
/// options (`nix/auspex/checks.nix`, `auspex-full`). Every value must arrive here, so the
/// module and the binary cannot drift apart.
#[test]
fn the_home_manager_contract_fixture_loads_whole() {
    let fixture = crate_dir().join("../../../../nix/auspex/expected.toml");
    let loaded = config::load(Some(&fixture)).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    let s = loaded.settings;
    assert_eq!(s.flake.as_deref(), Some("/home/me/clan/fleet"));
    assert_eq!(
        (
            s.timeouts.ssh_connect,
            s.timeouts.probe,
            s.timeouts.eval,
            s.timeouts.settle
        ),
        (5, 10, 300, 120)
    );
    assert_eq!((s.eval.workers, s.eval.max_memory_mb), (4, 4096));
    assert_eq!(
        s.prometheus,
        Some(PrometheusSettings {
            url: "http://prometheus.example:9090".to_owned(),
            stale_after: 60,
        })
    );
    assert_eq!(s.cache_url.as_deref(), Some("https://ncps.example"));
    assert_eq!(
        s.machines,
        BTreeMap::from([(
            "terra".to_owned(),
            MachineSettings {
                target: Some("root@192.168.1.10".to_owned()),
                prometheus_instance: Some("terra:9100".to_owned()),
            }
        )])
    );
    assert_eq!(
        s.keymap,
        BTreeMap::from([
            ("quit".to_owned(), vec!["q".to_owned(), "esc".to_owned()]),
            ("down".to_owned(), vec!["j".to_owned()]),
        ])
    );
    // `~/` is expanded against HOME; only the tail is fixed.
    assert!(
        s.theme_file
            .as_ref()
            .unwrap()
            .ends_with(".config/auspex/theme.toml"),
        "{:?}",
        s.theme_file
    );

    // And each value reaches the place that uses it.
    let mut w = Vec::new();
    let km = Keymap::with_overrides(&s.keymap, &mut w).unwrap();
    assert_eq!(km.keys_for(Action::Quit), ["q", "esc"]);
    assert_eq!(km.keys_for(Action::Down), ["j"]);
    let core = core_config(&s, "/fleet".to_owned());
    assert_eq!(core.target_overrides["terra"], "root@192.168.1.10");
    let prom = core.prometheus.unwrap();
    assert_eq!(prom.instances["terra"], "terra:9100");
    assert_eq!(prom.max_age_seconds, 60);
    assert!(core.binary_cache);
}

#[test]
fn an_unknown_key_is_a_warning_naming_it() {
    let (s, w) = config::parse(
        "future_feature = true\n[machines.terra]\nfrozen = true\ntarget = \"root@10.0.0.1\"\n",
    )
    .unwrap();
    assert_eq!(s.machines["terra"].target.as_deref(), Some("root@10.0.0.1"));
    assert_eq!(w.len(), 2, "{w:?}");
    assert!(w.iter().any(|w| w.contains("`future_feature`")));
    assert!(w.iter().any(|w| w.contains("`machines.terra.frozen`")));
}

#[test]
fn a_wrong_type_is_an_error_naming_the_key() {
    for (text, key) in [
        ("flake = 3\n", "`flake`"),
        ("[timeouts]\nprobe = \"ten\"\n", "`timeouts.probe`"),
        ("timeouts = 5\n", "`timeouts`"),
        ("[prometheus]\nurl = [\"a\"]\n", "`prometheus.url`"),
        ("[machines]\nterra = \"x\"\n", "`machines.terra`"),
        ("[keymap]\nquit = 1\n", "`keymap.quit`"),
        ("[keymap]\nquit = [\"q\", 2]\n", "`keymap.quit`"),
        ("[theme]\nfile = false\n", "`theme.file`"),
    ] {
        let e = config::parse(text).unwrap_err();
        assert!(e.contains(key), "{text:?}: {e}");
    }
}

#[test]
fn a_missing_default_file_is_no_error_and_a_missing_explicit_one_is() {
    let dir = tempdir();
    // Missing default: the binary starts (the demo, so nothing else is touched).
    let out = run_within(
        auspex(dir.path()).args(["--demo", "--json"]),
        std::time::Duration::from_secs(60),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Missing explicit: a usage error naming the file.
    let missing = dir.path().join("nope.toml");
    let out = run_within(
        auspex(dir.path())
            .args(["--demo", "--json", "--config"])
            .arg(&missing),
        std::time::Duration::from_secs(60),
    );
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(out.stdout, b"");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nope.toml"), "{err}");
}

#[test]
fn a_config_error_exits_2_before_anything_runs() {
    let dir = tempdir();
    let file = dir.path().join("bad.toml");
    std::fs::write(&file, "[eval]\nworkers = 0\n").unwrap();
    let out = run_within(
        auspex(dir.path())
            .args(["--demo", "--plain", "--config"])
            .arg(&file),
        std::time::Duration::from_secs(60),
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("`eval.workers`"));
}

#[test]
fn the_flake_comes_from_the_flag_then_clan_dir_then_the_file_then_the_current_directory() {
    assert_eq!(
        resolve_flake(Some("/f"), Some("/c"), Some("/k")),
        ("/f".to_owned(), FlakeSource::Flag)
    );
    assert_eq!(
        resolve_flake(None, Some("/c"), Some("/k")),
        ("/c".to_owned(), FlakeSource::ClanDir)
    );
    assert_eq!(
        resolve_flake(None, Some(""), Some("/k")),
        ("/k".to_owned(), FlakeSource::ConfigFile),
        "an empty CLAN_DIR is unset"
    );
    assert_eq!(
        resolve_flake(None, None, Some("/k")),
        ("/k".to_owned(), FlakeSource::ConfigFile)
    );
    assert_eq!(
        resolve_flake(None, None, None),
        (".".to_owned(), FlakeSource::Default)
    );
}

fn bump(v: &mut toml::Value) {
    match v {
        toml::Value::Integer(i) => *i += 1,
        toml::Value::Table(t) => t.iter_mut().for_each(|(_, v)| bump(v)),
        _ => {}
    }
}

/// Every number in the fixture happens to equal its default, so a key that was never read
/// would pass the test above. Bump each one and check each bump arrives.
#[test]
fn every_number_in_the_fixture_is_read_not_defaulted() {
    let text =
        std::fs::read_to_string(crate_dir().join("../../../../nix/auspex/expected.toml")).unwrap();
    let mut table: toml::Table = toml::from_str(&text).unwrap();
    table.iter_mut().for_each(|(_, v)| bump(v));
    let (s, w) = config::parse(&toml::to_string(&table).unwrap()).unwrap();
    assert_eq!(w, Vec::<String>::new());
    assert_eq!(
        (
            s.timeouts.ssh_connect,
            s.timeouts.probe,
            s.timeouts.eval,
            s.timeouts.settle
        ),
        (6, 11, 301, 121)
    );
    assert_eq!((s.eval.workers, s.eval.max_memory_mb), (5, 4097));
    assert_eq!(s.prometheus.unwrap().stale_after, 61);
}

/// Review finding 2: a huge timeout overflowed `Instant + Duration` inside an adapter and
/// panicked its task. Every timeout is between 1 s and a day, and the error names the key.
#[test]
fn timeouts_are_between_one_second_and_a_day() {
    for key in ["ssh_connect", "probe", "eval", "settle"] {
        for bad in ["0", "86401", "9223372036854775807"] {
            let e = config::parse(&format!("[timeouts]\n{key} = {bad}\n")).unwrap_err();
            assert!(
                e.contains(&format!("`timeouts.{key}`")),
                "{key} = {bad}: {e}"
            );
        }
    }
    let (s, _) = config::parse("[timeouts]\neval = 86400\nsettle = 86400\nprobe = 1\n").unwrap();
    assert_eq!(
        (s.timeouts.eval, s.timeouts.settle, s.timeouts.probe),
        (86_400, 86_400, 1)
    );
}

#[test]
fn a_settle_shorter_than_the_probe_is_a_warning() {
    let (_, w) = config::parse("[timeouts]\nsettle = 5\nprobe = 10\n").unwrap();
    assert!(
        w.iter()
            .any(|w| w.contains("timeouts.settle") && w.contains("timeouts.probe")),
        "{w:?}"
    );
    let (_, w) = config::parse("[timeouts]\nsettle = 10\nprobe = 10\n").unwrap();
    assert_eq!(w, Vec::<String>::new());
}

/// Review finding 6: `stale_after` without a `url` was dropped silently.
#[test]
fn prometheus_settings_without_a_url_are_a_warning() {
    let (s, w) = config::parse("[prometheus]\nstale_after = 30\n").unwrap();
    assert_eq!(s.prometheus, None);
    assert!(w.iter().any(|w| w.contains("prometheus.url")), "{w:?}");
}

/// Review finding 4: config, keymap and argument errors quote the operator's text, and were
/// printed raw. An escape sequence or a control character there must not reach the terminal.
#[test]
fn errors_on_stderr_carry_no_escape_sequences() {
    let dir = tempdir();
    let cases = [
        // A quoted TOML key with an escape, holding the wrong type: an error naming it.
        (
            "key.toml",
            "[machines.\"evil\\u001b]0;pwned\\u0007\"]\ntarget = 1\n",
        ),
        // A keymap value that is an OSC sequence: an error quoting it.
        (
            "keymap.toml",
            "[keymap]\ndown = \"\\u001b]0;pwned\\u0007\"\n",
        ),
        // An unknown key with an escape: a warning (the run goes on).
        ("warn.toml", "\"x\\u001b[31my\" = 1\n"),
    ];
    for (name, text) in cases {
        let file = dir.path().join(name);
        std::fs::write(&file, text).unwrap();
        let out = run_within(
            auspex(dir.path())
                .args(["--demo", "--plain", "--config"])
                .arg(&file),
            std::time::Duration::from_secs(60),
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.is_empty(), "{name}: something is said");
        assert!(
            !stderr.chars().any(|c| c == '\u{1b}' || c == '\u{7}'),
            "{name}: {stderr:?}"
        );
    }
    let out = run_within(
        auspex(dir.path()).arg("--\u{1b}]0;pwned\u{7}"),
        std::time::Duration::from_secs(60),
    );
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The sanitiser drops an OSC sequence whole, payload included.
    assert!(stderr.contains("unexpected argument"), "{stderr:?}");
    assert!(
        !stderr.chars().any(|c| c == '\u{1b}' || c == '\u{7}'),
        "{stderr:?}"
    );
}

/// Review mutation A: `stale_after` was parsed but nothing checked it reaches the core.
#[test]
fn stale_after_reaches_the_core() {
    let (s, _) = config::parse("[prometheus]\nurl = \"http://p\"\nstale_after = 33\n").unwrap();
    assert_eq!(
        core_config(&s, ".".to_owned())
            .prometheus
            .unwrap()
            .max_age_seconds,
        33
    );
}
