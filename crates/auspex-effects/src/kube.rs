//! `kubectl`, as a supervised child process: the cluster's node list, and cordon, drain and
//! uncordon of one node.
//!
//! auspex talks to the Kubernetes API with the operator's own `kubectl` and a kubeconfig on this
//! machine, not with a client library of its own (the workspace has none, and a client would be
//! a second implementation of kubeconfig's authentication: exec plugins, OIDC, certificate
//! files) and not with `kubectl` over ssh (the cluster's address is the kubeconfig's business,
//! and a node being updated is the one machine whose ssh may be restarting). Every `kubectl`
//! here goes through the process supervisor like every other child: its own session, no
//! terminal, stdin on `/dev/null`, capped output, and killed and reaped on cancellation.
//!
//! # The argv, exactly
//!
//! - nodes: `kubectl --kubeconfig <path> get nodes -o json`
//! - etcd pods: `kubectl --kubeconfig <path> get pods -n kube-system -l component=etcd -o json`
//! - cordon: `kubectl --kubeconfig <path> cordon <node>`
//! - uncordon: `kubectl --kubeconfig <path> uncordon <node>`
//! - drain: `kubectl --kubeconfig <path> drain <node> --ignore-daemonsets --delete-emptydir-data
//!   --timeout <t>s`
//!
//! `--kubeconfig` is always given, so `$KUBECONFIG` (which may name several files) and kubectl's
//! own default never choose the cluster behind the operator's back: the path is resolved once by
//! the shell and is the one the plan shown to the operator was made against. It must be absolute
//! (a relative one would be read against whatever directory auspex was started from).
//!
//! # Safety, enforced here
//!
//! A node name is the machine's name, and is checked against the domain's plain-name rule
//! ([`auspex_domain::fleet::is_plain_name`]) before anything is spawned: `kubectl drain` takes
//! one node, and a name that read as an option (`--all`) or listed two would act on more. A
//! refused name spawns nothing.
//!
//! **A drain is not a cordon that is undone by its own failure.** `kubectl drain` cordons the
//! node first and leaves it cordoned when it fails or times out; the core calls
//! [`uncordon`] itself, which is why this module never does it behind the caller's back.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::Duration;

use auspex_domain::fleet::is_plain_name;
use auspex_domain::roll::{
    ClusterId, EtcdPod, Node, merge_leases, parse_cluster_id, parse_etcd_pods, parse_leases,
    parse_nodes,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::clan::{ClanLine, RunParams, stream};
use crate::closure::{Failure, limits, text_of};
use crate::process::{OnExit, command, run, spawn, spawn_error};

/// The program, looked up on `PATH`.
pub const DEFAULT_KUBECTL: &str = "kubectl";

/// How long past `kubectl drain`'s own `--timeout` the drain may run before the process group is
/// killed: kubectl enforces its timeout itself, this is for a kubectl that hangs on the network.
pub const DRAIN_GRACE: Duration = Duration::from_secs(60);

/// What every call needs.
#[derive(Debug, Clone, Copy)]
pub struct KubeParams<'a> {
    /// `kubectl`: a bare name, looked up on `PATH`, or an absolute path.
    pub kubectl: &'a OsStr,
    /// The kubeconfig, an absolute path.
    pub kubeconfig: &'a Path,
    /// The kubeconfig context every call is pinned to (`--context=<name>`), resolved once for the
    /// plan: without it each call follows the kubeconfig's *current* context at that moment, which
    /// a `kubectl config use-context` in another terminal can change mid-roll. `None` only for the
    /// call that asks which context that is ([`cluster_id`]).
    pub context: Option<&'a str>,
    /// The budget of a short call (nodes, cordon, uncordon).
    pub timeout: Duration,
    /// [`DRAIN_GRACE`], a field so a test can shorten it.
    pub drain_grace: Duration,
}

impl<'a> KubeParams<'a> {
    /// `kubectl` from `PATH`, 30 seconds per short call, [`DRAIN_GRACE`].
    #[must_use]
    pub const fn new(kubectl: &'a OsStr, kubeconfig: &'a Path) -> Self {
        Self {
            kubectl,
            kubeconfig,
            context: None,
            timeout: Duration::from_secs(30),
            drain_grace: DRAIN_GRACE,
        }
    }

    fn head(&self) -> Result<Vec<OsString>, String> {
        if !self.kubeconfig.is_absolute() {
            return Err(format!(
                "the kubeconfig {} is not an absolute path",
                self.kubeconfig.display()
            ));
        }
        let mut head = vec![
            OsString::from("--kubeconfig"),
            self.kubeconfig.as_os_str().to_owned(),
        ];
        if let Some(context) = self.context {
            // One argument, `--context=<name>`: a name that starts with `-` is still its value.
            head.push(OsString::from(format!("--context={context}")));
        }
        Ok(head)
    }
}

