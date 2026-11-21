//! The plan's `--demo` fleet, scripted by hand: one slow host, one dead host, one host with a
//! failed unit, and one that is simply fine. The final report is pinned whole, as the JSON
//! `auspex --json` would print, because that is what the demo's snapshot tests will draw.

mod support;

use std::collections::BTreeMap;

use auspex_core::effects::EvalAnswer;
use auspex_core::{Event, Phase, PrometheusConfig};
use auspex_domain::status::NarCache;
use serde_json::json;
use support::*;

const FLEET: &str = r#"{
  "terra":    {"deploy": {"targetHost": "root@192.168.1.10", "buildHost": "localhost"}, "tags": ["base"]},
  "luna":     {"deploy": {"targetHost": "root@192.168.1.11"}, "tags": ["k8s-server"]},
  "mars":     {"deploy": {"targetHost": "root@192.168.1.20"}, "tags": ["k8s-master"]},
  "cerberus": {"deploy": {"targetHost": "root@192.168.1.2"}, "tags": ["server"]}
}"#;

#[test]
fn the_demo_fleet_reaches_its_final_report() {
    let mut config = full_config();
    config.prometheus = Some(PrometheusConfig {
        instances: BTreeMap::new(),
        max_age_seconds: 60,
    });
    let mut shell = Shell::new();
    shell.start_with(config, FLEET);
    let mut eval = shell.take_eval();

    let terra = path('0', "nixos-system-terra-25.05");
    let luna_now = path('1', "nixos-system-luna-25.05.a");
    let luna_next = path('2', "nixos-system-luna-25.05.b");
    let mars = path('3', "nixos-system-mars-25.05");
    let cerberus = path('4', "nixos-system-cerberus-25.05");

    // The dead host answers first: ssh gives up.
    shell.probe(
        "mars",
        unreachable("ssh: connect to host 192.168.1.20 port 22: Connection timed out"),
    );
    shell.probe("terra", observed(&terra, ""));
    shell.probe(
        "cerberus",
        observed_failed_unit(&cerberus, "forgejo-runner-cerberus.service"),
    );
    let mut prom = shell.take_prometheus();
    shell.resolve(
        &mut prom,
        Ok(BTreeMap::from([
            ("terra".to_owned(), sample(86_400, 5)),
            ("cerberus".to_owned(), sample(3_600, 5)),
        ])),
    );
    shell.eval_item(&mut eval, "terra", evaluated("terra", &terra));
    shell.eval_item(&mut eval, "mars", evaluated("mars", &mars));
    shell.eval_item(&mut eval, "cerberus", evaluated("cerberus", &cerberus));
    shell.eval_item(&mut eval, "luna", evaluated("luna", &luna_next));
    shell.resolve(&mut eval, EvalAnswer::Finished { error: None });

    // Everyone but the slow host is settled, and the fleet is not.
    let view = shell.view();
    assert_eq!(view.refresh.pending_machines, 1);
    assert!(!view.refresh.settled);
    assert_eq!(shell.row("luna").status.label(), "checking");

    // The slow host, last: an update is pending, and it is cached.
    shell.probe("luna", observed(&luna_now, ""));
    let mut cache = shell.take_cache("luna");
    shell.resolve(&mut cache, NarCache::Cached);

    let view = shell.view();
    assert!(view.refresh.settled);
    assert_eq!(view.refresh.phase, Phase::Settled);
    assert_eq!(shell.waiting(), 0);
    assert_eq!(shell.cancels.len(), 0, "nothing was left to cancel");

    let report = serde_json::to_value(&view.report).unwrap();
    let expected = expected_report(&terra, &luna_now, &luna_next, &mars, &cerberus);
    assert_eq!(
        report,
        expected,
        "actual:\n{}",
        serde_json::to_string_pretty(&report).unwrap()
    );

    // A refresh of the same fleet starts over: rows gone, the list asked for again.
    shell.send(Event::Refresh);
    assert_eq!(shell.inventory.len(), 1);
    assert_eq!(shell.view().report.machines.len(), 0);
}

