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
    CacheRequest, ClosureAnswer, DeployedClosureRequest, EvalRequest, InventoryAnswer,
    PendingClosureRequest, PreflightAnswer, ProbeRequest, PrometheusAnswer, PrometheusRequest,
    ResolveRequest,
};
use auspex_core::{Config, DiffConfig, FreezeConfig, PrometheusConfig};
use auspex_domain::closure::ClosurePaths;
use auspex_domain::eval::{EvalFailure, EvalOutcome, EvalSuccess};
use auspex_domain::fleet::parse_inventory;
use auspex_domain::observe::{
    DiskUsage, Load, Memory, ProbeExit, ProbeOutcome, PromSample, StorePath, parse_probe,
};
use auspex_domain::preflight::{TreeState, Upstream};
use auspex_domain::status::NarCache;
use auspex_effects::clan::{ClanRun, RunEnd};
use auspex_effects::nix::EvalItem;
use auspex_effects::{CancellationToken, Cancelled};
use tokio::sync::mpsc::UnboundedSender;

use crate::output::Stream;

/// The system the demo pretends to run on: `b` refuses the darwin host (`genetor`).
pub const LOCAL_SYSTEM: &str = "x86_64-linux";

/// The demo's flake, shown as the fleet's location.
pub const FLAKE: &str = "demo:omnissiah";

/// The demo machine marked public, so its update asks for its name to be typed.
pub const PUBLIC_MACHINE: &str = "cypramundi";

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
        diff: Some(DiffConfig {
            timeout_seconds: 120,
        }),
        allow_act: true,
        local_system: LOCAL_SYSTEM.to_owned(),
        // The demo has one machine whose update asks for its name typed.
        public_machines: std::collections::BTreeSet::from([PUBLIC_MACHINE.to_owned()]),
        preflight_timeout_seconds: 5,
        freeze: FreezeConfig::default(),
        rolling: auspex_core::RollingConfig::default(),
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
    Script {
        cached: true,
        ..m("sol", Probe::Behind, 420, 0)
    },
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
  "genetor":    {"machineClass": "darwin", "deploy": {"targetHost": "admin@192.168.1.109"}, "tags": ["all", "darwin", "workstation"]},
  "io":         {"deploy": {"targetHost": "root@io.lab.example"}, "tags": ["base", "dev"]},
  "luna":       {"deploy": {"targetHost": "root@192.168.1.11", "buildHost": "localhost"}, "tags": ["k8s", "k8s-server"]},
  "mars":       {"deploy": {"targetHost": "root@192.168.1.20", "buildHost": "localhost"}, "tags": ["all", "nixos", "k8s", "k8s-master"]},
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
                 root_disk_kb=494384795 211234400 283150395\n\
                 store_disk_kb=494384795 211234400 283150395\nauspex_end=1\n"
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
             root_disk_kb=245000000 98000000 134500000\n\
             store_disk_kb=245000000 98000000 134500000\nauspex_end=1\n"
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
        system: Some(
            if darwin(s.name) {
                "aarch64-darwin"
            } else {
                LOCAL_SYSTEM
            }
            .to_owned(),
        ),
    })
}

// ---------------------------------------------------------------------------------------
// The package preview's closures
// ---------------------------------------------------------------------------------------

/// Packages both closures hold, at the same version.
const SHARED: &[&str] = &[
    "bash-5.2p37",
    "coreutils-9.5",
    "util-linux-2.40.2",
    "zlib-1.3.1",
    "xz-5.6.3",
    "gcc-14.2.1.20250322-lib",
    "openresolv-3.13.2",
    "iproute2-6.12.0",
    "dbus-1.14.10",
    "shadow-4.16.0",
    "perl-5.40.0",
    "gnugrep-3.11",
    "findutils-4.10.0",
    "nixos-option",
];

/// Packages whose version changed: deployed, then pending. The first four are the ones the
/// preview always shows first; the rest are major-version bumps, which collapse into a count.
const CHANGED: &[(&str, &str, &str)] = &[
    ("linux", "6.12.38", "6.12.41"),
    ("systemd", "256.10", "257.2"),
    ("glibc", "2.40-66", "2.40-77"),
    ("openssl", "3.4.1", "3.4.2"),
    ("openssh", "9.9p1", "9.9p2"),
    ("curl", "8.11.1", "8.12.0"),
    ("postgresql", "16.6", "17.2"),
    ("nodejs", "20.18.1", "22.12.0"),
    ("firefox", "133.0", "134.0"),
    ("firefox-unwrapped", "133.0", "134.0"),
    (
        "gnome-settings-daemon",
        "47.2-gsettings-schemas",
        "48.0-gsettings-schemas",
    ),
    ("python3", "3.12.8", "3.13.1"),
];

