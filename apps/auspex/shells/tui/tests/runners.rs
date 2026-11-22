//! The runners view in the shell: the tab and the status line at 40 and 120 columns, the key that
//! opens it (and that it can do nothing else), the `--json` shape, the log's notes, the config's
//! wiring to the adapters (a token per forge, sent as a header, printed nowhere), and the
//! acceptance run: every runner of two forges listed with online and busy, and a stopped unit
//! turning its row red within one refresh.
//!
//! The frames are text files in `tests/fixtures/snapshots/`; `AUSPEX_UPDATE_SNAPSHOTS=1`
//! rewrites them. Nothing here leaves the loopback interface and no name in it is real:
//! `forge.example.org`, `alpha`, `beta`.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use auspex_core::Event;
use auspex_domain::observe::{RunnerUnit, UnitActive};
use auspex_domain::report::FleetReport;
use auspex_domain::runners::{
    ForgeAnswer, ForgeRunner, ForgeSlot, HostCaps, MachineFacts, RunnersConfig, Scope, report,
};
use auspex_tui::backend::{Backend, Real};
use auspex_tui::config::{FlakeSource, RunnerForgeSettings, RunnersSettings, Settings};
use auspex_tui::keymap::{Key, Keymap};
use auspex_tui::log::Log;
use auspex_tui::shell::Shell;
use auspex_tui::ui::Ui;
use common::{auspex, expect_file, press, render, run_within, settled_demo, tempdir, tools};
use crossterm::event::KeyCode;

const TOKEN_A: &str = "tkn-runners-a-5d3f19aa";
const TOKEN_B: &str = "tkn-runners-b-77c01e42";
const GIB: u64 = 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------

fn runner(name: &str, status: &str) -> ForgeRunner {
    ForgeRunner::new(
        name,
        status,
        &["docker".to_owned(), "linux".to_owned()],
        Some("v9.0.0"),
    )
}

const fn answered(runners: Vec<ForgeRunner>, queued: Result<u64, String>) -> ForgeAnswer {
    ForgeAnswer {
        runners: Ok(runners),
        queued,
    }
}

fn slot(forge: &str, scope: &str, answer: Option<ForgeAnswer>) -> ForgeSlot {
    ForgeSlot {
        forge: forge.to_owned(),
        scope: scope.to_owned(),
        site: forge.to_owned(),
        answer,
    }
}

fn unit(name: &str, active: UnitActive, sub: &str) -> RunnerUnit {
    RunnerUnit {
        unit: name.to_owned(),
        active,
        sub: sub.to_owned(),
    }
}

/// What the status bar says in the briefed example: 7 of 8 online, 2 busy, 3 queued.
fn calm() -> Vec<ForgeSlot> {
    vec![
        slot(
            "forge.example.org",
            "admin",
            Some(answered(
                vec![
                    runner("alpha", "idle"),
                    runner("alpha-docker", "active"),
                    runner("beta", "idle"),
                    runner("beta-nix", "active"),
                ],
                Ok(3),
            )),
        ),
        slot(
            "ci.example.org",
            "org:acme",
            Some(answered(
                vec![
                    runner("gamma", "idle"),
                    runner("gamma-2", "idle"),
                    runner("delta", "idle"),
                    runner("delta-old", "offline"),
                ],
                Ok(0),
            )),
        ),
    ]
}

/// A forge that is refused, a queue nobody can read, a unit that is down, a unit that is up
/// while the forge says offline, and memory near the cap.
fn trouble() -> (Vec<ForgeSlot>, Vec<RunnerUnit>, Vec<RunnerUnit>) {
    let slots = vec![
        slot(
            "forge.example.org",
            "admin",
            Some(ForgeAnswer {
                runners: Ok(vec![
                    runner("alpha", "idle"),
                    runner("beta", "offline"),
                    runner("gamma", "active"),
                ]),
                queued: Err("forge: the forge refused the token (HTTP 403)".to_owned()),
            }),
        ),
        slot(
            "ci.example.org",
            "repo:acme/fleet",
            Some(ForgeAnswer {
                runners: Err("forge: the forge refused the token (HTTP 403)".to_owned()),
                queued: Err("forge: the forge refused the token (HTTP 403)".to_owned()),
            }),
        ),
        slot("asking.example.org", "user", None),
    ];
    let alpha = vec![unit(
        "gitea-runner-alpha.service",
        UnitActive::Failed,
        "failed",
    )];
    let beta = vec![unit(
        "gitea-runner-beta.service",
        UnitActive::Active,
        "running",
    )];
    (slots, alpha, beta)
}

