//! The `--json` contract's snapshot. This test fails on any change to the serialised shape:
//! a renamed field, a changed enum spelling, a field that went missing. If the change is
//! intended, re-bless with `AUSPEX_BLESS=1`, and bump `JSON_VERSION` unless the change only
//! adds fields.

use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::Path;

use auspex_domain::eval::{EvalFailure, EvalOutcome, parse_eval_line};
use auspex_domain::fleet::{
    Machine, MachineClass, SharedIp, TargetSource, parse_inventory, resolve_target, shared_ips,
};
use auspex_domain::observe::{
    DiskUsage, Health, HealthSource, Load, Memory, ProbeExit, ProbeOutcome, PromSample,
    merge_health, parse_probe,
};
use auspex_domain::report::{
    FleetError, FleetErrorKind, FleetFacts, FleetReport, JSON_VERSION, fleet_report, machine_report,
};
use auspex_domain::status::{Badge, MachineFacts, NarCache, Outstanding};

const INVENTORY: &str = r#"{
  "terra":    {"deploy": {"targetHost": "root@192.168.1.10", "buildHost": "localhost"}, "tags": ["base", "k8s-base"]},
  "genetor":  {"deploy": {"targetHost": "admin@192.168.1.109"}, "tags": ["workstation"], "machineClass": "darwin"},
  "mars":     {"deploy": {"targetHost": "root@mars.ugalabugala.org"}, "tags": ["k8s-master"]},
  "cerberus": {"deploy": {"targetHost": "root@192.168.1.2"}, "tags": ["server"]},
  "luna":     {"tags": ["k8s-server"]},
  "broken":   {"deploy": {"targetHost": "-oProxyCommand=sh"}}
}"#;

const TERRA_NOW: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-terra-a";
const TERRA_NEXT: &str = "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-nixos-system-terra-b";
const GENETOR: &str = "/nix/store/2123456789abcdfghijklmnpqrsvwxyz-darwin-system-25.05";
const CERBERUS: &str = "/nix/store/3123456789abcdfghijklmnpqrsvwxyz-nixos-system-cerberus";

fn success(attr: &str, out: &str) -> EvalOutcome {
    parse_eval_line(
        &serde_json::json!({"attr": attr, "attrPath": [attr], "drvPath": format!("{out}.drv"), "outputs": {"out": out}})
            .to_string(),
    )
    .unwrap()
}

fn probe(body: &str) -> ProbeOutcome {
    parse_probe(
        &format!("auspex_probe=1\n{body}auspex_end=1\n"),
        "",
        ProbeExit::Code(0),
    )
}

fn snapshot_report() -> FleetReport {
    let machines = parse_inventory(INVENTORY).unwrap();
    let overrides: BTreeMap<&str, &str> = [("luna", "root@192.168.1.12")].into();

    let ip = |s: &str| s.parse::<IpAddr>().unwrap();
    let resolved: BTreeMap<String, Vec<IpAddr>> = [
        ("terra", "192.168.1.10"),
        ("genetor", "192.168.1.109"),
        ("cerberus", "192.168.1.2"),
        // The wildcard-DNS trap: mars's hostname answers with cerberus's address.
        ("mars", "192.168.1.2"),
        ("luna", "192.168.1.12"),
    ]
    .into_iter()
    .map(|(m, a)| (m.to_owned(), vec![ip(a)]))
    .collect();
    let shared = shared_ips(&resolved);

    rest(&machines, &overrides, shared, &probes(), &evals())
}

fn probes() -> BTreeMap<&'static str, ProbeOutcome> {
    [
        (
            "terra",
            probe(&format!(
                "probe=nixos\ncurrent_system={TERRA_NOW}\nconfiguration_revision=c68e00a\n\
                 system_state=degraded\nfailed_units=[{{\"unit\":\"k3s.service\"}}]\n\
                 uptime_seconds=1000\nload=0.5 0.4 0.3\nroot_disk_kb=1000 400 500\n"
            )),
        ),
        (
            "genetor",
            probe(&format!(
                "probe=darwin\ncurrent_system={GENETOR}\nconfiguration_revision=null\n\
                 uptime_seconds=5000\nload=1.5 1.25 1.0\nroot_disk_kb=2000 1000 1000\n"
            )),
        ),
        (
            "mars",
            parse_probe(
                "",
                "ssh: connect to host mars.ugalabugala.org port 22: Connection timed out\n",
                ProbeExit::Code(255),
            ),
        ),
        (
            "cerberus",
            probe(&format!(
                "probe=nixos\ncurrent_system={CERBERUS}\nsystem_state=running\nfailed_units=[]\n"
            )),
        ),
    ]
    .into()
}

