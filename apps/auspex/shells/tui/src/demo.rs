//! `--demo`: a scripted fleet answering the core's effects, with no I/O at all.
//!
//! No process, no network, no cache directory, no control directory: every answer below is
//! built from constants and handed to the same domain parsers the real adapters use, so the
//! rows read exactly as a real scan's would. The plan's three cases are here, a slow host
//! (`luna`, which answers last), a dead host (`mars`, ssh times out) and a failed unit
//! (`cerberus`), plus enough ordinary machines to scroll, a long name to truncate, an eval
//! error, a host-key failure, a reboot pending, a darwin host, and two hostname targets that
//! resolve to one address.
//!
//! Answers wait a scripted delay first, so the TUI shows spinners settling one by one. The
//! delays are tokio timers: with time paused (the snapshot tests) they elapse at once and
//! in a fixed order, and nothing drawn depends on the clock.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

use auspex_core::effects::{
    CacheRequest, EvalRequest, InventoryAnswer, ProbeRequest, PrometheusAnswer, PrometheusRequest,
    ResolveRequest,
};
use auspex_core::{Config, PrometheusConfig};
use auspex_domain::eval::{EvalFailure, EvalOutcome, EvalSuccess};
use auspex_domain::fleet::parse_inventory;
use auspex_domain::observe::{
    DiskUsage, Load, Memory, ProbeExit, ProbeOutcome, PromSample, StorePath, parse_probe,
};
use auspex_domain::status::NarCache;
use auspex_effects::nix::EvalItem;
use auspex_effects::{CancellationToken, Cancelled};
use tokio::sync::mpsc::UnboundedSender;

/// The demo's flake, shown as the fleet's location.
pub const FLAKE: &str = "demo:omnissiah";

/// The core configuration the demo starts with: Prometheus and a binary cache configured,
/// so every badge can appear.
#[must_use]
pub fn config() -> Config {
    Config {
        flake: FLAKE.to_owned(),
        target_overrides: BTreeMap::from([(
            "cypramundi".to_owned(),
            "root@198.51.100.4:2222".to_owned(),
        )]),
        prometheus: Some(PrometheusConfig {
            instances: BTreeMap::new(),
            max_age_seconds: 60,
        }),
        binary_cache: true,
    }
}

