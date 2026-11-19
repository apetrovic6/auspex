//! A hand-driven shell for the core's tests: it keeps every request the core makes, by
//! kind, and resolves the ones a test chooses, in the order the test chooses. No I/O, no
//! async runtime, no clock.

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the harness"
)]

use std::collections::BTreeMap;

use auspex_core::effects::{
    CacheRequest, DeployedClosureRequest, EvalAnswer, EvalRequest, InventoryRequest,
    PendingClosureRequest, ProbeRequest, PrometheusRequest, ResolveRequest,
};
use auspex_core::{
    Auspex, Config, Core, DiffConfig, Effect, Event, PrometheusConfig, RequestId, ViewModel,
};
use auspex_domain::eval::{EvalFailure, EvalOutcome, EvalSuccess};
use auspex_domain::fleet::{Machine, parse_inventory};
use auspex_domain::observe::{ProbeExit, ProbeOutcome, PromSample, StorePath, parse_probe};
use auspex_domain::report::MachineReport;
use crux_core::Request;
use crux_core::capability::Operation;

/// A store path whose hash starts with `c` (one of nix's base-32 characters).
pub fn path(c: char, name: &str) -> String {
    format!("/nix/store/{c}123456789abcdfghijklmnpqrsvwxyz-{name}")
}

/// A probe that ran to its end on NixOS. `extra` is more `key=value` lines.
pub fn observed(current: &str, extra: &str) -> ProbeOutcome {
    parse_probe(
        &format!(
            "auspex_probe=1\nprobe=nixos\ncurrent_system={current}\nsystem_state=running\n\
             failed_units=[]\nuptime_seconds=100\nload=0.5 0.25 0.125\n\
             root_disk_kb=1000 400 600\n{extra}auspex_end=1\n"
        ),
        "",
        ProbeExit::Code(0),
    )
}

/// A probe of a machine with one failed unit.
pub fn observed_failed_unit(current: &str, unit: &str) -> ProbeOutcome {
    parse_probe(
        &format!(
            "auspex_probe=1\nprobe=nixos\ncurrent_system={current}\nsystem_state=degraded\n\
             failed_units=[{{\"unit\":\"{unit}\"}}]\nuptime_seconds=100\n\
             load=0.5 0.25 0.125\nroot_disk_kb=1000 400 600\nauspex_end=1\n"
        ),
        "",
        ProbeExit::Code(0),
    )
}

/// ssh failed.
pub fn unreachable(reason: &str) -> ProbeOutcome {
    ProbeOutcome::Unreachable {
        reason: reason.to_owned(),
    }
}

/// An evaluated toplevel.
pub fn evaluated(machine: &str, out: &str) -> EvalOutcome {
    EvalOutcome::Success(EvalSuccess {
        attr: machine.to_owned(),
        attr_path: vec![machine.to_owned()],
        drv_path: StorePath::new(format!("{out}.drv")).unwrap(),
        out_path: StorePath::new(out).unwrap(),
    })
}

/// A failed evaluation.
pub fn eval_failed(machine: &str, lines: &[&str]) -> EvalOutcome {
    EvalOutcome::Error(EvalFailure {
        attr: machine.to_owned(),
        attr_path: vec![machine.to_owned()],
        error_tail: lines.iter().map(|l| (*l).to_owned()).collect(),
    })
}

/// A Prometheus sample.
pub const fn sample(uptime: u64, age_seconds: u64) -> PromSample {
    PromSample {
        uptime_seconds: Some(uptime),
        root_disk: None,
        load: None,
        memory: None,
        age_seconds,
    }
}

/// Machines from inventory JSON.
pub fn inventory(json: &str) -> Vec<Machine> {
    parse_inventory(json).unwrap()
}

/// Three IP-literal machines.
pub const THREE: &str = r#"{
  "terra": {"deploy": {"targetHost": "root@192.168.1.10"}},
  "luna":  {"deploy": {"targetHost": "root@192.168.1.11"}},
  "sol":   {"deploy": {"targetHost": "root@192.168.1.12"}}
}"#;

