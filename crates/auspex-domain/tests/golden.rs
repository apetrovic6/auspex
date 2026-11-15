//! Golden fixtures: each input file under `tests/fixtures/` against its expected output.
//! See `tests/fixtures/README.md`. `AUSPEX_BLESS=1` rewrites the expected files.

use std::fs;
use std::path::{Path, PathBuf};

use auspex_domain::eval::parse_eval_line;
use auspex_domain::fleet::parse_inventory;
use auspex_domain::observe::{ProbeExit, parse_probe};
use serde_json::{Value, json};

fn fixtures(dir: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
}

/// Inputs in `dir` with extension `ext`, sorted, so the run is deterministic.
fn cases(dir: &str, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(fixtures(dir))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == ext)
                && !p.to_string_lossy().ends_with(".expected.json")
        })
        .collect();
    v.sort();
    assert!(!v.is_empty(), "no fixtures in {dir}");
    v
}

fn expected_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap().to_string_lossy().into_owned();
    input.with_file_name(format!("{stem}.expected.json"))
}

fn check(input: &Path, got: &Value) {
    let path = expected_path(input);
    let pretty = serde_json::to_string_pretty(got).unwrap() + "\n";
    if std::env::var_os("AUSPEX_BLESS").is_some() {
        fs::write(&path, &pretty).unwrap();
        return;
    }
    let want = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("{} is missing; run with AUSPEX_BLESS=1", path.display()));
    let want: Value = serde_json::from_str(&want).unwrap();
    assert_eq!(
        &want,
        got,
        "{} differs from the parser's output:\n{pretty}",
        path.display()
    );
}

#[test]
fn probe_fixtures() {
    let cases = cases("probe", "stdout");
    assert!(cases.len() >= 12, "a probe fixture went missing");
    for stdout_path in cases {
        let stdout = fs::read_to_string(&stdout_path).unwrap();
        let stderr = fs::read_to_string(stdout_path.with_extension("stderr")).unwrap_or_default();
        let exit = fs::read_to_string(stdout_path.with_extension("exit")).unwrap();
        let exit = match exit.trim() {
            "timeout" => ProbeExit::TimedOut,
            s => s.strip_prefix("signal:").map_or_else(
                || ProbeExit::Code(s.parse().unwrap()),
                |n| ProbeExit::Signal(n.parse().unwrap()),
            ),
        };
        let outcome = parse_probe(&stdout, &stderr, exit);
        check(&stdout_path, &serde_json::to_value(outcome).unwrap());
    }
}

#[test]
fn crlf_fixture_really_has_crlf() {
    // Guards the fixture itself: a checkout that normalised line endings would make the
    // CRLF case pass vacuously.
    let s = fs::read(fixtures("probe").join("crlf.stdout")).unwrap();
    assert!(s.windows(2).any(|w| w == b"\r\n"));
}

#[test]
fn inventory_fixtures() {
    for input in cases("inventory", "json") {
        let machines = parse_inventory(&fs::read_to_string(&input).unwrap()).unwrap();
        check(&input, &serde_json::to_value(machines).unwrap());
    }
}

#[test]
fn eval_fixtures() {
    for input in cases("eval", "jsonl") {
        let lines: Vec<Value> = fs::read_to_string(&input)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| match parse_eval_line(l) {
                Ok(o) => json!({ "ok": o }),
                Err(e) => json!({ "err": e.to_string() }),
            })
            .collect();
        check(&input, &Value::Array(lines));
    }
}
