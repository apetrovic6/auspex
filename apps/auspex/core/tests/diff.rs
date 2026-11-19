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
            ('0', "nixos-system-now"),
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
            ('1', "nixos-system-next"),
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
fn a_no_from_the_cache_or_no_cache_at_all_is_not_cached_and_a_failed_lookup_is_failed() {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Failed);
    answer_cache(&mut shell, "sol", NarCache::NotCached);
    assert!(shell.deployed.is_empty() && shell.pending_closures.is_empty());
    // The cache said no: not cached.
    let sol = shell.row("sol");
    assert_eq!(sol.package_diff.unwrap().state, DiffState::NotCached);
    assert_eq!(sol.status, Badge::UpdatePending);
    // The lookup failed: nobody knows, and nobody says the path is missing.
    let luna = shell.row("luna");
    let diff = luna.package_diff.unwrap();
    assert_eq!(diff.state, DiffState::Failed);
    assert_eq!(
        diff.reason.as_deref(),
        Some(auspex_core::CACHE_LOOKUP_FAILED)
    );
    assert_eq!(luna.status, Badge::UpdatePending);
    assert!(shell.view().binary_cache);

    // No binary cache configured: the question is never asked, and the answer is the same.
    let mut config = bare_config();
    config.diff = diff_config().diff;
    let shell = scanned(config);
    assert!(shell.caches.is_empty() && shell.deployed.is_empty());
    assert!(
        !shell.view().binary_cache,
        "a shell can say there is no cache to ask"
    );
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
        DiffState::Failed,
        "a lookup the deadline cut is unknown, not 'not cached'"
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

#[test]
fn the_cores_own_backstop_fails_every_outstanding_preview_without_the_shell() {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    answer_cache(&mut shell, "sol", NarCache::Cached);
    let generation = shell.view().refresh.generation;
    let ids: Vec<_> = shell
        .deployed
        .iter()
        .map(|r| r.operation.id)
        .chain(shell.pending_closures.iter().map(|r| r.operation.id))
        .collect();
    assert_eq!(ids.len(), 4);
    shell.send(Event::DeadlinePassed { generation });
    assert!(!shell.view().refresh.settled, "the previews are still out");

    // A stale generation changes nothing.
    let before = shell.view();
    shell.send(Event::PreviewDeadlinePassed {
        generation: auspex_core::Generation(generation.0 + 1),
    });
    assert_eq!(shell.view(), before);

    // The backstop: nothing answered, and the fleet settles anyway.
    // Every query had its slot and nothing answered.
    for id in &ids {
        shell.send(Event::PreviewStarted {
            generation,
            id: *id,
        });
    }
    shell.send(Event::PreviewDeadlinePassed { generation });
    for id in ids {
        assert!(
            shell.cancels.contains(&id),
            "{id} is cancelled by the backstop"
        );
    }
    let view = shell.view();
    assert!(view.refresh.settled);
    assert!(
        !view.refresh.timed_out,
        "a preview is never a timed-out scan"
    );
    assert_eq!(view.refresh.diffs_pending, 0);
    for machine in ["luna", "sol"] {
        let row = shell.row(machine);
        let diff = row.package_diff.unwrap();
        assert_eq!(diff.state, DiffState::Failed, "{machine}");
        assert_eq!(diff.reason.as_deref(), Some(auspex_core::PREVIEW_TIMED_OUT));
        assert_eq!(row.status, Badge::UpdatePending);
        assert!(row.settled);
    }
    assert_eq!(shell.view().report.errors, []);
}

#[test]
fn a_failed_side_cancels_only_its_own_machines_other_side() {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    answer_cache(&mut shell, "sol", NarCache::Cached);
    let mut luna = shell.take_deployed("luna");
    let sol_ids = [
        shell
            .deployed
            .iter()
            .find(|r| r.operation.machine == "sol")
            .unwrap()
            .operation
            .id,
        shell
            .pending_closures
            .iter()
            .find(|r| r.operation.machine == "sol")
            .unwrap()
            .operation
            .id,
    ];
    shell.resolve(&mut luna, Err("boom".to_owned()));
    for id in sol_ids {
        assert!(
            !shell.cancels.contains(&id),
            "{id}: sol's queries are not luna's to cancel"
        );
    }
    assert_eq!(
        shell.row("sol").package_diff,
        None,
        "sol's preview is still collecting"
    );
    assert!(!shell.view().refresh.settled);
}

/// luna's two closures, resolved: `deployed` and `pending` replace the usual ones.
fn preview_of(deployed: ClosurePaths, pending: ClosurePaths) -> auspex_domain::diff::PackageDiff {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    let before = shell.row("luna");
    let mut d = shell.take_deployed("luna");
    let mut p = shell.take_pending("luna");
    shell.resolve(&mut d, Ok(deployed));
    shell.resolve(&mut p, Ok(pending));
    let row = shell.row("luna");
    assert_eq!(row.status, before.status, "a mismatch changes no status");
    assert_eq!(row.badges, before.badges);
    row.package_diff.unwrap()
}