fn with_report(
    base: &auspex_core::ViewModel,
    slots: &[ForgeSlot],
    machines: &[MachineFacts<'_>],
    config: &RunnersConfig,
) -> auspex_core::ViewModel {
    let r = report(slots, machines, config);
    let mut view = base.clone();
    view.report.runners = r.rows;
    view.report.runner_forges = r.forges;
    view
}

async fn calm_view() -> auspex_core::ViewModel {
    let shell = settled_demo().await;
    with_report(&shell.view(), &calm(), &[], &RunnersConfig::default())
}

async fn trouble_view() -> auspex_core::ViewModel {
    let shell = settled_demo().await;
    let (slots, alpha, beta) = trouble();
    let config = RunnersConfig {
        hosts: BTreeMap::from([(
            "alpha".to_owned(),
            HostCaps {
                cpus: Some(2),
                memory_bytes: Some(4 * GIB),
            },
        )]),
        ..RunnersConfig::default()
    };
    let machines = [
        MachineFacts {
            name: "alpha",
            units: Some(&alpha),
            memory: Some(auspex_domain::observe::Memory {
                total_bytes: 16 * GIB,
                available_bytes: 16 * GIB - 4 * GIB + 200 * 1024 * 1024,
            }),
            load: Some(auspex_domain::observe::Load {
                one: 1.5,
                five: 1.0,
                fifteen: 1.0,
            }),
        },
        MachineFacts {
            name: "beta",
            units: Some(&beta),
            memory: None,
            load: None,
        },
    ];
    with_report(&shell.view(), &slots, &machines, &config)
}

fn tab(view: &auspex_core::ViewModel, width: u16, height: u16) -> String {
    let mut ui = Ui::default();
    ui.input.runners_open = true;
    render(view, &mut ui, &Keymap::defaults(), &[], width, height)
}

fn bar(view: &auspex_core::ViewModel, width: u16) -> String {
    let frame = render(
        view,
        &mut Ui::default(),
        &Keymap::defaults(),
        &[],
        width,
        24,
    );
    format!("{}\n", frame.lines().last().unwrap())
}

// ---------------------------------------------------------------------------------------
// The tab and the status line, at 40 and 120 columns
// ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_tab_at_40_and_120_columns() {
    let calm = calm_view().await;
    let trouble = trouble_view().await;
    let off = settled_demo().await.view();
    let mut checking = off.clone();
    let asking = [slot("forge.example.org", "admin", None)];
    let r = report(&asking, &[], &RunnersConfig::default());
    checking.report.runners = r.rows;
    checking.report.runner_forges = r.forges;
    for (name, view) in [
        ("calm", &calm),
        ("trouble", &trouble),
        ("checking", &checking),
        ("off", &off),
    ] {
        for width in [40, 120] {
            let frame = tab(view, width, 24);
            expect_file(
                "snapshots",
                &format!("runners_tab_{name}_{width}.txt"),
                &frame,
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_status_line_says_it_at_40_and_120_columns() {
    let calm = calm_view().await;
    let trouble = trouble_view().await;
    let off = settled_demo().await.view();
    for (name, view) in [("calm", &calm), ("trouble", &trouble), ("off", &off)] {
        for width in [40, 120] {
            expect_file(
                "snapshots",
                &format!("runners_bar_{name}_{width}.txt"),
                &bar(view, width),
            );
        }
    }
    // The briefed words, whole, where there is room.
    assert!(
        bar(&calm, 140).contains("runners: 7/8 online, 2 busy, 3 queued"),
        "{}",
        bar(&calm, 140)
    );
    // With nothing configured the bar says nothing of runners.
    assert!(!bar(&off, 140).contains("runners"));
}

#[tokio::test(start_paused = true)]
async fn the_tab_names_every_badge_and_what_the_api_does_not_say() {
    let trouble = trouble_view().await;
    // The lines at the width of a 40- and of a 120-column pane, read as one text: wrapping may
    // split a phrase over two lines, and it must lose no word.
    for width in [36, 116] {
        let text: String = auspex_tui::runners::lines(&trouble.report, width)
            .into_iter()
            .map(|(_, l)| l)
            .collect::<Vec<_>>()
            .join(" ");
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for word in [
            "unit down",
            "offline",
            "unit up",
            "memory",
            "refused the token",
            "queue unknown",
            "asking",
            "busy",
        ] {
            assert!(
                text.contains(word),
                "{width}: {word:?} missing from\n{text}"
            );
        }
    }
    // The notes are at the end of the list: a taller pane has them.
    let tall = tab(&trouble, 120, 40);
    assert!(
        tall.contains("the forge does not say which job a runner runs"),
        "{tall}"
    );
    assert!(tall.contains("the API gives no time"), "{tall}");
}

// ---------------------------------------------------------------------------------------
// The key
// ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn r_opens_the_tab_scrolls_it_and_q_closes_it() {
    let shell = settled_demo().await;
    let keymap = Keymap::defaults();
    let mut ui = Ui::default();
    press(&shell, &mut ui, &keymap, Key::char('R'));
    assert!(ui.input.runners_open);
    press(&shell, &mut ui, &keymap, Key::char('j'));
    assert_eq!(ui.input.runners_scroll, 1);
    press(&shell, &mut ui, &keymap, Key::char('k'));
    assert_eq!(ui.input.runners_scroll, 0);
    // `q` closes it, and does not quit auspex from under it.
    press(&shell, &mut ui, &keymap, Key::char('q'));
    assert!(!ui.input.runners_open);
    assert!(!shell.view().quit);
    // Esc closes it too.
    press(&shell, &mut ui, &keymap, Key::char('R'));
    press(&shell, &mut ui, &keymap, Key::plain(KeyCode::Esc));
    assert!(!ui.input.runners_open);
    // And R again closes it.
    press(&shell, &mut ui, &keymap, Key::char('R'));
    press(&shell, &mut ui, &keymap, Key::char('R'));
    assert!(!ui.input.runners_open);
}

#[tokio::test(start_paused = true)]
async fn the_tab_is_read_only_no_key_in_it_acts_on_anything() {
    let shell = settled_demo().await;
    let keymap = Keymap::defaults();
    let mut ui = Ui::default();
    let before = shell.view();
    press(&shell, &mut ui, &keymap, Key::char('R'));
    // Refresh, update, update all, build, shell, freeze, act, diff, log, help, search, command:
    // each does nothing while the tab covers the list.
    for c in ['r', 'u', 'U', 'b', 's', 'f', 'A', 'd', 'L', '?', '/', ':'] {
        press(&shell, &mut ui, &keymap, Key::char(c));
        assert!(ui.input.runners_open, "{c} closed or changed the tab");
        assert!(
            !ui.input.diff_open && !ui.input.log_open && ui.input.prompt.is_none(),
            "{c}"
        );
    }
    let after = shell.view();
    assert_eq!(
        after.refresh.generation, before.refresh.generation,
        "no refresh"
    );
    assert_eq!(after.mode, before.mode, "still observing");
    assert!(after.action.is_none() && after.confirm.is_none() && !after.help_open);
}

#[tokio::test(start_paused = true)]
async fn the_key_is_in_the_help_and_can_be_rebound() {
    let help = auspex_tui::ui::help_lines(&Keymap::defaults());
    assert!(
        help.iter().any(|(k, d)| k == "R" && d.contains("runners")),
        "{help:?}"
    );
    let rebound = Keymap::with_overrides(
        &BTreeMap::from([("runners".to_owned(), vec!["ctrl-r".to_owned()])]),
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(
        rebound.keys_for(auspex_tui::keymap::Action::Runners),
        ["ctrl-r"]
    );
}

// ---------------------------------------------------------------------------------------
// `--json`
// ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn json_has_the_runners_and_what_each_forge_gave() {
    let trouble = trouble_view().await;
    let text = auspex_tui::headless::json(&trouble.report);
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(value["machines"].is_array());
    let rows = value["runners"].as_array().unwrap();
    // The same data as the tab: the same rows, the same badges.
    assert_eq!(rows.len(), trouble.report.runners.len());
    let alpha = rows.iter().find(|r| r["name"] == "alpha").unwrap();
    assert_eq!(
        alpha["badges"],
        serde_json::json!(["forge_up_unit_down", "memory_near_cap"])
    );
    assert_eq!(alpha["unit"]["active"], "failed");
    assert_eq!(alpha["memory"]["cap_bytes"], 4 * GIB);
    let beta = rows.iter().find(|r| r["name"] == "beta").unwrap();
    assert_eq!(
        beta["badges"],
        serde_json::json!(["offline", "forge_down_unit_up"])
    );
    let failed = rows
        .iter()
        .find(|r| r["forge"] == "ci.example.org")
        .unwrap();
    assert_eq!(failed["name"], serde_json::Value::Null);
    assert_eq!(failed["badges"], serde_json::json!(["forge_error"]));
    let forges = value["runner_forges"].as_array().unwrap();
    assert_eq!(forges.len(), 3);
    assert_eq!(forges[0]["queued"], serde_json::Value::Null);
    assert!(forges[0]["queue_error"].as_str().unwrap().contains("403"));
    assert_eq!(forges[2]["checking"], true);
    // It reads back as the domain's own type.
    let back: FleetReport = serde_json::from_str(&text).unwrap();
    assert_eq!(back.runners, trouble.report.runners);
}

// ---------------------------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_forge_failure_is_logged_once_and_again_when_it_returns_after_a_success() {
    let shell = settled_demo().await;
    let base = shell.view();
    let failing = |why: &str| {
        let slots = [slot(
            "forge.example.org",
            "admin",
            Some(ForgeAnswer {
                runners: Err(why.to_owned()),
                queued: Err(why.to_owned()),
            }),
        )];
        with_report(&base, &slots, &[], &RunnersConfig::default())
    };
    let fine = with_report(&base, &calm(), &[], &RunnersConfig::default());
    let asking = with_report(
        &base,
        &[slot("forge.example.org", "admin", None)],
        &[],
        &RunnersConfig::default(),
    );
    let lines = || {
        shell
            .log
            .lines()
            .iter()
            .filter(|l| l.starts_with("Runners:"))
            .count()
    };
    let down = failing("forge: no answer within 10000 ms");
    shell.note_runners(&asking);
    assert_eq!(lines(), 0, "being asked is not news");
    shell.note_runners(&down);
    shell.note_runners(&down);
    assert_eq!(lines(), 1, "once per reason");
    assert!(
        shell
            .log
            .lines()
            .iter()
            .any(|l| l == "Runners: forge.example.org: forge: no answer within 10000 ms"),
        "{:?}",
        shell.log.lines()
    );
    shell.note_runners(&failing("forge: the forge answered HTTP 502"));
    assert_eq!(lines(), 2, "another reason is news");
    shell.note_runners(&fine);
    shell.note_runners(&down);
    assert_eq!(lines(), 3, "news again after a success");
}

// ---------------------------------------------------------------------------------------
// The config
// ---------------------------------------------------------------------------------------

#[test]
fn the_binary_refuses_a_plain_http_runners_forge_before_anything_runs() {
    let dir = tempdir();
    let config = dir.path().join("config.toml");
    fs::write(
        &config,
        "[[runners.forge]]\nurl = \"http://forge.example.org\"\n",
    )
    .unwrap();
    let out = run_within(
        auspex(dir.path())
            .args(["--demo", "--json", "--config"])
            .arg(&config),
        Duration::from_secs(60),
    );
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(out.stdout, b"");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("`runners.forge[0].url`") && stderr.contains("https"),
        "{stderr}"
    );
}

#[test]
fn the_demo_has_no_runners_and_says_empty() {
    let dir = tempdir();
    let out = run_within(
        auspex(dir.path()).args(["--demo", "--json"]),
        Duration::from_secs(60),
    );
    let report: FleetReport = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report.runners.is_empty() && report.runner_forges.is_empty());
}

