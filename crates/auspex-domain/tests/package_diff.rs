//! Synthetic, hand-written NixOS closure fragments; no Nix evaluation or remote inputs.
use std::cmp::Ordering;

use auspex_domain::diff::{
    DiffCounts, DiffError, DiffState, DiffTruncation, HighlightKind, MAX_CLOSURE_PATHS,
    MAX_PACKAGE_CHANGES, MAX_PACKAGE_TEXT, MAX_PACKAGE_VERSIONS, MAX_PATH_BYTES, Package,
    PackageDiff, compare_versions, diff_closures, package_from_store_path,
};

fn path(name: &str) -> String {
    format!("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-{name}")
}

#[test]
fn parses_names_versions_and_outputs() {
    for (input, name, version) in [
        ("apache-httpd-2.0.48", "apache-httpd", Some("2.0.48")),
        ("hello-2.12.1", "hello", Some("2.12.1")),
        ("firefox-133.0", "firefox", Some("133.0")),
        ("linux-6.18.63", "linux", Some("6.18.63")),
        (
            "perl5.38.2-XML-Parser-2.46",
            "perl5.38.2-XML-Parser",
            Some("2.46"),
        ),
        ("glibc-2.40-66-bin", "glibc", Some("2.40-66")),
        ("python3-3.13.1", "python3", Some("3.13.1")),
        ("tool-1.0pre-dev-man", "tool", Some("1.0pre")),
        ("local-tools", "local-tools", None),
        ("tool--alpha", "tool--alpha", None),
        ("tool-1.0-custom", "tool", Some("1.0-custom")),
    ] {
        assert_eq!(
            package_from_store_path(&path(input)),
            Some(Package {
                name: name.into(),
                version: version.map(str::to_owned),
            }),
            "{input}"
        );
    }
    for suffix in [
        "out", "man", "dev", "lib", "bin", "doc", "info", "debug", "static", "headers",
    ] {
        assert_eq!(
            package_from_store_path(&path(&format!("tool-1.2-{suffix}")))
                .unwrap()
                .version
                .as_deref(),
            Some("1.2")
        );
    }
}

#[test]
fn excludes_generated_and_malformed_paths() {
    for name in [
        "nixos-system-terra-26.11.20261113.d5cc683",
        "darwin-system-26.11",
        "a.drv",
        "fleet-source",
        "etc",
        "etc-profile",
        "sshd.conf",
        "unit-a.service",
        "system-units",
        "system-path",
        "activation-script",
        "initrd",
        "a.service",
        "a.target",
        "a.socket",
        "a.timer",
        "a.path",
        "a.mount",
    ] {
        assert_eq!(package_from_store_path(&path(name)), None, "{name}");
    }
    for input in [
        "/tmp/tool-1",
        "/nix/store/short-tool-1",
        "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-tool-1",
        &path("tool-1/bin/x"),
        &path(""),
        &path("-1"),
    ] {
        assert_eq!(package_from_store_path(input), None, "{input}");
    }
}

#[test]
fn nix_version_ordering() {
    for (a, b) in [
        ("1.9", "1.10"),
        ("2.0pre", "2.0"),
        ("2.0pre1", "2.0pre2"),
        ("2.0", "2.0.0"),
        ("2.3a", "2.3.1"),
        ("2.3", "2.3a"),
        ("2.3pre", "2.3a"),
        ("1a", "1b"),
        ("1.2147483648", "1.2"),
        ("", "0"),
    ] {
        assert_eq!(compare_versions(a, b), Ordering::Less, "{a} < {b}");
        assert_eq!(compare_versions(b, a), Ordering::Greater, "{b} > {a}");
    }
    for (a, b) in [
        ("1.01", "1.1"),
        ("1-2", "1.2"),
        ("1..", "1"),
        ("", ".-"),
        ("1.0", "1.0"),
    ] {
        assert_eq!(compare_versions(a, b), Ordering::Equal, "{a} == {b}");
    }
}

