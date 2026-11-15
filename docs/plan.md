# auspex: implementation plan

Revised 2026-11-15 from the original plan (2026-11-14). The changes come from building it
on rust-app-template, from verifying the plan's assumptions against omnissiah, and from the
design discussion that followed.

## Goal and principles

auspex is a local TUI that shows which machines in a Clan fleet are reachable and up to
date, previews what a deploy would change, and deploys through the `clan` CLI. The target
fleet is omnissiah.

The name follows omnissiah's Adeptus Mechanicus naming: an auspex is the handheld scanner
you point at things to read their state. Use `auspex` for the crate, the binary and every
path (`~/.config/auspex`, `$XDG_CACHE_HOME/auspex`).

Principles:

- **Agentless.** Talk to machines over SSH only, through the system `ssh` binary, never an
  SSH library, so `~/.ssh/config`, agents, ProxyJump and Tailscale names keep working.
- **Don't reimplement Clan.** Deploys, vars and anything else the `clan` CLI does go through
  the CLI. No custom deploy engine.
- **Never block the UI.** Every slow thing is an effect with a timeout that reports back as
  an event. One dead host only ever affects its own row.
- **Evaluation is the slow part.** One machine per evaluator process, in parallel, cached.
  Never build just to show status.
- **Safe by default.** auspex starts read-only. Deploying needs an explicit mode switch, and
  risky deploys need typed confirmation.
- **Configurable.** Everything in `config.toml` is also a home-manager module option.
- **Out of scope:** an always-on agent daemon, onboarding wizards, anything that duplicates
  the `clan` CLI.

## Architecture: Crux, ports and adapters

auspex is a rust-app-template product (`cli new auspex --drop-example`, then
`just add crux-app auspex --shells tui`), developed on forge.noosphere.uk with AGit, and
mirrored to GitHub. The template supplies the workspace, CI pipeline, devenv, clippy and
invariant gates, `nix build`, and future template updates (`update`, RT-60).

Crux is a ports-and-adapters architecture. The core's `Effect` enum is the set of ports;
the shell's effect handlers are the adapters.

```
crates/
  auspex-domain/    pure rules, no I/O, no async. Modules: fleet, observe, deploy, eval.
                    serde types here are the --json contract (versioned).
  auspex-effects/   adapters: ssh, nix, clan, prometheus, k8s, cache (fs),
                    deploylog (SQLite), ntfy, notify. tokio lives here and nowhere else.
apps/auspex/
  core/             Crux: Model, Event, Effect (the ports), update(), view().
                    Orchestration only, calling domain functions.
  shells/tui/       ratatui rendering, key mapping, and wiring Effect → auspex-effects.
```

Dependencies point inward only: `shells/tui` → `core` → `domain`, and `shells/tui` →
`effects` → `domain`. `core` never sees `effects`. A template invariant can enforce this.

Testing falls into three layers:

1. **Domain:** pure unit tests, plus golden fixtures, for the parsers, the status rules,
   target resolution and the rollout planner. Most of the risk lives here.
