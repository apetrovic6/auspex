//! Updating many machines at once (`U`): who may update together, what a Kubernetes cluster has
//! to look like before and between machines, and what "healthy again" means.
//!
//! Everything here is a function of values. Running anything, waiting, and deciding *when* is the
//! core's and the shell's (`app/roll.rs`).
//!
//! # Rolling groups
//!
//! The operator names **rolling tags** in the configuration (`[rolling.<tag>]`); no tag name is
//! known to this code. Machines that share any rolling tag are one *group* and never update at
//! the same time: the relation is transitive, so a machine tagged `a` and `b` joins everything
//! tagged `a` with everything tagged `b`. (The fleet this was written for has three etcd members
//! in one cluster carrying overlapping tags: two of them updating at once could cost the quorum,
//! and "different tags, so in parallel" would have allowed exactly that.) Machines with no rolling
//! tag are *free*: the core updates a few of them at a time.
//!
//! # Kubernetes
//!
//! Node state is read from `kubectl get nodes -o json` ([`parse_nodes`]). A node is named like
//! its machine; a machine of a Kubernetes group that is not a node of the cluster is refused up
//! front, because waiting for it to become `Ready` could never end ([`cluster_refusals`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::observe::{ProbeOutcome, StorePath};
use crate::text::sanitize_line;

/// What `[rolling.<tag>]` says about the machines carrying `<tag>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RollRule {
    /// Check the cluster's node state before the roll starts and wait for the updated machine's
    /// node to be `Ready` before the next machine.
    pub kubernetes: bool,
    /// Cordon and drain the node before its machine is updated, uncordon it after. Off by
    /// default: a NixOS switch does not reboot the node, and RKE2's containers survive a restart
    /// of the service.
    pub drain: bool,
    /// How long `kubectl drain` may take, in seconds.
    pub drain_timeout_seconds: u64,
    /// The cluster's etcd is checked before every machine of the group goes out of service, and
    /// after its update the machine's etcd pod must be `Ready` again ([`etcd_unhealthy`]). Implies
    /// `kubernetes`. Off by default: a cluster with no etcd pods in `kube-system` (k3s with
    /// sqlite, a managed control plane, external etcd) could never pass it.
    pub etcd: bool,
}

/// The drain timeout when the configuration names none: ten minutes.
pub const DEFAULT_DRAIN_TIMEOUT_SECONDS: u64 = 600;

impl Default for RollRule {
    fn default() -> Self {
        Self {
            kubernetes: false,
            drain: false,
            drain_timeout_seconds: DEFAULT_DRAIN_TIMEOUT_SECONDS,
            etcd: false,
        }
    }
}

/// The rules by tag name.
pub type RollRules = BTreeMap<String, RollRule>;

/// A machine that is to be updated, as the planner sees it.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// The machine's name.
    pub name: &'a str,
    /// The inventory's tags.
    pub tags: &'a [String],
}

/// One machine of a rolling group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The machine, which is also its Kubernetes node's name.
    pub name: String,
    /// One of its rolling tags says `kubernetes = true`.
    pub kubernetes: bool,
    /// One of its rolling tags says `drain = true`: the drain's timeout in seconds (the
    /// smallest of those tags', so the strictest rule wins), else `None`.
    pub drain: Option<u64>,
    /// One of its rolling tags says `etcd = true`.
    pub etcd: bool,
}

impl Member {
    /// The machine's node is asked about and put out of service by auspex: it waits for the node
    /// (`kubernetes`) or drains it.
    #[must_use]
    pub const fn uses_cluster(&self) -> bool {
        self.kubernetes || self.drain.is_some()
    }
}

/// Machines that never update at the same time, in the order they update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The rolling tags that bind the group, sorted.
    pub tags: Vec<String>,
    /// Sorted by name.
    pub members: Vec<Member>,
}

impl Group {
    /// A machine of the group has to be checked against the cluster.
    #[must_use]
    pub fn kubernetes(&self) -> bool {
        self.members.iter().any(|m| m.kubernetes)
    }

    /// A machine of the group asks for the etcd check.
    #[must_use]
    pub fn etcd(&self) -> bool {
        self.members.iter().any(|m| m.etcd)
    }

    /// A machine of the group is waited for or drained in the cluster, so the group is one of the
    /// cluster's: at most one machine of all such groups is out of service at a time, because they
    /// share one kubeconfig.
    #[must_use]
    pub fn uses_cluster(&self) -> bool {
        self.members.iter().any(Member::uses_cluster)
    }

    /// The group as the operator reads it: `taghmata+fabricator`.
    #[must_use]
    pub fn label(&self) -> String {
        self.tags.join("+")
    }
}

/// Who updates with whom.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Rolling groups, ordered by their first machine's name.
    pub groups: Vec<Group>,
    /// Machines in no group, sorted: updated a few at a time.
    pub free: Vec<String>,
}

/// The rolling tags of `c`: those `rules` names.
fn rolling_tags<'a>(c: &Candidate<'a>, rules: &RollRules) -> BTreeSet<&'a str> {
    c.tags
        .iter()
        .map(String::as_str)
        .filter(|t| rules.contains_key(*t))
        .collect()
}

/// The root of `i`'s set, halving the path on the way.
const fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// Splits `candidates` into rolling groups and free machines.
///
/// A candidate is in a group when it carries a tag `rules` names; two candidates are in the same
/// group when a chain of shared rolling tags joins them. Order is by name throughout, so the same
/// fleet always rolls the same way.
#[must_use]
pub fn plan<'a>(candidates: &[Candidate<'a>], rules: &RollRules) -> Plan {
    let mut sorted: Vec<Candidate<'a>> = candidates.to_vec();
    sorted.sort_by(|a, b| a.name.cmp(b.name));
    sorted.dedup_by(|a, b| a.name == b.name);
    let rolling = |c: &Candidate<'a>| rolling_tags(c, rules);

    // Union-find over the candidates, joined through each rolling tag.
    let mut parent: Vec<usize> = (0..sorted.len()).collect();
    let mut first_with_tag: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, c) in sorted.iter().enumerate() {
        for tag in rolling(c) {
            match first_with_tag.get(tag) {
                Some(&j) => {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    parent[a.max(b)] = a.min(b);
                }
                None => {
                    first_with_tag.insert(tag, i);
                }
            }
        }
    }

    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut free = Vec::new();
    for (i, c) in sorted.iter().enumerate() {
        if rolling(c).is_empty() {
            free.push(c.name.to_owned());
        } else {
            let root = find(&mut parent, i);
            groups.entry(root).or_default().push(i);
        }
    }
    let groups = groups
        .into_values()
        .map(|indexes| {
            let mut tags = BTreeSet::new();
            let members = indexes
                .iter()
                .map(|&i| {
                    let c = &sorted[i];
                    let own = rolling(c);
                    tags.extend(own.iter().map(|t| (*t).to_owned()));
                    let applying = || own.iter().filter_map(|t| rules.get(*t));
                    Member {
                        name: c.name.to_owned(),
                        kubernetes: applying().any(|r| r.kubernetes || r.etcd),
                        etcd: applying().any(|r| r.etcd),
                        drain: applying()
                            .filter(|r| r.drain)
                            .map(|r| r.drain_timeout_seconds)
                            .min(),
                    }
                })
                .collect();
            Group {
                tags: tags.into_iter().collect(),
                members,
            }
        })
        .collect();
    Plan { groups, free }
}