fn evals() -> BTreeMap<&'static str, EvalOutcome> {
    [
        ("terra", success("terra", TERRA_NEXT)),
        ("genetor", success("genetor", GENETOR)),
        ("cerberus", success("cerberus", CERBERUS)),
        (
            "luna",
            parse_eval_line(r#"{"attr":"luna","attrPath":["luna"],"error":"error: attribute 'k3sToken' missing"}"#)
                .unwrap(),
        ),
    ]
    .into()
}

fn rest(
    machines: &[Machine],
    overrides: &BTreeMap<&str, &str>,
    shared: Vec<SharedIp>,
    probes: &BTreeMap<&str, ProbeOutcome>,
    evals: &BTreeMap<&str, EvalOutcome>,
) -> FleetReport {
    let prom = PromSample {
        uptime_seconds: Some(1010),
        root_disk: None,
        load: Some(Load {
            one: 0.25,
            five: 0.5,
            fifteen: 0.75,
        }),
        memory: Some(Memory {
            total_bytes: 8_589_934_592,
            available_bytes: 4_294_967_296,
        }),
        age_seconds: 15,
    };

    let reports = machines
        .iter()
        .map(|m| {
            let target = resolve_target(m, overrides.get(m.name.as_str()).copied());
            let p = probes.get(m.name.as_str());
            let ssh = match p {
                Some(ProbeOutcome::Observed(o)) => Some(o.ssh_health),
                _ => None,
            };
            let prom = (m.name == "terra").then_some(&prom);
            let facts = MachineFacts {
                probe: p,
                eval: evals.get(m.name.as_str()),
                cache: if m.name == "terra" {
                    NarCache::Failed
                } else {
                    NarCache::NotApplicable
                },
                frozen: m.name == "cerberus",
                ..MachineFacts::default()
            };
            machine_report(
                m,
                &target,
                &facts,
                merge_health(ssh.as_ref(), prom, 60),
                &shared,
            )
        })
        .collect();
    fleet_report(reports, shared, complete())
}

/// The scan's fleet-wide requests are all back, nothing else to say.
fn complete() -> FleetFacts {
    FleetFacts {
        scan_complete: true,
        ..FleetFacts::default()
    }
}

fn snapshot() -> serde_json::Value {
    serde_json::to_value(snapshot_report()).unwrap()
}

fn compare_snapshot(report: &FleetReport, file: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/json")
        .join(file);
    // Serialised from the struct, as `--json` will print it, so field order is what readers
    // see (going through `serde_json::Value` would sort the keys).
    let pretty = serde_json::to_string_pretty(report).unwrap() + "\n";
    if std::env::var_os("AUSPEX_BLESS").is_some() {
        fs::write(&path, &pretty).unwrap();
        return;
    }
    let want = fs::read_to_string(&path).expect("snapshot missing; run with AUSPEX_BLESS=1");
    // Compared as text, not as parsed JSON: key order is part of what readers see, and a
    // reordering is worth a look even though it is compatible.
    assert_eq!(
        want, pretty,
        "the --json shape changed; see this file's header"
    );
}

#[test]
fn json_contract_round_trips() {
    let got = snapshot();
    let parsed: auspex_domain::report::FleetReport = serde_json::from_value(got.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), got);
}