#[test]
fn realistic_closure_diff() {
    let before: Vec<_> = include_str!("fixtures/closure-before.txt")
        .lines()
        .collect();
    let after: Vec<_> = include_str!("fixtures/closure-after.txt").lines().collect();
    let diff = diff_closures(&before, Some(&after)).unwrap().unwrap();
    assert_eq!(
        diff.counts,
        DiffCounts {
            added: 1,
            removed: 1,
            changed: 4
        }
    );
    assert_eq!(diff.truncated, DiffTruncation::default());
    assert_eq!(diff.added[0].name, "ripgrep");
    assert_eq!(diff.removed[0].name, "nano");
    assert_eq!(diff.removed[0].after, []);
    assert_eq!(diff.added[0].before, []);
    assert_eq!(
        diff.changed
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["linux", "systemd", "openssh", "openssl"]
    );
    assert_eq!(
        diff.changed[3].before,
        [Some("3.0.16".into()), Some("3.4.1".into())]
    );
    assert_eq!(
        diff.changed[3].after,
        [Some("3.0.16".into()), Some("3.5.0".into())]
    );
    assert_eq!(
        diff.highlights
            .iter()
            .map(|h| (h.name.as_str(), h.kinds.as_slice()))
            .collect::<Vec<_>>(),
        [
            ("linux", &[HighlightKind::Kernel][..]),
            (
                "systemd",
                &[HighlightKind::Systemd, HighlightKind::MajorVersion][..]
            ),
            ("openssh", &[HighlightKind::MajorVersion][..]),
        ]
    );
    assert_eq!(diff.unversioned.added, 17);
    assert_eq!(diff.unversioned.removed, 3);
    assert_eq!(diff.rebuilt, ["glibc"]);
    let mut reversed = after.clone();
    reversed.reverse();
    assert_eq!(diff_closures(&before, Some(&reversed)).unwrap(), Some(diff));
}

#[test]
fn unknown_empty_unversioned_and_major_rules() {
    let empty: Vec<String> = vec![];
    assert_eq!(diff_closures(&[path("tool-1")], None).unwrap(), None);
    assert_eq!(
        diff_closures(&empty, Some(&empty)).unwrap().unwrap().counts,
        DiffCounts::default()
    );
    let added = diff_closures(&empty, Some(&[path("local-tools")]))
        .unwrap()
        .unwrap();
    assert_eq!(added.added, []);
    assert_eq!(added.unversioned.added, 1);
    assert_eq!(added.highlights, []);
    for (before, after, bump) in [
        (vec!["tool-9", "tool-10"], vec!["tool-10", "tool-11"], true),
        (vec!["tool-9", "tool-10"], vec!["tool-8", "tool-10"], true),
        (vec!["tool-10"], vec!["tool-9"], true),
        (vec![], vec!["tool-10"], false),
        (vec!["tool-10"], vec![], false),
        (vec!["tool"], vec!["tool-10"], false),
    ] {
        let before: Vec<_> = before.into_iter().map(path).collect();
        let after: Vec<_> = after.into_iter().map(path).collect();
        let diff = diff_closures(&before, Some(&after)).unwrap().unwrap();
        assert_eq!(
            diff.highlights
                .iter()
                .any(|h| h.kinds.contains(&HighlightKind::MajorVersion)),
            bump
        );
    }
    let before = [path("tool-01"), path("tool-1"), path("tool-2")];
    assert_eq!(
        diff_closures(&before, Some(&empty))
            .unwrap()
            .unwrap()
            .removed[0]
            .before,
        [Some("01".into()), Some("1".into()), Some("2".into())]
    );
}