// ---------------------------------------------------------------------------------------
// The wiring and the acceptance run: real adapters, fake ssh, nix, evaluator and two forges
// ---------------------------------------------------------------------------------------

/// A fake forge on 127.0.0.1 that answers `route(path)` and keeps the head of each request.
struct Forge {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

fn forge(route: impl Fn(&str) -> (u16, String) + Send + 'static) -> Option<Forge> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            assert!(
                !common::in_nix_build(),
                "cannot bind 127.0.0.1 inside the Nix build: {e}"
            );
            eprintln!("SKIPPED: cannot bind 127.0.0.1 ({e}); no loopback here");
            return None;
        }
    };
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 65536 {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            let target = head
                .split(' ')
                .nth(1)
                .unwrap_or("")
                .split('?')
                .next()
                .unwrap_or("")
                .to_owned();
            log.lock().unwrap().push(head);
            let (status, body) = route(&target);
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    Some(Forge { port, seen })
}

fn runners_json(list: &[(&str, &str)]) -> String {
    serde_json::Value::Array(
        list.iter()
            .enumerate()
            .map(|(i, (name, status))| {
                serde_json::json!({
                    "id": i + 1, "name": name, "status": status,
                    "labels": ["docker"], "version": "v9.0.0",
                })
            })
            .collect(),
    )
    .to_string()
}