fn node_arg(node: &str) -> Result<OsString, String> {
    if is_plain_name(node) {
        Ok(OsString::from(node))
    } else {
        Err(format!(
            "{node:?} is not a plain machine name, so it is not a node auspex will name to kubectl"
        ))
    }
}

/// The argv (after the program) of the node list.
///
/// # Errors
///
/// The kubeconfig is not absolute.
pub fn nodes_argv(params: &KubeParams<'_>) -> Result<Vec<OsString>, String> {
    let mut argv = params.head()?;
    argv.extend(["get", "nodes", "-o", "json"].map(OsString::from));
    Ok(argv)
}

/// The argv (after the program) of the node leases: each kubelet renews its lease every ten
/// seconds, which is the sign of life [`auspex_domain::roll::node_state`] waits for.
///
/// # Errors
///
/// The kubeconfig is not absolute.
pub fn leases_argv(params: &KubeParams<'_>) -> Result<Vec<OsString>, String> {
    let mut argv = params.head()?;
    argv.extend(["get", "leases", "-n", "kube-node-lease", "-o", "json"].map(OsString::from));
    Ok(argv)
}

/// The argv (after the program) of the etcd pods: the static pods of etcd, one per member, in
/// `kube-system` (RKE2 and kubeadm label them `component=etcd`).
///
/// # Errors
///
/// The kubeconfig is not absolute.
pub fn etcd_pods_argv(params: &KubeParams<'_>) -> Result<Vec<OsString>, String> {
    let mut argv = params.head()?;
    argv.extend(
        [
            "get",
            "pods",
            "-n",
            "kube-system",
            "-l",
            "component=etcd",
            "-o",
            "json",
        ]
        .map(OsString::from),
    );
    Ok(argv)
}

/// The argv (after the program) of the question "which cluster is this": the kubeconfig's
/// current context and its API server.
///
/// # Errors
///
/// The kubeconfig is not absolute.
pub fn cluster_id_argv(params: &KubeParams<'_>) -> Result<Vec<OsString>, String> {
    let mut argv = params.head()?;
    argv.extend(["config", "view", "--minify", "-o", "json"].map(OsString::from));
    Ok(argv)
}

/// The argv (after the program) of `cordon` or `uncordon` of `node`.
///
/// # Errors
///
/// The kubeconfig is not absolute, or `node` is not a plain name.
pub fn cordon_argv(
    params: &KubeParams<'_>,
    node: &str,
    cordon: bool,
) -> Result<Vec<OsString>, String> {
    let mut argv = params.head()?;
    argv.push(OsString::from(if cordon { "cordon" } else { "uncordon" }));
    argv.push(node_arg(node)?);
    Ok(argv)
}

/// The argv (after the program) of the drain of `node`, which kubectl itself gives up after
/// `timeout_seconds`.
///
/// # Errors
///
/// The kubeconfig is not absolute, or `node` is not a plain name.
pub fn drain_argv(
    params: &KubeParams<'_>,
    node: &str,
    timeout_seconds: u64,
) -> Result<Vec<OsString>, String> {
    let mut argv = params.head()?;
    argv.push(OsString::from("drain"));
    argv.push(node_arg(node)?);
    argv.extend(["--ignore-daemonsets", "--delete-emptydir-data", "--timeout"].map(OsString::from));
    argv.push(OsString::from(format!("{timeout_seconds}s")));
    Ok(argv)
}

/// Runs a short `kubectl` and returns its stdout as text, or one line on why not.
async fn text(
    params: &KubeParams<'_>,
    argv: Vec<OsString>,
    what: &str,
    cancel: &CancellationToken,
) -> Result<Result<String, String>, Cancelled> {
    let mut cmd = match command(params.kubectl) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Err(spawn_error(
                &format!("cannot run {what}"),
                params.kubectl,
                &e,
            )));
        }
    };
    cmd.args(argv);
    let deadline = Instant::now() + params.timeout;
    let ran = match run(cmd, None, OnExit::KillGroup, limits(), deadline, cancel).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(Err(spawn_error(
                &format!("cannot run {what}"),
                params.kubectl,
                &e,
            )));
        }
    };
    match text_of(&ran, what, params.timeout) {
        Err(Failure::Cancelled) => Err(Cancelled),
        Err(Failure::Why(why)) => Ok(Err(why)),
        Ok(out) => Ok(Ok(out)),
    }
}