/// Same version, different build: `nix` is rebuilt, which the preview says in its own line.
const REBUILT: &[(&str, &str)] = &[("nix", "2.28.3")];

const ADDED: &[(&str, &str)] = &[
    ("ripgrep", "14.1.1"),
    ("fd", "10.2.0"),
    ("tailscale", "1.80.0"),
];

const REMOVED: &[(&str, &str)] = &[("htop", "3.3.0"), ("nano", "8.3")];

/// `/nix/store/<hash>-<name>`, with a hash made from the name and a salt: a different salt is
/// the same package built differently.
fn store_path(name: &str, salt: u8) -> StorePath {
    StorePath::new(format!("/nix/store/{}-{name}", hash(name, salt)))
        .expect("the demo's paths are store paths")
}

fn closure(machine: &str, pending: bool) -> ClosurePaths {
    let mut paths: Vec<StorePath> = SHARED.iter().map(|n| store_path(n, 0)).collect();
    paths.push(
        StorePath::new(system(machine, u8::from(!pending)))
            .expect("the demo's paths are store paths"),
    );
    for (name, before, after) in CHANGED {
        paths.push(store_path(
            &format!("{name}-{}", if pending { after } else { before }),
            0,
        ));
    }
    for (name, version) in REBUILT {
        paths.push(store_path(&format!("{name}-{version}"), u8::from(pending)));
    }
    let (only_here, unversioned) = if pending {
        (ADDED, "boot.json")
    } else {
        (REMOVED, "kbd-layout")
    };
    for (name, version) in only_here {
        paths.push(store_path(&format!("{name}-{version}"), 0));
    }
    paths.push(store_path(unversioned, 0));
    // A line a machine's login banner could print on stdout: dropped, and counted.
    ClosurePaths {
        paths,
        dropped: usize::from(!pending),
    }
}

/// The script. `Demo::default()` is what `--demo` runs; the two fields exist for the shell's
/// own tests, to make the evaluator misbehave in the two ways the shell must not hide.
#[derive(Debug, Default)]
pub struct Demo {
    /// Machines the evaluator finishes without a result for.
    pub omit_from_eval: Vec<String>,
    /// The evaluator's stream ends with no `Finished` at all.
    pub end_without_finished: bool,
    /// Run `u` and `b` for real, with these adapters (fake `clan` and `nix` programs), over the
    /// scripted fleet. `None`: actions are scripted too and touch nothing.
    pub real_actions: Option<Box<crate::backend::Real>>,
    /// What the checks before an update find in the shell. `None`: a clean, pushed tree and a
    /// closure of 2.3 GiB. The shell's own tests set it to make each check warn or refuse.
    pub preflight_answer: Option<PreflightAnswer>,
    /// Run the checks before an update for real, with `real_actions`' adapters (a fake `git` and
    /// a fake `nix`), instead of answering them from `preflight_answer`. For the shell's own
    /// tests of what cancelling them does to the processes.
    pub real_preflight: bool,
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

    pub(crate) async fn deployed_closure(
        &self,
        op: &DeployedClosureRequest,
        cancel: &CancellationToken,
    ) -> Result<ClosureAnswer, Cancelled> {
        after(300, cancel).await?;
        Ok(match op.machine.as_str() {
            // The slow, failing one: what a hung `nix-store` over ssh looks like.
            "sol" => Err(format!(
                "deployed closure query timed out after {:?}",
                Duration::from_secs(op.timeout_seconds)
            )),
            machine => Ok(closure(machine, false)),
        })
    }

    pub(crate) async fn pending_closure(
        &self,
        op: &PendingClosureRequest,
        cancel: &CancellationToken,
    ) -> Result<ClosureAnswer, Cancelled> {
        after(500, cancel).await?;
        Ok(Ok(closure(&op.machine, true)))
    }

    pub(crate) async fn preflight(
        &self,
        op: &auspex_core::effects::PreflightRequest,
        cancel: &CancellationToken,
    ) -> Result<PreflightAnswer, Cancelled> {
        if self.real_preflight
            && let Some(real) = &self.real_actions
        {
            return real.preflight(op, cancel).await;
        }
        after(250, cancel).await?;
        Ok(self
            .preflight_answer
            .clone()
            .unwrap_or_else(|| PreflightAnswer {
                tree: Ok(TreeState {
                    changed_files: 0,
                    upstream: Upstream::Tracking {
                        name: "origin/main".to_owned(),
                        ahead: 0,
                        behind: 0,
                    },
                }),
                // The pending closure of the preview, 32 MiB a path: with the machine's own
                // closure known the estimate counts only the paths it does not have.
                closure: Ok(closure(&op.machine, true)
                    .paths
                    .into_iter()
                    .map(|p| (p, 32 << 20))
                    .collect()),
            }))
    }
}

