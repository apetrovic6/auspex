//! The `--json` contract's snapshot. This test fails on any change to the serialised shape:
//! a renamed field, a changed enum spelling, a field that went missing. If the change is
//! intended, re-bless with `AUSPEX_BLESS=1`, and bump `JSON_VERSION` unless the change only
//! adds fields.

use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::Path;

use auspex_domain::eval::{EvalOutcome, parse_eval_line};
use auspex_domain::fleet::{Machine, SharedIp, parse_inventory, resolve_target, shared_ips};
use auspex_domain::observe::{
    DiskUsage, Load, Memory, ProbeExit, ProbeOutcome, PromSample, merge_health, parse_probe,
};
use auspex_domain::report::{FleetReport, JSON_VERSION, fleet_report, machine_report};
use auspex_domain::status::{MachineFacts, NarCache};

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
    fleet_report(reports, shared)
}

fn snapshot() -> serde_json::Value {
    serde_json::to_value(snapshot_report()).unwrap()
}

#[test]
fn json_contract_snapshot() {
    assert_eq!(snapshot()["version"], JSON_VERSION);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/json/fleet-v1.json");
    // Serialised from the struct, as `--json` will print it, so field order is what readers
    // see (going through `serde_json::Value` would sort the keys).
    let pretty = serde_json::to_string_pretty(&snapshot_report()).unwrap() + "\n";
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
