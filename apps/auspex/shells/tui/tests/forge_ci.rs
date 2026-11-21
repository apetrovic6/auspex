//! The CI badge in the shell: the status bar for each state at 40 and 120 columns, the `--json`
//! shape, the config's wiring to the adapters (token read at start, sent as a header, never
//! printed anywhere), and the binary's own refusals.
//!
//! The frames are text files in `tests/fixtures/snapshots/`; `AUSPEX_UPDATE_SNAPSHOTS=1`
//! rewrites them. Nothing here leaves the loopback interface and no name in it is real:
//! `forge.example.org`, `owner/fleet`.

mod common;

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use auspex_core::Event;
use auspex_domain::ci::{CiState, FleetCi};
use auspex_domain::report::FleetReport;
use auspex_tui::backend::{Backend, Real};
use auspex_tui::config::{FlakeSource, ForgeSettings, Settings};
use auspex_tui::keymap::Keymap;
use auspex_tui::log::Log;
use auspex_tui::shell::Shell;
use auspex_tui::ui::Ui;
use common::{auspex, expect_file, render, run_within, settled_demo, tempdir, tools};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const TOKEN: &str = "tkn-wiring-5d3f19aa";

// ---------------------------------------------------------------------------------------
// The status bar, each state, at 40 and 120 columns
// ---------------------------------------------------------------------------------------

fn badge(state: CiState, commit: Option<&str>, reason: Option<&str>) -> FleetCi {
    FleetCi {
        commit: commit.map(str::to_owned),
        state,
        url: None,
        reason: reason.map(str::to_owned),
    }
}

/// The status bar of the settled demo with `ci` as the badge: the last line of the frame.
async fn bar(ci: FleetCi, checking: bool, width: u16) -> String {
    let shell = settled_demo().await;
    let mut view = shell.view();
    view.report.fleet_ci = ci;
    view.refresh.ci_checking = checking;
    let frame = render(
        &view,
        &mut Ui::default(),
        &Keymap::defaults(),
        &[],
        width,
        24,
    );
    let last = frame.lines().last().unwrap().to_owned();
    format!("{last}\n")
}