#[test]
fn caps_and_terminal_sanitising() {
    let empty: Vec<String> = vec![];
    let paths: Vec<_> = (0..MAX_CLOSURE_PATHS)
        .map(|i| path(&format!("pkg{i}-1")))
        .collect();
    let diff = diff_closures(&empty, Some(&paths)).unwrap().unwrap();
    assert_eq!(diff.counts.added, MAX_CLOSURE_PATHS);
    assert_eq!(diff.added.len(), MAX_PACKAGE_CHANGES);
    assert!(diff.truncated.added);
    assert_eq!(diff.added[0].name, "pkg0");
    let duplicate = vec![path("tool-1"); MAX_CLOSURE_PATHS];
    assert_eq!(
        diff_closures(&duplicate, Some(&duplicate))
            .unwrap()
            .unwrap()
            .counts,
        DiffCounts::default()
    );
    let too_many = vec![""; MAX_CLOSURE_PATHS + 1];
    assert_eq!(
        diff_closures(&too_many, Some(&[])),
        Err(DiffError::TooManyPaths)
    );
    assert_eq!(
        diff_closures(&[], Some(&too_many)),
        Err(DiffError::TooManyPaths)
    );
    let long = ["a".repeat(MAX_PATH_BYTES + 1)];
    assert_eq!(
        diff_closures(&long, Some(&empty)),
        Err(DiffError::PathTooLong)
    );
    assert_eq!(
        diff_closures(&empty, Some(&long)),
        Err(DiffError::PathTooLong)
    );
    let safe = diff_closures(
        &empty,
        Some(&[path("\u{1b}[31mto\u{202e}ol-1.\u{1b}[0m2\r\n")]),
    )
    .unwrap()
    .unwrap();
    assert_eq!(safe.added, []);
    assert_eq!(safe.invalid.pending, 1);
}

#[test]
fn version_and_highlight_caps_preserve_full_comparison() {
    let before: Vec<_> = (0..1000).map(|i| path(&format!("tool-1.{i}"))).collect();
    let mut after = before.clone();
    after.push(path("tool-2"));
    let diff = diff_closures(&before, Some(&after)).unwrap().unwrap();
    assert_eq!(diff.changed[0].before.len(), MAX_PACKAGE_VERSIONS);
    assert_eq!(diff.changed[0].after.len(), MAX_PACKAGE_VERSIONS);
    assert_eq!(
        diff.changed[0].before.first().unwrap().as_deref(),
        Some("1.0")
    );
    assert_eq!(
        diff.changed[0].before.last().unwrap().as_deref(),
        Some("1.15")
    );
    assert!(diff.changed[0].before_truncated && diff.changed[0].after_truncated);
    assert_eq!(diff.highlights[0].kinds, [HighlightKind::MajorVersion]);
    let before: Vec<_> = (0..1000).map(|i| path(&format!("pkg{i}-1"))).collect();
    let after: Vec<_> = (0..1000).map(|i| path(&format!("pkg{i}-2"))).collect();
    let diff = diff_closures(&before, Some(&after)).unwrap().unwrap();
    assert_eq!(diff.counts.changed, 1000);
    assert_eq!(diff.changed.len(), MAX_PACKAGE_CHANGES);
    assert_eq!(diff.highlights.len(), MAX_PACKAGE_CHANGES);
    assert!(diff.truncated.changed && diff.truncated.highlights);
    let removed = diff_closures(&before, Some(&[])).unwrap().unwrap();
    assert_eq!(removed.counts.removed, 1000);
    assert_eq!(removed.removed.len(), MAX_PACKAGE_CHANGES);
    assert!(removed.truncated.removed);
}

