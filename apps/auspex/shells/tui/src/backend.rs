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
    ActionKind, ActionRequest, AwaitAnswer, AwaitRequest, CacheRequest, ClosureAnswer,
    DeployedClosureRequest, EvalRequest, ForgeHeadAnswer, ForgeHeadRequest, ForgeStatusAnswer,
    ForgeStatusRequest, InventoryAnswer, InventoryRequest, KubeAnswer, KubeOp, KubeReply,
    KubeRequest, PendingClosureRequest, PreflightAnswer, PreflightRequest, ProbeRequest,
    PrometheusAnswer, PrometheusRequest, ResolveRequest, ShellRequest, TreeRequest,
};
use auspex_domain::fleet::ConnectTimeout;
use auspex_domain::observe::ProbeOutcome;
use auspex_domain::preflight::TreeState;
use auspex_domain::status::NarCache;
use auspex_effects::cache::{self, CacheParams, GitParams};
use auspex_effects::clan::{self, ClanLine, ClanLocator, ClanRun, ResolveParams, RunParams};
use auspex_effects::closure::{self, PendingParams};
use auspex_effects::forge::{self, Token};
use auspex_effects::http::HttpClient;
use auspex_effects::kube::{self, KubeParams};
use auspex_effects::nix::{self, EvalItem, EvalMachine, EvalParams, FlakeRef};
use auspex_effects::ssh::{self, ControlDir, ProbeParams};
use auspex_effects::{CancellationToken, Cancelled, act, dns, narinfo, preflight, prometheus};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::config::{FlakeSource, Settings};
use crate::demo::Demo;
use crate::output::Stream;

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

    /// `Effect::ForgeHead`: `HEAD` of the fleet checkout, for the CI badge.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn forge_head(
        &self,
        op: &ForgeHeadRequest,
        cancel: &CancellationToken,
    ) -> Result<ForgeHeadAnswer, Cancelled> {
        match self {
            Self::Real(r) => r.forge_head(op, cancel).await,
            // The demo configures no forge, so the core never asks; if it did, say so.
            Self::Demo(_) => Ok(Err("the demo has no forge".to_owned())),
        }
    }

    /// `Effect::ForgeStatus`: the forge's combined status of that commit.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn forge_status(
        &self,
        op: &ForgeStatusRequest,
        cancel: &CancellationToken,
    ) -> Result<ForgeStatusAnswer, Cancelled> {
        match self {
            Self::Real(r) => r.forge_status(op, cancel).await,
            Self::Demo(_) => Ok(Err("the demo has no forge".to_owned())),
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

    /// `Effect::DeployedClosure`.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn deployed_closure(
        &self,
        op: &DeployedClosureRequest,
        cancel: &CancellationToken,
        on_approval: &(dyn Fn(bool) + Sync),
    ) -> Result<ClosureAnswer, Cancelled> {
        match self {
            Self::Real(r) => r.deployed_closure(op, cancel, on_approval).await,
            Self::Demo(d) => d.deployed_closure(op, cancel).await,
        }
    }

    /// `Effect::PendingClosure`.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn pending_closure(
        &self,
        op: &PendingClosureRequest,
        cancel: &CancellationToken,
    ) -> Result<ClosureAnswer, Cancelled> {
        match self {
            Self::Real(r) => r.pending_closure(op, cancel).await,
            Self::Demo(d) => d.pending_closure(op, cancel).await,
        }
    }

    /// `Effect::Preflight`: the checks before an update that need the world. The tree check
    /// and the closure size run together, each bounded by `op.timeout_seconds`.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn preflight(
        &self,
        op: &PreflightRequest,
        cancel: &CancellationToken,
    ) -> Result<PreflightAnswer, Cancelled> {
        match self {
            Self::Real(r) => r.preflight(op, cancel).await,
            Self::Demo(d) => d.preflight(op, cancel).await,
        }
    }

    /// `Effect::Tree`: the state of the fleet repository, for the plan of an update of all.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn tree(
        &self,
        op: &TreeRequest,
        cancel: &CancellationToken,
    ) -> Result<Result<TreeState, String>, Cancelled> {
        match self {
            Self::Real(r) => r.tree(op, cancel).await,
            Self::Demo(d) => d.tree(op, cancel).await,
        }
    }

    /// The `kubectl` and kubeconfig this run talks to the cluster with, when it has any: what the
    /// shell's record of cordoned nodes needs to uncordon one at exit.
    #[must_use]
    pub fn kubectl_target(&self) -> Option<(OsString, PathBuf)> {
        match self {
            Self::Real(r) => r.kubectl_target(),
            Self::Demo(d) => d.real_actions.as_ref().and_then(|r| r.kubectl_target()),
        }
    }

    /// `Effect::RunAction`: the process's lines go to `sink` as they are printed; the result is
    /// how it ended, or why it could not be started or was refused.
    ///
    /// `starting` is called once, right before an update's `clan` is spawned, and not at all when
    /// it never is: from then on the machine may be changed.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired: the process group has been killed and reaped.
    pub async fn run_action(
        &self,
        op: &ActionRequest,
        cancel: &CancellationToken,
        sink: &(dyn Fn(Stream, &str) + Sync),
        starting: &(dyn Fn() + Sync),
    ) -> Result<Result<ClanRun, String>, Cancelled> {
        match self {
            Self::Real(r) => r.run_action(op, cancel, sink, starting).await,
            Self::Demo(d) => d.run_action(op, cancel, sink, starting).await,
        }
    }

    /// `Effect::Kube`: one call to the cluster; a drain's output goes to `sink`.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired: `kubectl` has been killed and reaped.
    pub async fn kube(
        &self,
        op: &KubeRequest,
        cancel: &CancellationToken,
        sink: &(dyn Fn(Stream, &str) + Sync),
    ) -> Result<KubeAnswer, Cancelled> {
        match self {
            Self::Real(r) => r.kube(op, cancel, sink).await,
            Self::Demo(d) => d.kube(op, cancel, sink).await,
        }
    }

    /// `Effect::Await`: probe the updated machine (and read its node) every `op.poll_seconds` until it
    /// is healthy, or `op.timeout_seconds` has passed.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired.
    pub async fn await_healthy(
        &self,
        op: &AwaitRequest,
        cancel: &CancellationToken,
        sink: &(dyn Fn(Stream, &str) + Sync),
    ) -> Result<AwaitAnswer, Cancelled> {
        match self {
            Self::Real(r) => r.await_loop(op, cancel, sink, true).await,
            Self::Demo(d) => d.await_healthy(op, cancel, sink).await,
        }
    }

    /// `Effect::OpenShell`: the program and argv of the operator's own ssh session, or why there
    /// is none. Only says what to run (the TUI loop runs it, with the terminal): nothing here
    /// starts a process.
    ///
    /// # Errors
    ///
    /// The demo has no machines; the real backend's ssh cannot be found or has no control
    /// directory.
    pub fn shell_command(&self, op: &ShellRequest) -> Result<(PathBuf, Vec<String>), String> {
        match self {
            Self::Real(r) => r.shell_command(op),
            Self::Demo(_) => Err(
                "this is the demo: its machines are scripted, there is nothing to ssh to"
                    .to_owned(),
            ),
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

/// What the CI badge needs of the shell: the forge's URL, and the token read at start (or why
/// it could not be). The token is a [`Token`]: it does not print.
struct ForgeAdapter {
    url: String,
    /// `Ok(None)`: no `forge.token_file`, ask anonymously. `Err`: the file was refused (group- or
    /// world-accessible, not yours, unreadable, not a token): no request is made, because
    /// asking anonymously instead would hide that the operator's token is not in use.
    token: Result<Option<Token>, String>,
}

/// The real adapters, with everything they need from the config.
pub struct Real {
    ssh: OsString,
    nix: OsString,
    nix_store: OsString,
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
    /// The CI badge's forge, when `[forge]` is configured.
    forge: Option<ForgeAdapter>,
    cache_dir: Option<PathBuf>,
    flake_source: FlakeSource,
    /// Which `clan` each fleet pins, found once and kept for the run.
    clan: ClanLocator,
    /// An explicit path to `clan`, set only by [`Self::for_actions`] (the tests' fake): wins over
    /// resolution. A real run always resolves the fleet's pinned `clan` from its lock.
    clan_path: Option<PathBuf>,
    /// `kubectl`, and the kubeconfig it is given (or why there is none): what `U` asks the
    /// cluster with.
    kubectl: OsString,
    kubeconfig: Result<PathBuf, String>,
}

impl std::fmt::Debug for Real {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Real").finish_non_exhaustive()
    }
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
            nix_store: program("AUSPEX_NIX_STORE", OsString::from(nix::DEFAULT_NIX_STORE)),
            nix_eval_jobs: program("AUSPEX_NIX_EVAL_JOBS", nix_eval_jobs()),
            git: program("AUSPEX_GIT", OsString::from(cache::DEFAULT_GIT)),
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
            // The token file is read here, once, at start.
            forge: settings.forge.as_ref().map(|f| ForgeAdapter {
                url: f.url.clone(),
                token: f
                    .token_file
                    .as_deref()
                    .map(forge::read_token_file)
                    .transpose(),
            }),
            cache_dir: cache::default_dir(),
            flake_source,
            clan: ClanLocator::new(),
            // Never from the environment: an inherited variable (a profile, a wrapper, a
            // leftover from debugging) would change which binary deploys. Only the tests'
            // constructor, `for_actions`, sets it.
            clan_path: None,
            kubectl: program("AUSPEX_KUBECTL", OsString::from(kube::DEFAULT_KUBECTL)),
            kubeconfig: crate::config::resolve_kubeconfig(settings.kubeconfig.as_deref()),
        }
    }

    /// How the fleet's pinned `clan` is found for `u`: with the locator's own `nix` (from
    /// `PATH`), not this backend's, which `AUSPEX_NIX` or the tests' constructor may have set. The locator
    /// runs that program to build the clan that then deploys, so an inherited override (a profile,
    /// a wrapper, a leftover) must not be able to choose it.
    fn resolve_params(&self) -> ResolveParams<'_> {
        let mut resolve = ResolveParams::new();
        resolve.config_path = self.clan_path.as_deref();
        resolve
    }

    /// `s`: ssh as the operator would run it by hand, to the machine's target. The program is
    /// `AUSPEX_SSH` or `ssh` (the same one the probes use, so a test's fake serves both), and
    /// the argv is the domain's [`SshOperation::Interactive`](auspex_domain::fleet::SshOperation).
    /// No control directory means no way to name the probes' socket, so no login: the same
    /// refusal every probe shows, rather than a login that silently skips the master.
    fn shell_command(&self, op: &ShellRequest) -> Result<(PathBuf, Vec<String>), String> {
        let dir = self
            .control_dir
            .as_ref()
            .map_err(|why| format!("ssh cannot be used: {why}"))?;
        ssh::interactive_command(&self.ssh, &op.target, dir)
    }

    /// Adapters that can run actions and nothing else: for the shell's own tests, which run
    /// the real `u` and `b` against fake `clan` and `nix` programs over a scripted fleet
    /// ([`crate::demo::Demo::real_actions`]). No control directory is created and no other
    /// adapter has what it needs, so a request of any other kind answers with a failure.
    #[must_use]
    pub fn for_actions(nix: OsString, clan_path: Option<PathBuf>) -> Self {
        let off = || "this backend only runs actions".to_owned();
        Self {
            ssh: OsString::from(ssh::DEFAULT_SSH),
            nix,
            nix_store: OsString::from(nix::DEFAULT_NIX_STORE),
            nix_eval_jobs: OsString::from(nix::DEFAULT_NIX_EVAL_JOBS),
            git: OsString::from(cache::DEFAULT_GIT),
            control_dir: Err(off()),
            connect_timeout: ConnectTimeout::DEFAULT,
            control_persist: 60,
            approval_deadline: Duration::from_secs(60),
            probe_deadline: Duration::from_secs(30),
            eval_timeout: Duration::from_secs(60),
            workers: 1,
            max_memory_mib: 1024,
            http: Err(off()),
            http_timeout: Duration::from_secs(5),
            dns_timeout: Duration::from_secs(5),
            prometheus_url: None,
            explicit_instances: BTreeSet::new(),
            cache_url: None,
            forge: None,
            cache_dir: None,
            flake_source: FlakeSource::Default,
            clan: ClanLocator::new(),
            clan_path,
            kubectl: OsString::from(kube::DEFAULT_KUBECTL),
            kubeconfig: Err(off()),
        }
    }

    /// These adapters with `git` and a binary cache URL as given: for the shell's own tests,
    /// which run the checks before an update against a fake `git` and a fake `nix`.
    #[must_use]
    pub fn with_git_and_cache(mut self, git: OsString, cache_url: Option<String>) -> Self {
        self.git = git;
        self.cache_url = cache_url;
        self
    }

    fn kubectl_target(&self) -> Option<(OsString, PathBuf)> {
        self.kubeconfig
            .as_ref()
            .ok()
            .map(|p| (self.kubectl.clone(), p.clone()))
    }

    /// These adapters with `kubectl` and the kubeconfig as given: for the shell's own tests, which
    /// run `U` against a fake `kubectl`.
    #[must_use]
    pub fn with_kubectl(mut self, kubectl: OsString, kubeconfig: PathBuf) -> Self {
        self.kubectl = kubectl;
        self.kubeconfig = Ok(kubeconfig);
        self
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
        if let Some(f) = &self.forge {
            if let Err(why) = &f.token {
                notes.push(format!(
                    "the CI badge is unknown, and no request is made: {why}"
                ));
            }
            if let Err(why) = &self.http {
                notes.push(format!("no HTTP client, so the CI badge is unknown: {why}"));
            }
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

    /// `Effect::ForgeHead` with the real `git`, each command bounded by the HTTP timeout.
    async fn forge_head(
        &self,
        op: &ForgeHeadRequest,
        cancel: &CancellationToken,
    ) -> Result<ForgeHeadAnswer, Cancelled> {
        let flake = match FlakeRef::new(&op.flake) {
            Ok(f) => f,
            Err(e) => return Ok(Err(self.fleet_error(&op.flake, &e.0))),
        };
        let mut git = GitParams::new();
        git.program = &self.git;
        git.timeout = self.http_timeout;
        let token = self
            .forge
            .as_ref()
            .and_then(|f| f.token.as_ref().ok())
            .and_then(Option::as_ref);
        forge::head(&git, &flake, op.want_origin, token, cancel).await
    }

    /// `Effect::ForgeStatus`: one request, with the token when there is one.
    async fn forge_status(
        &self,
        op: &ForgeStatusRequest,
        cancel: &CancellationToken,
    ) -> Result<ForgeStatusAnswer, Cancelled> {
        let Some(f) = &self.forge else {
            return Ok(Err("forge is not configured".to_owned()));
        };
        let client = match &self.http {
            Ok(c) => c,
            Err(why) => return Ok(Err(format!("forge: {why}"))),
        };
        let token = match &f.token {
            Ok(t) => t.as_ref(),
            Err(why) => return Ok(Err(why.clone())),
        };
        forge::combined_status(
            client,
            &f.url,
            &op.repo,
            &op.commit,
            token,
            self.http_timeout,
            cancel,
        )
        .await
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

    async fn deployed_closure(
        &self,
        op: &DeployedClosureRequest,
        cancel: &CancellationToken,
        on_approval: &(dyn Fn(bool) + Sync),
    ) -> Result<ClosureAnswer, Cancelled> {
        let control_dir = match &self.control_dir {
            Ok(d) => d,
            Err(why) => {
                return Ok(Err(format!("cannot query the deployed closure: {why}")));
            }
        };
        // The probe's parameters: the same ssh, control directory and approval phase, so a
        // warm master is reused and a cold one authenticates once, for both. Only the script
        // deadline differs: `timeouts.diff`.
        let mut params = ProbeParams::new(control_dir);
        params.ssh_program = &self.ssh;
        params.connect_timeout = self.connect_timeout;
        params.control_persist = self.control_persist;
        params.approval_deadline = self.approval_deadline;
        params.on_approval = Some(on_approval);
        params.deadline = Duration::from_secs(op.timeout_seconds);
        closure::deployed(&op.target, &params, cancel).await
    }

    async fn pending_closure(
        &self,
        op: &PendingClosureRequest,
        cancel: &CancellationToken,
    ) -> Result<ClosureAnswer, Cancelled> {
        let params = PendingParams {
            nix: &self.nix,
            nix_store: &self.nix_store,
            cache_url: self.cache_url.as_deref(),
            timeout: Duration::from_secs(op.timeout_seconds),
        };
        closure::pending(&op.store_path, &params, cancel).await
    }

    /// The fleet repository's state (one `git status`), bounded by `timeout`.
    async fn tree_of(
        &self,
        flake: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Result<TreeState, String>, Cancelled> {
        let flake_ref = match FlakeRef::new(flake) {
            Ok(f) => f,
            Err(e) => return Ok(Err(self.fleet_error(flake, &e.0))),
        };
        let mut git = GitParams::new();
        git.program = &self.git;
        git.timeout = timeout;
        preflight::fleet_tree(&git, &flake_ref, cancel).await
    }

    /// `Effect::Tree` with the real `git`.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired: the process group has been killed and reaped.
    pub async fn tree(
        &self,
        op: &TreeRequest,
        cancel: &CancellationToken,
    ) -> Result<Result<TreeState, String>, Cancelled> {
        self.tree_of(
            &op.flake,
            Duration::from_secs(op.timeout_seconds.max(1)),
            cancel,
        )
        .await
    }

    /// `Effect::Preflight` with the real adapters: `git` and `nix`, both bounded by
    /// `op.timeout_seconds` (at least a second).
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired: both processes have been killed and reaped.
    pub async fn preflight(
        &self,
        op: &PreflightRequest,
        cancel: &CancellationToken,
    ) -> Result<PreflightAnswer, Cancelled> {
        // The same bound for both; never "none". Each adapter enforces it on its own children.
        let timeout = Duration::from_secs(op.timeout_seconds.max(1));
        let tree = self.tree_of(&op.flake, timeout, cancel);
        let size = async {
            let params = PendingParams {
                nix: &self.nix,
                nix_store: &self.nix_store,
                cache_url: self.cache_url.as_deref(),
                timeout,
            };
            preflight::closure_size(&op.store_path, &params, cancel).await
        };
        let (tree, closure) = tokio::join!(tree, size);
        Ok(PreflightAnswer {
            tree: tree?,
            closure: closure?,
        })
    }

    /// The wait behind `Effect::Await`. With `probe` off the machine's own health is taken as
    /// given and only its node is waited for: the demo's scripted machines never change what they
    /// run, so a test of the node wait against a fake `kubectl` cannot probe them.
    pub(crate) async fn await_loop(
        &self,
        op: &AwaitRequest,
        cancel: &CancellationToken,
        sink: &(dyn Fn(Stream, &str) + Sync),
        probe: bool,
    ) -> Result<AwaitAnswer, Cancelled> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(op.timeout_seconds);
        // The update has just ended: a node counts as back only for a heartbeat from now on. (The
        // clock is this machine's; the kubelets' are NTP's: a skew of seconds would only make the
        // wait a little longer.)
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_secs()).ok());
        loop {
            let machine = if probe {
                let request = ProbeRequest {
                    id: op.id,
                    generation: auspex_core::effects::Generation::default(),
                    machine: op.machine.clone(),
                    class: op.class,
                    target: op.target.clone(),
                };
                let outcome = self.probe(&request, cancel, &|_| {}).await?;
                auspex_domain::roll::healthy(&outcome, &op.expect, &op.baseline)
            } else {
                Ok(())
            };
            let last = match (machine, &op.node) {
                (Err(why), _) => why,
                (Ok(()), None) => return Ok(Ok(())),
                (Ok(()), Some(node)) => {
                    let ask = KubeRequest {
                        id: op.id,
                        log: op.log,
                        context: op.context.clone(),
                        op: KubeOp::Nodes,
                    };
                    match self.kube(&ask, cancel, sink).await? {
                        Ok(KubeReply::Nodes(nodes)) => {
                            use auspex_domain::roll::NodeState;
                            match auspex_domain::roll::node_state(&nodes, node, since) {
                                NodeState::Ready => {
                                    match self.etcd_back(op, &nodes, node, cancel, sink).await? {
                                        None => return Ok(Ok(())),
                                        Some(why) => why,
                                    }
                                }
                                NodeState::NotReady => format!("node {node} is NotReady"),
                                NodeState::Stale => format!(
                                    "node {node} is Ready, but its kubelet has not been heard from \
                                     since the update"
                                ),
                                NodeState::Missing => format!("node {node} is not in the cluster"),
                            }
                        }
                        Ok(other) => format!("kubectl answered {other:?} to a node list"),
                        Err(why) => format!("cannot read the cluster: {why}"),
                    }
                }
            };
            sink(Stream::Note, &format!("{}: not yet: {last}", op.machine));
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Ok(Err(last));
            }
            tokio::select! {
                () = cancel.cancelled() => return Err(Cancelled),
                () = tokio::time::sleep(left.min(Duration::from_secs(op.poll_seconds.max(1)))) => {}
            }
        }
    }

    /// For an etcd group, whether the node's etcd pod is `Ready` again: `None` when it is (or the
    /// group does not ask), else why not. What cannot be read is not ready.
    async fn etcd_back(
        &self,
        op: &AwaitRequest,
        nodes: &[auspex_domain::roll::Node],
        node: &str,
        cancel: &CancellationToken,
        sink: &(dyn Fn(Stream, &str) + Sync),
    ) -> Result<Option<String>, Cancelled> {
        if !op.etcd {
            return Ok(None);
        }
        let ask = KubeRequest {
            id: op.id,
            log: op.log,
            context: op.context.clone(),
            op: KubeOp::EtcdPods,
        };
        Ok(match self.kube(&ask, cancel, sink).await? {
            Ok(KubeReply::EtcdPods(pods)) => {
                auspex_domain::roll::etcd_member_ready(nodes, &pods, node).err()
            }
            Ok(other) => Some(format!("kubectl answered {other:?} to a pod list")),
            Err(why) => Some(format!("cannot read etcd: {why}")),
        })
    }

    /// `Effect::Kube` with the real `kubectl`.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired: `kubectl` has been killed and reaped.
    pub async fn kube(
        &self,
        op: &KubeRequest,
        cancel: &CancellationToken,
        sink: &(dyn Fn(Stream, &str) + Sync),
    ) -> Result<KubeAnswer, Cancelled> {
        let kubeconfig = match &self.kubeconfig {
            Ok(p) => p,
            Err(why) => return Ok(Err(format!("no kubeconfig: {why}"))),
        };
        let mut params = KubeParams::new(&self.kubectl, kubeconfig);
        params.context = op.context.as_deref();
        Ok(match &op.op {
            KubeOp::Context => kube::cluster_id(&params, cancel)
                .await?
                .map(KubeReply::Cluster),
            KubeOp::Nodes => kube::nodes(&params, cancel).await?.map(KubeReply::Nodes),
            KubeOp::EtcdPods => kube::etcd_pods(&params, cancel)
                .await?
                .map(KubeReply::EtcdPods),
            KubeOp::Uncordon { node } => {
                sink(Stream::Note, &format!("kubectl uncordon {node}"));
                kube::set_cordon(&params, node, false, cancel)
                    .await?
                    .map(|()| KubeReply::Done)
            }
            KubeOp::Drain {
                node,
                timeout_seconds,
            } => {
                sink(
                    Stream::Note,
                    &format!(
                        "kubectl drain {node} --ignore-daemonsets --delete-emptydir-data \
                         --timeout {timeout_seconds}s"
                    ),
                );
                let (tx, mut rx) = unbounded_channel::<ClanLine>();
                let forward = |l: ClanLine| {
                    let stream = match l.stream {
                        clan::Stream::Stdout => Stream::Stdout,
                        clan::Stream::Stderr => Stream::Stderr,
                    };
                    sink(stream, &l.text);
                };
                let work = kube::drain(&params, node, *timeout_seconds, cancel, tx);
                pump(work, &mut rx, &forward)
                    .await?
                    .map(|()| KubeReply::Done)
            }
        })
    }

    /// `Update`: resolve the fleet's pinned `clan` (a build of `clan-cli` the first time, which
    /// can take minutes and is said in the output), then run it. `Build`: `nix build` here.
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when `cancel` fired: the process group has been killed and reaped. A refusal,
    /// or a program that cannot be started, is the inner `Err`.
    pub async fn run_action(
        &self,
        op: &ActionRequest,
        cancel: &CancellationToken,
        sink: &(dyn Fn(Stream, &str) + Sync),
        starting: &(dyn Fn() + Sync),
    ) -> Result<Result<ClanRun, String>, Cancelled> {
        let flake = match FlakeRef::new(&op.flake) {
            Ok(f) => f,
            Err(e) => return Ok(Err(self.fleet_error(&op.flake, &e.0))),
        };
        let params = RunParams::new();
        let (tx, mut rx) = unbounded_channel::<ClanLine>();
        let forward = |l: ClanLine| {
            let stream = match l.stream {
                clan::Stream::Stdout => Stream::Stdout,
                clan::Stream::Stderr => Stream::Stderr,
            };
            sink(stream, &l.text);
        };
        let ran = match op.kind {
            ActionKind::Update => {
                // Checked here as well as by `act::update`, before the (slow) resolution: a
                // refused name must not cost a build of clan.
                if let Err(r) = act::update_args(&op.machine) {
                    return Ok(Err(format!("refused: {r}")));
                }
                sink(Stream::Note, "resolving the fleet's pinned clan…");
                let resolve = self.resolve_params();
                let binary = match self.clan.resolve(&resolve, &flake, cancel).await? {
                    Ok(b) => b,
                    Err(e) => return Ok(Err(e.to_string())),
                };
                sink(
                    Stream::Note,
                    &format!("clan: {} ({})", binary.path.display(), binary.source),
                );
                sink(
                    Stream::Note,
                    &format!(
                        "$ clan {}",
                        act::update_argv(&flake, &op.machine)
                            .map(|a| shown(&a))
                            .unwrap_or_default()
                    ),
                );
                // From here the machine may be changed: the shell is told before `clan` exists.
                starting();
                let work = act::update(&binary.path, &flake, &op.machine, &params, cancel, tx);
                pump(work, &mut rx, &forward).await?
            }
            ActionKind::Build => {
                let local = clan::current_system();
                let work = act::build(
                    &self.nix,
                    &flake,
                    &op.machine,
                    op.class,
                    op.system.as_deref(),
                    &local,
                    &params,
                    cancel,
                    tx,
                );
                sink(
                    Stream::Note,
                    &format!(
                        "$ nix {}",
                        act::build_argv(&flake, &op.machine, op.class)
                            .map(|a| shown(&a))
                            .unwrap_or_default()
                    ),
                );
                pump(work, &mut rx, &forward).await?
            }
        };
        Ok(run_outcome(ran, sink))
    }
}