2. **Core:** Crux tests that resolve effects by hand, to check sequencing ("the list arrives
   before probes", "a stale generation is dropped", "cancel kills the in-flight request").
3. **Effects:** integration tests against a fixture flake, ssh to localhost, SQLite.

The domain starts as one crate with one module per concern. Split a module out only for a
concrete reason. The first known one: the phase-2 `clan-probe` binary would share the
`observe` protocol, which then becomes `crates/auspex-probe`.

### Crux specifics

- **Streaming:** deploy logs use `stream_from_shell`.
- **Cancellation needs an explicit effect.** `AbortHandle` drops the core's side of a
  command, but the shell's process doesn't know. Every long effect carries a request id, and
  the core emits `Cancel(id)`. The adapter kills the process (`kill_on_drop(true)` plus an
  explicit kill on cancel).
- **Refresh generation** lives in `Model`. Results tagged with an older generation are
  dropped in `update`.

### Headless output: `--plain` and `--json`

`--plain` and `--json` run the same core and effects without a terminal, until the fleet has
settled, then print the final view model. "Settled" means every machine has a status or an
error, or the global timeout has passed (`fleet_settled()` in the core). That gives one code
path, so the JSON always matches what the TUI shows. The JSON is a documented, versioned
contract, because the weekly digest and coding agents read it.

If scriptable mutating commands grow (`auspex update --tag k8s`, `auspex audit`), add a thin
CLI binary that drives the same core with the same `auspex-effects`. There is nothing to
untangle, because the adapters already live in their own crate.

## The target fleet: omnissiah

Verified on 2026-11-15, read-only (the full report is in omnissiah's
`.claude/auspex/verify-report.md`):

| Fact | Consequence |
|---|---|
| 11 machines: 10 `nixosConfigurations` and one `darwinConfigurations` (genetor) | Evaluate both. genetor's probe must not assume systemd |
| The inventory is at `.#clanInternals.inventoryClass.inventory.machines.<m>.deploy.{targetHost,buildHost}` | Read target hosts from here |
| Clan does **not** copy `deploy.targetHost` into `clan.core.networking.targetHost`. That option is a default hostname (`root@<m>.omnissiah`), and for genetor it disagrees with the inventory (.149 vs .109) | Never use the option as the source. Its hostnames are exactly the wildcard-DNS trap (see lints) |
| Most targets are LAN IPs; alfrost and cypramundi are public VPS IPs | Treat openssh, firewall and networking changes on public hosts as high risk |
| `deploy.buildHost = "localhost"` on most machines | Builds happen on the desktop; show that in the deploy log |
| Tags: base, server, workstation, laptop, dev, gaming, k8s, k8s-base, k8s-server, k8s-master | Tag filter and grouping; rolling updates per tag |
| k8s: terra (k8s-base), luna and sol (k8s-server), mars (k8s-master) | Never update the control plane all at once |
| Nothing sets `system.configurationRevision` | Step 1 ships a module for it |
| Forgejo CI builds every machine every 3 days into ncps | "Already cached" badge |
| ntfy runs on cerberus | Deploy notifications |
| node_exporter, Prometheus and Alertmanager run fleet-wide | Health metrics without SSH (with SSH fallback) |
| `builtins.getFlake` evaluates the dirty tree like `nix eval .#` | Either works; both need `--impure` |
| A batched eval of all machines is fragile: a fetch error or a missing attribute aborts the whole batch, and `tryEval` can't catch it. 8 machines take ~44 s, with **no warm speedup** | One evaluator process per machine (`nix-eval-jobs`), and the cache is essential |
| `clan vars check`/`list` have no JSON mode. `list` prints public values in clear | Use `check`'s exit code and section headers only. Never display `list` values |
| `clan` isn't on PATH in a plain shell, and `clan --version` is unsupported. clan-core is `dbae210` (2026-10-26) | auspex locates the `clan` binary itself (the fleet flake's dev shell, or `nix run` of its clan-core input) and records which one it used |

## Data sources

### Machines and target hosts

`machines()` uses the clan CLI's inventory output if it has a JSON mode (verify for the pinned
clan-core). Otherwise it reads the inventory attribute path above in one cheap eval.

Target host, in order of precedence:

1. the per-machine override in `config.toml`;
2. the inventory's `deploy.targetHost`;
3. the machine name.

The detail panel shows which source won.

Target syntax to parse: `[user@]host[:port][?Opt=val&Opt2=val]`, with IPv6 bracketed when a
port is given (`root@[fd00::1]:2222`). Query options become `-o Opt=val`.

### Evaluation

`nix-eval-jobs` over the fleet flake's `nixosConfigurations` and `darwinConfigurations`
toplevel `drvPath`s:

- parallel workers, with memory capped per worker;
- one JSON line per machine as soon as it finishes, so rows fill in progressively;
- an error stays on that machine's row (`!` plus nix's last stderr lines) and never aborts
  the others.