#[test]
fn critical_packages_and_major_components() {
    for (name, kind) in [
        ("linux", HighlightKind::Kernel),
        ("systemd", HighlightKind::Systemd),
        ("glibc", HighlightKind::Glibc),
        ("nix", HighlightKind::Nix),
    ] {
        let diff = diff_closures(
            &[path(&format!("{name}-2.1"))],
            Some(&[path(&format!("{name}-2.2"))]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(diff.highlights[0].kinds, [kind]);
    }
    let diff = diff_closures(&[path("tool-01.1")], Some(&[path("tool-1.2")]))
        .unwrap()
        .unwrap();
    assert_eq!(diff.highlights, []);
}

#[test]
fn unavailable_and_failed_previews_are_distinct_and_safe() {
    let unknown = PackageDiff::not_cached();
    assert_eq!(unknown.state, DiffState::NotCached);
    assert_eq!(unknown.counts, DiffCounts::default());
    assert_eq!(unknown.reason, None);
    let failed = PackageDiff::failed(&format!("\u{1b}[31merror\u{202e}\n{}", "é".repeat(1000)));
    assert_eq!(failed.state, DiffState::Failed);
    let reason = failed.reason.unwrap();
    assert!(reason.starts_with("erroré"));
    assert!(reason.len() <= 512);
    assert!(!reason.contains('\u{1b}') && !reason.contains('\u{202e}'));
    assert_eq!(failed.added, []);
    assert_eq!(failed.counts, DiffCounts::default());
    let long = [path(&format!("{}-1", "a".repeat(MAX_PACKAGE_TEXT)))];
    assert_eq!(diff_closures(&long, Some(&[])), Err(DiffError::NameTooLong));
}

#[test]
fn review_highlights_keep_versions_past_alphabetical_cap() {
    let mut before: Vec<_> = (0..198).map(|i| path(&format!("aaa{i}-1.0"))).collect();
    let mut after: Vec<_> = (0..198).map(|i| path(&format!("aaa{i}-1.1"))).collect();
    before.push(path("systemd-262"));
    after.push(path("systemd-263"));
    let diff = diff_closures(&before, Some(&after)).unwrap().unwrap();
    let systemd = diff.changed.iter().find(|p| p.name == "systemd").unwrap();
    assert_eq!(systemd.before, [Some("262".into())]);
    assert_eq!(systemd.after, [Some("263".into())]);
    let json = serde_json::to_value(&diff).unwrap();
    assert_eq!(json["highlights"][0]["before"], serde_json::json!(["262"]));
    assert_eq!(json["highlights"][0]["after"], serde_json::json!(["263"]));
    assert_eq!(diff.changed.len(), MAX_PACKAGE_CHANGES);
    assert!(diff.truncated.changed);
}

#[test]
fn review_real_output_names_collapse() {
    for (name, version, suffix) in [
        ("linux", "6.18.59", "modules"),
        ("glibc", "2.44-57", "getent"),
        ("gcc", "16.2.0", "libgcc"),
        ("foot", "1.26.0", "terminfo"),
        ("ffmpeg", "8.1", "data"),
        ("libxml2", "2.15.3", "py"),
        ("nvidia-x11", "615.26", "lib32"),
        ("util-linux", "2.42.4", "login"),
        ("util-linux", "2.42.4", "mount"),
        ("util-linux", "2.42.4", "swap"),
        ("util-linux", "2.42.4", "lastlog"),
        ("shadow", "4.19.0", "su"),
        ("vim", "9.2.1164", "xxd"),
        ("chromium", "157.0.0", "sandbox"),
        ("python3", "3.14.8", "env"),
        ("libressl", "4.2.0", "nc"),
        ("nss-cacert", "3.123", "p11kit"),
    ] {
        let diff =
            diff_closures::<String>(&[], Some(&[path(&format!("{name}-{version}-{suffix}"))]))
                .unwrap()
                .unwrap();
        assert_eq!(diff.added[0].after, [Some(version.into())], "{suffix}");
    }
    let before = [path("tool-1.0"), path("tool-1.0-custom7")];
    let diff = diff_closures(&before, Some(&[])).unwrap().unwrap();
    assert_eq!(diff.removed[0].before, [Some("1.0".into())]);
}

#[test]
fn review_unversioned_grouping_and_git_versions() {
    let before = [
        path("systemd"),
        path("systemd-262"),
        path("dbus-1"),
        path("dbus-1.16.2"),
        path("generic.desktop"),
        path("bashrc"),
        path("helix-tree-sitter-rust-abcdef0123456789abcdef0123456789abcdef0123"),
    ];
    let after = [
        path("systemd"),
        path("systemd-263"),
        path("dbus-1"),
        path("dbus-1.16.2"),
        path("generic.json"),
        path("bashrc"),
        path("helix-tree-sitter-rust-fedcba9876543210fedcba9876543210fedcba98"),
    ];
    let diff = diff_closures(&before, Some(&after)).unwrap().unwrap();
    assert_eq!(diff.counts.added, 0);
    assert_eq!(diff.counts.removed, 0);
    assert_eq!(diff.counts.changed, 2);
    assert_eq!(
        diff.changed
            .iter()
            .find(|p| p.name == "systemd")
            .unwrap()
            .before,
        [Some("262".into())]
    );
    let json = serde_json::to_value(diff).unwrap();
    assert_eq!(
        json["unversioned"],
        serde_json::json!({"added": 1, "removed": 1})
    );
    let dbus = diff_closures(&[path("dbus-1"), path("dbus-1.16.2")], Some(&[]))
        .unwrap()
        .unwrap();
    assert_eq!(dbus.removed[0].before, [Some("1.16.2".into())]);
}

#[test]
fn review_unit_package_is_not_a_generated_unit() {
    assert_eq!(
        package_from_store_path(&path("unit-1.34.2")).unwrap().name,
        "unit"
    );
    for name in ["unit-script-example-start", "unit-example.network"] {
        assert_eq!(package_from_store_path(&path(name)), None);
    }
}

#[test]
fn review_critical_same_version_rebuilds() {
    let before: Vec<_> = ["linux", "systemd", "glibc", "nix", "hello"]
        .map(|n| path(&format!("{n}-1.0")))
        .into();
    let after: Vec<_> = before
        .iter()
        .map(|p| p.replacen("0123456789", "1123456789", 1))
        .collect();
    let diff = diff_closures(&before, Some(&after)).unwrap().unwrap();
    assert_eq!(diff.counts, DiffCounts::default());
    assert_eq!(
        serde_json::to_value(&diff).unwrap()["rebuilt"],
        serde_json::json!(["glibc", "linux", "nix", "systemd"])
    );
    let unchanged = diff_closures(&after, Some(&after)).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(unchanged).unwrap()["rebuilt"],
        serde_json::json!([])
    );
}

#[test]
fn review_invalid_names_are_rejected_not_merged() {
    let paths = [
        path("ev\u{202e}il-1.0"),
        path("evil-1.0"),
        path("bad-1\\quote"),
        path("bad-1\"quote"),
        path("bad-1\u{1b}[0m"),
    ];
    let diff = diff_closures::<String>(&[], Some(&paths)).unwrap().unwrap();
    assert_eq!(diff.counts.added, 1);
    assert_eq!(diff.added[0].name, "evil");
    assert_eq!(
        serde_json::to_value(diff).unwrap()["invalid"],
        serde_json::json!({"deployed": 0, "pending": 4})
    );
}

#[test]
fn review_preview_json_byte_budget() {
    let mut before = Vec::new();
    let mut after = Vec::new();
    for i in 0..128 {
        for v in 0..16 {
            before.push(path(&format!("pkg{i}-1.{v}.{}", "a".repeat(225))));
            after.push(path(&format!("pkg{i}-2.{v}.{}", "b".repeat(225))));
        }
    }
    let diff = diff_closures(&before, Some(&after)).unwrap().unwrap();
    let bytes = serde_json::to_vec(&diff).unwrap();
    assert!(bytes.len() <= 256 * 1024, "{} bytes", bytes.len());
    assert_eq!(diff.counts.changed, 128);
    assert_eq!(
        serde_json::to_value(diff).unwrap()["truncated"]["bytes"],
        true
    );
}

#[test]
fn review_preview_records_toplevel_pair() {
    let deployed = path("nixos-system-example-26.11.20261113.abcdef0");
    let pending = path("nixos-system-example-26.11.20261116.fedcba9");
    let diff = diff_closures(&[deployed.as_str()], Some(&[pending.as_str()]))
        .unwrap()
        .unwrap();
    let json = serde_json::to_value(diff).unwrap();
    assert_eq!(json["deployed"], deployed);
    assert_eq!(json["pending"], pending);
}