/// What a `clan` run came to. An error is **nothing done**: no binary, a refused name, a spawn that
/// failed. The one error after the process exists (it could not be waited for, and was killed)
/// is a run that ended without a code: what it was doing may have happened, so it is a failure, not
/// a refusal (a drained node is not put back in service on it).
fn run_outcome(
    ran: Result<ClanRun, auspex_effects::clan::ClanError>,
    sink: &(dyn Fn(Stream, &str) + Sync),
) -> Result<ClanRun, String> {
    match ran {
        Err(e) if e.started => {
            sink(Stream::Note, &e.to_string());
            Ok(ClanRun {
                end: clan::RunEnd::Exited {
                    code: None,
                    signal: None,
                },
                lines_dropped: 0,
                long_lines: 0,
            })
        }
        other => other.map_err(|e| e.to_string()),
    }
}

/// An argv as one line, for the output's first line. Only shown, never run.
fn shown(argv: &[OsString]) -> String {
    argv.iter()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs `work` (which owns the sending half of `rx`) while handing each line to `forward` as
/// it arrives, then hands over whatever was still queued when `work` finished.
///
/// Lines first: when both are ready the line is taken, so the order the process printed in
/// is the order shown.
async fn pump<T>(
    work: impl std::future::Future<Output = T>,
    rx: &mut UnboundedReceiver<ClanLine>,
    forward: &impl Fn(ClanLine),
) -> T {
    tokio::pin!(work);
    let result = loop {
        tokio::select! {
            biased;
            Some(line) = rx.recv() => forward(line),
            r = &mut work => break r,
        }
    };
    while let Ok(line) = rx.try_recv() {
        forward(line);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `clan` that never started is an error (nothing was done); one that was started and then
    /// lost is a run that ended without a code, said in the output.
    #[test]
    fn only_a_clan_that_never_started_is_an_error() {
        use auspex_effects::clan::{ClanError, ClanStep};
        let said = std::sync::Mutex::new(Vec::<String>::new());
        let sink = |_: Stream, t: &str| said.lock().unwrap().push(t.to_owned());
        let never = ClanError {
            step: ClanStep::Run,
            reason: "not found".to_owned(),
            started: false,
        };
        assert_eq!(
            run_outcome(Err(never), &sink),
            Err("running clan: not found".to_owned())
        );
        let lost = ClanError {
            started: true,
            reason: "waiting for clan failed: x".to_owned(),
            step: ClanStep::Run,
        };
        let run = run_outcome(Err(lost), &sink).expect("it ran");
        assert!(!run.end.success());
        assert_eq!(
            said.lock().unwrap().as_slice(),
            ["running clan: waiting for clan failed: x"]
        );
    }

    /// `u` runs the fleet's pinned `clan`, found from its lock. An environment variable that
    /// named another binary would silently replace it in the release build, so none is read:
    /// the only way to name one is the tests' constructor.
    /// `AUSPEX_NIX` (and the tests' `nix`) is for evaluation and `b`; it must not be the program
    /// that builds the clan `u` deploys with.
    #[test]
    fn the_nix_that_resolves_clan_is_not_the_overridable_one() {
        let real = Real::for_actions(OsString::from("/overridden/nix"), None);
        let params = real.resolve_params();
        assert_eq!(params.nix, std::ffi::OsStr::new(nix::DEFAULT_NIX));
    }

    #[test]
    fn no_environment_variable_names_the_clan_that_deploys() {
        fn walk(dir: &std::path::Path, name: &str, read: &mut usize) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, name, read);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    *read += 1;
                    let text = std::fs::read_to_string(&path).unwrap();
                    assert!(!text.contains(name), "{} mentions {name}", path.display());
                }
            }
        }
        let mut read = 0;
        walk(
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")),
            concat!("AUSPEX", "_CLAN"),
            &mut read,
        );
        assert!(read > 5, "the sources were found");
    }

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