/// The final report, as `auspex --json` prints it.
#[expect(
    clippy::too_many_lines,
    reason = "one JSON literal, pinned whole; split, it would no longer read as the output"
)]
fn expected_report(
    terra: &str,
    luna_now: &str,
    luna_next: &str,
    mars: &str,
    cerberus: &str,
) -> serde_json::Value {
    let ssh_health = json!({
        "uptime_seconds": {"value": 100, "source": "ssh"},
        "root_disk": {"value": {"total_bytes": 1_024_000, "used_bytes": 409_600, "available_bytes": 614_400}, "source": "ssh"},
        "load": {"value": {"one": 0.5, "five": 0.25, "fifteen": 0.125}, "source": "ssh"},
        "memory": null
    });
    let prom_health = |uptime: u64| {
        json!({
            "uptime_seconds": {"value": uptime, "source": "prometheus"},
            "root_disk": {"value": {"total_bytes": 1_024_000, "used_bytes": 409_600, "available_bytes": 614_400}, "source": "ssh"},
            "load": {"value": {"one": 0.5, "five": 0.25, "fifteen": 0.125}, "source": "ssh"},
            "memory": null
        })
    };
    json!({
        "version": 1,
        "settled": true,
        "errors": [],
        "fleet_ci": {"commit": null, "state": "off", "url": null, "reason": null},
        "machines": [
            {
                "name": "cerberus",
                "class": "nixos",
                "tags": ["server"],
                "target": "root@192.168.1.2",
                "target_source": "inventory",
                "target_error": null,
                "build_host": null,
                "status": "in_sync",
                "badges": ["in_sync", "failed_units"],
                "settled": true,
                "reason": "failed units: forgejo-runner-cerberus.service",
                "current_system": cerberus,
                "pending_system": cerberus,
                "package_diff": null,
                "deployed_revision": null,
                "system_state": "degraded",
                "failed_units": ["forgejo-runner-cerberus.service"],
                "health": prom_health(3_600),
                "lints": [],
                "frozen": false
            },
            {
                "name": "luna",
                "class": "nixos",
                "tags": ["k8s-server"],
                "target": "root@192.168.1.11",
                "target_source": "inventory",
                "target_error": null,
                "build_host": null,
                "status": "update_pending",
                "badges": ["update_pending", "cached"],
                "settled": true,
                "reason": "pending nixos-system-luna-25.05.b",
                "current_system": luna_now,
                "pending_system": luna_next,
                "package_diff": null,
                "deployed_revision": null,
                "system_state": "running",
                "failed_units": [],
                "health": ssh_health,
                "lints": [],
                "frozen": false
            },
            {
                "name": "mars",
                "class": "nixos",
                "tags": ["k8s-master"],
                "target": "root@192.168.1.20",
                "target_source": "inventory",
                "target_error": null,
                "build_host": null,
                "status": "unreachable",
                "badges": ["unreachable"],
                "settled": true,
                "reason": "ssh: connect to host 192.168.1.20 port 22: Connection timed out",
                "current_system": null,
                "pending_system": mars,
                "package_diff": null,
                "deployed_revision": null,
                "system_state": null,
                "failed_units": null,
                "health": {"uptime_seconds": null, "root_disk": null, "load": null, "memory": null},
                "lints": [],
                "frozen": false
            },
            {
                "name": "terra",
                "class": "nixos",
                "tags": ["base"],
                "target": "root@192.168.1.10",
                "target_source": "inventory",
                "target_error": null,
                "build_host": "localhost",
                "status": "in_sync",
                "badges": ["in_sync"],
                "settled": true,
                "reason": null,
                "current_system": terra,
                "pending_system": terra,
                "package_diff": null,
                "deployed_revision": null,
                "system_state": "running",
                "failed_units": [],
                "health": prom_health(86_400),
                "lints": [],
                "frozen": false
            }
        ],
        "shared_ips": []
    })
}
