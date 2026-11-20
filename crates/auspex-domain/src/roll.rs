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
}

/// The drain timeout when the configuration names none: ten minutes.
pub const DEFAULT_DRAIN_TIMEOUT_SECONDS: u64 = 600;

impl Default for RollRule {
    fn default() -> Self {
        Self {
            kubernetes: false,
            drain: false,
            drain_timeout_seconds: DEFAULT_DRAIN_TIMEOUT_SECONDS,
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
                        kubernetes: applying().any(|r| r.kubernetes),
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
    /// The `Ready` condition is `True`.
    pub ready: bool,
    /// `spec.unschedulable`: cordoned.
    pub cordoned: bool,
}

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
}

/// Parses `kubectl get nodes -o json`.
///
/// A node with no `Ready` condition is not ready: absence of a report is not health.
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
            let name = n
                .metadata
                .and_then(|m| m.name)
                .map(|n| sanitize_line(&n))
                .filter(|n| !n.is_empty())
                .ok_or_else(|| "kubectl's node list has a node with no name".to_owned())?;
            let ready = n.status.and_then(|s| s.conditions).is_some_and(|cs| {
                cs.iter().any(|c| {
                    c.kind.as_deref() == Some("Ready") && c.status.as_deref() == Some("True")
                })
            });
            let cordoned = n.spec.and_then(|s| s.unschedulable).unwrap_or(false);
            Ok(Node {
                name,
                ready,
                cordoned,
            })
        })
        .collect()
}

/// Why a roll of `members` may not start in this cluster, one sentence each; empty when it may.
///
/// Any node of the cluster that is `NotReady` or cordoned refuses it (not only the members:
/// a roll takes one more node out of service for a while, and a cluster already short of one is
/// not the moment), and so does a member that is no node at all.
#[must_use]
pub fn cluster_refusals(nodes: &[Node], members: &[&Member]) -> Vec<String> {
    let mut why = Vec::new();
    for node in nodes {
        if !node.ready {
            why.push(format!("node {} is NotReady", node.name));
        }
        if node.cordoned {
            why.push(format!("node {} is cordoned", node.name));
        }
    }
    for m in members.iter().filter(|m| m.kubernetes) {
        if !nodes.iter().any(|n| n.name == m.name) {
            why.push(format!(
                "{} is not a node of the cluster (a node is named like its machine)",
                m.name
            ));
        }
    }
    why
}

/// A node, as the wait between machines sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    /// Its `Ready` condition is `True`.
    Ready,
    /// It is in the cluster and not ready.
    NotReady,
    /// No node of that name.
    Missing,
}

/// The state of node `name`.
#[must_use]
pub fn node_state(nodes: &[Node], name: &str) -> NodeState {
    match nodes.iter().find(|n| n.name == name) {
        Some(n) if n.ready => NodeState::Ready,
        Some(_) => NodeState::NotReady,
        None => NodeState::Missing,
    }
}

/// Whether `probe` shows a machine that took the update and is up: reachable, running exactly
/// `expect`, and with systemd past starting up without failed units.
///
/// `starting` and `degraded` are both "not yet": a machine just switched is often still starting,
/// so the caller asks again until its deadline. What it reports then is the last reason.
///
/// # Errors
///
/// Why the machine is not (yet) healthy, one line.
pub fn healthy(probe: &ProbeOutcome, expect: &StorePath) -> Result<(), String> {
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
    if o.has_failed_units() {
        let units = o.failed_units.as_deref().unwrap_or_default().join(", ");
        return Err(format!("failed units: {units}"));
    }
    match o.system_state.as_deref() {
        None | Some("running") => Ok(()),
        Some(state) => Err(format!("systemd is {state}")),
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
                drain: Some(90)
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
        assert_eq!(node_state(&nodes, "luna"), NodeState::Ready);
        assert_eq!(node_state(&nodes, "sol"), NodeState::NotReady);
        assert_eq!(node_state(&nodes, "nobody"), NodeState::Missing);
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
        assert_eq!(
            healthy(
                &probe(SYS, "system_state=running\nfailed_units=[]\n"),
                &expect
            ),
            Ok(())
        );
        assert_eq!(
            healthy(&probe(SYS, ""), &expect),
            Ok(()),
            "no systemd report (darwin)"
        );
        let old = healthy(&probe(OTHER, "system_state=running\n"), &expect).unwrap_err();
        assert!(old.contains("not the"), "{old}");
        let starting = healthy(&probe(SYS, "system_state=starting\n"), &expect).unwrap_err();
        assert_eq!(starting, "systemd is starting");
        let failed = healthy(
            &probe(
                SYS,
                "system_state=degraded\nfailed_units=[{\"unit\":\"rke2-server.service\"}]\n",
            ),
            &expect,
        )
        .unwrap_err();
        assert_eq!(failed, "failed units: rke2-server.service");
        let down = healthy(
            &ProbeOutcome::Unreachable {
                reason: "Connection refused".to_owned(),
            },
            &expect,
        )
        .unwrap_err();
        assert_eq!(down, "unreachable: Connection refused");
        assert!(
            healthy(
                &ProbeOutcome::Failed {
                    reason: "cut off".to_owned()
                },
                &expect
            )
            .is_err()
        );
    }
}