#[derive(Clone, Copy)]
enum Probe {
    /// Running what the evaluation gives.
    InSync,
    /// Running an older system.
    Behind,
    /// In sync, booted kernel differs.
    Reboot,
    /// In sync, one failed unit.
    FailedUnit(&'static str),
    /// Reachable, nothing installed.
    NotInstalled,
    /// ssh exits 255 with this line.
    Unreachable(&'static str),
}

#[derive(Clone, Copy)]
struct Script {
    name: &'static str,
    probe: Probe,
    probe_ms: u64,
    /// `None`: evaluates; `Some`: nix's error.
    eval_error: Option<&'static str>,
    eval_ms: u64,
    /// The pending toplevel is in the binary cache.
    cached: bool,
    /// Read from the eval cache (logged).
    from_cache: bool,
    /// Has a Prometheus sample.
    prometheus: bool,
}

const fn m(name: &'static str, probe: Probe, probe_ms: u64, eval_ms: u64) -> Script {
    Script {
        name,
        probe,
        probe_ms,
        eval_error: None,
        eval_ms,
        cached: false,
        from_cache: true,
        prometheus: true,
    }
}

/// The fleet, by name. Delays are distinct so the order of answers is fixed.
const FLEET: &[Script] = &[
    m("alfrost", Probe::InSync, 310, 0),
    m("callisto", Probe::InSync, 320, 0),
    m(
        "cerberus",
        Probe::FailedUnit("forgejo-runner.service"),
        330,
        0,
    ),
    Script {
        from_cache: false,
        ..m("cypramundi", Probe::Behind, 340, 1500)
    },
    Script {
        from_cache: false,
        ..m("deimos", Probe::NotInstalled, 350, 1600)
    },
    m("europa", Probe::InSync, 360, 0),
    m("ganymede", Probe::Reboot, 370, 0),
    m("genetor", Probe::InSync, 380, 0),
    m("io", Probe::InSync, 390, 0),
    Script {
        cached: true,
        from_cache: false,
        ..m("luna", Probe::Behind, 4000, 1700)
    },
    Script {
        prometheus: false,
        ..m(
            "mars",
            Probe::Unreachable("ssh: connect to host 192.168.1.20 port 22: Connection timed out"),
            1200,
            0,
        )
    },
    m(
        "mechanicus-forge-world-archive-node-01",
        Probe::InSync,
        400,
        0,
    ),
    m("phobos", Probe::InSync, 410, 0),
    m("sol", Probe::Behind, 420, 0),
    m("terra", Probe::InSync, 430, 0),
    m("titan", Probe::InSync, 440, 0),
    Script {
        prometheus: false,
        ..m(
            "triton",
            Probe::Unreachable("Host key verification failed."),
            450,
            0,
        )
    },
    Script {
        eval_error: Some("error: attribute 'hardware-configuration' missing"),
        from_cache: false,
        ..m("vulcan", Probe::InSync, 460, 1800)
    },
];

const INVENTORY: &str = r#"{
  "alfrost":    {"deploy": {"targetHost": "root@203.0.113.7"}, "tags": ["base", "server"]},
  "callisto":   {"deploy": {"targetHost": "root@192.168.1.31", "buildHost": "localhost"}, "tags": ["base"]},
  "cerberus":   {"deploy": {"targetHost": "root@192.168.1.2", "buildHost": "localhost"}, "tags": ["base", "server"]},
  "cypramundi": {"deploy": {"targetHost": "root@198.51.100.4"}, "tags": ["base", "server"]},
  "deimos":     {"deploy": {"targetHost": "root@192.168.1.41"}, "tags": ["base"]},
  "europa":     {"deploy": {"targetHost": "root@europa.lab.example"}, "tags": ["base", "dev"]},
  "ganymede":   {"deploy": {"targetHost": "root@192.168.1.33"}, "tags": ["base", "gaming"]},
  "genetor":    {"machineClass": "darwin", "deploy": {"targetHost": "admin@192.168.1.109"}, "tags": ["workstation"]},
  "io":         {"deploy": {"targetHost": "root@io.lab.example"}, "tags": ["base", "dev"]},
  "luna":       {"deploy": {"targetHost": "root@192.168.1.11", "buildHost": "localhost"}, "tags": ["k8s", "k8s-server"]},
  "mars":       {"deploy": {"targetHost": "root@192.168.1.20", "buildHost": "localhost"}, "tags": ["k8s", "k8s-master"]},
  "mechanicus-forge-world-archive-node-01": {"deploy": {"targetHost": "root@192.168.1.50"}, "tags": ["base"]},
  "phobos":     {"deploy": {"targetHost": "root@192.168.1.42"}, "tags": ["base", "laptop"]},
  "sol":        {"deploy": {"targetHost": "root@192.168.1.12", "buildHost": "localhost"}, "tags": ["k8s", "k8s-server"]},
  "terra":      {"deploy": {"targetHost": "root@192.168.1.10", "buildHost": "localhost"}, "tags": ["k8s", "k8s-base"]},
  "titan":      {"deploy": {"targetHost": "root@192.168.1.34"}, "tags": ["base"]},
  "triton":     {"deploy": {"targetHost": "root@192.168.1.35"}, "tags": ["base"]},
  "vulcan":     {}
}"#;

/// A valid nix32 hash, different per machine and per `generation` of its system.
fn hash(name: &str, generation: u8) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    let seed = name.bytes().fold(u32::from(generation) * 7 + 3, |a, b| {
        a.wrapping_mul(31).wrapping_add(u32::from(b))
    });
    (0..32u32)
        .map(|i| {
            let v = seed.wrapping_mul(i + 1).wrapping_add(i * 13) >> 3;
            ALPHABET[(v % 32) as usize] as char
        })
        .collect()
}

fn darwin(name: &str) -> bool {
    name == "genetor"
}