Cache under `$XDG_CACHE_HOME/auspex/`, keyed by:
- the format version;
- `git rev-parse HEAD`;
- a hash of `flake.lock`;
- when the tree is dirty, a hash of `git diff HEAD` plus the untracked file list.

Write to a temp file, then rename. A failed cache write is ignored. The cache matters because
re-evaluation isn't any faster warm.

### Observation: Prometheus first, SSH always for what only SSH knows

- **Prometheus (optional, `prometheus.url`):** one PromQL query for the whole fleet returns
  uptime, root disk, load and memory. Alertmanager gives the alerts firing per instance. The
  config maps a machine name to its `instance` label.
- **SSH, always,** one round trip per machine per refresh, for what Prometheus can't know:
  `current-system`, the booted versus current kernel and initrd, failed units, and the
  configuration revision.
- **SSH fallback, per machine:** if Prometheus is unreachable, has no series for that
  instance, or its data is older than about 2 scrape intervals, the SSH probe also collects
  uptime, disk and load. The detail panel shows each value's source.

The SSH probe is a short POSIX shell script that prints `key=value` lines (no `jq` on the
remote), parsed in Rust. There are two variants:

- **NixOS:** current-system, booted and current kernel and initrd, `nixos-version
  --configuration-revision`, `systemctl is-system-running`, `systemctl --failed
  --output=json`, plus uptime and disk when Prometheus can't answer.
- **darwin:** current-system, `darwin-version --configuration-revision` (it exists; it prints
  `null` until the revision module is imported), uptime, disk. No systemd fields.

ssh options:
- `BatchMode=yes`, `ConnectTimeout=5`, `ServerAliveInterval=5`, `-T`;
- `ControlMaster=auto`, `ControlPath=$XDG_RUNTIME_DIR/auspex/%C` (directory mode 0700),
  `ControlPersist=60s`;
- `--` before the destination.

Exit code 255 means ssh itself failed. Show ssh's last stderr line ("Host key verification
failed"), not a bare `✗`.

### Other sources

- **Cached badge:** `HEAD https://<ncps>/<hash>.narinfo` for the pending toplevel. One HTTP
  request, no Nix call.
- **CI badge:** the Forgejo API's latest run for the fleet flake's current commit.
- **Kubernetes:** for machines with k8s tags, node Ready/NotReady and cordoned state, read
  from the API in the same refresh. A rolling update refuses to start if a node is already
  NotReady.

## Status badges

| Badge | Status | Rule |
|---|---|---|
| spinner (grey) | checking | probe not back yet |
| spinner (cyan) | evaluating | online, toplevel not back yet |
| `✓` | in sync | current-system == evaluated toplevel |
| `↑` | update pending | paths differ |
| `◆` | cached | pending toplevel already in ncps (shown with `↑`) |
| `⚠` | failed units / alerts | any failed unit, or an alert firing |
| `⟳` | reboot needed | booted kernel/initrd != current |
| `?` | not installed | reachable, no `/run/current-system` |
| `✗` | unreachable | ssh exit 255 or timeout; the detail panel shows ssh's stderr |
| `!` | eval error | toplevel failed to evaluate; the detail panel shows nix's stderr |
| `❄` | frozen | `U` skips it |

## Lints (homelab safety)

- **Hostname targets.** Warn when a `targetHost` is a hostname rather than an IP.
  `*.ugalabugala.org` is a wildcard that answers for every name with cerberus's IP, so a
  hostname target once deployed the wrong closure onto the main server. **Refuse to deploy**
  if a target resolves to the same IP as another machine's.
- **SSH failures during deploys.** `clan machines update` goes through nixos-rebuild, which
  strips `SSH_AUTH_SOCK`, and too many agent keys cause auth failures. When a deploy fails at
  SSH, say so, and point at the fix.

## UI

