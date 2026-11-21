# `auspex-domain`

auspex's pure rules: the inventory parser, ssh targets and their resolution, the ssh
argument vector, the probe scripts and their parser, Prometheus merging, `nix-eval-jobs`
lines, the eval cache key, package closure diffs, the status badges, the CI badge's rules (`ci`: remote parsing, which repository to ask, what the forge's states mean), and the versioned `--json` contract.

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
`null`. `with_package_diff` adds a short highlighted version change and counts (or
`configuration only`) to an update-pending reason, without changing status or settlement.
Reasons are human text, never an interface for scripts.

The additive JSON v1 preview has `state` (`computed`, `not_cached`, `failed`), `reason`
(null except a sanitised failure line), `deployed` / `pending` system store paths,
`added`, `removed`, `changed`, `highlights`, `rebuilt`, exact versioned package `counts`,
`unversioned: {added, removed}`, `invalid: {deployed, pending}`, and `truncated`.
Toplevels are detected by `nixos-system-` / `darwin-system-`; absent or ambiguous
toplevels are null. Part 2 must check this pair before attaching a preview to a report.
Each change and highlight carries `name`, version sets `before` / `after` and independent
`before_truncated` / `after_truncated` flags. Highlights also have `kinds`.
Empty before/after means added/removed; null version members are never emitted.
Non-computed states have empty lists, zero counts and null toplevels.
`PackageDiff::failed` bounds its sanitised reason to 512 UTF-8 bytes.

Store names accept only Nix's `[A-Za-z0-9+._?=-]`; malformed roots, child paths and
invalid bytes are skipped and counted per side, never cleaned into another identity.
Names split before the first hyphen followed by an ASCII digit, or before a trailing
hexadecimal revision of at least seven characters (including letter-led git SHAs).
Known trailing outputs collapse after a version: `out man dev lib bin doc info debug
static headers modules getent libgcc terminfo data py lib32 login mount swap lastlog
su xxd sandbox env nc p11kit`. A custom `-[a-z][a-z0-9]*` suffix also collapses when
the same package has the unsuffixed version in that closure. Numeric revisions such
as glibc's `-66` remain.

The exclusion list drops `.drv`, `-source`, configuration and systemd-unit suffixes
(`.conf`, `.service`, `.target`, `.socket`, `.timer`, `.path`, `.mount`), prefixes
`nixos-system-`, `darwin-system-`, `etc-`, and exact `etc`, `system-units`,
`system-path`, `activation-script`, `initrd`. `unit-` is excluded only without a
digit-led version, preserving NGINX Unit. A single integer version is treated as a
generated directory when a dotted version with the same prefix coexists (`dbus-1`
beside `dbus-1.16.2`). This heuristic can hide a genuine integer version alongside
its dotted release; without derivation metadata the directory is indistinguishable.
All remaining unversioned names use nvd-like grouping: only distinct-name add/remove
counts, no individual filenames or hash churn. They never add null to a version set.

Version ordering follows Nix's component algorithm (`pre`, signed 32-bit numeric
parsing and lexical fallback on overflow), with lexical tie-breaking for stable sets.
Critical highlights identify exact `linux`, `systemd`, `glibc` and `nix` packages.
`major_version` means first-component sets differ (downgrades included); leading zeroes
are ignored. Both sides need versioned components; hexadecimal revisions have no major.
`rebuilt` lists only those four critical names with equal versions but different sets
of store paths. Other hash-only rebuilds vanish. Highlights are not safety judgements.

Limits: 100,000 input paths per side, 4096 bytes per path, 255 bytes per store name,
128 entries per list and 16 versions per package/side. The compact JSON preview has a
256 KiB byte budget including metadata and duplicated highlight versions, counted without
allocating a serialized copy. Critical highlights precede major highlights, then the
alphabetical fill, and count against category caps. Byte/cap omissions set the affected
list flags; byte omissions also set `truncated.bytes`. `rebuilt` is naturally bounded
by four names (`truncated.rebuilt` remains false). Counts and highlight classification
use complete inputs, before retention caps. Input overflow is an explicit `DiffError`.