/// One node of the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// `metadata.name`, sanitised.
    pub name: String,
    /// The `Ready` condition is `True` (not `False`, not `Unknown`, not absent).
    pub ready: bool,
    /// `spec.unschedulable`: cordoned.
    pub cordoned: bool,
    /// The newest sign of life the cluster has from the node's kubelet, in seconds since the
    /// epoch: the `Ready` condition's `lastHeartbeatTime`, or its lease's `renewTime` when that is
    /// later ([`merge_leases`]). `None` when the cluster said neither. The two kinds are kept apart
    /// too ([`Node::condition`], [`Node::lease`]): they are rewritten at very different rates, so
    /// only like can be compared with like ([`Beats`]).
    pub heartbeat: Option<i64>,
    /// The `Ready` condition's `lastHeartbeatTime` alone, in seconds since the epoch. A kubelet
    /// rewrites it when the status changes or every few minutes.
    pub condition: Option<i64>,
    /// The node's lease `renewTime`, in seconds since the epoch, when its lease was read and has one
    /// ([`merge_leases`]): the kubelet's own renewal, not just the condition's heartbeat (which is
    /// rewritten only every few minutes). Without any, how long a kubelet has been silent cannot be
    /// told ([`silent_kubelets`]).
    pub lease: Option<i64>,
    /// It carries the label `node-role.kubernetes.io/etcd` (RKE2's mark of an etcd member), with
    /// any value but `false`.
    pub etcd: bool,
}

/// The label RKE2 puts on a node that runs etcd.
pub const ETCD_LABEL: &str = "node-role.kubernetes.io/etcd";

#[derive(Deserialize)]
struct RawList {
    items: Option<Vec<RawNode>>,
}

#[derive(Deserialize)]
struct RawNode {
    metadata: Option<RawMeta>,
    spec: Option<RawSpec>,
    status: Option<RawStatus>,
}

#[derive(Deserialize)]
struct RawMeta {
    name: Option<String>,
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct RawSpec {
    unschedulable: Option<bool>,
}

#[derive(Deserialize)]
struct RawStatus {
    conditions: Option<Vec<RawCondition>>,
}

#[derive(Deserialize)]
struct RawCondition {
    #[serde(rename = "type")]
    kind: Option<String>,
    status: Option<String>,
    #[serde(rename = "lastHeartbeatTime")]
    heartbeat: Option<String>,
}

/// Seconds since the epoch of an RFC 3339 UTC time as Kubernetes writes it
/// (`2026-11-20T15:04:05Z`, or with a fraction as a lease's `renewTime` has).
///
/// Only the `Z` form is read: a time with an offset is `None`, never a guess, and so is anything
/// that is not a plausible date.
#[must_use]
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut ymd = date.split('-');
    let (year, month, day): (i64, i64, i64) = (
        ymd.next()?.parse().ok()?,
        ymd.next()?.parse().ok()?,
        ymd.next()?.parse().ok()?,
    );
    if ymd.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let time = time.split('.').next()?;
    let mut clock = time.split(':');
    let (hour, minute, second): (i64, i64, i64) = (
        clock.next()?.parse().ok()?,
        clock.next()?.parse().ok()?,
        clock.next()?.parse().ok()?,
    );
    if clock.next().is_some()
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..61).contains(&second)
    {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Parses `kubectl get nodes -o json`.
///
/// A node with no `Ready` condition is not ready: absence of a report is not health. `Unknown`
/// (what a node gets when its kubelet stops reporting) is not ready either.
///
/// # Errors
///
/// Not the JSON of a node list, or a node without a name.
pub fn parse_nodes(json: &str) -> Result<Vec<Node>, String> {
    let list: RawList = serde_json::from_str(json)
        .map_err(|e| format!("kubectl's node list is not the expected JSON: {e}"))?;
    let items = list
        .items
        .ok_or_else(|| "kubectl's node list has no `items`".to_owned())?;
    items
        .into_iter()
        .map(|n| {
            let (name, labels) = n.metadata.map_or((None, None), |m| (m.name, m.labels));
            let etcd = labels
                .and_then(|l| l.get(ETCD_LABEL).cloned())
                .is_some_and(|v| v != "false");
            let name = name
                .map(|n| sanitize_line(&n))
                .filter(|n| !n.is_empty())
                .ok_or_else(|| "kubectl's node list has a node with no name".to_owned())?;
            let conditions = n.status.and_then(|s| s.conditions).unwrap_or_default();
            let ready_condition = conditions
                .iter()
                .find(|c| c.kind.as_deref() == Some("Ready"));
            let ready = ready_condition.is_some_and(|c| c.status.as_deref() == Some("True"));
            let heartbeat = ready_condition
                .and_then(|c| c.heartbeat.as_deref())
                .and_then(parse_rfc3339);
            let cordoned = n.spec.and_then(|s| s.unschedulable).unwrap_or(false);
            Ok(Node {
                name,
                ready,
                cordoned,
                heartbeat,
                condition: heartbeat,
                lease: None,
                etcd,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct RawLeases {
    items: Option<Vec<RawLease>>,
}

#[derive(Deserialize)]
struct RawLease {
    metadata: Option<RawMeta>,
    spec: Option<RawLeaseSpec>,
}

#[derive(Deserialize)]
struct RawLeaseSpec {
    #[serde(rename = "renewTime")]
    renew_time: Option<String>,
}

/// Parses `kubectl get leases -n kube-node-lease -o json` into each node's last `renewTime`.
///
/// A node's kubelet renews its lease every ten seconds, while the `Ready` condition's own
/// heartbeat is only rewritten when the status changes or every few minutes: the lease is the
/// real sign of life, so it decides when it is there.
///
/// # Errors
///
/// Not the JSON of a lease list.
pub fn parse_leases(json: &str) -> Result<BTreeMap<String, i64>, String> {
    let list: RawLeases = serde_json::from_str(json)
        .map_err(|e| format!("kubectl's lease list is not the expected JSON: {e}"))?;
    Ok(list
        .items
        .unwrap_or_default()
        .into_iter()
        .filter_map(|l| {
            let name = sanitize_line(&l.metadata?.name?);
            let at = parse_rfc3339(&l.spec?.renew_time?)?;
            Some((name, at))
        })
        .collect())
}

/// Gives each node of `nodes` its lease's `renewTime` as its heartbeat when that is later than
/// the condition's.
pub fn merge_leases(nodes: &mut [Node], leases: &BTreeMap<String, i64>) {
    for n in nodes {
        if let Some(&renewed) = leases.get(&n.name) {
            n.heartbeat = Some(n.heartbeat.map_or(renewed, |h| h.max(renewed)));
            n.lease = Some(renewed);
        }
    }
}

/// The cluster `kubectl` is pointed at: what the plan names, and every later call is pinned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterId {
    /// The kubeconfig's current context, sanitised.
    pub context: String,
    /// That context's cluster's API server, sanitised, or `"unknown"`.
    pub server: String,
    /// The kubeconfig file the shell asked (absolute): the one a by-hand `kubectl` must be given.
    pub kubeconfig: String,
}

/// Parses `kubectl config view --minify -o json`.
///
/// # Errors
///
/// Not the JSON of a kubeconfig, or it has no current context.
pub fn parse_cluster_id(json: &str, kubeconfig: &str) -> Result<ClusterId, String> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("kubectl's config view is not the expected JSON: {e}"))?;
    let context = v
        .get("current-context")
        .and_then(serde_json::Value::as_str)
        .map(sanitize_line)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| "the kubeconfig has no current context".to_owned())?;
    let cluster_name = v
        .get("contexts")
        .and_then(serde_json::Value::as_array)
        .and_then(|cs| {
            cs.iter()
                .find(|c| c.get("name").and_then(serde_json::Value::as_str) == Some(&context))
        })
        .and_then(|c| c.get("context")?.get("cluster")?.as_str());
    let server = cluster_name
        .and_then(|name| {
            v.get("clusters")?.as_array()?.iter().find_map(|c| {
                (c.get("name")?.as_str()? == name)
                    .then(|| c.get("cluster")?.get("server")?.as_str().map(sanitize_line))
                    .flatten()
            })
        })
        .unwrap_or_else(|| "unknown".to_owned());
    Ok(ClusterId {
        context,
        server,
        kubeconfig: kubeconfig.to_owned(),
    })
}

/// How long a kubelet may be silent, in seconds, before its node is not healthy.
///
/// The node still says `Ready` meanwhile: the node controller turns a silent node's `Ready` into
/// `Unknown` only after about 40 s, so a bound of 40 s would add nothing to what `Ready` already
/// says. A kubelet renews its lease every 10 s, so 25 s is two missed renewals plus a margin for
/// the read's own timing (the leases are a second call, made just after the nodes). A constant, not
/// a setting.
pub const KUBELET_SILENCE_SECONDS: i64 = 25;

/// Whether the leases behind `nodes` were read: at least one node has a lease heartbeat.
///
/// When none has, the read failed (or the cluster keeps no node leases), and
/// [`silent_kubelets`] cannot judge anything.
#[must_use]
pub fn leases_read(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| n.lease.is_some())
}

/// The `Ready` nodes whose kubelet has been silent for more than [`KUBELET_SILENCE_SECONDS`], one
/// sentence each.
///
/// Age is by the node's **lease** (the condition's own heartbeat is rewritten only every few minutes
/// and says nothing about a kubelet that is alive), measured against the newest lease of any node
/// **in the same read**: the kubelets' clocks against each other, never against this machine's, so a
/// skew between the operator's machine and the cluster does not matter. The sentence names that
/// node, because the age is only as good as its clock: a node whose clock runs ahead makes every
/// other node look silent, and when more than one node is flagged a further sentence says so. What
/// the rule cannot see: a node whose own clock runs ahead can be silent for that much longer
/// before it is flagged, and a cluster whose every kubelet went quiet together looks healthy
/// (nodes with clocks that agree within a few seconds are assumed).
///
/// With no lease read at all (see [`leases_read`]) nothing is judged. A `Ready` node with no lease
/// while others have one is not vouched for.
#[must_use]
pub fn silent_kubelets(nodes: &[Node]) -> Vec<String> {
    if !leases_read(nodes) {
        return Vec::new();
    }
    // The first of the nodes with the newest lease: the reference the sentences name.
    let mut newest: Option<(&str, i64)> = None;
    for n in nodes {
        if let Some(l) = n.lease
            && newest.is_none_or(|(_, best)| l > best)
        {
            newest = Some((n.name.as_str(), l));
        }
    }
    let Some((reference, newest)) = newest else {
        return Vec::new();
    };
    let mut why = Vec::new();
    let mut behind = 0;
    for n in nodes.iter().filter(|n| n.ready) {
        match n.lease {
            Some(l) if newest - l > KUBELET_SILENCE_SECONDS => {
                behind += 1;
                why.push(format!(
                    "node {} is Ready, but its kubelet has been silent for {} s by {reference}'s \
                     clock (the newest lease in this read)",
                    n.name,
                    newest - l
                ));
            }
            Some(_) => {}
            None => why.push(format!(
                "node {} is Ready, but it has no lease, so its kubelet cannot be vouched for",
                n.name
            )),
        }
    }
    if behind > 1 {
        why.push(format!(
            "{behind} nodes are behind {reference}'s lease: if {reference}'s clock runs ahead of \
             theirs they are not silent (the nodes' clocks must agree within a few seconds)"
        ));
    }
    why
}

/// Why the cluster is not fit for the next node to go out of service, one sentence each; empty
/// when every node is `Ready`, schedulable and has a kubelet that has been heard from lately
/// ([`silent_kubelets`]).
#[must_use]
pub fn cluster_unhealthy(nodes: &[Node]) -> Vec<String> {
    let mut why = Vec::new();
    for node in nodes {
        if !node.ready {
            why.push(format!("node {} is NotReady", node.name));
        }
        if node.cordoned {
            why.push(format!("node {} is cordoned", node.name));
        }
    }
    why.extend(silent_kubelets(nodes));
    why
}

/// Why a roll of `members` may not start in this cluster, one sentence each; empty when it may.
///
/// Any node of the cluster that is `NotReady` or cordoned refuses it (not only the members:
/// a roll takes one more node out of service for a while, and a cluster already short of one is
/// not the moment), and so does a member that is no node at all.
#[must_use]
pub fn cluster_refusals(nodes: &[Node], members: &[&Member]) -> Vec<String> {
    let mut why = cluster_unhealthy(nodes);
    for m in members.iter().filter(|m| m.uses_cluster()) {
        if !nodes.iter().any(|n| n.name == m.name) {
            why.push(format!(
                "{} is not a node of the cluster (a node is named like its machine)",
                m.name
            ));
        }
    }
    why
}

/// One etcd static pod (`kube-system`, `component=etcd`), as the etcd check sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtcdPod {
    /// `spec.nodeName`: the node, which is the member.
    pub node: String,
    /// The pod is `Running` and its `Ready` condition is `True` (the kubelet's view of etcd's
    /// readiness probe).
    pub ready: bool,
}

