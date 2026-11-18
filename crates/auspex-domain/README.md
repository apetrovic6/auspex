# `auspex-domain`

auspex's pure rules: the inventory parser, ssh targets and their resolution, the ssh
argument vector, the probe scripts and their parser, Prometheus merging, `nix-eval-jobs`
lines, the eval cache key, package closure diffs, the status badges, and the versioned `--json` contract.

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

## Package preview (part 1)

`diff::diff_closures(deployed, pending)` takes runtime store-path lists, not derivations.
`None` pending returns `None` (unknown until built or cached); empty lists are known empty.
No core, adapter or UI obtains these inputs yet. `MachineReport::package_diff` defaults to
`null`; `with_package_diff` attaches a computed preview and appends short package counts
to an update-pending reason, without changing status, badges or settlement.

The additive JSON v1 field has `state` (`computed`, `not_cached`, `failed`), `reason`
(null except a sanitised failure line), `added`, `removed`, `changed`, `highlights`,
exact package `counts` and independent list `truncated` flags. Each change has `name`,
version sets `before` and `after`, and `before_truncated` / `after_truncated` flags.
An empty before/after means added/removed; a null set member means unversioned.
`not_cached` means unknown until built or cached; non-computed states have empty lists
and zero counts. `PackageDiff::failed` bounds the failure reason to 512 UTF-8 bytes.
Hash-only rebuilds vanish. Reasons are for people, never an interface for scripts.

Names split at the first hyphen followed by an ASCII digit, as required by this feature;
current Nix actually splits before any non-letter, so punctuation-only boundaries are
intentionally not treated as versions here. Known trailing outputs (`out`, `man`, `dev`,
`lib`, `bin`, `doc`, `info`, `debug`, `static`, `headers`) collapse after the version.
Unknown output names cannot be distinguished from version suffixes without derivation
metadata and remain distinct. Revisions such as glibc's `-66` remain in the version.

The exclusion list drops `.drv`, `-source`, configuration and systemd-unit suffixes
(`.conf`, `.service`, `.target`, `.socket`, `.timer`, `.path`, `.mount`), prefixes
`nixos-system-`, `darwin-system-`, `unit-`, `etc-`, and exact `etc`, `system-units`,
`system-path`, `activation-script`, `initrd`. This conservative heuristic can hide a real
package with one of these names and retain an unfamiliar generated file. Grouping all
unversioned names was rejected: it would hide real packages with no version.

Version ordering follows Nix's component algorithm (including `pre`, signed 32-bit numeric
parsing and lexical fallback on overflow), with lexical tie-breaking for stable sets.
Critical highlights identify exact `linux`, `systemd`, `glibc` and `nix` packages.
`major_version` means the sets of first Nix components differ (downgrades included);
numeric leading zeroes are ignored. Both sides must have a version, so additions,
removals and transitions from unversioned packages do not claim a major change.
Neither a highlight nor its absence is a deploy-safety judgement.

Limits: 100,000 input paths per side, 4096 bytes per path, 255 bytes per sanitised
package identity, 128 entries in each added/removed/changed/highlights list, and 16
versions per package and side. Input overflow is an explicit `DiffError`, never a partial
equality claim; output truncation preserves exact counts and highlights use complete
version sets. At most two highlight kinds apply to any package. All package text uses
the terminal sanitiser before identity comparison. Malformed paths and children of
store roots are ignored. `PackageDiff` documents the JSON fields in `src/diff.rs`;
the synthetic closure pair and computed/null report snapshots freeze the rules.
