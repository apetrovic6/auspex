//! The package preview through the core: which machines get its two closure queries, what
//! the answers make of them, and how a refresh, a failure and the deadlines treat them.
//! Requests are resolved by hand, in the order each test chooses.

mod support;

use auspex_core::effects::EvalAnswer;
use auspex_core::{Config, Event, PROBE_TIMED_OUT};
use auspex_domain::closure::ClosurePaths;
use auspex_domain::diff::DiffState;
use auspex_domain::observe::StorePath;
use auspex_domain::status::{Badge, NarCache};
use support::*;

/// terra runs what it should, luna and sol each have an update pending.
const FLEET: &str = THREE;

fn closure(packages: &[(char, &str)], dropped: usize) -> ClosurePaths {
    ClosurePaths {
        paths: packages
            .iter()
            .map(|(c, name)| StorePath::new(path(*c, name)).unwrap())
            .collect(),
        dropped,
    }
}

fn old() -> ClosurePaths {
    closure(
        &[
            ('a', "linux-6.12.38"),
            ('b', "htop-3.3.0"),
            ('c', "zlib-1.3.1"),
        ],
        2,
    )
}

fn new() -> ClosurePaths {
    closure(
        &[
            ('d', "linux-6.12.41"),
            ('f', "ripgrep-14.1.1"),
            ('c', "zlib-1.3.1"),
        ],
        1,
    )
}

/// Everything but the cache answers is in: probes, the evaluation, Prometheus. terra is in
/// sync; luna and sol have an update pending, so each has a cache request out.
fn scanned(config: Config) -> Shell {
    let mut shell = Shell::new();
    shell.start_with(config, FLEET);
    let mut eval = shell.take_eval();
    let now = path('0', "nixos-system-now");
    for machine in ["terra", "luna", "sol"] {
        shell.probe(machine, observed(&now, ""));
    }
    shell.eval_item(&mut eval, "terra", evaluated("terra", &now));
    shell.eval_item(
        &mut eval,
        "luna",
        evaluated("luna", &path('1', "nixos-system-next")),
    );
    shell.eval_item(
        &mut eval,
        "sol",
        evaluated("sol", &path('2', "nixos-system-next")),
    );
    shell.resolve(&mut eval, EvalAnswer::Finished { error: None });
    if !shell.prometheus.is_empty() {
        let mut prom = shell.take_prometheus();
        shell.resolve(&mut prom, Ok(std::collections::BTreeMap::default()));
    }
    shell
}

fn answer_cache(shell: &mut Shell, machine: &str, cache: NarCache) {
    let mut request = shell.take_cache(machine);
    shell.resolve(&mut request, cache);
}

#[test]
fn closures_are_requested_only_for_an_update_pending_machine_whose_pending_closure_is_cached() {
    let mut shell = scanned(diff_config());
    assert!(
        shell.deployed.is_empty() && shell.pending_closures.is_empty(),
        "nothing is asked before the cache says the pending closure is there"
    );
    assert_eq!(
        shell.caches.len(),
        2,
        "terra is in sync: its cache is never asked, so it has no preview either"
    );
    answer_cache(&mut shell, "luna", NarCache::Cached);
    answer_cache(&mut shell, "sol", NarCache::NotCached);

    let deployed: Vec<_> = shell
        .deployed
        .iter()
        .map(|r| r.operation.machine.clone())
        .collect();
    let pending: Vec<_> = shell
        .pending_closures
        .iter()
        .map(|r| r.operation.machine.clone())
        .collect();
    assert_eq!(
        deployed,
        ["luna"],
        "not terra (in sync), not sol (not cached)"
    );
    assert_eq!(pending, ["luna"]);

    let d = &shell.deployed[0].operation;
    assert_eq!(d.target.to_string(), "root@192.168.1.11");
    assert_eq!(d.timeout_seconds, 90, "timeouts.diff rides on the request");
    let p = &shell.pending_closures[0].operation;
    assert_eq!(p.store_path.as_str(), path('1', "nixos-system-next"));
    assert_eq!(p.timeout_seconds, 90);
    assert_ne!(d.id, p.id);

    // sol's pending closure is not cached: not_cached, with nothing asked. terra has none.
    assert_eq!(
        shell.row("sol").package_diff.unwrap().state,
        DiffState::NotCached
    );
    assert_eq!(shell.row("terra").package_diff, None);
    // luna's is on its way.
    assert_eq!(shell.row("luna").package_diff, None);
}