#[tokio::test(start_paused = true)]
async fn the_status_bar_says_each_state_at_40_and_120_columns() {
    let cases: [(&str, FleetCi, bool); 10] = [
        ("off", FleetCi::off(), false),
        (
            "checking",
            badge(CiState::Unknown, None, Some("still being checked")),
            true,
        ),
        (
            "checking_commit",
            badge(CiState::Unknown, Some(SHA), Some("still being checked")),
            true,
        ),
        ("success", badge(CiState::Success, Some(SHA), None), false),
        ("failure", badge(CiState::Failure, Some(SHA), None), false),
        ("pending", badge(CiState::Pending, Some(SHA), None), false),
        ("none", badge(CiState::None, Some(SHA), None), false),
        (
            "not_pushed",
            badge(
                CiState::NotPushed,
                Some(SHA),
                Some("HEAD is 2 commits ahead of origin/main"),
            ),
            false,
        ),
        (
            "unknown",
            badge(
                CiState::Unknown,
                Some(SHA),
                Some("forge: no answer within 10000 ms"),
            ),
            false,
        ),
        (
            "unknown_set_repo",
            badge(
                CiState::Unknown,
                Some(SHA),
                Some("the origin remote is on another host than forge.url: set forge.repo"),
            ),
            false,
        ),
    ];
    for (name, ci, checking) in cases {
        for width in [40, 120] {
            let frame = bar(ci.clone(), checking, width).await;
            expect_file("snapshots", &format!("forge_ci_{name}_{width}.txt"), &frame);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_badge_shows_the_short_commit_and_survives_40_columns() {
    for (state, word) in [
        (CiState::Success, "ci ok 01234567"),
        (CiState::Failure, "ci failed 01234567"),
        (CiState::Pending, "ci pending 01234567"),
        (CiState::None, "ci none 01234567"),
        (CiState::NotPushed, "ci not pushed 01234567"),
        (CiState::Unknown, "ci unknown 01234567"),
    ] {
        let wide = bar(badge(state, Some(SHA), None), false, 120).await;
        assert!(wide.contains(word), "{word}: {wide}");
        // The commit is eight characters, not the whole id.
        assert!(!wide.contains("0123456789"), "{wide}");
        let narrow = bar(badge(state, Some(SHA), None), false, 40).await;
        assert!(narrow.contains("ci "), "cut away at 40 columns: {narrow}");
    }
    let off = bar(FleetCi::off(), false, 120).await;
    assert!(!off.contains("ci "), "nothing to say when it is off: {off}");
}

// ---------------------------------------------------------------------------------------
// `--json`
// ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn json_has_a_top_level_fleet_ci_object_in_every_state() {
    let shell = settled_demo().await;
    for (ci, want) in [
        (
            FleetCi::off(),
            serde_json::json!({"commit": null, "state": "off", "url": null, "reason": null}),
        ),
        (
            FleetCi {
                commit: Some(SHA.to_owned()),
                state: CiState::Success,
                url: Some("https://ci.example.org/owner/fleet/actions/runs/1".to_owned()),
                reason: None,
            },
            serde_json::json!({
                "commit": SHA, "state": "success",
                "url": "https://ci.example.org/owner/fleet/actions/runs/1", "reason": null,
            }),
        ),
        (
            FleetCi::unknown(Some(SHA), "forge: the forge refused the token (HTTP 401)"),
            serde_json::json!({
                "commit": SHA, "state": "unknown", "url": null,
                "reason": "forge: the forge refused the token (HTTP 401)",
            }),
        ),
    ] {
        let mut report = shell.view().report;
        report.fleet_ci = ci;
        let text = auspex_tui::headless::json(&report);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["fleet_ci"], want, "{text}");
        // Top level, beside `machines`, and the document still reads as the domain's own type.
        assert!(value["machines"].is_array());
        assert_eq!(
            serde_json::from_str::<FleetReport>(&text).unwrap().fleet_ci,
            report.fleet_ci
        );
    }
}

// ---------------------------------------------------------------------------------------
// The wiring: config -> adapters, with a fake `git` and a fake forge
// ---------------------------------------------------------------------------------------

/// A fake forge on 127.0.0.1: answers every request with `status` and `body`, and keeps the
/// head of each request it got.
struct Forge {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

fn forge(status: u16, body: String) -> Option<Forge> {
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
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&head).into_owned());
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

fn success_body() -> String {
    serde_json::json!({
        "state": "success", "sha": SHA, "total_count": 1,
        "statuses": [{"state": "success", "target_url": "https://ci.example.org/owner/fleet/actions/runs/8"}],
    })
    .to_string()
}

/// What the child run is set up with.
struct Scene {
    dir: tempfile::TempDir,
    tools: common::Tools,
    /// `timeouts.probe` for the child, in seconds, when the default is not wanted.
    probe: Option<u64>,
    /// `git rev-parse` never answers.
    slow_git: bool,
    /// `git rev-parse` fails and its stderr holds the token.
    git_leaks_token: bool,
}

impl Scene {
    fn new() -> Option<Self> {
        let tools = tools(&["cat", "sleep"])?;
        let dir = tempdir();
        fs::create_dir(dir.path().join("run")).unwrap();
        fs::create_dir(dir.path().join("fleet")).unwrap();
        Some(Self {
            dir,
            tools,
            probe: None,
            slow_git: false,
            git_leaks_token: false,
        })
    }

    fn token(&self, mode: u32) -> PathBuf {
        let path = self.dir.path().join("token");
        fs::write(&path, format!("{TOKEN}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// Runs the child test; its stdout is what it printed.
    fn run(&self, forge: &Forge, token_file: Option<&Path>, status: &str) -> String {
        let nix = self.tools.script(
            self.dir.path(),
            "nix",
            "echo 'error: no fleet here' >&2; exit 1",
        );
        let rev_parse = if self.slow_git {
            "@sleep@ 30".to_owned()
        } else if self.git_leaks_token {
            format!("echo 'fatal: bad object {TOKEN}' >&2; exit 128")
        } else {
            format!("printf '{SHA}\\n'; exit 0")
        };
        let git = self.tools.script(
            self.dir.path(),
            "git",
            &format!(
                "case \" $* \" in\n\
                   *\" config \"*) printf 'core.bare\\0'; exit 0;;\n\
                   *\" rev-parse \"*) {rev_parse};;\n\
                   *\" status \"*) printf '{status}'; exit 0;;\n\
                   *\" remote \"*) echo 'a remote must not be read: forge.repo is set' >&2; exit 9;;\n\
                 esac\nexit 9"
            ),
        );
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        if let Some(probe) = self.probe {
            cmd.env("AUSPEX_TEST_FORGE_PROBE", probe.to_string());
        }
        cmd.args(["--exact", "forge_wiring_child", "--nocapture"])
            .env("AUSPEX_TEST_FORGE_DIR", self.dir.path())
            .env("AUSPEX_TEST_FORGE_PORT", forge.port.to_string())
            .env(
                "AUSPEX_TEST_FORGE_TOKEN_FILE",
                token_file.map(Path::as_os_str).unwrap_or_default(),
            )
            .env("AUSPEX_NIX", nix)
            .env("AUSPEX_GIT", git)
            .env("HOME", self.dir.path())
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env("XDG_RUNTIME_DIR", self.dir.path().join("run"));
        let out = run_within(&mut cmd, Duration::from_secs(60));
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "{stdout}\n{stderr}");
        // The secret is in neither stream, whatever the child printed: its own log, notes and
        // `--json`, and the Debug of its settings and adapters.
        assert!(
            !stdout.contains(TOKEN) && !stderr.contains(TOKEN),
            "{stdout}\n{stderr}"
        );
        stdout
    }
}

fn json_of(stdout: &str) -> serde_json::Value {
    let json = stdout
        .split("JSON-BEGIN\n")
        .nth(1)
        .and_then(|s| s.split("JSON-END").next())
        .unwrap_or_else(|| panic!("no JSON in {stdout}"));
    serde_json::from_str(json).unwrap()
}

#[test]
fn the_token_file_is_read_at_start_and_sent_as_a_header_and_nowhere_else() {
    let Some(scene) = Scene::new() else { return };
    let Some(forge) = forge(200, success_body()) else {
        return;
    };
    let token = scene.token(0o600);
    let out = scene.run(
        &forge,
        Some(&token),
        "## main...origin/main\\0 M flake.nix\\0",
    );
    let report = json_of(&out);
    assert_eq!(report["fleet_ci"]["state"], "success", "{out}");
    assert_eq!(report["fleet_ci"]["commit"], SHA);
    assert_eq!(
        report["fleet_ci"]["url"],
        "https://ci.example.org/owner/fleet/actions/runs/8"
    );
    assert_eq!(report["fleet_ci"]["reason"], serde_json::Value::Null);
    // A dirty tree is not a CI state: HEAD's own status is shown.
    let seen = forge.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "one request per refresh: {seen:?}");
    assert!(
        seen[0].starts_with(&format!(
            "GET /api/v1/repos/owner/fleet/commits/{SHA}/status HTTP/1.1"
        )),
        "{}",
        seen[0]
    );
    assert!(
        seen[0].to_ascii_lowercase().contains(&format!(
            "authorization: token {}",
            TOKEN.to_ascii_lowercase()
        )),
        "{}",
        seen[0]
    );
}

#[test]
fn without_a_token_file_the_forge_is_asked_anonymously() {
    let Some(scene) = Scene::new() else { return };
    let Some(forge) = forge(200, success_body()) else {
        return;
    };
    let out = scene.run(&forge, None, "## main...origin/main\\0");
    assert_eq!(json_of(&out)["fleet_ci"]["state"], "success", "{out}");
    let seen = forge.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert!(
        !seen[0].to_ascii_lowercase().contains("authorization"),
        "{}",
        seen[0]
    );
}

#[test]
fn a_token_file_that_others_can_read_is_refused_and_nothing_is_sent() {
    let Some(scene) = Scene::new() else { return };
    let Some(forge) = forge(200, success_body()) else {
        return;
    };
    let token = scene.token(0o644);
    let out = scene.run(&forge, Some(&token), "## main...origin/main\\0");
    let ci = &json_of(&out)["fleet_ci"];
    assert_eq!(ci["state"], "unknown", "{out}");
    let reason = ci["reason"].as_str().unwrap();
    assert!(
        reason.contains("accessible to group or others") && reason.contains("chmod 600"),
        "{reason}"
    );
    // Not asked anonymously instead: that would hide that the token is not in use.
    assert!(
        forge.seen.lock().unwrap().is_empty(),
        "{:?}",
        forge.seen.lock().unwrap()
    );
    // Said once at start, and on the Log tab's lines.
    assert!(
        out.contains("NOTE: the CI badge is unknown, and no request is made"),
        "{out}"
    );
    assert!(
        out.contains("LOG: CI badge: unknown: forge token file"),
        "{out}"
    );
}

/// Fix round 1, finding 3: the three git commands for HEAD are bounded by `timeouts.probe`, not
/// by the git adapter's 2 s default. With `probe = 1` a git that never answers is reported as
/// timed out after 1 s (the default would say 2 s).
#[test]
fn the_git_commands_for_head_are_bounded_by_timeouts_probe() {
    let Some(mut scene) = Scene::new() else {
        return;
    };
    scene.probe = Some(1);
    scene.slow_git = true;
    let Some(forge) = forge(200, success_body()) else {
        return;
    };
    let token = scene.token(0o600);
    let out = scene.run(&forge, Some(&token), "## main...origin/main\\0");
    let ci = &json_of(&out)["fleet_ci"];
    assert_eq!(ci["state"], "unknown", "{out}");
    let reason = ci["reason"].as_str().unwrap();
    assert!(reason.contains("timed out after 1s"), "{reason}");
    assert!(forge.seen.lock().unwrap().is_empty());
}

/// Hardening round, N1: what git says is checked for the token the shell holds, so a repository
/// that makes git print it (the child's `Real` passes the token to `head`) cannot put it in the
/// badge, the log or `--json`.
#[test]
fn git_text_holding_the_token_is_withheld_from_the_badge() {
    let Some(mut scene) = Scene::new() else {
        return;
    };
    scene.git_leaks_token = true;
    let Some(forge) = forge(200, success_body()) else {
        return;
    };
    let token = scene.token(0o600);
    // `run` already fails if the token is in anything the child printed.
    let out = scene.run(&forge, Some(&token), "## main...origin/main\\0");
    let ci = &json_of(&out)["fleet_ci"];
    assert_eq!(ci["state"], "unknown", "{out}");
    assert!(
        ci["reason"].as_str().unwrap().contains("not shown"),
        "{out}"
    );
    assert!(forge.seen.lock().unwrap().is_empty());
}

/// Fix round 1, finding 3: a reason is written to the log once, and again when the same failure
/// returns after a success in between.
#[tokio::test(start_paused = true)]
async fn a_reason_is_logged_again_when_it_returns_after_a_success() {
    let shell = settled_demo().await;
    let base = shell.view();
    let with = |ci: FleetCi| {
        let mut v = base.clone();
        v.report.fleet_ci = ci;
        v
    };
    let failed = with(FleetCi::unknown(
        Some(SHA),
        "forge: no answer within 10000 ms",
    ));
    let fine = with(badge(CiState::Success, Some(SHA), None));
    let lines = || {
        shell
            .log
            .lines()
            .iter()
            .filter(|l| l.starts_with("CI badge:"))
            .count()
    };
    shell.note_ci(&failed);
    shell.note_ci(&failed);
    assert_eq!(lines(), 1, "once per reason");
    shell.note_ci(&fine);
    assert_eq!(lines(), 1);
    shell.note_ci(&failed);
    assert_eq!(lines(), 2, "news again after a success");
}

#[test]
fn a_head_ahead_of_its_upstream_is_not_pushed_and_the_forge_is_not_asked() {
    let Some(scene) = Scene::new() else { return };
    let Some(forge) = forge(200, success_body()) else {
        return;
    };
    let token = scene.token(0o600);
    let out = scene.run(&forge, Some(&token), "## main...origin/main [ahead 2]\\0");
    let ci = &json_of(&out)["fleet_ci"];
    assert_eq!(ci["state"], "not_pushed", "{out}");
    assert_eq!(ci["commit"], SHA);
    assert!(forge.seen.lock().unwrap().is_empty());
}

#[test]
fn a_forge_that_answers_401_is_unknown_with_the_reason_on_the_log() {
    let Some(scene) = Scene::new() else { return };
    // A forge that echoes the credential it was sent back in its error.
    let Some(forge) = forge(
        401,
        format!(r#"{{"message":"bad credentials: token {TOKEN}"}}"#),
    ) else {
        return;
    };
    let token = scene.token(0o600);
    let out = scene.run(&forge, Some(&token), "## main...origin/main\\0");
    let ci = &json_of(&out)["fleet_ci"];
    assert_eq!(ci["state"], "unknown", "{out}");
    assert!(
        ci["reason"].as_str().unwrap().contains("refused the token"),
        "{out}"
    );
    assert!(
        out.contains("LOG: CI badge: unknown: forge: the forge refused the token"),
        "{out}"
    );
}

/// The child of the tests above: the real adapters (`Real`), configured as `config.toml` would
/// configure them but with the loopback `http` URL a unit test can serve, run to the end of one
/// refresh. Does nothing unless a parent set the scene up.
#[tokio::test]
async fn forge_wiring_child() {
    let Some(dir) = std::env::var_os("AUSPEX_TEST_FORGE_DIR").map(PathBuf::from) else {
        return;
    };
    let port = std::env::var("AUSPEX_TEST_FORGE_PORT").unwrap();
    let token_file = std::env::var_os("AUSPEX_TEST_FORGE_TOKEN_FILE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let mut timeouts = Settings::default().timeouts;
    if let Some(probe) = std::env::var("AUSPEX_TEST_FORGE_PROBE")
        .ok()
        .and_then(|p| p.parse().ok())
    {
        timeouts.probe = probe;
    }
    let settings = Settings {
        timeouts,
        forge: Some(ForgeSettings {
            url: format!("http://127.0.0.1:{port}/"),
            host: "forge.example.org".to_owned(),
            repo: Some("owner/fleet".to_owned()),
            token_file,
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
        Duration::from_secs(20),
    );
    shell.send(Event::Start(auspex_tui::app::core_config(
        &settings,
        dir.join("fleet").display().to_string(),
    )));
    tokio::time::timeout(Duration::from_secs(20), async {
        while !shell.view().refresh.settled {
            shell.changed().await;
        }
    })
    .await
    .expect("the refresh settles");
    let view = shell.view();
    shell.note_ci(&view);
    // Again: once per reason, so no second line.
    shell.note_ci(&view);
    println!(
        "JSON-BEGIN\n{}JSON-END",
        auspex_tui::headless::json(&view.report)
    );
    for line in shell.log.lines() {
        println!("LOG: {line}");
    }
    assert_eq!(
        shell
            .log
            .lines()
            .iter()
            .filter(|l| l.starts_with("CI badge:"))
            .count(),
        usize::from(view.report.fleet_ci.reason.is_some()),
        "{:?}",
        shell.log.lines()
    );
    shell.shutdown().await;
}

// ---------------------------------------------------------------------------------------
// The binary
// ---------------------------------------------------------------------------------------

#[test]
fn the_binary_refuses_a_plain_http_forge_url_before_anything_runs() {
    let dir = tempdir();
    let config = dir.path().join("config.toml");
    fs::write(&config, "[forge]\nurl = \"http://forge.example.org\"\n").unwrap();
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
        stderr.contains("`forge.url`") && stderr.contains("https"),
        "{stderr}"
    );
}

#[test]
fn the_demo_has_no_forge_and_says_off() {
    let dir = tempdir();
    let out = run_within(
        auspex(dir.path()).args(["--demo", "--json"]),
        Duration::from_secs(60),
    );
    let report: FleetReport = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report.fleet_ci, FleetCi::off());
}