/// Keeps `DiskUsage` in the imports honest: the snapshot's disk values come from ssh.
#[test]
fn disk_comes_from_ssh_when_prometheus_lacks_it() {
    let got = snapshot();
    let terra = &got["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "terra")
        .unwrap()["health"];
    assert_eq!(terra["root_disk"]["source"], "ssh");
    assert_eq!(terra["uptime_seconds"]["source"], "prometheus");
    let disk: DiskUsage = serde_json::from_value(terra["root_disk"]["value"].clone()).unwrap();
    assert_eq!(disk.total_bytes, 1_024_000);
}

#[test]
fn json_contract_snapshot() {
    assert_eq!(snapshot()["version"], JSON_VERSION);
    compare_snapshot(&snapshot_report(), "fleet-v1.json");
}

#[test]
fn json_contract_snapshot_package_diff() {
    let before: Vec<_> = include_str!("fixtures/closure-before.txt")
        .lines()
        .collect();
    let after: Vec<_> = include_str!("fixtures/closure-after.txt").lines().collect();
    let diff = auspex_domain::diff::diff_closures(&before, Some(&after))
        .unwrap()
        .unwrap();
    let mut report = snapshot_report();
    let terra = report
        .machines
        .iter_mut()
        .find(|m| m.name == "terra")
        .unwrap();
    *terra = terra.clone().with_package_diff(diff);
    compare_snapshot(&report, "fleet-v1-package-diff.json");
    let parsed: FleetReport =
        serde_json::from_value(serde_json::to_value(&report).unwrap()).unwrap();
    assert_eq!(parsed, report);
}

#[test]
fn package_diff_json_unknown() {
    let diff = auspex_domain::diff::diff_closures::<&str>(&[], None).unwrap();
    let pretty = serde_json::to_string_pretty(&diff).unwrap() + "\n";
    assert_eq!(
        pretty,
        include_str!("fixtures/json/package-diff-v1-unknown.json")
    );
    assert_eq!(
        serde_json::from_str::<Option<auspex_domain::diff::PackageDiff>>(&pretty).unwrap(),
        None
    );
}

#[test]
fn package_diff_report_headline_and_legacy() {
    use auspex_domain::diff::{PackageDiff, diff_closures};
    let base = snapshot_report()
        .machines
        .into_iter()
        .find(|m| m.name == "terra")
        .unwrap();
    let diff = diff_closures::<&str>(&[], Some(&[])).unwrap().unwrap();
    let attached = base.clone().with_package_diff(diff.clone());
    assert_eq!(
        attached.reason,
        Some(format!(
            "{}; packages +0 −0 ~0",
            base.reason.as_deref().unwrap()
        ))
    );
    assert_eq!(attached.status, base.status);
    assert_eq!(attached.badges, base.badges);
    assert_eq!(attached.settled, base.settled);
    for unavailable in [
        PackageDiff::not_cached(),
        PackageDiff::failed("\u{1b}[31mcache failed"),
    ] {
        assert_eq!(
            base.clone().with_package_diff(unavailable).reason,
            base.reason
        );
    }
    let mut in_sync = base.clone();
    in_sync.status = Badge::InSync;
    assert_eq!(
        in_sync.clone().with_package_diff(diff).reason,
        in_sync.reason
    );
    let mut json = serde_json::to_value(&base).unwrap();
    assert!(json["package_diff"].is_null());
    json.as_object_mut().unwrap().remove("package_diff");
    let legacy: auspex_domain::report::MachineReport = serde_json::from_value(json).unwrap();
    assert_eq!(legacy.package_diff, None);
}

/// A machine list that could not be read: no machines, one fleet error, not settled. It
/// must not read as a healthy, empty fleet.
fn failed_inventory_report() -> FleetReport {
    fleet_report(
        vec![],
        vec![],
        FleetFacts {
            scan_complete: true,
            timed_out: false,
            errors: vec![FleetError::new(
                FleetErrorKind::InventoryFailed,
                "\u{1b}[31merror:\u{1b}[0m flake 'path:/home/op/fleet' does not provide attribute 'clanInternals'",
            )],
        },
    )
}

#[test]
fn json_contract_snapshot_fleet_error() {
    let r = failed_inventory_report();
    assert!(!r.settled);
    assert_eq!(r.machines, Vec::new());
    compare_snapshot(&r, "fleet-v1-inventory-failed.json");
}

/// Item 12: every fleet error kind, and errors keep a report from settling.
#[test]
fn fleet_errors_are_reported_and_never_settle() {
    let kinds = [
        (FleetErrorKind::InventoryFailed, "inventory_failed"),
        (FleetErrorKind::InventoryTimedOut, "inventory_timed_out"),
        (FleetErrorKind::EvaluationFailed, "evaluation_failed"),
    ];
    for (kind, spelling) in kinds {
        assert_eq!(serde_json::to_value(kind).unwrap(), spelling);
        let r = fleet_report(
            vec![],
            vec![],
            FleetFacts {
                scan_complete: true,
                timed_out: false,
                errors: vec![FleetError {
                    kind,
                    reason: "x\u{202e}y\u{1b}[2J".to_owned(),
                }],
            },
        );
        assert!(!r.settled, "{spelling}");
        // Sanitised even when built without FleetError::new.
        assert_eq!(r.errors[0].reason, "xy");
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["errors"][0]["kind"], spelling);
        assert_eq!(json["errors"][0]["reason"], "xy");
    }
    // No errors: the field is present and empty.
    let r = fleet_report(vec![], vec![], complete());
    assert!(r.settled);
    assert_eq!(
        serde_json::to_value(&r).unwrap()["errors"],
        serde_json::json!([])
    );
}

