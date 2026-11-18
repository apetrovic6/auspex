# Golden fixtures — SYNTHETIC

Every file here was **written by hand**, not recorded from a machine. Store-path hashes,
revisions, addresses and error texts are made up to have the right shape. The plan's step-1
acceptance asks for fixtures recorded from real machines (terra as NixOS, genetor as darwin,
a powered-off host, a host-key failure); when those exist, replace the matching synthetic
input here and re-bless the expected output, keeping the file names so the tests need no
edit.

Layout (driven by `tests/golden.rs`):

| Directory | Input | Expected |
|---|---|---|
| `probe/` | `<case>.stdout`, optional `<case>.stderr`, `<case>.exit` (a status code, `signal:N`, or `timeout`) | `<case>.expected.json`: the `ProbeOutcome` |
| `inventory/` | `<case>.json`: the value of `clanInternals.inventoryClass.inventory.machines` | `<case>.expected.json`: the parsed machines |
| `eval/` | `<case>.jsonl`: `nix-eval-jobs` output, one line each | `<case>.expected.json`: one `{"ok": …}` or `{"err": "…"}` per line |
| `json/` | — | `fleet-v1.json`: the `--json` snapshot `tests/json_contract.rs` compares against |

`probe/crlf.stdout` has CRLF line endings on purpose; do not normalise it.

To regenerate every expected file after an intended change, run the tests with
`AUSPEX_BLESS=1` and review the diff. A missing expected file is a failure, not a pass.

The closure pair is hand-assembled from public nixpkgs names and versions, with
**synthetic store hashes**, not copied machine closures. It includes split outputs
(`modules`, `getent`, `sandbox`, `xxd`, `env` and others), a generated `dbus-1`
directory alongside the real version, an unversioned `systemd` directory, grammar
git revisions and generic generated filename shapes. Generic filenames and the
example toplevel are invented; no operator-specific filenames are included.