/// A configuration with Prometheus (max age 60 s) and a binary cache.
pub fn full_config() -> Config {
    Config {
        flake: "/fleet".to_owned(),
        target_overrides: BTreeMap::new(),
        prometheus: Some(PrometheusConfig {
            instances: BTreeMap::new(),
            max_age_seconds: 60,
        }),
        binary_cache: true,
        diff: None,
    }
}

/// [`full_config`] with the package preview on, `timeouts.diff` being 90 s.
pub fn diff_config() -> Config {
    Config {
        diff: Some(DiffConfig {
            timeout_seconds: 90,
        }),
        ..full_config()
    }
}

/// A configuration with neither Prometheus nor a cache.
pub fn bare_config() -> Config {
    Config {
        flake: "/fleet".to_owned(),
        ..Config::default()
    }
}

/// Every request the core has made and the test has not resolved yet, by kind.
pub struct Shell {
    pub core: Core<Auspex>,
    pub inventory: Vec<Request<InventoryRequest>>,
    pub probes: Vec<Request<ProbeRequest>>,
    pub evals: Vec<Request<EvalRequest>>,
    pub prometheus: Vec<Request<PrometheusRequest>>,
    pub caches: Vec<Request<CacheRequest>>,
    pub resolves: Vec<Request<ResolveRequest>>,
    pub deployed: Vec<Request<DeployedClosureRequest>>,
    pub pending_closures: Vec<Request<PendingClosureRequest>>,
    /// Every cancel the core has emitted, in order.
    pub cancels: Vec<RequestId>,
    /// Every request id the core has issued, in order.
    pub issued: Vec<RequestId>,
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl Shell {
    pub fn new() -> Self {
        Self {
            core: Core::new(),
            inventory: Vec::new(),
            probes: Vec::new(),
            evals: Vec::new(),
            prometheus: Vec::new(),
            caches: Vec::new(),
            resolves: Vec::new(),
            deployed: Vec::new(),
            pending_closures: Vec::new(),
            cancels: Vec::new(),
            issued: Vec::new(),
        }
    }

    pub fn view(&self) -> ViewModel {
        self.core.view()
    }

    /// Sends an event and keeps what it asked for.
    pub fn send(&mut self, event: Event) {
        let effects = self.core.process_event(event);
        self.absorb(effects);
    }

    fn absorb(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Render(_) => {}
                Effect::Inventory(r) => {
                    self.issued.push(r.operation.id);
                    self.inventory.push(r);
                }
                Effect::Probe(r) => {
                    self.issued.push(r.operation.id);
                    self.probes.push(r);
                }
                Effect::Evaluate(r) => {
                    self.issued.push(r.operation.id);
                    self.evals.push(r);
                }
                Effect::Prometheus(r) => {
                    self.issued.push(r.operation.id);
                    self.prometheus.push(r);
                }
                Effect::CacheLookup(r) => {
                    self.issued.push(r.operation.id);
                    self.caches.push(r);
                }
                Effect::ResolveAddresses(r) => {
                    self.issued.push(r.operation.id);
                    self.resolves.push(r);
                }
                Effect::DeployedClosure(r) => {
                    self.issued.push(r.operation.id);
                    self.deployed.push(r);
                }
                Effect::PendingClosure(r) => {
                    self.issued.push(r.operation.id);
                    self.pending_closures.push(r);
                }
                Effect::Cancel(r) => self.cancels.push(r.operation.id),
            }
        }
    }

    /// Resolves `request` and keeps what that asked for.
    pub fn resolve<Op: Operation>(&mut self, request: &mut Request<Op>, output: Op::Output) {
        let effects = self
            .core
            .resolve(request, output)
            .expect("the request resolves");
        self.absorb(effects);
    }

    /// Drops every queued request the core has cancelled, as a real shell does on `Cancel`.
    pub fn drop_cancelled(&mut self) {
        let cancelled = self.cancels.clone();
        let live = |id: &RequestId| !cancelled.contains(id);
        self.inventory.retain(|r| live(&r.operation.id));
        self.probes.retain(|r| live(&r.operation.id));
        self.evals.retain(|r| live(&r.operation.id));
        self.prometheus.retain(|r| live(&r.operation.id));
        self.caches.retain(|r| live(&r.operation.id));
        self.resolves.retain(|r| live(&r.operation.id));
        self.deployed.retain(|r| live(&r.operation.id));
        self.pending_closures.retain(|r| live(&r.operation.id));
    }

    /// The number of requests of every kind not yet taken by the test.
    pub const fn waiting(&self) -> usize {
        self.inventory.len()
            + self.probes.len()
            + self.evals.len()
            + self.prometheus.len()
            + self.caches.len()
            + self.resolves.len()
            + self.deployed.len()
            + self.pending_closures.len()
    }

    /// Start, and answer the machine list with `json`.
    pub fn start_with(&mut self, config: Config, json: &str) {
        self.send(Event::Start(config));
        let mut list = self.take_inventory();
        self.resolve(&mut list, Ok(inventory(json)));
    }

    pub fn take_inventory(&mut self) -> Request<InventoryRequest> {
        assert_eq!(self.inventory.len(), 1, "one inventory request outstanding");
        self.inventory.remove(0)
    }

    pub fn take_probe(&mut self, machine: &str) -> Request<ProbeRequest> {
        let i = self
            .probes
            .iter()
            .position(|r| r.operation.machine == machine)
            .unwrap_or_else(|| panic!("no probe request for {machine}"));
        self.probes.remove(i)
    }

    pub fn take_eval(&mut self) -> Request<EvalRequest> {
        assert_eq!(self.evals.len(), 1, "one evaluation request outstanding");
        self.evals.remove(0)
    }

    pub fn take_prometheus(&mut self) -> Request<PrometheusRequest> {
        assert_eq!(
            self.prometheus.len(),
            1,
            "one Prometheus request outstanding"
        );
        self.prometheus.remove(0)
    }

    pub fn take_cache(&mut self, machine: &str) -> Request<CacheRequest> {
        let i = self
            .caches
            .iter()
            .position(|r| r.operation.machine == machine)
            .unwrap_or_else(|| panic!("no cache request for {machine}"));
        self.caches.remove(i)
    }

    pub fn take_deployed(&mut self, machine: &str) -> Request<DeployedClosureRequest> {
        let i = self
            .deployed
            .iter()
            .position(|r| r.operation.machine == machine)
            .unwrap_or_else(|| panic!("no deployed-closure request for {machine}"));
        self.deployed.remove(i)
    }

    pub fn take_pending(&mut self, machine: &str) -> Request<PendingClosureRequest> {
        let i = self
            .pending_closures
            .iter()
            .position(|r| r.operation.machine == machine)
            .unwrap_or_else(|| panic!("no pending-closure request for {machine}"));
        self.pending_closures.remove(i)
    }

    /// Answers `machine`'s probe.
    pub fn probe(&mut self, machine: &str, outcome: ProbeOutcome) {
        let mut r = self.take_probe(machine);
        self.resolve(&mut r, outcome);
    }

    /// Sends one item of the evaluation stream (which stays outstanding).
    pub fn eval_item(
        &mut self,
        eval: &mut Request<EvalRequest>,
        machine: &str,
        outcome: EvalOutcome,
    ) {
        self.resolve(
            eval,
            EvalAnswer::Machine {
                machine: machine.to_owned(),
                outcome,
            },
        );
    }

    /// The report row for `machine`.
    pub fn row(&self, machine: &str) -> MachineReport {
        self.view()
            .report
            .machines
            .into_iter()
            .find(|m| m.name == machine)
            .unwrap_or_else(|| panic!("no row for {machine}"))
    }
}