/// Item 11: the global deadline passing first means not settled, whatever the machines say.
#[test]
fn deadline_means_not_settled() {
    let one = snapshot_report()
        .machines
        .into_iter()
        .find(|m| m.settled)
        .expect("the snapshot has a settled machine");
    let settled = vec![one];
    let done = fleet_report(settled.clone(), vec![], complete());
    assert!(done.settled);
    let cut = fleet_report(
        settled,
        vec![],
        FleetFacts {
            scan_complete: true,
            timed_out: true,
            errors: vec![],
        },
    );
    assert!(!cut.settled);
}

fn machine(name: &str, target: &str) -> Machine {
    Machine {
        name: name.to_owned(),
        class: MachineClass::Nixos,
        tags: vec![],
        target_host: Some(target.to_owned()),
        build_host: None,
    }
}

/// F3: a machine is settled when its probe (or a refused target) and its eval are back,
/// whichever order they arrive in.
#[test]
fn settled_needs_the_probe_and_the_eval() {
    let mars = machine("mars", "root@192.168.1.20");
    let broken = machine("broken", "-oProxyCommand=sh");
    let dead = parse_probe(
        "",
        "ssh: connect to host 192.168.1.20 port 22: Connection timed out\n",
        ProbeExit::Code(255),
    );
    let err =
        parse_eval_line(r#"{"attr":"mars","attrPath":["mars"],"error":"error: boom"}"#).unwrap();
    let ok = success("mars", TERRA_NOW);
    let up = probe(&format!(
        "probe=nixos\ncurrent_system={TERRA_NOW}\nsystem_state=running\nfailed_units=[]\n"
    ));
    let report = |m: &Machine, probe: Option<&ProbeOutcome>, eval: Option<&EvalOutcome>| {
        machine_report(
            m,
            &resolve_target(m, None),
            &MachineFacts {
                probe,
                eval,
                ..MachineFacts::default()
            },
            Health::default(),
            &[],
        )
    };
    let cases = [
        (report(&mars, None, None), false),
        (report(&mars, Some(&dead), None), false),
        (report(&mars, None, Some(&err)), false),
        (report(&mars, Some(&dead), Some(&err)), true),
        (report(&mars, Some(&up), None), false),
        (report(&mars, Some(&up), Some(&ok)), true),
        (report(&broken, None, None), false),
        (report(&broken, None, Some(&err)), true),
    ];
    for (i, (r, want)) in cases.into_iter().enumerate() {
        assert_eq!(r.settled, want, "case {i}: {r:?}");
        assert_eq!(
            fleet_report(vec![r], vec![], complete()).settled,
            want,
            "case {i}"
        );
    }
}

/// M61: machines are sorted by the report, not by whoever built the list.
#[test]
fn fleet_report_sorts_machines() {
    let mut r = snapshot_report();
    let want: Vec<String> = r.machines.iter().map(|m| m.name.clone()).collect();
    assert!(want.windows(2).all(|w| w[0] < w[1]));
    r.machines.reverse();
    let sorted = fleet_report(r.machines, vec![], complete());
    let got: Vec<String> = sorted.machines.iter().map(|m| m.name.clone()).collect();
    assert_eq!(got, want);
}

/// Correctness 2, M65 to M68: every published enum spelling, including the ones the
/// snapshot happens not to contain. The match makes a new variant a compile error here.
#[test]
#[expect(
    clippy::items_after_statements,
    reason = "each spelling table sits next to the loop that checks it"
)]
fn package_diff_enum_spellings_are_pinned() {
    use auspex_domain::diff::{DiffState, HighlightKind};
    fn diff_state(state: DiffState) -> &'static str {
        match state {
            DiffState::Computed => "computed",
            DiffState::NotCached => "not_cached",
            DiffState::Failed => "failed",
        }
    }
    for state in [DiffState::Computed, DiffState::NotCached, DiffState::Failed] {
        assert_eq!(serde_json::to_value(state).unwrap(), diff_state(state));
    }
    fn highlight(kind: HighlightKind) -> &'static str {
        match kind {
            HighlightKind::Kernel => "kernel",
            HighlightKind::Systemd => "systemd",
            HighlightKind::Glibc => "glibc",
            HighlightKind::Nix => "nix",
            HighlightKind::MajorVersion => "major_version",
        }
    }
    for kind in [
        HighlightKind::Kernel,
        HighlightKind::Systemd,
        HighlightKind::Glibc,
        HighlightKind::Nix,
        HighlightKind::MajorVersion,
    ] {
        assert_eq!(serde_json::to_value(kind).unwrap(), highlight(kind));
    }
}