/// A forge that lists `list` on its first page (and nothing on the next) and `jobs` waiting jobs.
fn listing(
    list: &'static [(&'static str, &'static str)],
    jobs: usize,
) -> impl Fn(&str) -> (u16, String) + Send {
    let page = Arc::new(Mutex::new(0_usize));
    move |path| {
        if path.ends_with("/jobs") {
            let waiting: Vec<_> = (0..jobs)
                .map(|i| serde_json::json!({"id": i, "name": "job", "status": "waiting"}))
                .collect();
            return (200, serde_json::Value::Array(waiting).to_string());
        }
        let mut n = page.lock().unwrap();
        *n += 1;
        // The first request of each listing is page 1, the second page 2: empty. Two requests
        // per listing, one listing per refresh.
        if *n % 2 == 1 {
            (200, runners_json(list))
        } else {
            (200, "[]".to_owned())
        }
    }
}

const INVENTORY: &str = r#"{"alpha":{"deploy":{"targetHost":"root@192.0.2.10"}},"beta":{"deploy":{"targetHost":"root@192.0.2.11"}}}"#;

fn store(name: &str) -> String {
    format!("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-{name}")
}

/// What the child run is set up with.
struct Scene {
    dir: tempfile::TempDir,
    tools: common::Tools,
}

impl Scene {
    fn new() -> Option<Self> {
        let tools = tools(&["cat", "sleep"])?;
        let dir = tempdir();
        for d in ["run", "fleet", "units", "stdin"] {
            fs::create_dir(dir.path().join(d)).unwrap();
        }
        Some(Self { dir, tools })
    }

