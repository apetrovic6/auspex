//! What answers each effect: the real adapters in `auspex-effects`, or the `--demo` script.
//!
//! [`crate::shell::Shell::fulfil`] decides *that* an effect is answered and carries the
//! request; this module decides *how*. Every method returns the operation's output, or
//! `Err(Cancelled)` when its token fired.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use auspex_core::effects::{
    CacheRequest, EvalRequest, InventoryAnswer, InventoryRequest, ProbeRequest, PrometheusAnswer,
    PrometheusRequest, ResolveRequest,
};
use auspex_domain::fleet::ConnectTimeout;
use auspex_domain::observe::ProbeOutcome;
use auspex_domain::status::NarCache;
use auspex_effects::cache::{self, CacheParams, GitParams};
use auspex_effects::http::HttpClient;
use auspex_effects::nix::{self, EvalItem, EvalMachine, EvalParams, FlakeRef};
use auspex_effects::ssh::{self, ControlDir, ProbeParams};
use auspex_effects::{CancellationToken, Cancelled, dns, narinfo, prometheus};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{FlakeSource, Settings};
use crate::demo::Demo;

/// The Prometheus `instance` label a machine has when the config names none.
#[must_use]
pub fn default_instance(machine: &str) -> String {
    format!("{machine}:9100")
}

/// What to ask Prometheus for, keyed by the label to query, valued by the label the core
/// asked with.
///
/// The core asks under the label the config names (`prometheus_instance`) or, when it names
/// none, under the machine's own name (`PrometheusConfig::instance_for`). The default label
/// is `<name>:9100`, so a label the config does not name is queried as that.
#[must_use]
pub fn prometheus_queries(
    asked: &[String],
    explicit: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    asked
        .iter()
        .map(|label| {
            let query = if explicit.contains(label) {
                label.clone()
            } else {
                default_instance(label)
            };
            (query, label.clone())
        })
        .collect()
}

/// Prometheus's answer, keyed back by the labels the core asked with.
#[must_use]
pub fn rekey<T>(
    answer: BTreeMap<String, T>,
    queries: &BTreeMap<String, String>,
) -> BTreeMap<String, T> {
    answer
        .into_iter()
        .filter_map(|(query, v)| queries.get(&query).map(|l| (l.clone(), v)))
        .collect()
}

/// The adapters or the demo.
pub enum Backend {
    /// `auspex-effects`.
    Real(Box<Real>),
    /// The `--demo` script: no I/O at all.
    Demo(Demo),
}

impl Backend {
    /// `Effect::Inventory`.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn inventory(
        &self,
        op: &InventoryRequest,
        cancel: &CancellationToken,
    ) -> Result<InventoryAnswer, Cancelled> {
        match self {
            Self::Real(r) => r.inventory(op, cancel).await,
            Self::Demo(d) => d.inventory(cancel).await,
        }
    }

    /// `Effect::Probe`.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn probe(
        &self,
        op: &ProbeRequest,
        cancel: &CancellationToken,
        on_approval: &(dyn Fn(bool) + Sync),
    ) -> Result<ProbeOutcome, Cancelled> {
        match self {
            Self::Real(r) => r.probe(op, cancel, on_approval).await,
            Self::Demo(d) => d.probe(op, cancel).await,
        }
    }

    /// `Effect::Evaluate`: items go to `tx`, ending with `Finished` unless cancelled.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn evaluate(
        &self,
        op: &EvalRequest,
        cancel: &CancellationToken,
        tx: UnboundedSender<EvalItem>,
    ) -> Result<(), Cancelled> {
        match self {
            Self::Real(r) => r.evaluate(op, cancel, tx).await,
            Self::Demo(d) => d.evaluate(op, cancel, tx).await,
        }
    }

    /// `Effect::Prometheus`.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn prometheus(
        &self,
        op: &PrometheusRequest,
        cancel: &CancellationToken,
    ) -> Result<PrometheusAnswer, Cancelled> {
        match self {
            Self::Real(r) => r.prometheus(op, cancel).await,
            Self::Demo(d) => d.prometheus(op, cancel).await,
        }
    }

    /// `Effect::CacheLookup`.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn cache_lookup(
        &self,
        op: &CacheRequest,
        cancel: &CancellationToken,
    ) -> Result<NarCache, Cancelled> {
        match self {
            Self::Real(r) => r.cache_lookup(op, cancel).await,
            Self::Demo(d) => d.cache_lookup(op, cancel).await,
        }
    }

    /// `Effect::ResolveAddresses`.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn resolve(
        &self,
        op: &ResolveRequest,
        cancel: &CancellationToken,
    ) -> Result<BTreeMap<String, Vec<std::net::IpAddr>>, Cancelled> {
        match self {
            Self::Real(r) => r.resolve(op, cancel).await,
            Self::Demo(d) => d.resolve(op, cancel).await,
        }
    }
}

