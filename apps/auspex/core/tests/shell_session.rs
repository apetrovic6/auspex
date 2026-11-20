//! `s`: the operator's own ssh session, decided by the core alone: a hand-driven shell, no
//! process, no terminal.
//!
//! What the core owes before it asks the shell to lend the terminal to ssh: act mode is on (a
//! headless run cannot be), the selected machine has a usable target and it is *that* target
//! that goes out, and only one session is open at a time. And after: the status bar says how
//! ssh ended. The terminal itself is the TUI's business (`tests/handoff.rs` there).

mod support;

use auspex_core::effects::{ShellEnd, ShellRequest};
use auspex_core::{Config, Event, Mode};
use auspex_domain::fleet::Target;
use crux_core::Request;
use support::{Shell, THREE, observed, unreachable};

fn config() -> Config {
    Config {
        flake: "/fleet".to_owned(),
        allow_act: true,
        local_system: "x86_64-linux".to_owned(),
        ..Config::default()
    }
}

/// luna, sol and terra listed (no probe or evaluation answered: `s` needs neither), luna
/// selected.
fn listed(config: Config, json: &str) -> Shell {
    let mut s = Shell::new();
    s.start_with(config, json);
    s
}

fn act_on(s: &mut Shell) {
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    assert_eq!(s.view().mode, Mode::Act);
}

fn notice(s: &Shell) -> String {
    s.view().notice.expect("a notice")
}

fn take(s: &mut Shell) -> Request<ShellRequest> {
    assert_eq!(s.shells.len(), 1, "one ssh request outstanding");
    s.shells.remove(0)
}

#[test]
fn in_observe_mode_s_says_act_mode_is_off_and_asks_for_nothing() {
    let mut s = listed(config(), THREE);
    assert_eq!(s.view().mode, Mode::Observe);
    s.send(Event::ShellSelected);
    assert_eq!(notice(&s), "act mode is off (A)");
    assert!(s.shells.is_empty(), "no request");
    assert!(s.view().confirm.is_none(), "no question");
}

#[test]
fn a_headless_core_never_opens_ssh() {
    let mut s = listed(
        Config {
            allow_act: false,
            ..config()
        },
        THREE,
    );
    // Act mode cannot be entered, so there is no way round the refusal.
    s.send(Event::ToggleAct);
    s.send(Event::ConfirmAnswer(true));
    s.send(Event::ShellSelected);
    assert!(notice(&s).contains("not available"), "{}", notice(&s));
    assert!(s.shells.is_empty());
}

#[test]
fn s_asks_for_the_selected_machines_resolved_target() {
    let mut s = listed(config(), THREE);
    act_on(&mut s);
    s.send(Event::ShellSelected);
    let request = take(&mut s);
    assert_eq!(request.operation.machine, "luna");
    assert_eq!(
        request.operation.target,
        Target::parse("root@192.168.1.11").unwrap()
    );

    // Another row, another target; and the config's override wins over the inventory's, as
    // for every other use of the target.
    let mut s = listed(config(), THREE);
    act_on(&mut s);
    s.send(Event::MoveDown(1));
    s.send(Event::ShellSelected);
    let request = take(&mut s);
    assert_eq!(request.operation.machine, "sol");
    assert_eq!(
        request.operation.target,
        Target::parse("root@192.168.1.12").unwrap()
    );

    let mut s = Shell::new();
    s.start_with(
        Config {
            target_overrides: [("luna".to_owned(), "admin@10.9.9.9:2200".to_owned())].into(),
            ..config()
        },
        THREE,
    );
    act_on(&mut s);
    s.send(Event::ShellSelected);
    assert_eq!(
        take(&mut s).operation.target,
        Target::parse("admin@10.9.9.9:2200").unwrap()
    );
}

#[test]
fn a_machine_with_no_usable_target_is_refused_and_not_fallen_back_from() {
    // The inventory's target does not parse; the machine name would, as a hostname. Falling
    // back to it would log in somewhere the configuration did not say.
    let mut s = listed(
        config(),
        r#"{"luna": {"deploy": {"targetHost": "-oProxyCommand=sh"}}}"#,
    );
    act_on(&mut s);
    s.send(Event::ShellSelected);
    assert!(
        notice(&s).contains("luna has no usable ssh target"),
        "{}",
        notice(&s)
    );
    assert!(s.shells.is_empty(), "no request");
}

#[test]
fn with_no_machine_selected_s_says_so() {
    let mut s = listed(config(), "{}");
    act_on(&mut s);
    s.send(Event::ShellSelected);
    assert_eq!(notice(&s), "no machine selected");
    assert!(s.shells.is_empty());
}