#[derive(Deserialize)]
struct RawPodList {
    items: Option<Vec<RawPod>>,
}

#[derive(Deserialize)]
struct RawPod {
    spec: Option<RawPodSpec>,
    status: Option<RawPodStatus>,
}

#[derive(Deserialize)]
struct RawPodSpec {
    #[serde(rename = "nodeName")]
    node_name: Option<String>,
}

#[derive(Deserialize)]
struct RawPodStatus {
    phase: Option<String>,
    conditions: Option<Vec<RawCondition>>,
}

/// Parses `kubectl get pods -n kube-system -l component=etcd -o json`. A pod not yet scheduled
/// (no `nodeName`) is no member's and is left out; a pod with no `Ready` condition is not ready.
///
/// # Errors
///
/// Not the JSON of a pod list.
pub fn parse_etcd_pods(json: &str) -> Result<Vec<EtcdPod>, String> {
    let list: RawPodList = serde_json::from_str(json)
        .map_err(|e| format!("kubectl's pod list is not the expected JSON: {e}"))?;
    let items = list
        .items
        .ok_or_else(|| "kubectl's pod list has no `items`".to_owned())?;
    Ok(items
        .into_iter()
        .filter_map(|p| {
            let node = p
                .spec
                .and_then(|s| s.node_name)
                .map(|n| sanitize_line(&n))
                .filter(|n| !n.is_empty())?;
            let status = p.status;
            let running = status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running");
            let ready = status
                .and_then(|s| s.conditions)
                .unwrap_or_default()
                .iter()
                .any(|c| c.kind.as_deref() == Some("Ready") && c.status.as_deref() == Some("True"));
            Some(EtcdPod {
                node,
                ready: running && ready,
            })
        })
        .collect())
}

/// Why etcd is not fit for the next member to go out of service, one sentence each; empty when
/// every etcd member has a ready etcd pod.
///
/// Members are the nodes labelled [`ETCD_LABEL`]. Every one of them must have a ready pod (the
/// rule is stricter than the quorum on purpose: no counting, no assumption about the size or
/// about learners), and what cannot be told is not healthy: no labelled node at all, or a pod on a
/// node that is not labelled, means which nodes are members is not known.
#[must_use]
pub fn etcd_unhealthy(nodes: &[Node], pods: &[EtcdPod]) -> Vec<String> {
    let members: Vec<&Node> = nodes.iter().filter(|n| n.etcd).collect();
    if members.is_empty() {
        return vec![format!(
            "cannot tell which nodes are etcd members (no node has the label {ETCD_LABEL})"
        )];
    }
    let mut why = Vec::new();
    for m in &members {
        match pods.iter().find(|p| p.node == m.name) {
            None => why.push(format!("etcd member {} has no etcd pod", m.name)),
            Some(p) if !p.ready => why.push(format!("etcd on {} is not ready", m.name)),
            Some(_) => {}
        }
    }
    for p in pods {
        if !members.iter().any(|m| m.name == p.node) {
            why.push(format!(
                "there is an etcd pod on {} but the node is not labelled {ETCD_LABEL}, so which \
                 nodes are etcd members is not certain",
                p.node
            ));
        }
    }
    why
}