/// The cluster's nodes, each with its newest heartbeat (the `Ready` condition's, or its lease's
/// when that is later).
///
/// The leases are a second call, and a failure of it is not a failure of this one: the nodes
/// are then without lease heartbeats, and a caller that waits for one says it is stale.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired: the process group has been killed and reaped. A kubectl
/// that failed, timed out or printed something that is not a node list is the inner line.
pub async fn nodes(
    params: &KubeParams<'_>,
    cancel: &CancellationToken,
) -> Result<Result<Vec<Node>, String>, Cancelled> {
    let argv = match nodes_argv(params) {
        Ok(a) => a,
        Err(why) => return Ok(Err(why)),
    };
    let mut nodes = match text(params, argv, "kubectl get nodes", cancel).await? {
        Ok(out) => match parse_nodes(&out) {
            Ok(n) => n,
            Err(why) => return Ok(Err(why)),
        },
        Err(why) => return Ok(Err(why)),
    };
    if let Ok(argv) = leases_argv(params)
        && let Ok(out) = text(params, argv, "kubectl get leases", cancel).await?
        && let Ok(leases) = parse_leases(&out)
    {
        merge_leases(&mut nodes, &leases);
    }
    Ok(Ok(nodes))
}

/// The cluster's etcd pods, one per member: what [`auspex_domain::roll::etcd_unhealthy`] judges.
///
/// # Errors
///
/// [`Cancelled`] as [`nodes`]; a kubectl that failed, or printed something that is not a pod list,
/// is the inner line.
pub async fn etcd_pods(
    params: &KubeParams<'_>,
    cancel: &CancellationToken,
) -> Result<Result<Vec<EtcdPod>, String>, Cancelled> {
    let argv = match etcd_pods_argv(params) {
        Ok(a) => a,
        Err(why) => return Ok(Err(why)),
    };
    Ok(text(params, argv, "kubectl get pods", cancel)
        .await?
        .and_then(|out| parse_etcd_pods(&out)))
}

/// Which cluster `params`' kubeconfig is pointed at: its current context and API server.
///
/// # Errors
///
/// [`Cancelled`] as [`nodes`]; a kubectl that failed, or a kubeconfig with no current context, is
/// the inner line.
pub async fn cluster_id(
    params: &KubeParams<'_>,
    cancel: &CancellationToken,
) -> Result<Result<ClusterId, String>, Cancelled> {
    let kubeconfig = match kubeconfig_text(params.kubeconfig) {
        Ok(k) => k,
        Err(why) => return Ok(Err(why)),
    };
    let argv = match cluster_id_argv(params) {
        Ok(a) => a,
        Err(why) => return Ok(Err(why)),
    };
    Ok(text(params, argv, "kubectl config view", cancel)
        .await?
        .and_then(|out| parse_cluster_id(&out, kubeconfig)))
}

/// The kubeconfig path as text, for the command the operator is told to run by hand: a path that
/// is not valid UTF-8 would be printed as a different one, so it is refused instead.
fn kubeconfig_text(path: &Path) -> Result<&str, String> {
    path.to_str().ok_or_else(|| {
        format!(
            "the kubeconfig path {} is not valid UTF-8, so a command to run by hand cannot name \
             it: use another path (`kubernetes.kubeconfig`)",
            path.display()
        )
    })
}

/// Cordons (`cordon`) or uncordons `node`.
///
/// # Errors
///
/// [`Cancelled`] as [`nodes`]; kubectl's failure is the inner line.
pub async fn set_cordon(
    params: &KubeParams<'_>,
    node: &str,
    cordon: bool,
    cancel: &CancellationToken,
) -> Result<Result<(), String>, Cancelled> {
    let argv = match cordon_argv(params, node, cordon) {
        Ok(a) => a,
        Err(why) => return Ok(Err(why)),
    };
    let what = if cordon {
        "kubectl cordon"
    } else {
        "kubectl uncordon"
    };
    Ok(text(params, argv, what, cancel).await?.map(|_| ()))
}

