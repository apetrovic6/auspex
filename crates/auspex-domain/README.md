# `auspex-domain`

auspex's pure rules: the inventory parser, ssh targets and their resolution, the ssh
argument vector, the probe scripts and their parser, Prometheus merging, `nix-eval-jobs`
lines, the eval cache key, the status badges, and the versioned `--json` contract.

**Admission rule:** a function belongs here when it takes values and returns values. No I/O,
no async, no process spawning, no filesystem, no clock, no environment. Whatever *obtains* an
input is an effect (`auspex-effects`); whatever *sequences* effects is the Crux core. A rule
that needs "now" takes an age as an argument.

Tests: unit tests beside the code, golden fixtures under `tests/fixtures/` (synthetic for
now; see the README there), snapshots of the `--json` shape in `tests/json_contract.rs`,
and `tests/probe_scripts.rs`, which runs the probe scripts under a local `sh` against a
fake root and shim commands (the only test that spawns a process).
`AUSPEX_BLESS=1 cargo test -p auspex-domain` regenerates the expected files.

Caller obligations:

- Evaluate a flat machine-name-to-toplevel attribute set, including for one machine; after
  draining an exited evaluator, use `EvalFailure::no_result` for machines without a result.
- Set `NarCache::Pending` before publishing an update-pending row whose cache lookup will
  follow, and set `Outstanding` flags before publishing rows awaiting health or alerts.
  Their defaults mean nothing is pending. Cache lookups delay settlement only for updates;
  failures add `cache lookup failed` to the reason, without changing the JSON v1 shape.
- Keep `FleetFacts::scan_complete` false during address resolution: machine settlement does
  not cover shared-IP lints.
- Run ssh without a controlling terminal and with `SSH_ASKPASS_REQUIRE=never`; removing
  `SSH_ASKPASS` alone does not disable the compiled-in askpass fallback.

Evaluation `attr` display text is sanitised at parse time; `attr_path` retains machine
identity unchanged.