- **Layout:** two panes at ≥ 100 columns, a single column below.
- **Navigation is vim-style:**
  - `j`/`k`, arrows, counts (`5j`);
  - `gg`/`G`, `Ctrl-d`/`Ctrl-u`;
  - `/` search with `n`/`N`;
  - `:` commands (`:update terra`, `:filter tag=k8s`);
  - `q`/`Esc`/`Ctrl-C` to quit.

  The keymap is remappable in `config.toml`. `?` shows a help overlay generated from the live
  keymap, so keys and help can't disagree.
- **Modes:** auspex starts in **observe** mode. Deploy, reboot, rollback and freeze only work
  after toggling **act** mode, shown in the status bar.
- **Typed confirmation:** updating a public host, or one whose diff touches openssh, firewall
  or networking options, requires typing the machine's name.
- **Theming:**
  - **The default is the terminal's 16 ANSI colours only.** Both stylix and nixarchy's
    omatheme recolour the terminal, so auspex follows the active theme with no
    configuration, including omatheme's runtime switches.
  - **Optional `~/.config/auspex/theme.toml`** with base16 slots (`base00`–`base0F`). The
    home-manager module's `stylix` switch writes it from `config.lib.stylix.colors`.
  - **Reading omatheme's active theme** is a low-priority polish item (only enginseer uses
    nixarchy).
- **Notifications:** ntfy (a topic in the config) for deploy results and machines that go
  unreachable. Also `notify-send` when auspex isn't the focused tab.

## Packaging and configuration

The auspex flake exports:

- `packages.<system>.auspex` and `overlays.default`;
- `homeManagerModules.auspex`, with options that mirror `config.toml` one to one:
  - fleet flake path;
  - per-machine overrides (target, Prometheus instance, frozen);
  - Prometheus and Alertmanager URLs;
  - ntfy topic;
  - keymap, theme and the `stylix` switch;
  - timeouts.

  It writes `~/.config/auspex/config.toml`. A hand-written config works without the module.
- `nixosModules.configurationRevision` and `darwinModules.configurationRevision`, which set
  `system.configurationRevision = self.rev or self.dirtyRev or null`. Import them into the
  fleet flake; auspex never edits the fleet's repo.

Adopting it is `inputs.auspex.url = "git+https://forge.noosphere.uk/manjo/auspex";` plus the
imports. Also `nix run git+https://forge.noosphere.uk/manjo/auspex` from day one.

## Build order

Each step is usable on its own, passes its acceptance checks, and leaves `just ci` green
before the next one starts.

### Step 1: Fleet status

- **Crates:** `auspex-domain` and `auspex-effects`. Crux core and TUI shell as above.
- **CLI (clap, in the shell):**
  - `--flake <path>` (default `.`, env `CLAN_DIR`);
  - `--demo` (a fake effect executor with a scripted fleet: one slow host, one dead host,
    one failed unit);
  - `--plain` and `--json` (headless).
- **Machines and target hosts** from the inventory, with sources, plus the hostname lint.
- **Evaluation** with `nix-eval-jobs` and the cache.
- **Observation:** the SSH probe (`current-system` and revision) plus Prometheus, with the
  per-machine SSH fallback.
- **The cached badge** (ncps narinfo).
- **The `configurationRevision` modules** (moved here from step 3), so the detail panel
  shows the deployed commit from day one.
- **UI:**
  - the machine list with badges;
  - a detail panel: status and reason, host and its source, now and next store path (hash
    prefix plus version), deployed commit;
  - the responsive layout;
  - vim navigation and `?` help;
  - observe mode only.
- **The home-manager module,** with the options step 1 uses.

Acceptance:

- [ ] `just ci` passes. It includes:
  - domain tests with golden fixtures recorded from real machines (terra as NixOS, genetor
    as darwin, a powered-off host, a host-key failure);
  - a core sequencing test;
  - an effects integration test with `nix-eval-jobs` against a fixture flake (no inputs; a
    fake nixos config, a fake darwin config, a fake inventory, and one machine that fails).
    It's skipped when `nix` isn't on PATH. The fixture uses a `path:` flake ref, so it works
    untracked.
- [ ] `auspex --plain` on omnissiah lists all 11 machines, genetor included, each with the
  right target and source. A machine that fails to evaluate shows `!` with the reason.