#[test]
fn a_closure_of_another_system_than_the_scan_saw_is_failed_not_shown() {
    // The machine switched between the probe and the query: its toplevel is not the probe's.
    let moved = closure(&[('9', "nixos-system-switched"), ('c', "zlib-1.3.1")], 0);
    let diff = preview_of(moved, new());
    assert_eq!(diff.state, DiffState::Failed);
    assert_eq!(diff.reason.as_deref(), Some(auspex_core::SYSTEM_CHANGED));

    // No system toplevel in the deployed closure at all: it does not hold the probe's.
    let bare = closure(&[('c', "zlib-1.3.1")], 0);
    let diff = preview_of(bare, new());
    assert_eq!(diff.reason.as_deref(), Some(auspex_core::SYSTEM_CHANGED));

    // The pending side is not the evaluated toplevel.
    let other = closure(&[('8', "nixos-system-elsewhere"), ('c', "zlib-1.3.1")], 0);
    let diff = preview_of(old(), other);
    assert_eq!(diff.state, DiffState::Failed);
    assert_eq!(
        diff.reason.as_deref(),
        Some(auspex_core::PENDING_SYSTEM_MISMATCH)
    );

    // And the matching pair is a computed diff, which is what the other tests use.
    assert_eq!(preview_of(old(), new()).state, DiffState::Computed);
}

/// Re-review N1: a host closure routinely holds other system toplevels (a declarative
/// container, a microvm, a specialisation). The check is membership of the system the scan
/// saw, not uniqueness of the toplevel, so these computed before and must still.
#[test]
fn a_closure_with_a_second_system_toplevel_is_computed() {
    let with_container = |system: (char, &str), container: (char, &str)| {
        closure(
            &[
                system,
                container,
                ('a', "linux-6.12.38"),
                ('b', "htop-3.3.0"),
                ('c', "zlib-1.3.1"),
            ],
            0,
        )
    };
    let deployed = with_container(
        ('0', "nixos-system-now"),
        ('7', "nixos-system-webcontainer-25.05"),
    );
    let pending = closure(
        &[
            ('1', "nixos-system-next"),
            ('6', "nixos-system-webcontainer-25.11"),
            ('d', "linux-6.12.41"),
            ('c', "zlib-1.3.1"),
        ],
        0,
    );
    let diff = preview_of(deployed.clone(), pending.clone());
    assert_eq!(diff.state, DiffState::Computed, "{diff:?}");
    // The toplevels that were checked are the scan's, not "ambiguous" nulls.
    assert_eq!(
        diff.deployed.as_deref(),
        Some(path('0', "nixos-system-now").as_str())
    );
    assert_eq!(
        diff.pending.as_deref(),
        Some(path('1', "nixos-system-next").as_str())
    );
    assert_eq!(diff.counts.changed, 1);

    // The container on one side only, and the probe's system still absent: a real mismatch.
    let switched = with_container(
        ('9', "nixos-system-switched"),
        ('7', "nixos-system-webcontainer-25.05"),
    );
    assert_eq!(
        preview_of(switched, pending).reason.as_deref(),
        Some(auspex_core::SYSTEM_CHANGED)
    );
    let elsewhere = closure(
        &[
            ('8', "nixos-system-elsewhere"),
            ('6', "nixos-system-webcontainer-25.11"),
        ],
        0,
    );
    assert_eq!(
        preview_of(deployed, elsewhere).reason.as_deref(),
        Some(auspex_core::PENDING_SYSTEM_MISMATCH)
    );
}

/// Re-review N2: a preview that never got a query slot says so, not "timed out".
#[test]
fn the_backstop_says_no_slot_for_a_preview_that_never_started() {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    answer_cache(&mut shell, "sol", NarCache::Cached);
    let generation = shell.view().refresh.generation;
    // luna's deployed query got a slot; sol's two are still queued.
    let luna_deployed = shell
        .deployed
        .iter()
        .find(|r| r.operation.machine == "luna")
        .unwrap()
        .operation
        .id;
    // A stale or unknown start changes nothing.
    shell.send(Event::PreviewStarted {
        generation: auspex_core::Generation(generation.0 + 1),
        id: luna_deployed,
    });
    shell.send(Event::PreviewStarted {
        generation,
        id: luna_deployed,
    });
    shell.send(Event::PreviewDeadlinePassed { generation });
    assert_eq!(
        shell.row("luna").package_diff.unwrap().reason.as_deref(),
        Some(auspex_core::PREVIEW_TIMED_OUT)
    );
    assert_eq!(
        shell.row("sol").package_diff.unwrap().reason.as_deref(),
        Some(auspex_core::PREVIEW_NO_SLOT)
    );
}

/// Re-review N5: a refresh cancels the backstop's work: the old generation's backstop does
/// nothing to the new generation's previews.
#[test]
fn an_old_generations_backstop_leaves_the_new_generations_previews_alone() {
    let mut shell = scanned(diff_config());
    answer_cache(&mut shell, "luna", NarCache::Cached);
    let old_generation = shell.view().refresh.generation;
    shell.send(Event::Refresh);
    shell.drop_cancelled();
    // The new refresh lists its machines and gets as far as a preview.
    let mut list = shell.take_inventory();
    shell.resolve(&mut list, Ok(inventory(THREE)));
    let before = shell.view();
    shell.send(Event::PreviewDeadlinePassed {
        generation: old_generation,
    });
    assert_eq!(shell.view(), before);
    assert!(shell.cancels.len() >= 2, "only the refresh's own cancels");
    let cancels = shell.cancels.len();
    shell.send(Event::PreviewDeadlinePassed {
        generation: old_generation,
    });
    assert_eq!(
        shell.cancels.len(),
        cancels,
        "and the stale backstop cancels nothing"
    );
}