    fn token(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, format!("{text}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// What `systemctl list-units` said on `host`, in the probe's own words.
    fn units(&self, host: &str, text: &str) {
        fs::write(self.dir.path().join("units").join(host), text).unwrap();
    }

    /// Runs the child test; its stdout is what it printed.
    fn run(&self, a: &Forge, b: &Forge, tokens: [&Path; 2]) -> String {
        let nix = self.tools.script(
            self.dir.path(),
            "nix",
            &format!("printf '%s\\n' '{INVENTORY}'"),
        );
        let line = |m: &str| {
            format!(
                r#"{{"attr":"{m}","attrPath":["{m}"],"drvPath":"{}.drv","outputs":{{"out":"{}"}}}}"#,
                store(&format!("nixos-system-{m}")),
                store(&format!("nixos-system-{m}"))
            )
        };
        let eval = self.tools.script(
            self.dir.path(),
            "nix-eval-jobs",
            &format!("printf '%s\\n' '{}' '{}'", line("alpha"), line("beta")),
        );
        // The probe's script arrives on stdin: keep it, to see which glob it was given, and
        // answer with the unit lines the test put in `units/<host>`.
        let ssh = self.tools.script(
            self.dir.path(),
            "ssh",
            &format!(
                r#"for a; do case "$prev" in --) dest=$a;; esac; prev=$a; done
host=${{dest#*@}}
@cat@ > "@dir@/stdin/$host"
units=$(@cat@ "@dir@/units/$host" 2>/dev/null)
printf 'auspex_probe=1\nprobe=nixos\ncurrent_system={}\nsystem_state=running\nfailed_units=[]\n'
printf 'runner_units=%s\n' "$units"
printf 'auspex_end=1\n'"#,
                store("nixos-system-alpha"),
            ),
        );
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "runners_wiring_child", "--nocapture"])
            .env("AUSPEX_TEST_RUNNERS_DIR", self.dir.path())
            .env(
                "AUSPEX_TEST_RUNNERS_PORTS",
                format!("{},{}", a.port, b.port),
            )
            .env(
                "AUSPEX_TEST_RUNNERS_TOKENS",
                format!("{},{}", tokens[0].display(), tokens[1].display()),
            )
            .env("AUSPEX_NIX", nix)
            .env("AUSPEX_NIX_EVAL_JOBS", eval)
            .env("AUSPEX_SSH", ssh)
            .env("HOME", self.dir.path())
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env("XDG_RUNTIME_DIR", self.dir.path().join("run"));
        let out = run_within(&mut cmd, Duration::from_secs(90));
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "{stdout}\n{stderr}");
        // Neither secret is in either stream, whatever the child printed: its own log, notes and
        // reports, and the Debug of its settings and adapters.
        for token in [TOKEN_A, TOKEN_B] {
            assert!(
                !stdout.contains(token) && !stderr.contains(token),
                "{stdout}\n{stderr}"
            );
        }
        stdout
    }
}

fn reports(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .split("JSON-BEGIN\n")
        .skip(1)
        .map(|s| {
            let json = s.split("JSON-END").next().unwrap();
            serde_json::from_str(json).unwrap()
        })
        .collect()
}

fn row<'v>(report: &'v serde_json::Value, forge: &str, name: &str) -> &'v serde_json::Value {
    report["runners"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["forge"] == forge && r["name"] == name)
        .unwrap_or_else(|| panic!("no {forge}/{name} in {report}"))
}

const A: &[(&str, &str)] = &[("alpha", "idle"), ("beta", "active")];
const B: &[(&str, &str)] = &[("alpha", "idle"), ("gamma", "offline")];

/// The acceptance run: every runner of two forges is listed with online and busy, the forges'
/// tokens are sent each to its own forge, the probe was given the glob, and a stopped unit turns
/// its row red within one refresh.
#[test]
fn every_runner_of_two_forges_is_listed_and_a_stopped_unit_turns_its_row_red() {
    let Some(scene) = Scene::new() else { return };
    let Some(a) = forge(listing(A, 2)) else {
        return;
    };
    let Some(b) = forge(listing(B, 0)) else {
        return;
    };
    scene.units("192.0.2.10", "gitea-runner-alpha.service active running");
    scene.units("192.0.2.11", "gitea-runner-beta.service active running");
    let (ta, tb) = (
        scene.token("token-a", TOKEN_A, 0o600),
        scene.token("token-b", TOKEN_B, 0o600),
    );
    let out = scene.run(&a, &b, [&ta, &tb]);
    let both = reports(&out);
    assert_eq!(both.len(), 2, "two refreshes: {out}");
    let (first, second) = (&both[0], &both[1]);

    // First refresh: both forges, every runner, online and busy as the forges said.
    let rows = first["runners"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "{first}");
    assert_eq!(row(first, "127.0.0.1", "alpha")["status"], "idle");
    assert_eq!(row(first, "127.0.0.1", "beta")["status"], "active");
    assert_eq!(
        row(first, "127.0.0.1 (repo:acme/fleet)", "gamma")["status"],
        "offline"
    );
    assert_eq!(first["runner_forges"][0]["queued"], 2);
    assert_eq!(first["runner_forges"][1]["queued"], 0);
    // Joined with the machines' units: nothing is wrong.
    assert_eq!(row(first, "127.0.0.1", "alpha")["machine"], "alpha");
    assert_eq!(
        row(first, "127.0.0.1", "alpha")["unit"]["unit"],
        "gitea-runner-alpha.service"
    );
    assert_eq!(
        row(first, "127.0.0.1", "alpha")["badges"],
        serde_json::json!([])
    );
    assert_eq!(
        row(first, "127.0.0.1", "beta")["badges"],
        serde_json::json!([])
    );
    assert_eq!(
        row(first, "127.0.0.1 (repo:acme/fleet)", "gamma")["badges"],
        serde_json::json!(["offline"])
    );
    // The runner with no machine is listed, with no unit.
    assert_eq!(
        row(first, "127.0.0.1 (repo:acme/fleet)", "gamma")["machine"],
        serde_json::Value::Null
    );

    // Second refresh, after alpha's unit stopped: the forge still says idle, the row is red.
    let alpha = row(second, "127.0.0.1", "alpha");
    assert_eq!(alpha["status"], "idle");
    assert_eq!(alpha["unit"]["active"], "inactive");
    assert_eq!(alpha["badges"], serde_json::json!(["forge_up_unit_down"]));
    assert_eq!(
        row(second, "127.0.0.1", "beta")["badges"],
        serde_json::json!([])
    );
    assert!(out.contains("RED: 127.0.0.1/alpha"), "{out}");

    // The probe was given the configured glob, and only that.
    let script = fs::read_to_string(scene.dir.path().join("stdin/192.0.2.10")).unwrap();
    assert!(
        script.starts_with("auspex_runner_glob='gitea-runner-*'\n"),
        "{script}"
    );

    // Each forge got its own token, as a header, on every request, and never the other's.
    for (forge, mine, other) in [(&a, TOKEN_A, TOKEN_B), (&b, TOKEN_B, TOKEN_A)] {
        let seen = forge.seen.lock().unwrap().clone();
        assert!(seen.len() >= 6, "three requests, twice: {seen:?}");
        for head in &seen {
            let low = head.to_ascii_lowercase();
            assert!(
                low.contains(&format!("authorization: token {mine}")),
                "{head}"
            );
            assert!(!low.contains(other), "{head}");
        }
    }
}

