# `auspex-domain`

auspex's pure rules: the inventory parser, ssh targets and their resolution, the ssh
argument vector, the probe scripts and their parser, Prometheus merging, `nix-eval-jobs`
lines, the eval cache key, the status badges, and the versioned `--json` contract.

**Admission rule:** a function belongs here when it takes values and returns values. No I/O,
no async, no process spawning, no filesystem, no clock, no environment. Whatever *obtains* an
input is an effect (`auspex-effects`); whatever *sequences* effects is the Crux core. A rule
that needs "now" takes an age as an argument.

Tests: unit tests beside the code, golden fixtures under `tests/fixtures/` (synthetic for
now; see the README there), and a snapshot of the `--json` shape in `tests/json_contract.rs`.
`AUSPEX_BLESS=1 cargo test -p auspex-domain` regenerates the expected files.