impl Demo {
    /// `Effect::RunAction`: a few scripted lines and a result, touching nothing. An update of
    /// the unreachable host fails at ssh, so the explanation can be seen.
    pub(crate) async fn run_action(
        &self,
        op: &auspex_core::effects::ActionRequest,
        cancel: &CancellationToken,
        sink: &(dyn Fn(Stream, &str) + Sync),
    ) -> Result<Result<ClanRun, String>, Cancelled> {
        use auspex_core::ActionKind;
        if let Some(real) = &self.real_actions {
            return real.run_action(op, cancel, sink).await;
        }
        let unreachable =
            script(&op.machine).is_some_and(|s| matches!(s.probe, Probe::Unreachable(_)));
        sink(
            Stream::Note,
            &format!(
                "demo: nothing is run; {} {} is scripted",
                op.kind.label(),
                op.machine
            ),
        );
        let steps: &[(Stream, &str)] = match op.kind {
            ActionKind::Build => &[
                (Stream::Stderr, "these 3 derivations will be built:"),
                (Stream::Stdout, "building the system configuration..."),
                (
                    Stream::Stdout,
                    "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-demo",
                ),
            ],
            ActionKind::Update if unreachable => &[
                (Stream::Stdout, "building the system configuration..."),
                (
                    Stream::Stderr,
                    "ssh: connect to host 192.168.1.20 port 22: Connection timed out",
                ),
            ],
            ActionKind::Update => &[
                (Stream::Stdout, "building the system configuration..."),
                (Stream::Stdout, "copying closure to the target host..."),
                (Stream::Stderr, "warning: the demo has no real machine"),
                (Stream::Stdout, "activating the new configuration..."),
                (Stream::Stdout, "done"),
            ],
        };
        for (stream, text) in steps {
            after(400, cancel).await?;
            sink(*stream, text);
        }
        let code = if unreachable && op.kind == ActionKind::Update {
            255
        } else {
            0
        };
        Ok(Ok(ClanRun {
            end: RunEnd::Exited {
                code: Some(code),
                signal: None,
            },
            lines_dropped: 0,
            long_lines: 0,
        }))
    }
}

impl Demo {
    /// `Effect::Kube`: the demo has no cluster (and none of its machines has a rolling tag, so
    /// `U` never asks); a call that arrives anyway says so. A run with real adapters (a test with
    /// a fake `kubectl`) asks them.
    pub(crate) async fn kube(
        &self,
        op: &auspex_core::effects::KubeRequest,
        cancel: &CancellationToken,
        sink: &(dyn Fn(Stream, &str) + Sync),
    ) -> Result<auspex_core::effects::KubeAnswer, Cancelled> {
        if let Some(real) = &self.real_actions {
            return real.kube(op, cancel, sink).await;
        }
        after(100, cancel).await?;
        Ok(Err("the demo has no cluster".to_owned()))
    }

    /// `Effect::Await`: a scripted machine is healthy a moment after its (scripted) update. With
    /// real adapters and a node to wait for, the node is waited for with the real `kubectl`.
    pub(crate) async fn await_healthy(
        &self,
        op: &auspex_core::effects::AwaitRequest,
        cancel: &CancellationToken,
        sink: &(dyn Fn(Stream, &str) + Sync),
    ) -> Result<auspex_core::effects::AwaitAnswer, Cancelled> {
        after(300, cancel).await?;
        match &self.real_actions {
            Some(real) if op.node.is_some() => real.await_loop(op, cancel, sink, false).await,
            _ => Ok(Ok(())),
        }
    }
}

impl Demo {
    /// `Effect::Tree`: a clean, pushed tree (or what `preflight_answer` says), as for the checks.
    pub(crate) async fn tree(
        &self,
        op: &auspex_core::effects::TreeRequest,
        cancel: &CancellationToken,
    ) -> Result<Result<TreeState, String>, Cancelled> {
        if self.real_preflight
            && let Some(real) = &self.real_actions
        {
            return real.tree(op, cancel).await;
        }
        after(100, cancel).await?;
        Ok(self.preflight_answer.clone().map_or_else(
            || {
                Ok(TreeState {
                    changed_files: 0,
                    upstream: Upstream::Tracking {
                        name: "origin/main".to_owned(),
                        ahead: 0,
                        behind: 0,
                    },
                })
            },
            |a| a.tree,
        ))
    }
}