- [ ] A powered-off machine shows `✗` within 10 s, with ssh's reason, and never delays other
  rows.
- [ ] A second run on a clean tree reads every eval from the cache (log the cache hit).
- [ ] With Prometheus stopped or unreachable, every row still gets its data over SSH.
- [ ] Snapshot tests of `--demo` at 40 and 120 columns, using ratatui's `TestBackend`.

### Step 2: Update with streamed log

- **Act mode:**
  - `u` runs `clan machines update <m>`, with output streaming into a Log tab, coloured by
    stream;
  - `b` builds only;
  - `s` suspends the TUI and opens `ssh <m>`, restoring the TUI on exit.
- **`U` updates every `↑` machine.** Rolling mode per tag: one machine at a time, waiting
  until it probes healthy before the next. For the k8s tags, drain before and uncordon
  after, and refuse if any node is already NotReady.
- **Freezing:** `f` freezes a machine so `U` skips it; freezes persist in the config.
- **Before any update:**
  - warn if the fleet tree is dirty or unpushed;
  - check free disk on the target against the new closure size;
  - apply the typed-confirmation rule;
  - apply the duplicate-IP refusal.
- **A local SQLite log** of everything auspex does: deploy, reboot, rollback, freeze, with
  machine, commit, start, end, result and who.
- **ntfy and desktop notifications.**

Acceptance:
- [ ] Updating one machine streams output live and records a row in SQLite.
- [ ] `U` on the k8s tags never has two of luna, sol, mars updating at once.
- [ ] A frozen machine is skipped by `U` and marked `❄`.
- [ ] Quitting during an update asks for confirmation, and the clan process is not orphaned
  (cancel effect).
- [ ] auspex finds and uses the right `clan` binary without it being on PATH.

### Step 3: Health

- **Full probe** per class, plus Alertmanager alerts.
- **Badges `⚠` and `⟳`.** `j` opens a failed unit's journal; `R` reboots after typed
  confirmation.
- **"N commits behind",** and how many of those commits touch files that machine imports.
- **The Forgejo CI badge.**
- **A CI runners view** (a tab, plus a summary line in the status bar), one row per runner:
  - **From the Forgejo API of each forge** (forge.ugalabugala.org and forge.noosphere.uk; both
    have the runners cerberus, terra, luna and sol registered by default): registered and
    online or offline, idle or busy, the job and repository it's running, and its labels.
  - **Per forge:** the number of queued jobs and the oldest one's age.
  - **From Prometheus:** the runner host's CPU and memory against its cap (terra and sol run 2
    CPUs / 4 GiB, luna 3 GiB; cerberus is capped at 3 CPUs / 8 GiB), so a job close to an
    OOM kill or a thrashing host shows before it fails.
  - **From the SSH probe:** the state of the `forgejo-runner-*` systemd units on that host. A
    runner the forge calls offline usually shows up here as a failed or inactive unit.
  - **Badges:** offline, or forge-online with the unit down (`✗`); a job running longer than a
    configurable limit; memory near the cap (`⚠`).
  - **Config / home-manager options:** each forge's URL and a read-only API token
    (`token_file`), never inline.

Acceptance:
- [ ] A unit forced to fail shows `⚠`, and its journal opens with `j`.
- [ ] genetor probes without errors (no systemd fields).
- [ ] The runners view lists every runner on both forges with its online/busy state and
  current job. Stopping one runner's unit turns its row red within one refresh.

### Step 4: Clan vars tab

Per machine, each generator's status: ok, missing, needs regeneration, shared. It's read from
`clan vars check`'s exit code and section headers only. Never display values: `clan vars
list` prints public values in clear, so it isn't used for display.

### Step 5: Diff, generations, safe deploys

Design-level; refine after step 3.
- **Package diff:** `nix path-info -r --store ssh-ng://<host>` against the new closure,
  compared by name and version in Rust (nvd-style). Kernel, systemd and major-version bumps
  are highlighted.