#[test]
fn a_failed_lookup_or_no_cache_at_all_is_not_cached_and_asks_nothing() {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Failed);
    answer_cache(&mut shell, "sol", NarCache::NotCached);
    assert!(shell.deployed.is_empty() && shell.pending_closures.is_empty());
    for machine in ["luna", "sol"] {
        let row = shell.row(machine);
        assert_eq!(
            row.package_diff.unwrap().state,
            DiffState::NotCached,
            "{machine}"
        );
        assert_eq!(row.status, Badge::UpdatePending);
    }

    // No binary cache configured: the question is never asked, and the answer is the same.
    let mut config = bare_config();
    config.diff = diff_config().diff;
    let shell = scanned(config);
    assert!(shell.caches.is_empty() && shell.deployed.is_empty());
    assert_eq!(
        shell.row("luna").package_diff.unwrap().state,
        DiffState::NotCached
    );
    assert_eq!(shell.row("terra").package_diff, None);
}

#[test]
fn the_preview_off_asks_for_nothing_and_leaves_the_field_null() {
    let mut shell = scanned(full_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    answer_cache(&mut shell, "sol", NarCache::NotCached);
    assert!(shell.deployed.is_empty() && shell.pending_closures.is_empty());
    for machine in ["terra", "luna", "sol"] {
        assert_eq!(shell.row(machine).package_diff, None, "{machine}");
    }
    assert!(shell.view().refresh.settled);
}

#[test]
fn both_answers_make_the_diff_and_the_report_carries_it() {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    answer_cache(&mut shell, "sol", NarCache::NotCached);
    let before = shell.row("luna");

    let mut deployed = shell.take_deployed("luna");
    shell.resolve(&mut deployed, Ok(old()));
    assert_eq!(
        shell.row("luna").package_diff,
        None,
        "one side alone is not a diff"
    );
    let mut pending = shell.take_pending("luna");
    shell.resolve(&mut pending, Ok(new()));

    let row = shell.row("luna");
    let diff = row.package_diff.clone().expect("both sides are in");
    assert_eq!(diff.state, DiffState::Computed);
    assert_eq!(diff.counts.added, 1, "ripgrep");
    assert_eq!(diff.counts.removed, 1, "htop");
    assert_eq!(diff.counts.changed, 1, "linux");
    assert_eq!(
        (diff.invalid.deployed, diff.invalid.pending),
        (2, 1),
        "the lines the adapters dropped are counted with the ones the domain rejected"
    );
    assert!(
        row.reason
            .as_deref()
            .unwrap()
            .contains("linux 6.12.38 → 6.12.41, +1 −1 ~1"),
        "{:?}",
        row.reason
    );
    // The preview never changes what the status says.
    assert_eq!(row.status, before.status);
    assert_eq!(row.badges, before.badges);
    assert!(row.settled);
    assert!(shell.view().refresh.settled);
    assert!(shell.view().report.settled);
}

#[test]
fn a_failed_side_fails_the_preview_cancels_the_other_and_changes_nothing_else() {
    for failing in ["deployed", "pending"] {
        let mut shell = scanned(diff_config());
        answer_cache(&mut shell, "luna", NarCache::Cached);
        answer_cache(&mut shell, "sol", NarCache::NotCached);
        let before = shell.row("luna");
        let mut deployed = shell.take_deployed("luna");
        let mut pending = shell.take_pending("luna");
        let (deployed_id, pending_id) = (deployed.operation.id, pending.operation.id);

        let line = "deployed closure query timed out after 90s\u{1b}[31m";
        if failing == "deployed" {
            shell.resolve(&mut deployed, Err(line.to_owned()));
        } else {
            shell.resolve(&mut pending, Err(line.to_owned()));
        }
        let other = if failing == "deployed" {
            pending_id
        } else {
            deployed_id
        };
        assert!(
            shell.cancels.contains(&other),
            "{failing}: the other side is cancelled"
        );

        let row = shell.row("luna");
        let diff = row.package_diff.clone().unwrap();
        assert_eq!(diff.state, DiffState::Failed, "{failing}");
        assert_eq!(
            diff.reason.as_deref(),
            Some("deployed closure query timed out after 90s"),
            "{failing}: sanitised"
        );
        assert_eq!(row.status, before.status);
        assert_eq!(row.badges, before.badges);
        assert!(row.settled);
        assert!(
            !shell.view().refresh.timed_out,
            "a failed preview is not a timed-out scan"
        );
        assert!(shell.view().refresh.settled);

        // The other side answers anyway (a shell may resolve before it sees the cancel): the
        // preview stays failed.
        if failing == "deployed" {
            shell.resolve(&mut pending, Ok(new()));
        } else {
            shell.resolve(&mut deployed, Ok(old()));
        }
        assert_eq!(shell.row("luna").package_diff, Some(diff), "{failing}");
    }
}

#[test]
fn a_refresh_cancels_outstanding_closure_queries_and_their_late_answers_are_dropped() {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    answer_cache(&mut shell, "sol", NarCache::NotCached);
    let mut deployed = shell.take_deployed("luna");
    let mut pending = shell.take_pending("luna");
    let ids = [deployed.operation.id, pending.operation.id];

    shell.send(Event::Refresh);
    for id in ids {
        assert!(
            shell.cancels.contains(&id),
            "{id} is cancelled by the refresh"
        );
    }
    // Old answers, resolved late: nothing in the new generation changes.
    let before = shell.view();
    shell.resolve(&mut deployed, Ok(old()));
    shell.resolve(&mut pending, Ok(new()));
    assert_eq!(shell.view(), before);
    assert_eq!(shell.view().report.machines.len(), 0, "the new list is out");

    // And a quit cancels them too.
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    let ids: Vec<_> = shell
        .deployed
        .iter()
        .map(|r| r.operation.id)
        .chain(shell.pending_closures.iter().map(|r| r.operation.id))
        .collect();
    shell.send(Event::Quit);
    for id in ids {
        assert!(shell.cancels.contains(&id), "{id} is cancelled by the quit");
    }
}

#[test]
fn the_fleet_is_not_settled_while_a_preview_is_out_and_the_global_deadline_does_not_cut_it() {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    answer_cache(&mut shell, "sol", NarCache::NotCached);
    let view = shell.view();
    assert!(!view.refresh.settled, "luna's preview is still out");
    assert!(
        !shell.row("luna").settled,
        "its row is not final yet either"
    );
    assert!(shell.row("sol").settled && shell.row("terra").settled);
    assert!(!view.report.settled);
    assert_eq!(view.refresh.diffs_pending, 1);

    // The global deadline passes with the preview out.
    let generation = view.refresh.generation;
    let ids = [
        shell.deployed[0].operation.id,
        shell.pending_closures[0].operation.id,
    ];
    shell.send(Event::DeadlinePassed { generation });
    for id in ids {
        assert!(
            !shell.cancels.contains(&id),
            "{id}: the deadline leaves the preview its own timeouts.diff"
        );
    }
    let view = shell.view();
    assert!(!view.refresh.settled, "still waiting for the preview");
    assert!(
        !view.refresh.timed_out,
        "a preview in flight does not make the scan a timed-out one"
    );

    // It answers: settled, and still not timed out.
    let mut deployed = shell.take_deployed("luna");
    let mut pending = shell.take_pending("luna");
    shell.resolve(&mut deployed, Ok(old()));
    shell.resolve(&mut pending, Ok(new()));
    let view = shell.view();
    assert!(view.refresh.settled);
    assert!(!view.refresh.timed_out);
    assert_eq!(
        view.report.machines[0].package_diff.as_ref().unwrap().state,
        DiffState::Computed
    );
    assert_eq!(view.refresh.diffs_pending, 0);
}

#[test]
fn a_preview_that_times_out_is_failed_and_settles_the_fleet_after_the_deadline() {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    answer_cache(&mut shell, "sol", NarCache::NotCached);
    let generation = shell.view().refresh.generation;
    shell.send(Event::DeadlinePassed { generation });
    assert!(!shell.view().refresh.settled);

    // The shell enforces `timeouts.diff`: it answers the request with a failure.
    let mut deployed = shell.take_deployed("luna");
    shell.resolve(
        &mut deployed,
        Err("deployed closure query timed out after 90s".to_owned()),
    );
    let view = shell.view();
    assert!(view.refresh.settled);
    assert!(!view.refresh.timed_out);
    let luna = &view.report.machines[0];
    assert_eq!(luna.status, Badge::UpdatePending);
    assert_eq!(luna.package_diff.as_ref().unwrap().state, DiffState::Failed);
    assert!(view.report.errors.is_empty(), "never a fleet error");
}

#[test]
fn a_scan_the_deadline_did_cut_short_is_still_timed_out_with_its_previews_unharmed() {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    // sol's cache lookup is still out, and terra... is fine. The deadline passes.
    let generation = shell.view().refresh.generation;
    let cache = shell.take_cache("sol");
    shell.send(Event::DeadlinePassed { generation });
    assert!(
        shell.cancels.contains(&cache.operation.id),
        "the scan's own request is cancelled"
    );
    let view = shell.view();
    assert!(view.refresh.timed_out, "the lookup was cut off");
    assert_eq!(
        shell.row("sol").package_diff.unwrap().state,
        DiffState::NotCached
    );
    assert!(!view.refresh.settled, "luna's preview is still out");
    let mut deployed = shell.take_deployed("luna");
    let mut pending = shell.take_pending("luna");
    shell.resolve(&mut deployed, Ok(old()));
    shell.resolve(&mut pending, Ok(new()));
    assert!(shell.view().refresh.settled);
    assert!(shell.view().refresh.timed_out);
}

#[test]
fn no_closure_is_asked_for_a_machine_that_is_not_update_pending() {
    let mut shell = Shell::new();
    shell.start_with(diff_config(), FLEET);
    let mut eval = shell.take_eval();
    let a = path('0', "nixos-system-a");
    // terra is in sync, luna unreachable, sol's evaluation fails.
    shell.probe("terra", observed(&a, ""));
    shell.probe("luna", unreachable("ssh: Connection refused"));
    shell.probe("sol", observed(&a, ""));
    shell.eval_item(&mut eval, "terra", evaluated("terra", &a));
    shell.eval_item(
        &mut eval,
        "luna",
        evaluated("luna", &path('1', "nixos-system-b")),
    );
    shell.eval_item(&mut eval, "sol", eval_failed("sol", &["error: boom"]));
    shell.resolve(&mut eval, EvalAnswer::Finished { error: None });
    assert!(
        shell.caches.is_empty(),
        "no update pending, no cache question"
    );
    assert!(shell.deployed.is_empty() && shell.pending_closures.is_empty());
    for machine in ["terra", "luna", "sol"] {
        assert_eq!(shell.row(machine).package_diff, None, "{machine}");
    }
}

#[test]
fn a_timed_out_probe_never_gets_a_preview() {
    let mut shell = Shell::new();
    shell.start_with(diff_config(), FLEET);
    let generation = shell.view().refresh.generation;
    shell.send(Event::DeadlinePassed { generation });
    assert_eq!(shell.row("luna").reason.as_deref(), Some(PROBE_TIMED_OUT));
    assert_eq!(shell.row("luna").package_diff, None);
    assert!(shell.deployed.is_empty() && shell.pending_closures.is_empty());
}