#[test]
fn a_token_file_others_can_read_fails_that_forge_alone_and_sends_nothing_to_it() {
    let Some(scene) = Scene::new() else { return };
    let Some(a) = forge(listing(A, 0)) else {
        return;
    };
    let Some(b) = forge(listing(B, 0)) else {
        return;
    };
    scene.units("192.0.2.10", "gitea-runner-alpha.service active running");
    scene.units("192.0.2.11", "gitea-runner-beta.service active running");
    let (ta, tb) = (
        scene.token("token-a", TOKEN_A, 0o600),
        scene.token("token-b", TOKEN_B, 0o644),
    );
    let out = scene.run(&a, &b, [&ta, &tb]);
    let first = &reports(&out)[0];
    // The forge with the good token is listed whole.
    assert_eq!(row(first, "127.0.0.1", "alpha")["status"], "idle");
    // The other is one row with its reason, never asked anonymously instead.
    let failed = first["runners"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["forge"] == "127.0.0.1 (repo:acme/fleet)")
        .unwrap();
    assert_eq!(failed["name"], serde_json::Value::Null, "{first}");
    assert_eq!(failed["badges"], serde_json::json!(["forge_error"]));
    let why = failed["error"].as_str().unwrap();
    assert!(
        why.contains("accessible to group or others") && why.contains("chmod 600"),
        "{why}"
    );
    assert!(
        b.seen.lock().unwrap().is_empty(),
        "nothing was sent: {:?}",
        b.seen.lock().unwrap()
    );
    // It is said once at start, and once in the log.
    assert!(
        out.contains("NOTE: the runners of a forge are unknown, and no request is made"),
        "{out}"
    );
    assert_eq!(
        out.matches("LOG: Runners: 127.0.0.1 (repo:acme/fleet): ")
            .count(),
        1,
        "{out}"
    );
}

/// The child of the tests above: the real adapters (`Real`), configured as `config.toml` would
/// configure them but with the loopback `http` URLs a unit test can serve, run to the end of one
/// refresh, then (after the unit file of alpha is rewritten as stopped) of a second. Does nothing
/// unless a parent set the scene up.
#[tokio::test]
async fn runners_wiring_child() {
    let Some(dir) = std::env::var_os("AUSPEX_TEST_RUNNERS_DIR").map(PathBuf::from) else {
        return;
    };
    let ports: Vec<String> = std::env::var("AUSPEX_TEST_RUNNERS_PORTS")
        .unwrap()
        .split(',')
        .map(str::to_owned)
        .collect();
    let tokens: Vec<PathBuf> = std::env::var("AUSPEX_TEST_RUNNERS_TOKENS")
        .unwrap()
        .split(',')
        .map(PathBuf::from)
        .collect();
    let settings = Settings {
        runners: Some(RunnersSettings {
            unit_glob: "gitea-runner-*".to_owned(),
            memory_warn_percent: 90,
            forges: vec![
                RunnerForgeSettings {
                    url: format!("http://127.0.0.1:{}/", ports[0]),
                    name: "127.0.0.1".to_owned(),
                    site: format!("127.0.0.1:{}", ports[0]),
                    scope: Scope::Admin,
                    token_file: Some(tokens[0].clone()),
                },
                RunnerForgeSettings {
                    url: format!("http://127.0.0.1:{}/", ports[1]),
                    name: "127.0.0.1 (repo:acme/fleet)".to_owned(),
                    site: format!("127.0.0.1:{}", ports[1]),
                    scope: Scope::Repo {
                        owner: "acme".to_owned(),
                        name: "fleet".to_owned(),
                    },
                    token_file: Some(tokens[1].clone()),
                },
            ],
            hosts: BTreeMap::new(),
            machines: BTreeMap::new(),
        }),
        ..Settings::default()
    };
    let real = Real::new(&settings, FlakeSource::Flag);
    for note in real.notes() {
        println!("NOTE: {note}");
    }
    println!("DEBUG: {settings:?} {real:?}");
    let shell = Shell::new(
        Backend::Real(Box::new(real)),
        Log::memory(),
        Duration::from_secs(30),
    );
    shell.send(Event::Start(auspex_tui::app::core_config(
        &settings,
        dir.join("fleet").display().to_string(),
    )));
    let settle = |shell: &Arc<Shell>| {
        let shell = Arc::clone(shell);
        async move {
            tokio::time::timeout(Duration::from_secs(30), async {
                while !shell.view().refresh.settled {
                    shell.changed().await;
                }
            })
            .await
            .expect("the refresh settles");
        }
    };
    let print = |shell: &Arc<Shell>| {
        let view = shell.view();
        shell.note_runners(&view);
        for r in &view.report.runners {
            if r.is_red() {
                println!(
                    "RED: {}/{}",
                    r.forge,
                    r.name.as_deref().unwrap_or("(forge)")
                );
            }
        }
        println!(
            "JSON-BEGIN\n{}JSON-END",
            auspex_tui::headless::json(&view.report)
        );
    };
    settle(&shell).await;
    print(&shell);
    // alpha's runner unit stops. The forge has not noticed.
    fs::write(
        dir.join("units/192.0.2.10"),
        "gitea-runner-alpha.service inactive dead",
    )
    .unwrap();
    shell.send(Event::Refresh);
    settle(&shell).await;
    print(&shell);
    for line in shell.log.lines() {
        println!("LOG: {line}");
    }
    shell.shutdown().await;
}