#[test]
fn s_does_not_care_how_healthy_the_machine_is() {
    // ssh is for the machine that is broken as much as for the one that is fine.
    let mut s = listed(config(), THREE);
    s.probe("luna", unreachable("No route to host"));
    s.probe("sol", observed("/nix/store/aaaa", ""));
    act_on(&mut s);
    s.send(Event::ShellSelected);
    assert_eq!(take(&mut s).operation.machine, "luna");
}

#[test]
fn only_one_session_is_open_at_a_time() {
    let mut s = listed(config(), THREE);
    act_on(&mut s);
    s.send(Event::ShellSelected);
    let mut first = take(&mut s);
    s.send(Event::ShellSelected);
    assert_eq!(notice(&s), "ssh to luna is already open");
    assert!(s.shells.is_empty(), "no second request");
    // Once it has ended, the next one is allowed.
    s.resolve(&mut first, ShellEnd::Exited { code: 0 });
    s.send(Event::ShellSelected);
    assert_eq!(take(&mut s).operation.machine, "luna");
}

#[test]
fn how_ssh_ended_is_said_in_the_status_bar() {
    let cases = [
        (ShellEnd::Exited { code: 0 }, "ssh to luna exited (code 0)"),
        (
            ShellEnd::Exited { code: 255 },
            "ssh to luna exited (code 255)",
        ),
        (
            ShellEnd::Signaled { signal: 9 },
            "ssh to luna exited (killed by signal 9)",
        ),
        (
            ShellEnd::NotStarted {
                reason: "ssh not found on PATH".to_owned(),
            },
            "ssh to luna did not start: ssh not found on PATH",
        ),
    ];
    for (end, says) in cases {
        let mut s = listed(config(), THREE);
        act_on(&mut s);
        s.send(Event::ShellSelected);
        let mut request = take(&mut s);
        assert_eq!(s.view().notice, None, "nothing to say while it runs");
        s.resolve(&mut request, end);
        assert_eq!(notice(&s), says);
    }
}

#[test]
fn an_answer_for_a_session_that_is_not_open_changes_nothing() {
    let mut s = listed(config(), THREE);
    act_on(&mut s);
    s.send(Event::ShellSelected);
    let mut request = take(&mut s);
    s.resolve(&mut request, ShellEnd::Exited { code: 0 });
    let said = notice(&s);
    // A second answer for the same request (or one for an id nobody asked) is not news.
    s.send(Event::ShellEnded {
        id: auspex_core::RequestId(9999),
        end: ShellEnd::Exited { code: 1 },
    });
    assert_eq!(notice(&s), said);
}

#[test]
fn a_termination_signal_while_ssh_is_open_quits() {
    // The shell ends ssh itself; the core only has to not wait for it.
    let mut s = listed(config(), THREE);
    act_on(&mut s);
    s.send(Event::ShellSelected);
    let _request = take(&mut s);
    s.send(Event::Terminate);
    assert!(s.view().quit);
}

#[test]
fn an_answer_for_another_request_while_a_session_is_open_does_not_close_it() {
    // The id is what ties an answer to the session: a stray one (a stale request, a bug) must
    // not end the session the operator is in, or the next `s` would open a second one.
    let mut s = listed(config(), THREE);
    act_on(&mut s);
    s.send(Event::ShellSelected);
    let _open = take(&mut s);
    s.send(Event::ShellEnded {
        id: auspex_core::RequestId(9999),
        end: ShellEnd::Exited { code: 1 },
    });
    assert_eq!(s.view().notice, None, "the stray answer says nothing");
    s.send(Event::ShellSelected);
    assert_eq!(notice(&s), "ssh to luna is already open");
    assert!(s.shells.is_empty(), "and no second request was made");
}

#[test]
fn s_refuses_a_machine_name_the_domain_does_not_allow_even_with_a_good_target() {
    // The inventory parser would have refused these names; the core does not rely on that, as
    // for `u`. The name is what the status bar shows and what the core is asked about.
    use auspex_domain::fleet::{Machine, MachineClass};
    let bad = ["-h", "a b", "x;y", "a/b", ""];
    let mut s = Shell::new();
    s.send(Event::Start(config()));
    let mut list = s.take_inventory();
    let machines = bad
        .iter()
        .map(|name| Machine {
            name: (*name).to_owned(),
            class: MachineClass::Nixos,
            tags: Vec::new(),
            target_host: Some("root@192.168.1.50".to_owned()),
            build_host: None,
        })
        .collect();
    s.resolve(&mut list, Ok(machines));
    act_on(&mut s);
    s.send(Event::MoveTop);
    for _ in bad {
        s.send(Event::ShellSelected);
        assert!(
            notice(&s).contains("not a plain machine name"),
            "{}",
            notice(&s)
        );
        assert!(s.shells.is_empty(), "no request");
        s.send(Event::MoveDown(1));
    }
}