fn system(name: &str, generation: u8) -> String {
    let kind = if darwin(name) {
        "darwin-system"
    } else {
        "nixos-system"
    };
    let release = if generation == 0 {
        "25.11.20261101.6a2f1c0"
    } else {
        "25.11.20261019.b81e2d4"
    };
    format!(
        "/nix/store/{}-{kind}-{name}-{release}",
        hash(name, generation)
    )
}

fn script(name: &str) -> Option<&'static Script> {
    FLEET.iter().find(|s| s.name == name)
}

/// Waits `ms` of tokio time, or until cancelled.
async fn after(ms: u64, cancel: &CancellationToken) -> Result<(), Cancelled> {
    tokio::select! {
        () = tokio::time::sleep(Duration::from_millis(ms)) => Ok(()),
        () = cancel.cancelled() => Err(Cancelled),
    }
}

fn probe_text(s: &Script) -> ProbeOutcome {
    let current = match s.probe {
        Probe::Behind => system(s.name, 1),
        _ => system(s.name, 0),
    };
    if let Probe::Unreachable(line) = s.probe {
        return parse_probe("", &format!("{line}\n"), ProbeExit::Code(255));
    }
    if darwin(s.name) {
        return parse_probe(
            &format!(
                "auspex_probe=1\nprobe=darwin\ncurrent_system={current}\n\
                 configuration_revision=null\nuptime_seconds=432000\nload=1.21 1.05 0.98\n\
                 root_disk_kb=494384795 211234400 283150395\nauspex_end=1\n"
            ),
            "",
            ProbeExit::Code(0),
        );
    }
    let current_line = if matches!(s.probe, Probe::NotInstalled) {
        String::new()
    } else {
        current
    };
    let kernel = format!("/nix/store/{}-linux-6.12.41/bzImage", hash(s.name, 9));
    let booted = if matches!(s.probe, Probe::Reboot) {
        format!("/nix/store/{}-linux-6.12.38/bzImage", hash(s.name, 8))
    } else {
        kernel.clone()
    };
    let (state, failed) = match s.probe {
        Probe::FailedUnit(unit) => ("degraded", format!("[{{\"unit\":\"{unit}\"}}]")),
        _ => ("running", "[]".to_owned()),
    };
    let revision = if matches!(s.probe, Probe::NotInstalled) {
        String::new()
    } else {
        "6a2f1c0e9d".to_owned()
    };
    parse_probe(
        &format!(
            "auspex_probe=1\nprobe=nixos\ncurrent_system={current_line}\n\
             booted_kernel={booted}\nbooted_initrd=\ncurrent_kernel={kernel}\ncurrent_initrd=\n\
             configuration_revision={revision}\nsystem_state={state}\nfailed_units={failed}\n\
             uptime_seconds=1209600\nload=0.42 0.37 0.31\n\
             root_disk_kb=245000000 98000000 134500000\nauspex_end=1\n"
        ),
        "",
        ProbeExit::Code(0),
    )
}

fn outcome(s: &Script) -> EvalOutcome {
    let attr = s.name.to_owned();
    if let Some(error) = s.eval_error {
        return EvalOutcome::Error(EvalFailure {
            attr: attr.clone(),
            attr_path: vec![attr],
            error_tail: vec![
                "… while evaluating the attribute 'config.system.build.toplevel'".to_owned(),
                error.to_owned(),
                "at /nix/store/…-source/machines/vulcan/configuration.nix:3:5:".to_owned(),
            ],
        });
    }
    let out = system(s.name, 0);
    EvalOutcome::Success(EvalSuccess {
        attr: attr.clone(),
        attr_path: vec![attr],
        drv_path: StorePath::new(format!("/nix/store/{}-{}.drv", hash(s.name, 5), "toplevel"))
            .expect("the demo's paths are store paths"),
        out_path: StorePath::new(out).expect("the demo's paths are store paths"),
    })
}

