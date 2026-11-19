# auspex

The Crux app: one core holding the decisions, one terminal shell drawing them.

```
core/           what to ask the fleet, in what order, and what the answers mean — no I/O
shells/tui      terminal (ratatui): rendering, keys, and fulfilling the core's effects
```

The core never performs an effect; it asks for one, and the shell fulfils it by calling
`crates/auspex-effects` (ssh, nix, Prometheus, the binary cache). The rules both sides apply
live in `crates/auspex-domain`. Dependencies point inward: the core never sees the effects
crate.

```sh
just apps/auspex/dev              # format, lint, build and test every crate here
just apps/auspex/shells/tui/run
```

## Running it

```sh
auspex                       # the TUI, read-only (observe mode)
auspex --plain               # scan once, print one line per machine, exit
auspex --json                # scan once, print the fleet report (the versioned contract)
auspex --demo                # a scripted fleet: no machine, nix, cache or network is touched
auspex --flake ~/clan/fleet  # the fleet flake; else CLAN_DIR, then config `flake`, then .
```

Headless exit status: 0 for a scan that completed with no fleet-level error; 1 only when the
fleet has a problem (the machine list or the evaluator failed, or `timeouts.settle` cut the
scan short); 2 when auspex itself could not do its job (a usage or config error, no usable
terminal, the report could not be written, a closed pipe on stdout); 128 + n after signal n.
Unreachable or out-of-date machines are findings, not failures. With Prometheus configured
but unreachable, headless mode says so on stderr and takes health from ssh.

The package carries `nix-eval-jobs` (used only when none is on `PATH`), which with the nix
libraries it links adds about 170 MiB to the closure: the price of `nix run` working on a
system without it.

In the TUI, `?` lists the live keymap (`[keymap]` in the config rebinds any action; an empty
list unbinds it) and `L` shows the log: eval cache hits, caching off and why, config
warnings. A plain-path fleet inside a git repository is evaluated as a `git+file`
reference, so **a machine file that is not tracked yet is not evaluated** (the same rule as
`nix build .#…`): `git add` it.

## Installing

Run directly with `nix run git+https://forge.noosphere.uk/manjo/auspex`, or add
auspex to your fleet flake:

```nix
inputs.auspex.url = "git+https://forge.noosphere.uk/manjo/auspex";
```

Without `inputs.auspex.inputs.nixpkgs.follows = "nixpkgs"`, auspex uses its pinned
devenv-nixpkgs (import-from-derivation evaluation and potentially a second glibc).
Following your nixpkgs avoids that pin's evaluation cost but changes the derivation
and can require a full rebuild instead of a cache hit.
A `path:` reference to a Git checkout does not work with the template's source
fileset; use `git+file:` or a plain path instead.

Import its Home Manager module (no Home Manager input is required by auspex):

```nix
{
  imports = [ inputs.auspex.homeManagerModules.auspex ];
  programs.auspex = {
    enable = true;
    settings.flake = "~/clan/fleet"; # ~/ is your home directory; a path value is not copied to the store
    settings.timeouts.probe = 10;
    # stylix.enable = true; # optional, requires Stylix colours
  };
}
```

The module installs `auspex` and writes only configured settings to
`~/.config/auspex/config.toml`. With no settings, it leaves that file unmanaged.
`stylix.enable` selects a generated Base16 `theme.toml`, unless you set another
`settings.theme.file` (then no unused theme is written); without a theme, auspex uses
the terminal's ANSI colours. A hand-written config works without Home Manager.
A leading `~/` in `settings.flake` or the selected theme path uses Home Manager's home directory.
The package is also available as `packages.<system>.auspex` (and `default`);
`overlays.default` adds `pkgs.auspex`.

## Configuration

`~/.config/auspex/config.toml` (or `--config PATH`) accepts:

```toml
[ssh]
control_persist = 600 # seconds; 60..3600

[timeouts]
ssh_connect = 5      # TCP connection timeout; 1..60 seconds
ssh_approval = 60    # authentication/agent approval; 5..600 seconds
probe = 10          # script execution, after authentication
settle = 120        # global refresh deadline, including approval
diff = 120          # one machine's package preview; 1..86400 seconds (see below)

[diff]
enabled = true      # false: never query a closure; package_diff stays null
```

The config file's `flake` and `CLAN_DIR` take a leading `~/` (your `$HOME`; `~user/` is refused);
`--flake` is left to your shell. Precedence: `--flake`, then `CLAN_DIR`, then `flake`, then `.`.

