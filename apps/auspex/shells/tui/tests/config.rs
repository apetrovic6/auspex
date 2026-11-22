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
#[expect(
    clippy::too_many_lines,
    reason = "one assertion per option of the whole contract: split, it stops reading as one fixture"
)]
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
    assert_eq!(s.ssh.control_persist, 600);
    assert_eq!(s.timeouts.ssh_approval, 60);
    assert_eq!(s.timeouts.diff, 120);
    // The fixture says `false`, the opposite of the default, so a key never read fails here.
    assert!(!s.diff.enabled);
    assert_eq!(
        s.prometheus,
        Some(PrometheusSettings {
            url: "http://prometheus.example:9090".to_owned(),
            stale_after: 60,
        })
    );
    assert_eq!(s.cache_url.as_deref(), Some("https://ncps.example"));
    // The CI badge's forge: every key, so one never read fails here.
    assert_eq!(
        s.forge,
        Some(config::ForgeSettings {
            url: "https://forge.example.org/".to_owned(),
            host: "forge.example.org".to_owned(),
            repo: Some("owner/fleet".to_owned()),
            token_file: Some("/home/me/.config/auspex/forge-token".into()),
        })
    );
    // The runners view: every key, each the opposite of its default, so one never read fails here.
    let runners = s.runners.clone().unwrap();
    assert_eq!(runners.unit_glob, "ci-runner-*");
    assert_eq!(runners.memory_warn_percent, 80);
    assert_eq!(
        runners.forges,
        vec![
            config::RunnerForgeSettings {
                url: "https://forge.example.org/".to_owned(),
                // Two entries on one host are told apart by their scope.
                name: "forge.example.org (admin)".to_owned(),
                scope: auspex_domain::runners::Scope::Admin,
                token_file: Some("/home/me/.config/auspex/forge-admin-token".into()),
            },
            config::RunnerForgeSettings {
                url: "https://forge.example.org/".to_owned(),
                name: "forge.example.org (repo:owner/fleet)".to_owned(),
                scope: auspex_domain::runners::Scope::Repo {
                    owner: "owner".to_owned(),
                    name: "fleet".to_owned(),
                },
                token_file: None,
            },
        ]
    );
    assert_eq!(
        runners.hosts,
        BTreeMap::from([(
            "alpha".to_owned(),
            auspex_domain::runners::HostCaps {
                cpus: Some(2),
                memory_bytes: Some(4 << 30),
            }
        )])
    );
    assert_eq!(
        runners.machines,
        BTreeMap::from([("alpha-docker".to_owned(), "alpha".to_owned())])
    );
    assert_eq!(
        s.machines,
        BTreeMap::from([(
            "terra".to_owned(),
            MachineSettings {
                target: Some("root@192.168.1.10".to_owned()),
                prometheus_instance: Some("terra:9100".to_owned()),
                // The fixture says `true`, the opposite of the default.
                public: true,
                frozen: true,
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
    // `U`: the rolling rules, how many machines at once, and the kubeconfig, each the opposite of
    // what it would be if the key were never read.
    assert_eq!(
        s.rolling,
        auspex_domain::roll::RollRules::from([
            (
                "taghmata".to_owned(),
                auspex_domain::roll::RollRule {
                    kubernetes: true,
                    drain: true,
                    drain_timeout_seconds: 900,
                    etcd: true,
                }
            ),
            (
                "skitarii".to_owned(),
                auspex_domain::roll::RollRule {
                    kubernetes: false,
                    drain: false,
                    drain_timeout_seconds: auspex_domain::roll::DEFAULT_DRAIN_TIMEOUT_SECONDS,
                    etcd: false,
                }
            ),
        ])
    );
    assert_eq!((s.update.parallel, s.update.ready_timeout), (3, 300));
    assert_eq!(s.kubeconfig.as_deref(), Some("/home/me/.kube/fleet"));
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
    // `machines.terra.public = true` reaches the core, which is where typed confirmation is decided;
    // and the checks before an update share `timeouts.diff`.
    assert_eq!(
        core.public_machines,
        std::collections::BTreeSet::from(["terra".to_owned()])
    );
    assert_eq!(core.preflight_timeout_seconds, 120);
    let prom = core.prometheus.unwrap();
    assert_eq!(prom.instances["terra"], "terra:9100");
    assert_eq!(prom.max_age_seconds, 60);
    // `machines.terra.frozen` reaches the core as a declared freeze, and nothing else does.
    assert_eq!(
        core.freeze.declared,
        std::collections::BTreeSet::from(["terra".to_owned()])
    );
    assert_eq!(
        core.forge,
        Some(auspex_core::ForgeConfig {
            host: "forge.example.org".to_owned(),
            repo: Some("owner/fleet".to_owned()),
        }),
        "the core gets the host and the repository, never the URL or the token file"
    );
    assert!(core.binary_cache);
    assert_eq!(
        core.diff, None,
        "diff.enabled = false switches the preview off"
    );
}

#[test]
fn an_unknown_key_is_a_warning_naming_it() {
    let (s, w) = config::parse(
        "future_feature = true\n[machines.terra]\nhue = \"red\"\ntarget = \"root@10.0.0.1\"\n",
    )
    .unwrap();
    assert_eq!(s.machines["terra"].target.as_deref(), Some("root@10.0.0.1"));
    assert_eq!(w.len(), 2, "{w:?}");
    assert!(w.iter().any(|w| w.contains("`future_feature`")));
    assert!(w.iter().any(|w| w.contains("`machines.terra.hue`")));
}

#[test]
fn a_wrong_type_is_an_error_naming_the_key() {
    for (text, key) in [
        ("flake = 3\n", "`flake`"),
        ("[timeouts]\nprobe = \"ten\"\n", "`timeouts.probe`"),
        ("timeouts = 5\n", "`timeouts`"),
        ("[prometheus]\nurl = [\"a\"]\n", "`prometheus.url`"),
        ("[machines]\nterra = \"x\"\n", "`machines.terra`"),
        (
            "[machines.terra]\npublic = \"yes\"\n",
            "`machines.terra.public`",
        ),
        ("[machines.terra]\npublic = 1\n", "`machines.terra.public`"),
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
        resolve_flake(Some("/f"), Some("/c"), Some("/k")).unwrap(),
        ("/f".to_owned(), FlakeSource::Flag)
    );
    assert_eq!(
        resolve_flake(None, Some("/c"), Some("/k")).unwrap(),
        ("/c".to_owned(), FlakeSource::ClanDir)
    );
    assert_eq!(
        resolve_flake(None, Some(""), Some("/k")).unwrap(),
        ("/k".to_owned(), FlakeSource::ConfigFile),
        "an empty CLAN_DIR is unset"
    );
    assert_eq!(
        resolve_flake(None, None, Some("/k")).unwrap(),
        ("/k".to_owned(), FlakeSource::ConfigFile)
    );
    assert_eq!(
        resolve_flake(None, None, None).unwrap(),
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
    assert_eq!(s.ssh.control_persist, 601);
    assert_eq!(s.timeouts.ssh_approval, 61);
    assert_eq!(s.timeouts.diff, 121);
    assert_eq!(s.prometheus.unwrap().stale_after, 61);
    assert_eq!(s.rolling["taghmata"].drain_timeout_seconds, 901);
    assert_eq!((s.update.parallel, s.update.ready_timeout), (4, 301));
}

/// Review finding 2: a huge timeout overflowed `Instant + Duration` inside an adapter and
/// panicked its task. Every timeout is between 1 s and a day, and the error names the key.
#[test]
fn timeouts_are_between_one_second_and_a_day() {
    for key in ["ssh_connect", "probe", "eval", "settle", "diff"] {
        for bad in ["0", "86401", "9223372036854775807", "-1"] {
            let e = config::parse(&format!("[timeouts]\n{key} = {bad}\n")).unwrap_err();
            assert!(e.contains("between 1"), "{key} = {bad}: {e}");
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
fn ssh_persistence_and_approval_bounds_name_the_key() {
    for (section, key, min, max) in [
        ("ssh", "control_persist", 60, 3600),
        ("timeouts", "ssh_approval", 5, 600),
    ] {
        for value in [min - 1, max + 1, -1] {
            let error = config::parse(&format!("[{section}]\n{key} = {value}")).unwrap_err();
            assert!(error.contains(&format!("`{section}.{key}`")), "{error}");
        }
        for value in [min, max] {
            assert!(config::parse(&format!("[{section}]\n{key} = {value}")).is_ok());
        }
    }
    let (defaults, _) = config::parse("").unwrap();
    assert_eq!(defaults.ssh.control_persist, 600);
    assert_eq!(defaults.timeouts.ssh_approval, 60);
}

#[test]
fn a_settle_shorter_than_the_probe_is_a_warning() {
    let (_, w) = config::parse("[timeouts]\nsettle = 5\nprobe = 10\n").unwrap();
    assert!(
        w.iter()
            .any(|w| w.contains("timeouts.settle") && w.contains("timeouts.probe")),
        "{w:?}"
    );
    let (_, w) = config::parse("[timeouts]\nsettle = 70\nprobe = 10\n").unwrap();
    assert_eq!(w, Vec::<String>::new());
}

#[test]
fn settle_warns_when_it_cuts_off_connection_or_authentication() {
    let (_, warnings) = config::parse("[timeouts]\nssh_approval = 600\nsettle = 120\n").unwrap();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("timeouts.ssh_approval")
                && warning.contains("timeouts.settle")),
        "{warnings:?}"
    );
    let (_, warnings) =
        config::parse("[timeouts]\nssh_approval = 5\nprobe = 10\nsettle = 15\n").unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
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
fn the_package_preview_is_on_by_default_with_a_two_minute_budget() {
    let (s, w) = config::parse("").unwrap();
    assert_eq!(w, Vec::<String>::new());
    assert!(s.diff.enabled);
    assert_eq!(s.timeouts.diff, 120);
    let core = core_config(&s, ".".to_owned());
    assert_eq!(
        core.diff,
        Some(auspex_core::DiffConfig {
            timeout_seconds: 120
        })
    );
}

#[test]
fn timeouts_diff_reaches_the_core_and_enabled_false_switches_it_off() {
    let (s, _) = config::parse("[timeouts]\ndiff = 45\n").unwrap();
    assert_eq!(
        core_config(&s, ".".to_owned()).diff,
        Some(auspex_core::DiffConfig {
            timeout_seconds: 45
        })
    );
    let (s, _) = config::parse("[diff]\nenabled = false\n[timeouts]\ndiff = 45\n").unwrap();
    assert_eq!(core_config(&s, ".".to_owned()).diff, None);
}

#[test]
fn diff_enabled_must_be_a_boolean_and_an_unknown_diff_key_is_a_warning() {
    for bad in ["1", "\"yes\"", "[true]"] {
        let e = config::parse(&format!("[diff]\nenabled = {bad}\n")).unwrap_err();
        assert!(e.contains("`diff.enabled`"), "{bad}: {e}");
        assert!(e.contains("true or false"), "{bad}: {e}");
    }
    assert!(config::parse("diff = 5\n").unwrap_err().contains("`diff`"));
    let (_, w) = config::parse("[diff]\nenabled = true\nverbose = 1\n").unwrap();
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].contains("`diff.verbose`"));
}

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

/// One warning per cause: `settle < probe` implies `settle < ssh_approval + probe`, and the
/// second condition must not repeat the first.
#[test]
fn a_settle_shorter_than_the_probe_is_one_warning_not_two() {
    let (_, warnings) = config::parse("[timeouts]\nprobe = 30\nsettle = 20\n").unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("timeouts.probe"), "{warnings:?}");
}

/// `machines.<m>.public`, end to end: the file, the loader, the core's config. Only the
/// machines that say `true` are public, a machine the file does not mention is not, and the
/// default is `false`.
#[test]
fn machines_public_reaches_the_core_and_defaults_to_false() {
    let (s, w) = config::parse(
        "[machines.luna]\npublic = true\n[machines.sol]\npublic = false\n\
         [machines.terra]\ntarget = \"root@10.0.0.1\"\n",
    )
    .unwrap();
    assert!(w.is_empty(), "{w:?}");
    assert!(s.machines["luna"].public);
    assert!(!s.machines["sol"].public);
    assert!(!s.machines["terra"].public, "the default is false");
    let core = core_config(&s, ".".to_owned());
    assert_eq!(
        core.public_machines,
        std::collections::BTreeSet::from(["luna".to_owned()])
    );
    // No machines at all: none is public.
    let (s, _) = config::parse("").unwrap();
    assert!(core_config(&s, ".".to_owned()).public_machines.is_empty());
}

/// What `[update]`, `[rolling.*]` and `[kubernetes]` say reaches the core's configuration: a
/// value read by the loader and dropped on the way would roll with the defaults.
#[test]
fn the_rolling_settings_reach_the_cores_configuration() {
    let (s, _) = config::parse(
        "[rolling.taghmata]\nkubernetes = true\ndrain = true\ndrain_timeout = 77\netcd = true\n\
         [update]\nparallel = 5\nready_timeout = 321\npoll_interval = 7\n\
         [kubernetes]\nkubeconfig = \"/k/config\"\n",
    )
    .unwrap();
    let core = auspex_tui::app::core_config(&s, "/fleet".to_owned());
    assert_eq!(core.rolling.parallel, 5);
    assert_eq!(core.rolling.ready_timeout_seconds, 321);
    assert_eq!(core.rolling.poll_interval_seconds, 7);
    assert_eq!(
        core.rolling.rules["taghmata"],
        auspex_domain::roll::RollRule {
            kubernetes: true,
            drain: true,
            drain_timeout_seconds: 77,
            etcd: true
        }
    );
    assert_eq!(
        core.rolling.kubernetes_unavailable, None,
        "a kubeconfig was set"
    );
}

/// `kubernetes.kubeconfig` starts with `~/` like `flake`, and that is the home directory.
#[test]
fn a_tilde_slash_kubeconfig_is_the_home_directory() {
    let home = std::env::var("HOME").unwrap();
    assert!(std::path::Path::new(&home).is_absolute(), "{home}");
    let (s, _) = config::parse("[kubernetes]\nkubeconfig = \"~/.kube/fleet\"\n").unwrap();
    assert_eq!(s.kubeconfig, Some(format!("{home}/.kube/fleet")));
}

// ---------------------------------------------------------------------------------------
// `[forge]`: the CI badge (AUS-18)
// ---------------------------------------------------------------------------------------

#[test]
fn no_forge_section_is_no_badge() {
    let (s, w) = config::parse("").unwrap();
    assert_eq!(s.forge, None);
    assert_eq!(w.len(), 0);
    assert_eq!(core_config(&s, "/fleet".to_owned()).forge, None);
}

#[test]
fn only_a_url_is_enough_and_the_rest_is_optional() {
    let (s, w) = config::parse("[forge]\nurl = \"https://forge.example.org\"\n").unwrap();
    assert!(w.is_empty(), "{w:?}");
    assert_eq!(
        s.forge,
        Some(config::ForgeSettings {
            url: "https://forge.example.org/".to_owned(),
            host: "forge.example.org".to_owned(),
            repo: None,
            token_file: None,
        })
    );
    // A path prefix is kept, a query is not.
    let (s, _) = config::parse("[forge]\nurl = \"https://forge.example.org/git/?x=1\"\n").unwrap();
    assert_eq!(s.forge.unwrap().url, "https://forge.example.org/git/");
}

#[test]
fn forge_settings_without_a_url_are_ignored_with_a_warning() {
    let (s, w) = config::parse("[forge]\nrepo = \"owner/fleet\"\n").unwrap();
    assert_eq!(s.forge, None);
    assert!(
        w.iter()
            .any(|w| w.contains("no `forge.url`") && w.contains("off")),
        "{w:?}"
    );
    // An empty table says nothing and is not worth a warning.
    let (s, w) = config::parse("[forge]\n").unwrap();
    assert_eq!((s.forge, w), (None, vec![]));
}

#[test]
fn the_forge_url_is_https_only_and_never_quoted() {
    for (url, why) in [
        ("http://forge.example.org", "https"),
        ("ssh://forge.example.org", "https"),
        ("https://", "empty host"),
        ("forge.example.org", "bad URL"),
        ("https://user:hunter2@forge.example.org", "credentials"),
    ] {
        let e = config::parse(&format!("[forge]\nurl = \"{url}\"\n")).unwrap_err();
        assert!(e.contains("`forge.url`") && e.contains(why), "{url}: {e}");
        assert!(!e.contains("hunter2"), "{e}");
    }
    let e = config::parse("[forge]\nurl = 3\n").unwrap_err();
    assert!(e.contains("`forge.url` must be a string"), "{e}");
}

/// Fix round 1, finding 2: an IPv6 `forge.url` keeps no brackets in the host the core compares
/// with the remote's, so the badge asks the forge instead of "unknown: set forge.repo".
#[test]
fn an_ipv6_forge_url_has_its_brackets_stripped_and_matches_the_remote() {
    let (s, _) = config::parse("[forge]\nurl = \"https://[fd00::1]:3000/forgejo\"\n").unwrap();
    let forge = s.forge.clone().unwrap();
    assert_eq!(forge.host, "fd00::1");
    assert_eq!(forge.url, "https://[fd00::1]:3000/forgejo");
    let core = core_config(&s, "/fleet".to_owned()).forge.unwrap();
    assert_eq!(core.host, "fd00::1");
    let remote = auspex_domain::ci::parse_remote("ssh://git@[fd00::1]:22/owner/fleet.git").unwrap();
    let head = auspex_domain::ci::HeadFacts {
        commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        upstream: auspex_domain::preflight::Upstream::None,
        origin: Some(Ok(remote)),
    };
    assert!(matches!(
        auspex_domain::ci::plan(&head, &core.host, None),
        auspex_domain::ci::Plan::Ask { .. }
    ));
}

#[test]
fn the_forge_repo_is_owner_slash_name() {
    let (s, _) =
        config::parse("[forge]\nurl = \"https://f.example.org\"\nrepo = \"owner/fleet\"\n")
            .unwrap();
    assert_eq!(s.forge.unwrap().repo.as_deref(), Some("owner/fleet"));
    for bad in ["fleet", "a/b/c", "", "owner/fl eet", "/owner/fleet"] {
        let e = config::parse(&format!(
            "[forge]\nurl = \"https://f.example.org\"\nrepo = \"{bad}\"\n"
        ))
        .unwrap_err();
        assert!(
            e.contains("`forge.repo`") && e.contains("owner/name"),
            "{bad}: {e}"
        );
    }
}

#[test]
fn the_token_file_is_an_absolute_path_and_tilde_is_home() {
    let home = std::env::var("HOME").unwrap();
    let (s, _) = config::parse(
        "[forge]\nurl = \"https://f.example.org\"\ntoken_file = \"~/.config/auspex/forge-token\"\n",
    )
    .unwrap();
    assert_eq!(
        s.forge.unwrap().token_file,
        Some(format!("{home}/.config/auspex/forge-token").into())
    );
    for bad in ["forge-token", "./forge-token", "~other/forge-token"] {
        let e = config::parse(&format!(
            "[forge]\nurl = \"https://f.example.org\"\ntoken_file = \"{bad}\"\n"
        ))
        .unwrap_err();
        assert!(e.contains("`forge.token_file`"), "{bad}: {e}");
    }
    // A token is never a key of its own: there is no way to write one inline.
    let (s, w) =
        config::parse("[forge]\nurl = \"https://f.example.org\"\ntoken = \"hunter2\"\n").unwrap();
    assert!(s.forge.is_some());
    assert!(
        w.iter().any(|w| w.contains("unknown key `forge.token`")),
        "{w:?}"
    );
    assert!(!w.iter().any(|w| w.contains("hunter2")), "{w:?}");
}

#[test]
fn debug_printing_the_settings_shows_a_path_and_never_a_token() {
    let (s, _) = config::parse(
        "[forge]\nurl = \"https://f.example.org\"\ntoken_file = \"/run/secrets/forge-token\"\n",
    )
    .unwrap();
    let shown = format!("{s:?}");
    assert!(shown.contains("/run/secrets/forge-token"), "{shown}");
}

// ---------------------------------------------------------------------------------------
// `[runners]`: the CI runners view (AUS-19)
// ---------------------------------------------------------------------------------------

#[test]
fn no_runners_section_is_no_view() {
    let (s, w) = config::parse("").unwrap();
    assert_eq!((s.runners.clone(), w), (None, vec![]));
    assert_eq!(core_config(&s, "/fleet".to_owned()).runners, None);
}

#[test]
fn a_minimal_forge_takes_the_defaults() {
    let (s, w) = config::parse("[[runners.forge]]\nurl = \"https://forge.example.org\"\n").unwrap();
    assert_eq!(w, Vec::<String>::new());
    let r = s.runners.unwrap();
    // What the briefed config says when nothing is: the NixOS module's unit names, 90 percent,
    // the admin scope, no token, no host assumed.
    assert_eq!(r.unit_glob, "gitea-runner-*");
    assert_eq!(r.memory_warn_percent, 90);
    assert_eq!(r.forges.len(), 1);
    assert_eq!(r.forges[0].scope, auspex_domain::runners::Scope::Admin);
    assert_eq!(r.forges[0].name, "forge.example.org");
    assert_eq!(r.forges[0].token_file, None);
    assert!(r.hosts.is_empty() && r.machines.is_empty());
}

#[test]
fn the_core_is_told_the_forges_the_rules_and_the_caps() {
    let text =
        std::fs::read_to_string(crate_dir().join("../../../../nix/auspex/expected.toml")).unwrap();
    let (s, _) = config::parse(&text).unwrap();
    let core = core_config(&s, "/fleet".to_owned()).runners.unwrap();
    assert_eq!(
        core.forges
            .iter()
            .map(|f| (f.forge.as_str(), f.scope.as_str()))
            .collect::<Vec<_>>(),
        [
            ("forge.example.org (admin)", "admin"),
            ("forge.example.org (repo:owner/fleet)", "repo:owner/fleet"),
        ]
    );
    assert_eq!(core.join.unit_glob, "ci-runner-*");
    assert_eq!(core.join.memory_warn_percent, 80);
    assert_eq!(core.join.hosts["alpha"].cpus, Some(2));
    assert_eq!(core.join.machines["alpha-docker"], "alpha");
}

#[test]
fn runners_settings_without_a_forge_are_ignored_with_a_warning() {
    let (s, w) = config::parse("[runners]\nunit_glob = \"x-*\"\n").unwrap();
    assert_eq!(s.runners, None);
    assert!(
        w.iter()
            .any(|w| w.contains("no `[[runners.forge]]`") && w.contains("off")),
        "{w:?}"
    );
    let (s, w) = config::parse("[runners]\n").unwrap();
    assert_eq!((s.runners, w), (None, vec![]));
}

#[test]
fn every_runners_refusal_names_its_key_and_quotes_no_url() {
    let forge = "[[runners.forge]]\nurl = \"https://forge.example.org\"\n";
    for (text, key, why) in [
        (
            "[[runners.forge]]\nurl = \"http://forge.example.org\"\n".to_owned(),
            "`runners.forge[0].url`",
            "https",
        ),
        (
            "[[runners.forge]]\nurl = \"https://user:hunter2@forge.example.org\"\n".to_owned(),
            "`runners.forge[0].url`",
            "credentials",
        ),
        (
            "[[runners.forge]]\nscope = \"admin\"\n".to_owned(),
            "`runners.forge[0].url`",
            "required",
        ),
        (
            format!("{forge}scope = \"global\"\n"),
            "`runners.forge[0].scope`",
            "admin, user, org:<name> or repo:<owner>/<name>",
        ),
        (
            format!("{forge}scope = \"repo:a/b/c\"\n"),
            "`runners.forge[0].scope`",
            "repo:<owner>/<name>",
        ),
        (
            format!("{forge}token_file = \"relative/token\"\n"),
            "`runners.forge[0].token_file`",
            "absolute",
        ),
        (
            format!("{forge}token_file = \"~bob/token\"\n"),
            "`runners.forge[0].token_file`",
            "~/",
        ),
        (
            format!("{forge}\n[runners.hosts.alpha]\nmemory = \"4 GiB\"\n"),
            "`runners.hosts.alpha.memory`",
            "a whole number and a unit",
        ),
        (
            format!("{forge}\n[runners.hosts.alpha]\nmemory = \"0GiB\"\n"),
            "`runners.hosts.alpha.memory`",
            "more than 0",
        ),
        (
            format!("{forge}\n[runners.hosts.alpha]\ncpus = 0\n"),
            "`runners.hosts.alpha.cpus`",
            "1 or more",
        ),
        (
            format!("{forge}\n[runners]\nmemory_warn_percent = 0\n"),
            "`runners.memory_warn_percent`",
            "from 1 to 100",
        ),
        (
            format!("{forge}\n[runners]\nmemory_warn_percent = 101\n"),
            "`runners.memory_warn_percent`",
            "from 1 to 100",
        ),
        (
            format!("{forge}\n[runners]\nunit_glob = \"a;rm -rf /\"\n"),
            "`runners.unit_glob`",
            "shell script",
        ),
        (
            format!("{forge}\n[runners]\nunit_glob = \"\"\n"),
            "`runners.unit_glob`",
            "1 to 64",
        ),
        (
            format!("{forge}\n[runners.machines]\nalpha = 3\n"),
            "`runners.machines.alpha`",
            "a string",
        ),
        (
            "[runners]\nforge = \"https://forge.example.org\"\n".to_owned(),
            "`runners.forge`",
            "list of tables",
        ),
    ] {
        let e = config::parse(&text).unwrap_err();
        assert!(e.contains(key) && e.contains(why), "{text:?}: {e}");
        assert!(!e.contains("hunter2"), "{e}");
    }
}

#[test]
fn an_unknown_runners_key_is_ignored_with_a_warning() {
    let (s, w) = config::parse(
        "[[runners.forge]]\nurl = \"https://forge.example.org\"\nfuture = 1\n\
         [runners]\nsomething = true\n[runners.hosts.alpha]\ncolour = \"red\"\n",
    )
    .unwrap();
    assert!(s.runners.is_some());
    for key in [
        "runners.forge[0].future",
        "runners.something",
        "runners.hosts.alpha.colour",
    ] {
        assert!(w.iter().any(|w| w.contains(key)), "{key}: {w:?}");
    }
}

#[test]
fn memory_sizes_are_read_as_written() {
    for (text, bytes) in [
        ("4GiB", 4_u64 << 30),
        ("512MiB", 512 << 20),
        ("2GB", 2_000_000_000),
    ] {
        let (s, _) = config::parse(&format!(
            "[[runners.forge]]\nurl = \"https://forge.example.org\"\n\
             [runners.hosts.alpha]\nmemory = \"{text}\"\n"
        ))
        .unwrap();
        assert_eq!(
            s.runners.unwrap().hosts["alpha"].memory_bytes,
            Some(bytes),
            "{text}"
        );
    }
}

#[test]
fn two_entries_on_different_hosts_keep_their_host_alone() {
    let (s, _) = config::parse(
        "[[runners.forge]]\nurl = \"https://a.example.org\"\n\
         [[runners.forge]]\nurl = \"https://b.example.org\"\nscope = \"user\"\n",
    )
    .unwrap();
    let names: Vec<_> = s
        .runners
        .unwrap()
        .forges
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert_eq!(names, ["a.example.org", "b.example.org"]);
}

/// N1: two entries are told apart by everything that tells two forges apart (host, port, path),
/// then by scope, then by position: a name two entries share would put each one's runners under
/// both headings and make `--json` unable to say whose a row is.
#[test]
fn every_runners_forge_has_a_name_of_its_own() {
    let names = |toml: &str| -> Vec<String> {
        config::parse(toml)
            .unwrap()
            .0
            .runners
            .unwrap()
            .forges
            .into_iter()
            .map(|f| f.name)
            .collect()
    };
    let entry = |url: &str, scope: &str| {
        format!("[[runners.forge]]\nurl = \"{url}\"\nscope = \"{scope}\"\n")
    };
    // Another port, another path: other forges, named so, with no scope needed.
    assert_eq!(
        names(&format!(
            "{}{}{}",
            entry("https://forge.example.org", "admin"),
            entry("https://forge.example.org:8443", "admin"),
            entry("https://forge.example.org/forgejo/", "admin"),
        )),
        [
            "forge.example.org",
            "forge.example.org:8443",
            "forge.example.org/forgejo"
        ]
    );
    // The port that is the scheme's own is no part of the name.
    assert_eq!(
        names(&entry("https://forge.example.org:443", "user")),
        ["forge.example.org"]
    );
    // The same forge twice, by scope.
    assert_eq!(
        names(&format!(
            "{}{}",
            entry("https://forge.example.org", "admin"),
            entry("https://forge.example.org", "user")
        )),
        ["forge.example.org (admin)", "forge.example.org (user)"]
    );
    // The same forge and scope twice (two tokens, say): by position.
    assert_eq!(
        names(&format!(
            "{}{}{}",
            entry("https://forge.example.org", "admin"),
            entry("https://forge.example.org", "admin"),
            entry("https://forge.example.org", "admin"),
        )),
        [
            "forge.example.org (admin)",
            "forge.example.org (admin) #2",
            "forge.example.org (admin) #3"
        ]
    );
    // Never two the same, whatever the mix.
    let all = names(&format!(
        "{}{}{}{}",
        entry("https://a.example.org", "admin"),
        entry("https://a.example.org", "admin"),
        entry("https://a.example.org:8443", "admin"),
        entry("https://a.example.org", "user"),
    ));
    let mut sorted = all.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len(), "{all:?}");
}

/// B2: a `unit_glob` that starts with `-` is a `systemctl` option, not a pattern, and is refused at
/// load, each of them naming the key and none quoted.
#[test]
fn a_unit_glob_that_is_an_option_is_refused_at_load() {
    let forge = "[[runners.forge]]\nurl = \"https://forge.example.org\"\n";
    for bad in [
        "-Hroot@host.example.org",
        "-Mother",
        "-thelp",
        "--user",
        "-",
        "-x*",
    ] {
        let e = config::parse(&format!("{forge}[runners]\nunit_glob = \"{bad}\"\n")).unwrap_err();
        assert!(
            e.contains("`runners.unit_glob`") && e.contains("must not start with `-`"),
            "{bad}: {e}"
        );
        if bad.len() > 2 {
            assert!(!e.contains(bad), "{bad}: {e}");
        }
    }
    // A hyphen anywhere else is a unit name's own.
    for ok in ["gitea-runner-*", "a-", "x-y-z"] {
        let (s, _) = config::parse(&format!("{forge}[runners]\nunit_glob = \"{ok}\"\n")).unwrap();
        assert_eq!(s.runners.unwrap().unit_glob, ok);
    }
}