// ---------------------------------------------------------------------------------------
// Fix round 1 (review of PR #43)
// ---------------------------------------------------------------------------------------

/// N1: `note_runners` runs with every frame. One forge answering and another failing is a steady
/// state, and the failure is written once, not once a frame.
#[tokio::test(start_paused = true)]
async fn a_failing_forge_beside_a_good_one_is_logged_once_however_often_it_is_drawn() {
    let shell = settled_demo().await;
    let slots = [
        slot(
            "forge.example.org",
            "admin",
            answered(vec![runner("alpha", "idle")], Ok(0)).into(),
        ),
        slot(
            "forge.example.org:8443",
            "admin",
            Some(ForgeAnswer {
                runners: Err("forge: the forge answered HTTP 502".to_owned()),
                queued: Err("forge: the forge answered HTTP 502".to_owned()),
            }),
        ),
    ];
    let view = with_report(&shell.view(), &slots, &[], &RunnersConfig::default());
    for _ in 0..5 {
        shell.note_runners(&view);
    }
    let n = shell
        .log
        .lines()
        .iter()
        .filter(|l| l.starts_with("Runners:"))
        .count();
    assert_eq!(n, 1, "{:?}", shell.log.lines());
}

/// N1: two entries on one host are two headings, each with its own runners.
#[tokio::test(start_paused = true)]
async fn each_forge_entry_shows_only_its_own_runners() {
    let shell = settled_demo().await;
    let slots = [
        slot(
            "forge.example.org",
            "admin",
            answered(vec![runner("alpha", "idle")], Ok(0)).into(),
        ),
        slot(
            "forge.example.org:8443",
            "admin",
            answered(vec![runner("beta", "idle")], Ok(0)).into(),
        ),
    ];
    let view = with_report(&shell.view(), &slots, &[], &RunnersConfig::default());
    let lines: Vec<String> = auspex_tui::runners::lines(&view.report, 116)
        .into_iter()
        .map(|(_, l)| l)
        .collect();
    let at = |needle: &str| lines.iter().position(|l| l.starts_with(needle)).unwrap();
    let (first, second) = (
        at("forge.example.org (admin)"),
        at("forge.example.org:8443"),
    );
    assert!(first < at("alpha") && at("alpha") < second, "{lines:?}");
    assert!(second < at("beta"), "{lines:?}");
}