- **Service restart preview:** `switch-to-configuration dry-activate` on the target.
- **Lockout warning:** highlight changes to openssh, firewall and networking options; red on
  public hosts. This feeds the typed-confirmation rule.
- **Option-level diff** between deployed and pending.
- **Generations:** a generations list, and rollback to one. After a deploy, if the machine
  stops answering or units fail within a few minutes, offer a one-key rollback.
- **A per-machine timeline** (deploys, reboots, failures), and closure size per deploy.

### Step 6: Config inspector

Press `i` on a machine. Four parts:
1. A tree browser over the evaluated config, evaluated lazily through a long-lived
   `nix repl` subprocess. Options that throw show the error in place.
2. A provenance panel: value, type, description, default, priority (mkDefault/mkForce),
   `definitionsWithLocations` with overridden definitions shown, and declarations.
3. A by-file view: every option a chosen file sets.
4. Compare: one option across machines, or deployed versus pending.

### Step 7: Polish

- `/` tag filter and grouping.
- Parallel updates outside rolling mode.
- A command run across a tag.
- A cross-machine journal for one unit.
- Wake-on-LAN.
- Reading omatheme's active theme.
- Handing a failed unit's journal plus config to a coding agent in herdr.

## Phase 2 backlog

Not scheduled. Pick from here once step 7 ships.

| Idea | How |
|---|---|
| Exposure and SSO audit | Collect every Caddy virtual host, ingress, Pangolin tunnel and VPS firewall port, and mark each Pocket ID protected, LAN/VPN only, or public. Flag Pocket ID clients in tofunix with no app left. As an `auspex audit` command CI can run |
| Container CVE report | Match Harbor's scan results and SBOMs against the images the nixidy manifests reference, and list images nothing references |
| Service health | HTTP-check each machine's Caddy virtual hosts |
| Flake input staleness | Lock age and commits behind per input; flag pinned revs |
| Drift detection | nix-env profiles, services started by hand, a dirty `/etc` |
| Build queue | Parse `--log-format internal-json`, like nix-output-monitor |
| Store size and GC | Per machine, with a GC action |
| Host CVE counts | vulnix, unique CVE IDs per system closure |
| Clan backups status | The last backup per machine, via the clan CLI |
| Inventory view | Services, roles and tags from the Clan inventory |
| Probe binary | `clan-probe --json` as a Clan service, replacing the shell script (that's when `auspex-probe` becomes a crate) |
| Upstream | clan-lib's MachineStatus remote check is a TODO (clan-core #4748); offer the probe logic there |

## Related tools

**Weekly homelab digest:** a scheduled job that summarises the week and posts it to Memos or
Matrix. auspex's part is to keep `--json` and the SQLite log stable and documented.

## Known traps

Each one fails quietly, so test for it.

- **`readlink -f` lies.** With `/run/current-system` missing, `readlink -f` still prints a path
  and exits 0. Use plain `readlink`, and check the output starts with `/nix/store/`.
- **ControlPersist can hold pipes open.** On some OpenSSH versions the backgrounded master
  inherits the first ssh's stdout and stderr. Wait on the child's exit, then give the pipe
  readers about 250 ms and abort them.
- **Not every eval error is catchable.** `tryEval` catches only `throw` and `assert`; fetch
  errors and missing attributes abort. That's why evaluation is one process per machine.
- **Flakes only see tracked files.** The fixture test uses a `path:` flake ref.
- **BatchMode turns host-key prompts into failures.** That's correct, but show ssh's last
  stderr line.
- **Probes must not land before the list.** Send the machine-list event before spawning any
  probe.
- **Aborting must kill the process.** Use explicit `Cancel` effects plus
  `kill_on_drop(true)`.
- **Bare IPv6 vs host:port.** Exactly one colon means host:port; more than one means a bare
  IPv6 address. Display it bracketed when a port is present.
- **The inventory and the option disagree.** genetor's `clan.core.networking.targetHost` is
  .149 while the inventory says .109. Only the inventory counts.
