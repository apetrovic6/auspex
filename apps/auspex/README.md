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
auspex                       # the TUI; starts read-only (observe mode), `A` turns act mode on
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

Clan's inventory adds `all` and the machine's class (`nixos` or `darwin`) to every machine's
tags. The TUI's detail panel leaves those two out of its `tags:` list (they say nothing the
class does not); `--json` keeps the inventory's full list, and `--plain` prints no tags.

The package carries `nix-eval-jobs` (used only when none is on `PATH`), which with the nix
libraries it links adds about 170 MiB to the closure: the price of `nix run` working on a
system without it.

In the TUI, `?` lists the live keymap (`[keymap]` in the config rebinds any action; an empty
list unbinds it). A plain-path fleet inside a git repository is evaluated as a `git+file`
reference, so **a machine file that is not tracked yet is not evaluated** (the same rule as
`nix build .#…`): `git add` it.

## Acting on a machine

auspex starts in **observe** mode, always: nothing is persisted, and `--plain` and `--json` can
never leave it. `A` asks `turn on act mode? y/N`; the status bar then shows `ACT` in a warning
colour, and `A` turns it off again at once. In observe mode `u` only says `act mode is off (A)`.

| Key | What it does |
|---|---|
| `u` | Act mode only. Runs the checks below, then asks `update luna to 0a1b2c3d nixos-system-luna-…? y/N` (the pending system as the detail panel shows it, hash prefix first; the machine's name typed, for a public one), then runs exactly `clan machines update --host-key-check strict --flake <fleet> -- luna` with the fleet's pinned `clan` (found from its lock, never from `PATH`). One machine, named, validated against the inventory; one action at a time. |
| `b` | Builds the machine's pending system here (`nix build --no-link --print-out-paths --no-write-lock-file <fleet>#nixosConfigurations.luna.config.system.build.toplevel`; `darwinConfigurations.<m>.system` for darwin). Changes nothing on any machine, so it needs no act mode. Refused for a machine of another system than this one. |
| `L` | The Log tab: the streamed output of the current and last action, standard error in its own colour, the last 10 000 lines of each, following the tail until you scroll up (`k`, `Ctrl-u`, `gg`; `G` follows again). `Tab` switches to the adapters' notes. |

**Questions are answered only once they could be read.** A `y/N` question is not answered by a key that arrives before it has been on screen, whole, for 0.3 s (typeahead such as `Ayuy` in one burst, or a `y` typed by habit right behind the `u`, is discarded and the question stays open). A pane too small to show the whole question shows "terminal too small to ask" instead, and nothing answers it until the pane is large enough and the 0.3 s have passed. A paste is never a `y`: bracketed paste is on, so pasted text is not typed, and with a readable question open it answers no. A second `Ctrl-C` that arrives within that 0.3 s of the quit question is ignored for the same reason.

**`clan` deploys the working tree as it is at `y`**, not the evaluation auspex showed in the question, which may be hours old (auspex left open, the tree edited, `git pull`). auspex compares the pending system with the one it asked about: if a refresh changed it, `y` asks again about the new one and deploys nothing. Edits to the tree since the last evaluation are not seen by that check.

### Before an update: the checks and the confirmation

`u` does not ask at once. It first looks at what the update is about to do, and then shows
**one dialog** with everything it found: what **refuses** the update first (there is no way to
proceed; any key closes the dialog), then the **warnings**, then the question. While it looks
(a `git status` and a size query, at most `timeouts.diff`) the dialog says so, and any key
cancels the look.

| Check | Refuses | Warns |
|---|---|---|
| **The fleet repository** (`git status` in the fleet directory, read-only: no hooks, no filters, no index rewrite) | never | uncommitted changes to tracked files (untracked files do not count: nix ignores them); `HEAD` ahead of its upstream; no upstream (or a deleted one, or a detached `HEAD`); the check could not run |
| **Free disk on the target's `/nix/store`** (the probe's `df -Pk /nix/store`, not `/`: with impermanence `/` is a tmpfs) | free is less than the pending closure's size plus 1 GiB | the free space or the closure's size is not known (the size is the sum of NAR sizes from `nix path-info --recursive --size`, against the local store first and then the binary cache) |
| **Duplicate address** (the shared-IP lint) | the machine's target resolves to an address another machine's does too (the dialog names which), **or its address is not known** (the rule fails closed: a hostname that did not resolve cannot be told apart from another machine) | other machines' addresses are not known |

A check that fails to *run* is a warning, not a refusal, except the duplicate-address rule.
The free space is the probe's, from the last scan, and the closure size is the whole closure
(a deploy copies less, since the target already has most of it): the check errs towards
refusing, by design.