#[test]
#[expect(
    clippy::items_after_statements,
    reason = "each spelling table sits next to the loop that checks it"
)]
fn every_enum_spelling_is_pinned() {
    fn badge(b: Badge) -> &'static str {
        match b {
            Badge::Checking => "checking",
            Badge::Evaluating => "evaluating",
            Badge::InSync => "in_sync",
            Badge::UpdatePending => "update_pending",
            Badge::Cached => "cached",
            Badge::FailedUnits => "failed_units",
            Badge::RebootNeeded => "reboot_needed",
            Badge::NotInstalled => "not_installed",
            Badge::Unreachable => "unreachable",
            Badge::EvalError => "eval_error",
            Badge::Frozen => "frozen",
        }
    }
    for b in [
        Badge::Checking,
        Badge::Evaluating,
        Badge::InSync,
        Badge::UpdatePending,
        Badge::Cached,
        Badge::FailedUnits,
        Badge::RebootNeeded,
        Badge::NotInstalled,
        Badge::Unreachable,
        Badge::EvalError,
        Badge::Frozen,
    ] {
        assert_eq!(serde_json::to_value(b).unwrap(), badge(b), "{b:?}");
    }
    fn source(s: TargetSource) -> &'static str {
        match s {
            TargetSource::Override => "override",
            TargetSource::Inventory => "inventory",
            TargetSource::MachineName => "machine_name",
        }
    }
    for s in [
        TargetSource::Override,
        TargetSource::Inventory,
        TargetSource::MachineName,
    ] {
        assert_eq!(serde_json::to_value(s).unwrap(), source(s), "{s:?}");
    }
    fn class(c: MachineClass) -> &'static str {
        match c {
            MachineClass::Nixos => "nixos",
            MachineClass::Darwin => "darwin",
        }
    }
    for c in [MachineClass::Nixos, MachineClass::Darwin] {
        assert_eq!(serde_json::to_value(c).unwrap(), class(c));
    }
    fn health(h: HealthSource) -> &'static str {
        match h {
            HealthSource::Prometheus => "prometheus",
            HealthSource::Ssh => "ssh",
        }
    }
    for h in [HealthSource::Prometheus, HealthSource::Ssh] {
        assert_eq!(serde_json::to_value(h).unwrap(), health(h));
    }
    fn fleet_error(k: FleetErrorKind) -> &'static str {
        match k {
            FleetErrorKind::InventoryFailed => "inventory_failed",
            FleetErrorKind::InventoryTimedOut => "inventory_timed_out",
            FleetErrorKind::EvaluationFailed => "evaluation_failed",
        }
    }
    for k in [
        FleetErrorKind::InventoryFailed,
        FleetErrorKind::InventoryTimedOut,
        FleetErrorKind::EvaluationFailed,
    ] {
        assert_eq!(serde_json::to_value(k).unwrap(), fleet_error(k));
    }
}

/// F10: inventory text reaches the report sanitised, even from a hand-built `Machine`.
#[test]
fn inventory_text_is_sanitised_in_the_report() {
    let m = Machine {
        name: "te\u{1b}]0;pwned\u{7}rra".to_owned(),
        class: MachineClass::Nixos,
        tags: vec!["\u{1b}[2J".to_owned(), "base\u{202e}".to_owned()],
        target_host: Some("root@192.168.1.10".to_owned()),
        build_host: Some("\u{1b}[31mx".to_owned()),
    };
    let r = machine_report(
        &m,
        &resolve_target(&m, None),
        &MachineFacts::default(),
        Health::default(),
        &[],
    );
    assert_eq!(r.name, "terra");
    assert_eq!(r.tags, ["base"]);
    assert_eq!(r.build_host.as_deref(), Some("x"));
}

fn terra() -> Machine {
    machine("terra", "root@192.168.1.10")
}