/// Drains `node`, streaming kubectl's output to `tx`.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired (or `tx`'s receiver was dropped): the process group has been
/// killed and reaped. A drain that failed or timed out is the inner line, and the node is then
/// **still cordoned** (see the module comment).
pub async fn drain(
    params: &KubeParams<'_>,
    node: &str,
    timeout_seconds: u64,
    cancel: &CancellationToken,
    tx: UnboundedSender<ClanLine>,
) -> Result<Result<(), String>, Cancelled> {
    let argv = match drain_argv(params, node, timeout_seconds) {
        Ok(a) => a,
        Err(why) => return Ok(Err(why)),
    };
    let cannot = |e: &std::io::Error| spawn_error("cannot run kubectl drain", params.kubectl, e);
    let mut cmd = match command(params.kubectl) {
        Ok(c) => c,
        Err(e) => return Ok(Err(cannot(&e))),
    };
    cmd.args(argv);
    let sp = match spawn(cmd, false, OnExit::KillGroup, None) {
        Ok(s) => s,
        Err(e) => return Ok(Err(cannot(&e))),
    };
    let run_params = RunParams {
        timeout: Duration::from_secs(timeout_seconds).saturating_add(params.drain_grace),
        ..RunParams::new()
    };
    match stream(sp, &run_params, cancel, tx).await? {
        Err(e) => Ok(Err(e.to_string())),
        Ok(ran) if ran.end.success() => Ok(Ok(())),
        Ok(ran) => Ok(Err(match ran.end {
            crate::clan::RunEnd::TimedOut => {
                format!("kubectl drain did not finish within {timeout_seconds}s and was stopped")
            }
            crate::clan::RunEnd::Exited { code, signal } => format!(
                "kubectl drain {}",
                match (code, signal) {
                    (Some(c), _) => format!("exited {c}"),
                    (None, Some(s)) => format!("was killed by signal {s}"),
                    (None, None) => "ended".to_owned(),
                }
            ),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(path: &'static str) -> KubeParams<'static> {
        KubeParams::new(OsStr::new("kubectl"), Path::new(path))
    }

    #[test]
    fn a_kubeconfig_that_is_not_utf8_is_refused_not_printed_as_another_path() {
        use std::os::unix::ffi::OsStrExt as _;
        assert_eq!(kubeconfig_text(Path::new("/k/c")), Ok("/k/c"));
        let bad = Path::new(OsStr::from_bytes(b"/k/\xff/config"));
        let why = kubeconfig_text(bad).unwrap_err();
        assert!(why.contains("not valid UTF-8"), "{why}");
    }

    fn strings(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_argvs_are_exactly_these() {
        let p = params("/home/me/.kube/config");
        assert_eq!(
            strings(&nodes_argv(&p).unwrap()),
            [
                "--kubeconfig",
                "/home/me/.kube/config",
                "get",
                "nodes",
                "-o",
                "json"
            ]
        );
        assert_eq!(
            strings(&cordon_argv(&p, "luna", true).unwrap()),
            ["--kubeconfig", "/home/me/.kube/config", "cordon", "luna"]
        );
        assert_eq!(
            strings(&cordon_argv(&p, "luna", false).unwrap()),
            ["--kubeconfig", "/home/me/.kube/config", "uncordon", "luna"]
        );
        assert_eq!(
            strings(&drain_argv(&p, "luna", 600).unwrap()),
            [
                "--kubeconfig",
                "/home/me/.kube/config",
                "drain",
                "luna",
                "--ignore-daemonsets",
                "--delete-emptydir-data",
                "--timeout",
                "600s"
            ]
        );
    }

    #[test]
    fn every_call_after_the_plan_is_pinned_to_the_context_and_the_plan_asks_which_it_is() {
        let mut p = params("/k/config");
        assert_eq!(
            strings(&cluster_id_argv(&p).unwrap()),
            [
                "--kubeconfig",
                "/k/config",
                "config",
                "view",
                "--minify",
                "-o",
                "json"
            ]
        );
        assert_eq!(
            strings(&leases_argv(&p).unwrap()),
            [
                "--kubeconfig",
                "/k/config",
                "get",
                "leases",
                "-n",
                "kube-node-lease",
                "-o",
                "json"
            ]
        );
        assert_eq!(
            strings(&etcd_pods_argv(&p).unwrap()),
            [
                "--kubeconfig",
                "/k/config",
                "get",
                "pods",
                "-n",
                "kube-system",
                "-l",
                "component=etcd",
                "-o",
                "json"
            ]
        );
        p.context = Some("rke2");
        let pinned = "--context=rke2";
        assert_eq!(
            strings(&nodes_argv(&p).unwrap()),
            [
                "--kubeconfig",
                "/k/config",
                pinned,
                "get",
                "nodes",
                "-o",
                "json"
            ]
        );
        assert_eq!(
            strings(&cordon_argv(&p, "luna", false).unwrap()),
            ["--kubeconfig", "/k/config", pinned, "uncordon", "luna"]
        );
        assert_eq!(
            strings(&drain_argv(&p, "luna", 5).unwrap())[..3],
            ["--kubeconfig", "/k/config", pinned]
        );
        // A context that looks like an option is still one argument, the flag's value.
        p.context = Some("--all");
        assert_eq!(strings(&nodes_argv(&p).unwrap())[2], "--context=--all");
        assert_eq!(strings(&etcd_pods_argv(&p).unwrap())[2], "--context=--all");
    }

    #[test]
    fn a_node_that_is_not_a_plain_name_is_refused() {
        let p = params("/k");
        for bad in ["", "--all", "-l", "a b", "luna sol", "a/b", "luna\n"] {
            assert!(cordon_argv(&p, bad, true).is_err(), "{bad:?}");
            assert!(drain_argv(&p, bad, 1).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_relative_kubeconfig_is_refused() {
        assert!(nodes_argv(&params("kube/config")).is_err());
        assert!(drain_argv(&params("config"), "luna", 1).is_err());
    }
}