Home Manager exposes the same keys under `programs.auspex.settings`.
A warm control master skips authentication; a cold connection gets a separate approval
allowance, so approving an agent request does not consume the script's probe timeout.
Down hosts still fail at SSH's connect timeout. The status bar counts connecting hosts;
the phase includes DNS and authentication, not just agent approval. Headless mode writes
one connection notice to stderr, never to JSON stdout. A stalled DNS lookup or
authentication can use the entire `ssh_approval` allowance. The global `settle` deadline
still bounds the whole refresh; the loader warns when it is shorter than approval plus probe.

Masters remain reusable for ten idle minutes by default. Any process running as the
operator can reuse that authenticated connection; the checked, operator-owned 0700
control directory (never a symlink) restricts access to the operator. Increase persistence
only if that longer authenticated lifetime is acceptable.
These SSH connections stay open for up to `ssh.control_persist` idle seconds **after
auspex quits**. Lower the value to shorten that window, or close a known master explicitly:
`ssh -S "$XDG_RUNTIME_DIR/auspex/<socket-hash>" -O exit -- user@host` (use its actual
socket path and destination).

## Package preview

For a machine whose update is pending, auspex shows **what** would change, nvd-style: the
packages added, removed and changed between what it runs and what the fleet evaluates to,
with the kernel, systemd, glibc and nix called out and major-version bumps counted. The
detail panel shows the headline, the critical changes with their versions, a count of the
other major-version changes, then the counts of added, removed, changed, rebuilt and
unversioned packages; `d` opens the full, scrollable list (`j`/`k`, `Ctrl-d`/`Ctrl-u`, `gg`/`G`;
`q` or `Esc` closes it). `--plain` ends a pending machine's line with the headline; `--json`
carries the whole preview as `package_diff` (see `auspex_domain::report`).

Two lists of store paths are compared, and both queries write nothing to the store:

- **What the machine runs:** `nix-store --query --requisites /run/current-system`, over the
  same ssh control master the probe opened: no new approval prompt while the master is up, and
  through the same approval phase when it is not. It works on nix-darwin too. Its output is
  capped at 4 MiB; a line that is not a store path is dropped and counted.
- **What the update would run:** only when the binary cache already has the pending
  toplevel (the `cached` badge). If the path is valid in the local store, `nix-store --query
  --requisites` locally; otherwise `nix path-info --recursive --store <cache> <path>`, which
  writes nothing to the local store (nix keeps its own narinfo cache under `~/.cache/nix`; the
  query passes `--option narinfo-cache-negative-ttl 0` so a path that is missing right now is
  not remembered as missing for an hour). A pending toplevel that the cache says it does not
  have is reported as `not_cached`, with no network call (so is every update-pending machine
  when no cache is configured, and the panel says which); a cache lookup that failed is a
  `failed` preview, because nobody checked. A preview is built from what exists, never from a
  build. Each closure's own system must be the one the scan saw (the probe's
  `current_system`, the evaluation's out path), or the preview is `failed` ("the machine
  changed system while it was being queried").

A query that fails or runs out of `timeouts.diff` makes that machine's preview `failed`, with
the reason; it never changes the machine's status or fails the scan. A refresh cancels
queries still running. `timeouts.settle` does not cut a preview short: the queries start late
in a refresh, so headless mode waits for them, at most `timeouts.diff` after they were
requested. Both are backed by a deadline in the core itself, `timeouts.settle` plus
`timeouts.diff` after the refresh started, at which a preview still out is cancelled and
`failed` ("timed out"): headless mode therefore waits at most `settle + diff` (240 s by
default), and never depends on an adapter answering. Previews have their own small pool of
query slots (8, both queries of four machines at a time), separate from the 32 shared by probes
and cache lookups, so a slow preview cannot hold up another machine's `cached` badge. Set
`diff.enabled = false` to switch the feature off.

Stamp deployed machines with the **fleet's** revision, not auspex's:

```nix
# In a NixOS module:
imports = [ inputs.auspex.nixosModules.configurationRevision ];
# In a nix-darwin module:
imports = [ inputs.auspex.darwinModules.configurationRevision ];
```

Both imports expect the fleet's `self` in module arguments (Clan provides it;
otherwise set `specialArgs = { inherit self; };`). Alternatively import
`inputs.auspex.lib.mkConfigurationRevisionModule self` to capture it explicitly.
An explicit `system.configurationRevision` overrides the module's default.

## What this app is not

auspex has one shell, in Rust, and will never have another. So the core carries none of the
template's multi-shell machinery: no `Facet` typegen, no `codegen` binary, no `ffi.rs`, no
`boltffi.toml`, no `cdylib`/`staticlib`, no wasm32 pass, and its Justfile has no
code-generation or native-platform recipes.