/// N2: with an update pending, a machine is not settled until its ncps lookup answers, so
/// `--json` cannot print before `cached` can appear.
#[test]
fn settled_waits_for_the_cache_lookup() {
    let m = terra();
    let p = probe(&format!(
        "probe=nixos\ncurrent_system={TERRA_NOW}\nsystem_state=running\nfailed_units=[]\n"
    ));
    let e = success("terra", TERRA_NEXT);
    let r = |cache| {
        machine_report(
            &m,
            &resolve_target(&m, None),
            &MachineFacts {
                probe: Some(&p),
                eval: Some(&e),
                cache,
                ..MachineFacts::default()
            },
            Health::default(),
            &[],
        )
    };
    let before = r(NarCache::Pending);
    assert!(!before.settled);
    assert!(!fleet_report(vec![before], vec![], complete()).settled);
    for (cache, badges) in [
        (NarCache::Cached, vec![Badge::UpdatePending, Badge::Cached]),
        (NarCache::NotCached, vec![Badge::UpdatePending]),
        (NarCache::Failed, vec![Badge::UpdatePending]),
        (NarCache::NotApplicable, vec![Badge::UpdatePending]),
    ] {
        let after = r(cache);
        assert!(after.settled, "{cache:?}");
        assert_eq!(after.badges, badges, "{cache:?}");
        assert!(
            fleet_report(vec![after], vec![], complete()).settled,
            "{cache:?}"
        );
    }
}

/// N2: an outstanding Prometheus (or Alertmanager) answer keeps the machine unsettled.
#[test]
fn settled_waits_for_prometheus_and_alerts() {
    let m = terra();
    let p = probe(&format!(
        "probe=nixos\ncurrent_system={TERRA_NOW}\nsystem_state=running\nfailed_units=[]\n"
    ));
    let e = success("terra", TERRA_NOW);
    let r = |outstanding| {
        machine_report(
            &m,
            &resolve_target(&m, None),
            &MachineFacts {
                probe: Some(&p),
                eval: Some(&e),
                outstanding,
                ..MachineFacts::default()
            },
            Health::default(),
            &[],
        )
    };
    assert!(r(Outstanding::default()).settled);
    for o in [
        Outstanding {
            prometheus: true,
            alerts: false,
        },
        Outstanding {
            prometheus: false,
            alerts: true,
        },
    ] {
        let rep = r(o);
        assert!(!rep.settled, "{o:?}");
        assert!(
            !fleet_report(vec![rep], vec![], complete()).settled,
            "{o:?}"
        );
    }
}

/// N2: a scan still in progress is not settled, even with no machines yet, or with every
/// row it has settled. The default `FleetFacts` is "in progress".
#[test]
fn scan_in_progress_is_not_settled() {
    assert!(!fleet_report(vec![], vec![], FleetFacts::default()).settled);
    let one = snapshot_report()
        .machines
        .into_iter()
        .find(|m| m.settled)
        .expect("the snapshot has a settled machine");
    assert!(!fleet_report(vec![one.clone()], vec![], FleetFacts::default()).settled);
    assert!(fleet_report(vec![one], vec![], complete()).settled);
    // An empty fleet whose scan really is complete is settled (and healthy).
    assert!(fleet_report(vec![], vec![], complete()).settled);
}

#[test]
fn evaluator_exit_without_a_result_settles_as_an_error() {
    let m = terra();
    let p = probe(&format!(
        "probe=nixos\ncurrent_system={TERRA_NOW}\nsystem_state=running\nfailed_units=[]\n"
    ));
    let e = EvalOutcome::Error(EvalFailure::no_result("terra"));
    assert_eq!(e.machine(), Some("terra"));
    let EvalOutcome::Error(failure) = &e else {
        panic!()
    };
    assert_eq!(failure.attr, "terra");
    let raw_machine = "te\u{1b}]0;pwn\u{7}rra\u{202e}";
    let display_failure = EvalFailure::no_result(raw_machine);
    assert_eq!(display_failure.attr, "terra");
    assert_eq!(
        EvalOutcome::Error(display_failure).machine(),
        Some(raw_machine)
    );
    assert_eq!(
        failure.error_tail,
        ["error: evaluator exited without a result for this machine"]
    );
    let row = machine_report(
        &m,
        &resolve_target(&m, None),
        &MachineFacts {
            probe: Some(&p),
            eval: Some(&e),
            ..MachineFacts::default()
        },
        Health::default(),
        &[],
    );
    assert_eq!(row.status, Badge::EvalError);
    assert_eq!(row.reason.as_deref(), failure.summary());
    assert!(row.settled);
    assert!(fleet_report(vec![row], vec![], complete()).settled);
}