/// Whether the etcd of `node` is back after its update: `Ok` when it is no member (no label and no
/// pod: nothing to wait for), or its pod is ready; otherwise why not.
///
/// # Errors
///
/// The node is a member, or has an etcd pod, and the pod is missing or not ready.
pub fn etcd_member_ready(nodes: &[Node], pods: &[EtcdPod], node: &str) -> Result<(), String> {
    let labelled = nodes.iter().any(|n| n.name == node && n.etcd);
    match pods.iter().find(|p| p.node == node) {
        Some(p) if p.ready => Ok(()),
        Some(_) => Err(format!("etcd on {node} is not ready")),
        None if labelled => Err(format!("etcd member {node} has no etcd pod")),
        None => Ok(()),
    }
}

/// A node, as the wait between machines sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    /// Its `Ready` condition is `True`, and (when asked) its kubelet has been heard from since.
    Ready,
    /// It is in the cluster and not ready.
    NotReady,
    /// `Ready`, but nothing has been heard from its kubelet since the wait began: the condition
    /// may be left over from before the switch.
    Stale,
    /// No node of that name.
    Missing,
}

/// A node's two kinds of heartbeat at one look: the `Ready` condition's and its lease's.
///
/// They are rewritten at very different rates (the lease every 10 s, the condition every few
/// minutes), so a lease is never compared with a condition: that would let a lease renewed before
/// the switch beat a condition recorded minutes earlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Beats {
    /// The `Ready` condition's `lastHeartbeatTime`.
    pub condition: Option<i64>,
    /// The node lease's `renewTime`.
    pub lease: Option<i64>,
}

impl Beats {
    /// The beats `node` shows now.
    #[must_use]
    pub const fn of(node: &Node) -> Self {
        Self {
            condition: node.condition,
            lease: node.lease,
        }
    }

    /// Whether `node` shows a heartbeat newer than this record, of the **same kind**: a lease newer
    /// than a recorded lease, or a condition heartbeat newer than a recorded condition heartbeat.
    /// A beat with no recorded beat of its kind proves nothing: it may have been there all along.
    #[must_use]
    pub fn beaten_by(&self, node: &Node) -> bool {
        let newer = |now: Option<i64>, before: Option<i64>| matches!((now, before), (Some(a), Some(b)) if a > b);
        newer(node.lease, self.lease) || newer(node.condition, self.condition)
    }
}

/// What a node must have shown for [`node_state`] to call it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    /// Nothing: `Ready` is enough.
    Unasked,
    /// A heartbeat newer than the node's own when the wait began, of the same kind
    /// ([`Beats::beaten_by`]). Two readings of the **same node's** clock: the kubelets' clocks are
    /// never compared with this machine's.
    Beats(Beats),
}

/// The state of node `name`.
///
/// With [`Since::Beats`], a `Ready` node counts only once it shows a heartbeat newer than the one
/// recorded when the wait began: `Ready=True` stays true for the node controller's grace period
/// (about 40 s) after the kubelet's last report, so a node restarted for less than that would
/// otherwise pass on the condition it had before the switch.
#[must_use]
pub fn node_state(nodes: &[Node], name: &str, since: Since) -> NodeState {
    match nodes.iter().find(|n| n.name == name) {
        Some(n) if n.ready => match since {
            Since::Unasked => NodeState::Ready,
            Since::Beats(record) if record.beaten_by(n) => NodeState::Ready,
            Since::Beats(_) => NodeState::Stale,
        },
        Some(_) => NodeState::NotReady,
        None => NodeState::Missing,
    }
}

/// The beats node `name` has now, for [`Since::Beats`]: what to record when a wait begins.
///
/// `None` when the node is not in the list (nothing is recorded then, and the first look that has
/// it records instead).
#[must_use]
pub fn heartbeat_of(nodes: &[Node], name: &str) -> Option<Since> {
    nodes
        .iter()
        .find(|n| n.name == name)
        .map(|n| Since::Beats(Beats::of(n)))
}

/// Fills in what the record of a wait lacks from this look.
///
/// A kind of heartbeat that was not there when the record was taken (its read failed, or the node
/// had none) is recorded now, to be beaten by a later one; it proves nothing itself. Call it after
/// judging the look with [`node_state`].
pub fn note_heartbeat(record: &mut Option<Since>, nodes: &[Node], name: &str) {
    let (Some(Since::Beats(beats)), Some(node)) = (record, nodes.iter().find(|n| n.name == name))
    else {
        return;
    };
    beats.condition = beats.condition.or(node.condition);
    beats.lease = beats.lease.or(node.lease);
}

/// The names of the failed units of a probe, when it observed the machine and could read them.
#[must_use]
pub fn failed_units(probe: &ProbeOutcome) -> Option<Vec<String>> {
    match probe {
        ProbeOutcome::Observed(o) => o.failed_units.clone(),
        _ => None,
    }
}