/// An absolute path to `name` on an absolute `PATH` entry, as `auspex-effects` would find
/// it, or `None`.
#[must_use]
pub fn on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Which `nix-eval-jobs` to run.
///
/// The one on `PATH` when it is there, so the system's own wins; otherwise the
/// absolute store path the package compiled in (`AUSPEX_NIX_EVAL_JOBS`, set in
/// `nix/auspex/package.nix`), the way `auspex-effects` finds `prlimit`. A dev build has no
/// compiled-in path and simply asks `PATH`.
#[must_use]
pub fn nix_eval_jobs() -> OsString {
    if on_path(nix::DEFAULT_NIX_EVAL_JOBS).is_some() {
        return OsString::from(nix::DEFAULT_NIX_EVAL_JOBS);
    }
    match option_env!("AUSPEX_NIX_EVAL_JOBS") {
        Some(p) if Path::new(p).is_absolute() => OsString::from(p),
        _ => OsString::from(nix::DEFAULT_NIX_EVAL_JOBS),
    }
}

/// A program, overridable by an undocumented environment variable. The overrides exist for
/// the end-to-end and signal tests (a fake `ssh`, `nix` or `nix-eval-jobs`); they are not
/// configuration, and the program must be an absolute path or a bare name either way
/// (`auspex-effects` refuses a relative one).
fn program(variable: &str, default: OsString) -> OsString {
    std::env::var_os(variable)
        .filter(|v| !v.is_empty())
        .unwrap_or(default)
}

/// The real adapters, with everything they need from the config.
pub struct Real {
    ssh: OsString,
    nix: OsString,
    nix_eval_jobs: OsString,
    git: OsString,
    control_dir: Result<ControlDir, String>,
    connect_timeout: ConnectTimeout,
    control_persist: u64,
    approval_deadline: Duration,
    probe_deadline: Duration,
    eval_timeout: Duration,
    workers: u32,
    max_memory_mib: u32,
    http: Result<HttpClient, String>,
    http_timeout: Duration,
    dns_timeout: Duration,
    prometheus_url: Option<String>,
    /// The labels the config names; any other label the core asks for is a machine name
    /// and is looked up as `<name>:9100`.
    explicit_instances: BTreeSet<String>,
    cache_url: Option<String>,
    cache_dir: Option<PathBuf>,
    flake_source: FlakeSource,
}

impl Real {
    /// The adapters configured from `settings`. Nothing is spawned here; the control
    /// directory is created and checked once.
    ///
    /// # Panics
    ///
    /// Never: `settings.timeouts.ssh_connect` was validated when the config was parsed, and
    /// the default is in range.
    #[must_use]
    pub fn new(settings: &Settings, flake_source: FlakeSource) -> Self {
        let t = settings.timeouts;
        let explicit_instances = settings
            .machines
            .values()
            .filter_map(|m| m.prometheus_instance.clone())
            .collect();
        Self {
            ssh: program("AUSPEX_SSH", OsString::from(ssh::DEFAULT_SSH)),
            nix: program("AUSPEX_NIX", OsString::from(nix::DEFAULT_NIX)),
            nix_eval_jobs: program("AUSPEX_NIX_EVAL_JOBS", nix_eval_jobs()),
            git: OsString::from(cache::DEFAULT_GIT),
            control_dir: ControlDir::prepare(&ControlDir::default_path()).map_err(|e| e.0),
            connect_timeout: ConnectTimeout::new(t.ssh_connect).unwrap_or(ConnectTimeout::DEFAULT),
            control_persist: settings.ssh.control_persist,
            approval_deadline: Duration::from_secs(t.ssh_approval),
            probe_deadline: Duration::from_secs(t.probe),
            eval_timeout: Duration::from_secs(t.eval),
            workers: settings.eval.workers,
            max_memory_mib: settings.eval.max_memory_mb,
            http: HttpClient::new(),
            // One HTTP request is no slower than one probe should be.
            http_timeout: Duration::from_secs(t.probe.max(1)),
            dns_timeout: Duration::from_secs(t.ssh_connect.max(1)),
            prometheus_url: settings.prometheus.as_ref().map(|p| p.url.clone()),
            explicit_instances,
            cache_url: settings.cache_url.clone(),
            cache_dir: cache::default_dir(),
            flake_source,
        }
    }

