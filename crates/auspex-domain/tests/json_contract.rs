//! The `--json` contract's snapshot. This test fails on any change to the serialised shape:
//! a renamed field, a changed enum spelling, a field that went missing. If the change is
//! intended, re-bless with `AUSPEX_BLESS=1`, and bump `JSON_VERSION` unless the change only
//! adds fields.

use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::Path;

use auspex_domain::eval::{EvalOutcome, parse_eval_line};
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
use auspex_domain::status::{Badge, MachineFacts, NarCache};

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
        &serde_json::json!({"attr": attr, "drvPath": format!("{out}.drv"), "outputs": {"out": out}})
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
            parse_eval_line(r#"{"attr":"luna","error":"error: attribute 'k3sToken' missing"}"#)
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
                    NarCache::Cached
                } else {
                    NarCache::Unknown
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
    fleet_report(reports, shared, FleetFacts::default())
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

/// A machine list that could not be read: no machines, one fleet error, not settled. It
/// must not read as a healthy, empty fleet.
fn failed_inventory_report() -> FleetReport {
    fleet_report(
        vec![],
        vec![],
        FleetFacts {
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
    let r = fleet_report(vec![], vec![], FleetFacts::default());
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
    let done = fleet_report(settled.clone(), vec![], FleetFacts::default());
    assert!(done.settled);
    let cut = fleet_report(
        settled,
        vec![],
        FleetFacts {
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
    let err = parse_eval_line(r#"{"attr":"mars","error":"error: boom"}"#).unwrap();
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
            fleet_report(vec![r], vec![], FleetFacts::default()).settled,
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
    let sorted = fleet_report(r.machines, vec![], FleetFacts::default());
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