/// Whether `probe` shows a machine that took the update and is up: reachable, running exactly
/// `expect`, systemd past starting up, and **no failed unit that `baseline` does not already
/// hold**.
///
/// `baseline` is the machine's failed units when its turn began. A unit that was already failed
/// is not the update's doing (and would otherwise fail the machine on every roll, for as long as
/// someone leaves it failed); a unit that is newly failed is, and stops the group. `degraded` is
/// accepted only when every failed unit is in the baseline, and never when the list could not be
/// read. `starting` and an unreadable systemd state on a NixOS machine are "not yet".
///
/// # Errors
///
/// Why the machine is not (yet) healthy, one line.
pub fn healthy(
    probe: &ProbeOutcome,
    expect: &StorePath,
    baseline: &[String],
) -> Result<(), String> {
    let o = match probe {
        ProbeOutcome::Observed(o) => o,
        ProbeOutcome::Unreachable { reason } => return Err(format!("unreachable: {reason}")),
        ProbeOutcome::Failed { reason } => return Err(format!("its probe failed: {reason}")),
    };
    match &o.current_system {
        Some(now) if now == expect => {}
        Some(now) => {
            return Err(format!(
                "it runs {} {}, not the {} {} that was deployed",
                &now.hash()[..8],
                now.name(),
                &expect.hash()[..8],
                expect.name()
            ));
        }
        None => return Err("it reports no current system".to_owned()),
    }
    let new: Vec<&str> = o
        .failed_units
        .iter()
        .flatten()
        .filter(|u| !baseline.contains(u))
        .map(String::as_str)
        .collect();
    if !new.is_empty() {
        return Err(format!("new failed units: {}", new.join(", ")));
    }
    let darwin = o.class == Some(crate::fleet::MachineClass::Darwin);
    match o.system_state.as_deref() {
        Some("running") => Ok(()),
        // Failed units that are all in the baseline were failed before: nothing new is wrong.
        Some("degraded") if o.failed_units.is_some() => Ok(()),
        Some("degraded") => {
            Err("systemd is degraded and its failed units could not be read".to_owned())
        }
        Some(state) => Err(format!("systemd is {state}")),
        None if darwin => Ok(()),
        None => Err("systemd's state was not reported".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::{ProbeExit, parse_probe};

    fn tags(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| (*s).to_owned()).collect()
    }

    fn rules(spec: &[(&str, RollRule)]) -> RollRules {
        spec.iter().map(|(t, r)| ((*t).to_owned(), *r)).collect()
    }

    fn k8s() -> RollRule {
        RollRule {
            kubernetes: true,
            ..RollRule::default()
        }
    }

    fn plan_of(machines: &[(&str, &[&str])], rules: &RollRules) -> Plan {
        let owned: Vec<(String, Vec<String>)> = machines
            .iter()
            .map(|(n, t)| ((*n).to_owned(), tags(t)))
            .collect();
        let candidates: Vec<Candidate<'_>> = owned
            .iter()
            .map(|(n, t)| Candidate { name: n, tags: t })
            .collect();
        plan(&candidates, rules)
    }

    fn names(g: &Group) -> Vec<&str> {
        g.members.iter().map(|m| m.name.as_str()).collect()
    }

    #[test]
    fn machines_sharing_a_rolling_tag_are_one_group_in_name_order() {
        let p = plan_of(
            &[
                ("terra", &["taghmata", "skitarii"]),
                ("alfrost", &["base"]),
                ("luna", &["taghmata"]),
                ("sol", &["taghmata"]),
                ("mars", &["taghmata"]),
            ],
            &rules(&[("taghmata", k8s())]),
        );
        assert_eq!(p.free, ["alfrost"]);
        assert_eq!(p.groups.len(), 1);
        assert_eq!(names(&p.groups[0]), ["luna", "mars", "sol", "terra"]);
        assert_eq!(p.groups[0].tags, ["taghmata"]);
        assert!(p.groups[0].kubernetes());
    }

    #[test]
    fn a_tag_the_config_does_not_name_binds_nothing() {
        let p = plan_of(
            &[("luna", &["skitarii"]), ("sol", &["skitarii"])],
            &rules(&[("taghmata", k8s())]),
        );
        assert_eq!(p.groups.len(), 0);
        assert_eq!(p.free, ["luna", "sol"]);
        let none = plan_of(&[("luna", &["taghmata"])], &RollRules::new());
        assert!(none.groups.is_empty(), "no rules, no groups");
    }

    /// The overlap that matters: a machine on two rolling tags joins both tags' machines, so two
    /// machines with no tag in common are still never updated together.
    #[test]
    fn rolling_tags_join_transitively() {
        let p = plan_of(
            &[
                ("a", &["one"]),
                ("b", &["one", "two"]),
                ("c", &["two"]),
                ("d", &["three"]),
            ],
            &rules(&[
                ("one", RollRule::default()),
                ("two", RollRule::default()),
                ("three", RollRule::default()),
            ]),
        );
        assert_eq!(p.groups.len(), 2);
        assert_eq!(names(&p.groups[0]), ["a", "b", "c"]);
        assert_eq!(p.groups[0].tags, ["one", "two"]);
        assert_eq!(p.groups[0].label(), "one+two");
        assert_eq!(names(&p.groups[1]), ["d"]);
        assert_eq!(p.free.len(), 0);
        assert!(!p.groups[0].kubernetes());
    }

    #[test]
    fn a_member_takes_its_kubernetes_and_drain_from_its_own_rolling_tags() {
        let drain = RollRule {
            kubernetes: true,
            drain: true,
            drain_timeout_seconds: 90,
            etcd: false,
        };
        let slow = RollRule {
            drain: true,
            drain_timeout_seconds: 300,
            ..RollRule::default()
        };
        let p = plan_of(
            &[
                ("luna", &["cp", "workers"]),
                ("mars", &["cp"]),
                ("sol", &["workers"]),
            ],
            &rules(&[("cp", drain), ("workers", slow)]),
        );
        let m = &p.groups[0].members;
        assert_eq!(
            m[0],
            Member {
                name: "luna".to_owned(),
                kubernetes: true,
                drain: Some(90),
                etcd: false
            },
            "the strictest (smallest) timeout wins"
        );
        assert_eq!(m[1].drain, Some(90));
        assert!(!m[2].kubernetes);
        assert_eq!(m[2].drain, Some(300));
        let off = plan_of(&[("luna", &["cp"])], &rules(&[("cp", k8s())]));
        assert_eq!(off.groups[0].members[0].drain, None, "drain is opt-in");
    }

    #[test]
    fn a_name_listed_twice_is_one_machine() {
        let p = plan_of(&[("luna", &[]), ("luna", &[])], &RollRules::new());
        assert_eq!(p.free, ["luna"]);
    }

    const NODES: &str = r#"{"apiVersion":"v1","items":[
      {"metadata":{"name":"luna"},"spec":{},
       "status":{"conditions":[{"type":"MemoryPressure","status":"False"},{"type":"Ready","status":"True"}]}},
      {"metadata":{"name":"mars"},"spec":{"unschedulable":true},
       "status":{"conditions":[{"type":"Ready","status":"True"}]}},
      {"metadata":{"name":"sol"},
       "status":{"conditions":[{"type":"Ready","status":"False"}]}},
      {"metadata":{"name":"terra"},"status":{}}
    ]}"#;

    #[test]
    fn nodes_are_parsed_with_their_ready_and_cordoned_state() {
        let nodes = parse_nodes(NODES).unwrap();
        let state: Vec<(&str, bool, bool)> = nodes
            .iter()
            .map(|n| (n.name.as_str(), n.ready, n.cordoned))
            .collect();
        assert_eq!(
            state,
            [
                ("luna", true, false),
                ("mars", true, true),
                ("sol", false, false),
                ("terra", false, false),
            ],
            "no Ready condition is not ready"
        );
        assert_eq!(node_state(&nodes, "luna", Since::Unasked), NodeState::Ready);
        assert_eq!(
            node_state(&nodes, "sol", Since::Unasked),
            NodeState::NotReady
        );
        assert_eq!(
            node_state(&nodes, "nobody", Since::Unasked),
            NodeState::Missing
        );
    }

    #[test]
    fn a_node_list_that_is_not_one_is_an_error_not_an_empty_cluster() {
        for bad in [
            "",
            "[]",
            "{}",
            r#"{"items":null}"#,
            r#"{"items":[{"metadata":{}}]}"#,
            r#"{"items":[{"metadata":{"name":""}}]}"#,
            "No resources found",
        ] {
            assert!(parse_nodes(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse_nodes(r#"{"items":[]}"#).unwrap(), Vec::new());
    }

    #[test]
    fn node_names_are_sanitised() {
        let n = parse_nodes(r#"{"items":[{"metadata":{"name":"a\u001b[2Jb"}}]}"#).unwrap();
        assert_eq!(n[0].name, "ab");
    }

    fn member(name: &str, kubernetes: bool) -> Member {
        Member {
            name: name.to_owned(),
            kubernetes,
            drain: None,
            etcd: false,
        }
    }

    #[test]
    fn a_cluster_with_a_notready_or_cordoned_node_refuses_the_roll() {
        let nodes = parse_nodes(NODES).unwrap();
        let (luna, sol) = (member("luna", true), member("sol", true));
        let why = cluster_refusals(&nodes, &[&luna, &sol]);
        assert_eq!(
            why,
            [
                "node mars is cordoned",
                "node sol is NotReady",
                "node terra is NotReady"
            ]
        );
        let ok = parse_nodes(
            r#"{"items":[{"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"True"}]}}]}"#,
        )
        .unwrap();
        assert_eq!(cluster_refusals(&ok, &[&luna]), Vec::<String>::new());
    }

    #[test]
    fn a_member_that_is_no_node_refuses_the_roll_but_only_when_it_is_kubernetes() {
        let ok = parse_nodes(
            r#"{"items":[{"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"True"}]}}]}"#,
        )
        .unwrap();
        let stranger = member("alfrost", true);
        assert_eq!(
            cluster_refusals(&ok, &[&stranger]),
            ["alfrost is not a node of the cluster (a node is named like its machine)"]
        );
        let plain = member("alfrost", false);
        assert_eq!(cluster_refusals(&ok, &[&plain]), Vec::<String>::new());
    }

    const SYS: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-luna-25.05";
    const OTHER: &str = "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-nixos-system-luna-25.05";

    fn probe(current: &str, extra: &str) -> ProbeOutcome {
        parse_probe(
            &format!(
                "auspex_probe=1\nprobe=nixos\ncurrent_system={current}\n{extra}auspex_end=1\n"
            ),
            "",
            ProbeExit::Code(0),
        )
    }

    #[test]
    fn healthy_means_up_on_the_deployed_system_and_not_degraded() {
        let expect = StorePath::new(SYS).unwrap();
        let none: &[String] = &[];
        assert_eq!(
            healthy(
                &probe(SYS, "system_state=running\nfailed_units=[]\n"),
                &expect,
                none
            ),
            Ok(())
        );
        let old = healthy(&probe(OTHER, "system_state=running\n"), &expect, none).unwrap_err();
        assert!(old.contains("not the"), "{old}");
        let starting = healthy(&probe(SYS, "system_state=starting\n"), &expect, none).unwrap_err();
        assert_eq!(starting, "systemd is starting");
        let failed = healthy(
            &probe(
                SYS,
                "system_state=degraded\nfailed_units=[{\"unit\":\"rke2-server.service\"}]\n",
            ),
            &expect,
            none,
        )
        .unwrap_err();
        assert_eq!(failed, "new failed units: rke2-server.service");
        let down = healthy(
            &ProbeOutcome::Unreachable {
                reason: "Connection refused".to_owned(),
            },
            &expect,
            none,
        )
        .unwrap_err();
        assert_eq!(down, "unreachable: Connection refused");
        assert!(
            healthy(
                &ProbeOutcome::Failed {
                    reason: "cut off".to_owned()
                },
                &expect,
                none
            )
            .is_err()
        );
    }

    /// A machine with a unit that was failed before the update (the operator's terra has one) is
    /// healthy after it; a unit that fails because of it is not, and `degraded` with an unreadable
    /// list never is.
    #[test]
    fn a_unit_that_was_failed_before_the_update_is_not_the_updates_failure() {
        let expect = StorePath::new(SYS).unwrap();
        let before = vec!["old.service".to_owned()];
        let degraded = "system_state=degraded\nfailed_units=[{\"unit\":\"old.service\"}]\n";
        assert_eq!(healthy(&probe(SYS, degraded), &expect, &before), Ok(()));
        // The update fixed it, or never touched it: running is running.
        assert_eq!(
            healthy(
                &probe(SYS, "system_state=running\nfailed_units=[]\n"),
                &expect,
                &before
            ),
            Ok(())
        );
        // A second unit failed: that one is new.
        let two = "system_state=degraded\nfailed_units=[{\"unit\":\"old.service\"},{\"unit\":\"new.service\"}]\n";
        assert_eq!(
            healthy(&probe(SYS, two), &expect, &before).unwrap_err(),
            "new failed units: new.service"
        );
        // Degraded and no readable list: nothing says what failed.
        let unreadable =
            healthy(&probe(SYS, "system_state=degraded\n"), &expect, &before).unwrap_err();
        assert!(unreadable.contains("could not be read"), "{unreadable}");
        // No systemd report on NixOS is "not yet"; on darwin there is none to give.
        assert!(healthy(&probe(SYS, ""), &expect, &before).is_err());
        let darwin = parse_probe(
            &format!("auspex_probe=1\nprobe=darwin\ncurrent_system={SYS}\nauspex_end=1\n"),
            "",
            ProbeExit::Code(0),
        );
        assert_eq!(healthy(&darwin, &expect, &before), Ok(()));
        assert_eq!(failed_units(&probe(SYS, degraded)), Some(before));
        assert_eq!(failed_units(&probe(SYS, "")), None);
    }

    #[test]
    fn rfc3339_times_are_read_as_utc_seconds_and_nothing_else_is() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-11-20T15:04:05Z"), Some(1_795_187_045));
        assert_eq!(
            parse_rfc3339("2026-11-20T15:04:05.123456Z"),
            Some(1_795_187_045),
            "a lease's renewTime has a fraction"
        );
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        for bad in [
            "",
            "2026-11-20T15:04:05",
            "2026-11-20T15:04:05+01:00",
            "2026-13-20T15:04:05Z",
            "2026-11-20 15:04:05Z",
            "yesterday",
            "2026-11-20T25:04:05Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }

    const LUNA_READY_AT: &str = r#"{"items":[
      {"metadata":{"name":"luna"},"status":{"conditions":[
        {"type":"Ready","status":"True","lastHeartbeatTime":"2026-11-20T15:00:00Z"}]}},
      {"metadata":{"name":"sol"},"status":{"conditions":[
        {"type":"Ready","status":"Unknown","lastHeartbeatTime":"2026-11-20T15:00:00Z"}]}}]}"#;

    /// `Ready` from before the switch is not `Ready` after it: the wait wants a heartbeat that is
    /// newer than the node's own when the wait began, of the same kind (the condition's, or the
    /// lease's, which a kubelet renews every ten seconds). `Unknown`, which a node gets when its
    /// kubelet stops reporting, is not ready.
    #[test]
    fn a_ready_that_is_older_than_the_update_does_not_count() {
        let mut nodes = parse_nodes(LUNA_READY_AT).unwrap();
        let before = parse_rfc3339("2026-11-20T15:00:00Z").unwrap();
        assert_eq!(nodes[0].heartbeat, Some(before));
        assert_eq!(nodes[0].condition, Some(before));
        let beats = |condition, lease| Since::Beats(Beats { condition, lease });
        assert_eq!(node_state(&nodes, "luna", Since::Unasked), NodeState::Ready);
        assert_eq!(
            node_state(&nodes, "luna", beats(Some(before), None)),
            NodeState::Stale,
            "the same heartbeat is not a new one"
        );
        assert_eq!(
            node_state(&nodes, "luna", beats(Some(before - 1), None)),
            NodeState::Ready
        );
        assert_eq!(
            node_state(&nodes, "luna", beats(Some(before + 1), None)),
            NodeState::Stale,
            "nothing is compared with a clock: a record from the future is simply not beaten"
        );
        assert_eq!(
            node_state(&nodes, "sol", Since::Unasked),
            NodeState::NotReady,
            "Ready=Unknown is not Ready"
        );
        assert_eq!(
            heartbeat_of(&nodes, "luna"),
            Some(beats(Some(before), None))
        );
        assert_eq!(heartbeat_of(&nodes, "nobody"), None);
        // Its lease says the kubelet is alive after the update ended.
        let leases = parse_leases(
            r#"{"items":[{"metadata":{"name":"luna"},"spec":{"renewTime":"2026-11-20T15:00:30.5Z"}},
                         {"metadata":{"name":"gone"},"spec":{"renewTime":"2026-11-20T15:00:30Z"}},
                         {"metadata":{"name":"bad"},"spec":{"renewTime":"never"}}]}"#,
        )
        .unwrap();
        assert_eq!(leases.len(), 2, "an unreadable time is no time");
        merge_leases(&mut nodes, &leases);
        assert_eq!(nodes[0].heartbeat, Some(before + 30));
        assert_eq!(nodes[0].condition, Some(before), "the kinds stay apart");
        assert!(nodes[0].lease.is_some() && nodes[1].lease.is_none());
        // A lease newer than a recorded lease, or a condition newer than a recorded condition.
        assert_eq!(
            node_state(&nodes, "luna", beats(None, Some(before))),
            NodeState::Ready
        );
        assert_eq!(
            node_state(&nodes, "luna", beats(Some(before - 1), Some(before + 31))),
            NodeState::Ready,
            "the condition beats its record even though the lease does not"
        );
        // A node with no heartbeat at all is stale when asked, never Ready by default.
        let silent = parse_nodes(
            r#"{"items":[{"metadata":{"name":"luna"},"status":{"conditions":[{"type":"Ready","status":"True"}]}}]}"#,
        )
        .unwrap();
        assert_eq!(
            node_state(&silent, "luna", beats(None, None)),
            NodeState::Stale
        );
        assert_eq!(
            node_state(&nodes, "nobody", beats(Some(1), Some(1))),
            NodeState::Missing
        );
        assert!(parse_leases("[]").is_err());
    }

    /// R1. The record and the proof must be of one kind. The record was taken with the leases
    /// unread (so it holds the condition's heartbeat only); the next look has a lease from before
    /// the switch, which is newer than that *condition* but proves nothing: it is recorded, and only
    /// a later lease beats it. Same for a node list with no condition heartbeat at all.
    #[test]
    fn a_lease_with_no_recorded_lease_proves_nothing_and_is_recorded() {
        let base = parse_rfc3339("2026-11-20T15:00:00Z").unwrap();
        let node = |condition: Option<i64>, lease: Option<i64>| Node {
            name: "luna".to_owned(),
            ready: true,
            cordoned: false,
            heartbeat: condition.max(lease),
            condition,
            lease,
            etcd: false,
        };
        for condition in [Some(base), None] {
            // The record: leases unread.
            let mut record = heartbeat_of(&[node(condition, None)], "luna");
            // A lease from before the switch, newer than the condition: not a proof.
            let look = [node(condition, Some(base + 60))];
            let state = node_state(&look, "luna", record.unwrap_or(Since::Unasked));
            assert_eq!(state, NodeState::Stale, "condition {condition:?}");
            note_heartbeat(&mut record, &look, "luna");
            // The same lease again: still nothing.
            assert_eq!(
                node_state(&look, "luna", record.unwrap()),
                NodeState::Stale,
                "condition {condition:?}"
            );
            // A renewal: the proof.
            let renewed = [node(condition, Some(base + 70))];
            assert_eq!(
                node_state(&renewed, "luna", record.unwrap()),
                NodeState::Ready,
                "condition {condition:?}"
            );
        }
        // A record is only ever filled in, never lowered or replaced: a later look without a lease
        // (the read failed again) leaves it as it was.
        let mut record = heartbeat_of(&[node(Some(base), Some(base + 5))], "luna");
        note_heartbeat(&mut record, &[node(Some(base + 9), None)], "luna");
        assert_eq!(
            record,
            Some(Since::Beats(Beats {
                condition: Some(base),
                lease: Some(base + 5)
            }))
        );
    }

    /// Four nodes, `Ready`, each with the lease time given (seconds after a fixed minute); `None`
    /// leaves the node without a lease.
    fn leased(spec: &[(&str, Option<i64>)]) -> Vec<Node> {
        let base = parse_rfc3339("2026-11-20T15:00:00Z").unwrap();
        let mut nodes = parse_nodes(&format!(
            r#"{{"items":[{}]}}"#,
            spec.iter()
                .map(|(n, _)| format!(
                    r#"{{"metadata":{{"name":"{n}"}},"status":{{"conditions":[{{"type":"Ready","status":"True"}}]}}}}"#
                ))
                .collect::<Vec<_>>()
                .join(",")
        ))
        .unwrap();
        let leases = spec
            .iter()
            .filter_map(|(n, at)| at.map(|at| ((*n).to_owned(), base + at)))
            .collect();
        merge_leases(&mut nodes, &leases);
        nodes
    }

    /// N2. A kubelet that stopped renewing is still `Ready` for the node controller's grace (about
    /// 40 s). It counts as down once its newest heartbeat is more than 25 s older than the newest of
    /// the same read: 25 s itself (two missed renewals and a margin) is allowed, 26 s is not. The
    /// reviewer's case, a kubelet silent for about 30 s, is refused.
    #[test]
    fn a_ready_node_whose_kubelet_is_more_than_25_seconds_behind_the_others_is_not_healthy() {
        let at = |mars: i64| {
            leased(&[
                ("luna", Some(100)),
                ("mars", Some(100 - mars)),
                ("sol", Some(100)),
            ])
        };
        assert_eq!(cluster_unhealthy(&at(0)), Vec::<String>::new());
        assert_eq!(
            cluster_unhealthy(&at(25)),
            Vec::<String>::new(),
            "25 s is allowed"
        );
        assert_eq!(
            cluster_unhealthy(&at(26)),
            [
                "node mars is Ready, but its kubelet has been silent for 26 s by luna's clock (the newest lease in this read)"
            ]
        );
        // The review's RR2: three missed renewals. The plan says the same, naming node and age.
        let (luna, sol) = (member("luna", true), member("sol", true));
        assert_eq!(
            cluster_refusals(&at(30), &[&luna, &sol]),
            [
                "node mars is Ready, but its kubelet has been silent for 30 s by luna's clock (the newest lease in this read)"
            ]
        );
        // The newest heartbeat is the reference, not a clock: the same ages at any date.
        let far = leased(&[("luna", Some(1_000_000)), ("mars", Some(1_000_000 - 26))]);
        assert_eq!(cluster_unhealthy(&far).len(), 1);
        // A node that is NotReady is said once, as NotReady.
        let mut nodes = at(90);
        nodes[1].ready = false;
        assert_eq!(cluster_unhealthy(&nodes), ["node mars is NotReady"]);
    }

    /// N2. Normal renewal jitter is not silence: a kubelet that renewed 10 s before the others (one
    /// whole interval behind) and one 12 s behind (an interval plus the read's own timing) are
    /// healthy, in the read before a machine and in the plan.
    #[test]
    fn a_node_twelve_seconds_behind_the_newest_heartbeat_is_not_refused() {
        let (luna, sol) = (member("luna", true), member("sol", true));
        for behind in [10, 12] {
            let nodes = leased(&[
                ("luna", Some(100)),
                ("mars", Some(100 - behind)),
                ("sol", Some(100)),
            ]);
            assert_eq!(
                cluster_unhealthy(&nodes),
                Vec::<String>::new(),
                "{behind} s behind"
            );
            assert_eq!(
                cluster_refusals(&nodes, &[&luna, &sol]),
                Vec::<String>::new(),
                "{behind} s behind, in the plan"
            );
        }
    }

    /// N2. Without any lease the condition's own heartbeat (rewritten every few minutes) would make
    /// every node look silent, so nothing is judged; with leases, a node with none of its own is
    /// not vouched for.
    #[test]
    fn without_leases_silence_is_not_judged_and_a_node_without_one_is_not_vouched_for() {
        let mut none = leased(&[("luna", None), ("mars", None)]);
        none[0].heartbeat = Some(1_000);
        none[1].heartbeat = Some(1_000_000);
        assert!(!leases_read(&none));
        assert_eq!(cluster_unhealthy(&none), Vec::<String>::new());
        assert_eq!(silent_kubelets(&none), Vec::<String>::new());
        let some = leased(&[("luna", Some(5)), ("mars", None)]);
        assert!(leases_read(&some));
        assert_eq!(
            cluster_unhealthy(&some),
            ["node mars is Ready, but it has no lease, so its kubelet cannot be vouched for"]
        );
        // R4. Not aged by its condition heartbeat: that is rewritten only every few minutes, so
        // "silent for 240 s" would be a wrong reason for a kubelet that may be fine.
        let mut condition_only = leased(&[("luna", Some(500)), ("mars", None)]);
        condition_only[1].heartbeat = Some(100);
        condition_only[1].condition = Some(100);
        assert_eq!(
            cluster_unhealthy(&condition_only),
            ["node mars is Ready, but it has no lease, so its kubelet cannot be vouched for"]
        );
        assert_eq!(cluster_unhealthy(&[]), Vec::<String>::new());
    }

    /// R3. The age is only as good as the reference clock, so the sentence names the reference. A
    /// node whose clock runs 2 min ahead makes every other node look silent: each sentence points
    /// at luna, and a last one says that luna's clock may be the wrong one. One node behind gets
    /// no such remark.
    #[test]
    fn the_sentences_name_the_reference_clock_and_say_when_it_may_be_the_wrong_one() {
        let ahead = leased(&[
            ("luna", Some(120)),
            ("mars", Some(0)),
            ("sol", Some(1)),
            ("terra", Some(0)),
        ]);
        let why = cluster_unhealthy(&ahead);
        assert_eq!(why.len(), 4, "{why:?}");
        assert_eq!(
            why[0],
            "node mars is Ready, but its kubelet has been silent for 120 s by luna's clock (the newest lease in this read)"
        );
        assert!(
            why[1].starts_with("node sol is Ready") && why[1].contains("119 s by luna's clock")
        );
        assert!(why[2].starts_with("node terra is Ready"));
        assert_eq!(
            why[3],
            "3 nodes are behind luna's lease: if luna's clock runs ahead of theirs they are not silent (the nodes' clocks must agree within a few seconds)"
        );
        // One node behind: its own sentence, no remark about the reference.
        let one = leased(&[("luna", Some(100)), ("mars", Some(50)), ("sol", Some(100))]);
        assert_eq!(cluster_unhealthy(&one).len(), 1);
    }

    #[test]
    fn the_cluster_is_named_by_its_context_and_server() {
        let id = parse_cluster_id(
            r#"{"current-context":"rke2","contexts":[{"name":"rke2","context":{"cluster":"c1","user":"u"}}],
                "clusters":[{"name":"c1","cluster":{"server":"https://10.0.0.1:6443"}}]}"#,
            "/k/config",
        )
        .unwrap();
        assert_eq!(
            id,
            ClusterId {
                context: "rke2".to_owned(),
                server: "https://10.0.0.1:6443".to_owned(),
                kubeconfig: "/k/config".to_owned()
            }
        );
        let no_server = parse_cluster_id(r#"{"current-context":"x"}"#, "/k").unwrap();
        assert_eq!(no_server.server, "unknown");
        assert!(parse_cluster_id(r#"{"contexts":[]}"#, "/k").is_err());
        assert!(parse_cluster_id(r#"{"current-context":""}"#, "/k").is_err());
        assert!(parse_cluster_id("nope", "/k").is_err());
    }

    fn etcd_nodes(spec: &[(&str, bool)]) -> Vec<Node> {
        let items: Vec<String> = spec
            .iter()
            .map(|(n, etcd)| {
                let labels = if *etcd {
                    r#","labels":{"node-role.kubernetes.io/etcd":"true"}"#
                } else {
                    ""
                };
                format!(r#"{{"metadata":{{"name":"{n}"{labels}}}}}"#)
            })
            .collect();
        parse_nodes(&format!(r#"{{"items":[{}]}}"#, items.join(","))).unwrap()
    }

    fn pod(node: &str, ready: bool) -> EtcdPod {
        EtcdPod {
            node: node.to_owned(),
            ready,
        }
    }

    #[test]
    fn the_etcd_label_marks_a_member_unless_it_says_false() {
        let n = parse_nodes(
            r#"{"items":[
              {"metadata":{"name":"a","labels":{"node-role.kubernetes.io/etcd":"true"}}},
              {"metadata":{"name":"b","labels":{"node-role.kubernetes.io/etcd":""}}},
              {"metadata":{"name":"c","labels":{"node-role.kubernetes.io/etcd":"false"}}},
              {"metadata":{"name":"d","labels":{"other":"true"}}},
              {"metadata":{"name":"e"}}]}"#,
        )
        .unwrap();
        let flags: Vec<bool> = n.iter().map(|n| n.etcd).collect();
        assert_eq!(flags, [true, true, false, false, false]);
    }

    #[test]
    fn etcd_pods_are_ready_only_when_running_and_ready() {
        let pods = parse_etcd_pods(
            r#"{"items":[
              {"spec":{"nodeName":"a"},"status":{"phase":"Running","conditions":[{"type":"Ready","status":"True"}]}},
              {"spec":{"nodeName":"b"},"status":{"phase":"Running","conditions":[{"type":"Ready","status":"False"}]}},
              {"spec":{"nodeName":"c"},"status":{"phase":"Pending","conditions":[{"type":"Ready","status":"True"}]}},
              {"spec":{"nodeName":"d"},"status":{"phase":"Running"}},
              {"spec":{"nodeName":"e"}},
              {"spec":{},"status":{"phase":"Pending"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            pods,
            [
                pod("a", true),
                pod("b", false),
                pod("c", false),
                pod("d", false),
                pod("e", false)
            ]
        );
        for bad in ["", "nope", "[]", r#"{"kind":"List"}"#] {
            assert!(parse_etcd_pods(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse_etcd_pods(r#"{"items":[]}"#).unwrap(), Vec::new());
    }

    #[test]
    fn etcd_is_healthy_when_every_member_has_a_ready_pod() {
        let nodes = etcd_nodes(&[("a", true), ("b", true), ("w", false)]);
        let pods = [pod("a", true), pod("b", true)];
        assert_eq!(etcd_unhealthy(&nodes, &pods), Vec::<String>::new());
    }

    #[test]
    fn a_member_whose_etcd_is_not_ready_or_missing_makes_etcd_unhealthy() {
        let nodes = etcd_nodes(&[("a", true), ("b", true), ("c", true)]);
        let why = etcd_unhealthy(&nodes, &[pod("a", true), pod("b", false)]);
        assert_eq!(
            why,
            ["etcd on b is not ready", "etcd member c has no etcd pod"]
        );
    }

    #[test]
    fn what_cannot_be_told_is_not_healthy() {
        // No node says it is a member.
        let none = etcd_nodes(&[("a", false)]);
        assert_eq!(etcd_unhealthy(&none, &[pod("a", true)]).len(), 1);
        assert!(
            etcd_unhealthy(&[], &[])
                .iter()
                .any(|w| w.contains("cannot tell"))
        );
        // A pod on a node that is not labelled.
        let nodes = etcd_nodes(&[("a", true), ("b", false)]);
        let why = etcd_unhealthy(&nodes, &[pod("a", true), pod("b", true)]);
        assert_eq!(why.len(), 1);
        assert!(why[0].contains("etcd pod on b"), "{why:?}");
    }

    #[test]
    fn a_member_is_back_when_its_pod_is_ready_and_a_non_member_has_nothing_to_wait_for() {
        let nodes = etcd_nodes(&[("a", true), ("w", false)]);
        assert!(etcd_member_ready(&nodes, &[pod("a", true)], "a").is_ok());
        assert!(etcd_member_ready(&nodes, &[pod("a", false)], "a").is_err());
        assert!(etcd_member_ready(&nodes, &[], "a").is_err());
        assert!(etcd_member_ready(&nodes, &[pod("a", true)], "w").is_ok());
        // An etcd pod on an unlabelled node still has to be ready.
        assert!(etcd_member_ready(&nodes, &[pod("w", false)], "w").is_err());
    }

    #[test]
    fn etcd_implies_kubernetes_in_the_plan_and_is_off_by_default() {
        assert!(!RollRule::default().etcd);
        let on = RollRule {
            etcd: true,
            ..RollRule::default()
        };
        let p = plan_of(
            &[("a", &["cp"]), ("b", &["cp"]), ("c", &["other"])],
            &rules(&[("cp", on), ("other", k8s())]),
        );
        assert!(p.groups[0].members.iter().all(|m| m.etcd && m.kubernetes));
        assert!(p.groups[0].etcd());
        assert!(!p.groups[1].members[0].etcd);
        assert!(!p.groups[1].etcd());
    }
}