/// N6, T1: a stopped unit turns the row red in the tab as well as in `--json`: the line is `Red`
/// even when the same row is also near its memory cap (the warning colour must not win).
#[tokio::test(start_paused = true)]
async fn the_tab_draws_a_down_unit_red_even_beside_a_memory_warning() {
    use auspex_tui::runners::Kind;
    let trouble = trouble_view().await;
    let alpha = trouble
        .report
        .runners
        .iter()
        .find(|r| r.name.as_deref() == Some("alpha"))
        .unwrap();
    assert!(alpha.is_red() && alpha.badges.len() == 2, "{alpha:?}");
    for width in [36, 116] {
        let lines = auspex_tui::runners::lines(&trouble.report, width);
        let alpha_line = lines.iter().find(|(_, l)| l.starts_with("alpha")).unwrap();
        assert_eq!(alpha_line.0, Kind::Red, "{width}: {alpha_line:?}");
        // The rest of the row, in a narrow pane, is as red as its first line.
        if width < 100 {
            let n = lines.iter().position(|l| l == alpha_line).unwrap();
            assert_eq!(lines[n + 1].0, Kind::Red, "{:?}", lines[n + 1]);
        }
        // A forge that failed is red; one still being asked and a note are not.
        assert!(
            lines
                .iter()
                .any(|(k, l)| *k == Kind::Red && l.contains("refused"))
        );
        assert!(
            lines
                .iter()
                .any(|(k, l)| *k == Kind::Note && l.starts_with("busy:"))
        );
    }
    // Memory near the cap alone is the warning colour.
    let calm_warn = {
        let shell = settled_demo().await;
        let machines = [MachineFacts {
            name: "alpha",
            units: None,
            memory: Some(auspex_domain::observe::Memory {
                total_bytes: 16 * GIB,
                available_bytes: 16 * GIB - 4 * GIB + 100 * 1024 * 1024,
            }),
            load: None,
        }];
        let config = RunnersConfig {
            hosts: BTreeMap::from([(
                "alpha".to_owned(),
                HostCaps {
                    cpus: None,
                    memory_bytes: Some(4 * GIB),
                },
            )]),
            ..RunnersConfig::default()
        };
        with_report(
            &shell.view(),
            &[slot(
                "forge.example.org",
                "admin",
                answered(vec![runner("alpha", "idle")], Ok(0)).into(),
            )],
            &machines,
            &config,
        )
    };
    let lines = auspex_tui::runners::lines(&calm_warn.report, 116);
    assert_eq!(
        lines
            .iter()
            .find(|(_, l)| l.starts_with("alpha"))
            .unwrap()
            .0,
        Kind::Warn
    );
}

/// N3: the refresh state and the pending count stay on the status bar at 40 columns whether or not
/// runners are configured (they were before `[runners]`); the runners line follows them.
#[tokio::test(start_paused = true)]
async fn the_status_bar_keeps_its_refresh_state_and_pending_count_beside_the_runners() {
    let calm = calm_view().await;
    let off = settled_demo().await.view();
    let narrow = bar(&calm, 40);
    let plain = bar(&off, 40);
    assert!(
        narrow.contains("settled") && narrow.contains("0 pending"),
        "{narrow}"
    );
    // What the bar says without `[runners]` is not pushed off by having it.
    assert!(
        narrow.starts_with("OBSERVE │ settled │ 0 pending")
            && plain.starts_with("OBSERVE │ settled │ 0 pending"),
        "{narrow} / {plain}"
    );
    // With room, all of it, in that order.
    let wide = bar(&calm, 140);
    let at = |w: &str| wide.find(w).unwrap_or_else(|| panic!("{w} in {wide}"));
    assert!(
        at("settled") < at("0 pending") && at("0 pending") < at("runners: 7/8"),
        "{wide}"
    );
}

/// N1: the log's memory of what it already wrote is by entry, not by name: a view whose entries
/// share a name (nothing the config can make now, but a report is a value) still logs once.
#[tokio::test(start_paused = true)]
async fn the_log_remembers_by_entry_and_not_by_name() {
    let shell = settled_demo().await;
    let down = |why: &str| {
        Some(ForgeAnswer {
            runners: Err(why.to_owned()),
            queued: Err(why.to_owned()),
        })
    };
    let slots = [
        slot(
            "same",
            "admin",
            answered(vec![runner("alpha", "idle")], Ok(0)).into(),
        ),
        slot("same", "admin", down("forge: the forge answered HTTP 502")),
    ];
    let view = with_report(&shell.view(), &slots, &[], &RunnersConfig::default());
    for _ in 0..5 {
        shell.note_runners(&view);
    }
    let n = shell
        .log
        .lines()
        .iter()
        .filter(|l| l.starts_with("Runners:"))
        .count();
    assert_eq!(n, 1, "{:?}", shell.log.lines());
}

// ---------------------------------------------------------------------------------------
// Fix round 2 (re-review of PR #43)
// ---------------------------------------------------------------------------------------

/// N-C (R10): the same failure after a good answer is news again. A log that never forgot a
/// reason would stay silent about a forge that went down, came back and went down the same way.
#[tokio::test(start_paused = true)]
async fn the_same_failure_after_a_recovery_is_logged_again() {
    let shell = settled_demo().await;
    let base = shell.view();
    let down = with_report(
        &base,
        &[slot(
            "forge.example.org",
            "admin",
            Some(ForgeAnswer {
                runners: Err("forge: the forge answered HTTP 502".to_owned()),
                queued: Err("forge: the forge answered HTTP 502".to_owned()),
            }),
        )],
        &[],
        &RunnersConfig::default(),
    );
    let up = with_report(&base, &calm(), &[], &RunnersConfig::default());
    let lines = || {
        shell
            .log
            .lines()
            .iter()
            .filter(|l| l.starts_with("Runners:"))
            .count()
    };
    shell.note_runners(&down);
    shell.note_runners(&down);
    assert_eq!(lines(), 1);
    shell.note_runners(&up);
    shell.note_runners(&down);
    assert_eq!(
        lines(),
        2,
        "the same reason, after the forge was well again"
    );
    shell.note_runners(&down);
    assert_eq!(lines(), 2, "and once more is not news");
}
