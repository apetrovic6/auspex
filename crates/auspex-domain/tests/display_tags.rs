//! Clan's implicit tags (`all` and the machine's class) are dropped from the display list and
//! kept in the data: `tags` and `--json` carry the inventory's full list.

use auspex_domain::fleet::{Machine, MachineClass, resolve_target};
use auspex_domain::observe::Health;
use auspex_domain::report::{MachineReport, machine_report};
use auspex_domain::status::MachineFacts;

fn report(class: MachineClass, tags: &[&str]) -> MachineReport {
    let machine = Machine {
        name: "m".to_owned(),
        class,
        tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        target_host: None,
        build_host: None,
    };
    machine_report(
        &machine,
        &resolve_target(&machine, None),
        &MachineFacts::default(),
        Health::default(),
        &[],
    )
}

fn shown(r: &MachineReport) -> Vec<&str> {
    r.display_tags().collect()
}

#[test]
fn all_is_hidden() {
    let r = report(MachineClass::Nixos, &["all", "k8s"]);
    assert_eq!(shown(&r), ["k8s"]);
}

#[test]
fn the_machines_own_class_is_hidden() {
    assert_eq!(
        shown(&report(MachineClass::Nixos, &["nixos", "k8s"])),
        ["k8s"]
    );
    assert_eq!(
        shown(&report(MachineClass::Darwin, &["darwin", "ws"])),
        ["ws"]
    );
}

#[test]
fn the_other_class_is_not_the_machines_own_and_stays() {
    assert_eq!(shown(&report(MachineClass::Nixos, &["darwin"])), ["darwin"]);
}

#[test]
fn only_implicit_tags_leaves_nothing() {
    assert_eq!(
        shown(&report(MachineClass::Nixos, &["all", "nixos"])),
        Vec::<&str>::new()
    );
}

#[test]
fn the_full_list_survives_in_the_data_and_in_json() {
    let r = report(MachineClass::Nixos, &["all", "nixos", "k8s"]);
    assert_eq!(r.tags, ["all", "nixos", "k8s"]);
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(json["tags"], serde_json::json!(["all", "nixos", "k8s"]));
}