The question is `update luna to 0a1b2c3d nixos-system-luna-…? y/N` for a machine that is not **public**.
For a machine marked public (`machines.<m>.public = true`, see Configuration) it is `type luna,
then Enter`: `y` is just a letter there, only the machine's exact name confirms, and Esc
cancels. Each stage is a question of its own (the "checking" dialog, then the dialog with what
the checks found, a refusal or a warning included): a key typed before the frame that shows it
is discarded, so a key pressed while the checks ran counts for none of them. Everything is
decided again at the moment you confirm, so a refresh that happened while the dialog was open
cannot let a refused update through; and if it changed the pending system, nothing is deployed
and the checks run again for the new one.

*Not yet:* the plan also asks for the typed name when the update's NixOS **option diff**
touches openssh, the firewall or networking. That needs a diff of options between the deployed
and the pending system, which does not exist (AUS-22). The decision is one function,
`auspex_domain::preflight::option_diff_touches_network`, which says `false` today and is
already consulted; it is not approximated from package names.

The status bar names the running action and, at the end, how it ended. A deploy that fails at
ssh says so, and what to change (`nixos-rebuild` strips `SSH_AUTH_SOCK`; an agent with many keys
makes the server give up). After an update the machine is probed and evaluated again.

Every `u` and `b` is a row in `$XDG_STATE_HOME/auspex/log.sqlite` (start, end, result, exit code
and a short summary, never the output: the last stderr lines of a failure or a cancel (which can include whatever a failing script printed there), the fixed text `exit 0, N lines` for a success; see the schema in `crates/auspex-effects`). If the log
cannot be written the status bar says so; the action is not affected.

**Quitting during an action** asks `an update of luna is running; quit and stop it? (an activation already under way on the machine may complete) y/N`. `y` (or a
second `Ctrl-C`) kills the whole process group, waits until it is gone, logs the row
`cancelled` and only then exits; a third `Ctrl-C` stops waiting for that answer (the shell still
gives the kill up to 5 s to finish). `SIGTERM`, `SIGINT` and
`SIGHUP` take the same path without the question.

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
ssh_approval = 60    # authentication/agent approval, per host; 5..600 seconds
probe = 10          # script execution, after authentication
settle = 120        # global refresh deadline, including approval
diff = 120          # one machine's package preview, and the checks before an update; 1..86400 seconds (see below)

[diff]
enabled = true      # false: never query a closure; package_diff stays null

[machines.alfrost]
public = true       # updating it asks for its name to be typed, not y (default false)
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

**Why approval prompts come one at a time.** An ssh agent that asks before it signs (Bitwarden,
for one) shows its prompts one after another, and a prompt that is answered after ssh gave up
is wasted. auspex connects to every cold host at once, because most of a connection (the
network, a down host's timeout) needs no agent, but it serialises the *approval window*: a
host's `ssh_approval` window ends that long after the later of its own start and the moment the
most recent connection in this run ended (approved, refused, failed, timed out or cancelled),
and it is worked out again whenever another connection ends. So each answered prompt gives
the hosts still waiting a fresh window, and nine prompts answered a few seconds apart all
land within their windows. A host that is down fails at SSH's connect timeout, in parallel
with the others, and costs nothing extra; a host that stalls without a prompt holds up nobody
and only times out on its own window. Hosts whose master is already up are never involved.
The status bar and the headless note count the hosts connecting, which may or may not be
waiting for the agent. A fleet of N cold hosts needs `settle` to cover the N prompts, answered
one by one. Two limits: the window is per auspex process (a TUI and a `--json` run do not see
each other's connections, so their prompts can interleave at the agent), and auspex assumes that
killing a waiting ssh (on refresh, quit or timeout) withdraws its request from the agent. If your
agent keeps showing the prompt of a client that has gone, the next prompt can appear
behind it.

Masters remain reusable for ten idle minutes by default. Any process running as the
operator can reuse that authenticated connection; the checked, operator-owned 0700
control directory (never a symlink) restricts access to the operator. Increase persistence
only if that longer authenticated lifetime is acceptable.
These SSH connections stay open for up to `ssh.control_persist` idle seconds **after
auspex quits**. Lower the value to shorten that window, or close a known master explicitly:
`ssh -o ControlPath="$XDG_RUNTIME_DIR/auspex/%C" -O exit user@host` (add `-p PORT` for a
non-default port): ssh works out the socket name itself. Without `XDG_RUNTIME_DIR` (or with a
relative one) the directory is `auspex-<uid>` under `$TMPDIR`, or `/tmp`; use that in place of
`$XDG_RUNTIME_DIR/auspex`.

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
query slots (8 queries at once, each of the two queries of a machine taking a slot of its own),
separate from the 32 shared by probes and cache lookups, so a slow preview cannot hold up
another machine's `cached` badge. `timeouts.diff` starts when a query gets its slot; a preview
that never got one before `settle + diff` is `failed` ("no preview slot became free"). Set
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