    /// What is wrong locally and will affect every request of a kind, said once at start
    /// rather than only on every row.
    #[must_use]
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Err(why) = &self.control_dir {
            notes.push(format!("ssh cannot be used, so every probe fails: {why}"));
        }
        if let Err(why) = &self.http
            && (self.prometheus_url.is_some() || self.cache_url.is_some())
        {
            notes.push(format!(
                "no HTTP client, so Prometheus and the cache badge are off: {why}"
            ));
        }
        notes
    }

    fn fleet_error(&self, flake: &str, reason: &str) -> String {
        format!(
            "cannot read the fleet at {flake} (from {}): {reason}",
            self.flake_source.describe()
        )
    }

    async fn inventory(
        &self,
        op: &InventoryRequest,
        cancel: &CancellationToken,
    ) -> Result<InventoryAnswer, Cancelled> {
        let flake = match FlakeRef::new(&op.flake) {
            Ok(f) => f,
            Err(e) => return Ok(Err(self.fleet_error(&op.flake, &e.0))),
        };
        // The machine list is one evaluation of the flake: the same budget as the rest.
        let answer = nix::inventory(&self.nix, &flake, self.eval_timeout, cancel).await?;
        Ok(answer.map_err(|reason| self.fleet_error(&op.flake, &reason)))
    }

    async fn probe(
        &self,
        op: &ProbeRequest,
        cancel: &CancellationToken,
        on_approval: &(dyn Fn(bool) + Sync),
    ) -> Result<ProbeOutcome, Cancelled> {
        let control_dir = match &self.control_dir {
            Ok(d) => d,
            Err(why) => {
                return Ok(ProbeOutcome::Unreachable {
                    reason: format!("cannot probe: {why}"),
                });
            }
        };
        let mut params = ProbeParams::new(control_dir);
        params.ssh_program = &self.ssh;
        params.connect_timeout = self.connect_timeout;
        params.control_persist = self.control_persist;
        params.approval_deadline = self.approval_deadline;
        params.on_approval = Some(on_approval);
        params.deadline = self.probe_deadline;
        ssh::probe(&op.target, op.class, &params, cancel).await
    }

    async fn evaluate(
        &self,
        op: &EvalRequest,
        cancel: &CancellationToken,
        tx: UnboundedSender<EvalItem>,
    ) -> Result<(), Cancelled> {
        let flake = match FlakeRef::new(&op.flake) {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(EvalItem::Finished {
                    error: Some(self.fleet_error(&op.flake, &e.0)),
                });
                return Ok(());
            }
        };
        let machines: Vec<EvalMachine> = op
            .machines
            .iter()
            .map(|m| EvalMachine {
                name: m.name.clone(),
                class: m.class,
            })
            .collect();
        let mut eval = EvalParams::new();
        eval.program = &self.nix_eval_jobs;
        eval.workers = self.workers;
        eval.max_memory_mib = self.max_memory_mib;
        eval.timeout = self.eval_timeout;
        let mut git = GitParams::new();
        git.program = &self.git;
        let cache = CacheParams {
            cache_dir: self.cache_dir.as_deref(),
            flake_dir: Path::new(&op.flake),
            git,
        };
        cache::evaluate_cached(&flake, &machines, &eval, &cache, cancel, tx).await
    }

    async fn prometheus(
        &self,
        op: &PrometheusRequest,
        cancel: &CancellationToken,
    ) -> Result<PrometheusAnswer, Cancelled> {
        let Some(url) = &self.prometheus_url else {
            return Ok(Err("prometheus is not configured".to_owned()));
        };
        let client = match &self.http {
            Ok(c) => c,
            Err(why) => return Ok(Err(why.clone())),
        };
        let asked = prometheus_queries(&op.instances, &self.explicit_instances);
        let wanted: BTreeSet<String> = asked.keys().cloned().collect();
        let answer = prometheus::query(
            client,
            url,
            &wanted,
            &SystemTime::now,
            self.http_timeout,
            cancel,
        )
        .await?;
        Ok(answer.map(|samples| rekey(samples, &asked)))
    }

    async fn cache_lookup(
        &self,
        op: &CacheRequest,
        cancel: &CancellationToken,
    ) -> Result<NarCache, Cancelled> {
        let (Some(url), Ok(client)) = (&self.cache_url, &self.http) else {
            return Ok(NarCache::Failed);
        };
        narinfo::lookup(client, url, &op.store_path, self.http_timeout, cancel).await
    }

    async fn resolve(
        &self,
        op: &ResolveRequest,
        cancel: &CancellationToken,
    ) -> Result<BTreeMap<String, Vec<std::net::IpAddr>>, Cancelled> {
        dns::resolve(&op.hosts, self.dns_timeout, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unnamed_instances_are_asked_as_name_9100_and_answered_by_name() {
        let explicit = BTreeSet::from(["terra-exporter:9100".to_owned()]);
        let asked = ["luna".to_owned(), "terra-exporter:9100".to_owned()];
        let q = prometheus_queries(&asked, &explicit);
        assert_eq!(
            q,
            BTreeMap::from([
                ("luna:9100".to_owned(), "luna".to_owned()),
                (
                    "terra-exporter:9100".to_owned(),
                    "terra-exporter:9100".to_owned()
                ),
            ])
        );
        let answer = BTreeMap::from([
            ("luna:9100".to_owned(), 1),
            ("terra-exporter:9100".to_owned(), 2),
            ("stranger:9100".to_owned(), 3),
        ]);
        assert_eq!(
            rekey(answer, &q),
            BTreeMap::from([
                ("luna".to_owned(), 1),
                ("terra-exporter:9100".to_owned(), 2),
            ])
        );
    }
}