/// The script. `Demo::default()` is what `--demo` runs; the two fields exist for the shell's
/// own tests, to make the evaluator misbehave in the two ways the shell must not hide.
#[derive(Debug, Default)]
pub struct Demo {
    /// Machines the evaluator finishes without a result for.
    pub omit_from_eval: Vec<String>,
    /// The evaluator's stream ends with no `Finished` at all.
    pub end_without_finished: bool,
}

impl Demo {
    pub(crate) async fn inventory(
        &self,
        cancel: &CancellationToken,
    ) -> Result<InventoryAnswer, Cancelled> {
        after(150, cancel).await?;
        Ok(parse_inventory(INVENTORY).map_err(|e| e.to_string()))
    }

    pub(crate) async fn probe(
        &self,
        op: &ProbeRequest,
        cancel: &CancellationToken,
    ) -> Result<ProbeOutcome, Cancelled> {
        let Some(s) = script(&op.machine) else {
            return Ok(ProbeOutcome::Unreachable {
                reason: "not in the demo script".to_owned(),
            });
        };
        after(s.probe_ms, cancel).await?;
        Ok(probe_text(s))
    }

    pub(crate) async fn evaluate(
        &self,
        op: &EvalRequest,
        cancel: &CancellationToken,
        tx: UnboundedSender<EvalItem>,
    ) -> Result<(), Cancelled> {
        let mut order: Vec<&Script> = op
            .machines
            .iter()
            .filter(|m| !self.omit_from_eval.contains(&m.name))
            .filter_map(|m| script(&m.name))
            .collect();
        order.sort_by_key(|s| (s.eval_ms, s.name));
        let _ = tx.send(EvalItem::Note(
            "demo: a scripted fleet; no machine, cache or network is contacted".to_owned(),
        ));
        let mut now = 0;
        for s in order {
            after(s.eval_ms - now, cancel).await?;
            now = s.eval_ms;
            let _ = tx.send(EvalItem::Machine {
                machine: s.name.to_owned(),
                outcome: outcome(s),
                from_cache: s.from_cache,
            });
        }
        let _ = tx.send(EvalItem::Note(
            "eval results stored in the cache: cypramundi, deimos, luna".to_owned(),
        ));
        if !self.end_without_finished {
            let _ = tx.send(EvalItem::Finished { error: None });
        }
        Ok(())
    }

    pub(crate) async fn prometheus(
        &self,
        op: &PrometheusRequest,
        cancel: &CancellationToken,
    ) -> Result<PrometheusAnswer, Cancelled> {
        after(250, cancel).await?;
        Ok(Ok(op
            .instances
            .iter()
            .filter(|i| script(i).is_some_and(|s| s.prometheus))
            .map(|i| {
                let sample = PromSample {
                    uptime_seconds: Some(1_209_650),
                    root_disk: Some(DiskUsage {
                        total_bytes: 250_880_000_000,
                        used_bytes: 100_352_000_000,
                        available_bytes: 137_728_000_000,
                    }),
                    load: Some(Load {
                        one: 0.4,
                        five: 0.35,
                        fifteen: 0.3,
                    }),
                    memory: Some(Memory {
                        total_bytes: 34_359_738_368,
                        available_bytes: 21_474_836_480,
                    }),
                    age_seconds: 12,
                };
                (i.clone(), sample)
            })
            .collect()))
    }

    pub(crate) async fn cache_lookup(
        &self,
        op: &CacheRequest,
        cancel: &CancellationToken,
    ) -> Result<NarCache, Cancelled> {
        after(120, cancel).await?;
        Ok(match script(&op.machine) {
            Some(s) if s.cached => NarCache::Cached,
            _ => NarCache::NotCached,
        })
    }

    pub(crate) async fn resolve(
        &self,
        op: &ResolveRequest,
        cancel: &CancellationToken,
    ) -> Result<BTreeMap<String, Vec<IpAddr>>, Cancelled> {
        after(200, cancel).await?;
        // Both lab names answer with one address: the wildcard-DNS trap the lint is for.
        let shared: IpAddr = [192, 168, 1, 30].into();
        Ok(op
            .hosts
            .keys()
            .map(|machine| (machine.clone(), vec![shared]))
            .collect())
    }
}
