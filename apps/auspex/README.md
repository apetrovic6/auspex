# auspex

The Crux app: one core holding the decisions, one terminal shell drawing them.

```
core/           what to ask the fleet, in what order, and what the answers mean — no I/O
shells/tui      terminal (ratatui): rendering, keys, and fulfilling the core's effects
```

The core never performs an effect; it asks for one, and the shell fulfils it by calling
`crates/auspex-effects` (ssh, nix, Prometheus, the binary cache, the forge's CI status, the CI runners). The rules both sides apply
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

auspex starts in **observe** mode, always: act mode is never persisted, and `--plain` and `--json` can
never leave it. `A` asks `turn on act mode? y/N`; the status bar then shows `ACT` in a warning
colour, and `A` turns it off again at once. In observe mode `u` and `s` only say `act mode is off (A)`.

| Key | What it does |
|---|---|
| `u` | Act mode only. Runs the checks below, then asks `update luna to 0a1b2c3d nixos-system-luna-…? y/N` (the pending system as the detail panel shows it, hash prefix first; the machine's name typed, for a public one), then runs exactly `clan machines update --host-key-check strict --flake <fleet> -- luna` with the fleet's pinned `clan` (found from its lock, never from `PATH`). One machine, named, validated against the inventory; one action at a time. |
| `U` | Act mode only. Updates **every machine with an update pending**, rolling per tag (see *Updating everything* below): one question for the whole plan (the order, what runs in parallel, what is skipped and why), then each machine goes through the checks of `u` at its turn. Frozen machines are skipped, and while it is unknown which are frozen `U` refuses outright, as `u` does. TUI only: `--plain` and `--json` never have it. |
| `b` | Builds the machine's pending system here (`nix build --no-link --print-out-paths --no-write-lock-file <fleet>#nixosConfigurations.luna.config.system.build.toplevel`; `darwinConfigurations.<m>.system` for darwin). Changes nothing on any machine, so it needs no act mode. Refused for a machine of another system than this one. |
| `s` | Act mode only. Suspends the TUI and opens `ssh` to the selected machine (see below); when ssh ends the TUI is back, and the status bar says `ssh to luna exited (code N)`. |
| `f` | Freezes the machine: `u` refuses it from then on (`luna is frozen; f to unfreeze`) and a later `U` will skip it. Shows `❄` in the list and `frozen` in the detail panel. **Needs no act mode**, because it only prevents changes. On a frozen machine `f` asks `unfreeze luna? updates to it will be possible again. y/N` (answered like every question: only once it could be read). A freeze is kept across restarts, per fleet, in `$XDG_STATE_HOME/auspex/state.toml` (see *Freezing* below), and every freeze and unfreeze is a row in the action log. |
| `L` | The Log tab: the streamed output of the current and last action, standard error in its own colour, the last 10 000 lines of each, following the tail until you scroll up (`k`, `Ctrl-u`, `gg`; `G` follows again). `Tab` switches to the adapters' notes. |
| `R` | The Runners tab (see *CI runners* below): every runner the configured forges list, with its machine's unit, memory against its cap and badges, and what each forge's queue holds. Read-only; `q` or `Esc` closes it. |

**`s` is your own ssh, not auspex's.** It runs `ssh -o ControlMaster=no -o ControlPath=<runtime dir>/auspex/%C [-p port] -- user@host` for the machine's target (the same user, host and port the probes use), with the terminal. auspex's own `-o` options come first, so your `ControlPath` and `ControlMaster` never apply, with a live master or without one. Your agent, your `SSH_ASKPASS` and your prompts (host keys, passwords, passphrases) work, because none of auspex's probe guards (`BatchMode`, no tty, askpass removed) apply to it. **Which connection it uses depends on whether auspex holds a live control master for that host.** If it does, the session is a channel on *that* connection: no second agent approval, but the connection is auspex's, so its settings decide the transport, not your `~/.ssh/config`: a keepalive of 5 s (a network drop of about 15 s ends the session: 5 s times `ServerAliveCountMax`, which is 3 unless the machine's options set it, 1 to 5, so 5 to 25 s; a direct ssh under your own keepalive settings might have survived it), and its `Compression`, `TCPKeepAlive` and `IPQoS`. What is per session still follows your config (`ForwardAgent`, `RemoteCommand`, `SendEnv`, the tty). If there is no live master, ssh connects directly under your `~/.ssh/config` entirely, and leaves no master behind (a socket left by a dead master makes ssh print `Control socket connect(...): Connection refused` first). While ssh runs auspex draws nothing and reads no key (its probes and an update's output carry on, and the Log tab has them when you are back), and Ctrl-C, Ctrl-\ and Ctrl-Z go to ssh: Ctrl-Z does not suspend it (auspex has no job control to bring it back, so a stopped ssh is continued at once). Signals: ssh is in auspex's process group, so the terminal's own signals reach auspex as well as ssh and auspex ignores them: that includes a `kill -INT` or `kill -QUIT` aimed at auspex from another terminal while ssh runs (SIGTERM and SIGHUP are obeyed, below). After ssh ends auspex waits 100 ms for a late copy of such a signal and discards it, which is a timing heuristic: on a very loaded machine one that is noticed later still quits auspex. The SIGTSTP handler, installed the first time you press `s`, stays for the rest of the run, so a `kill -TSTP` aimed at auspex itself no longer stops it. ssh may leave your terminal in raw mode (it cannot restore it when it is killed): auspex saves the terminal's modes before ssh and puts them back as soon as it ends, on every path. A panic in auspex while ssh runs writes only its message, as plain text with no escape sequences, to the terminal ssh is using; it does not touch the terminal's modes or screen. **To get back, leave ssh the way you would anywhere: `exit`, Ctrl-D, or `~.` on a new line.** Whatever its exit code, or if it was killed, the TUI redraws and says how it ended; keys typed to a dead ssh are discarded. If auspex is sent `SIGTERM` or `SIGHUP` while ssh runs, it ends ssh (`SIGTERM`, then `SIGKILL` after 3 s), leaves your terminal as it is (normal screen, cursor shown) and exits with 128 plus the signal. `s` is refused with a reason in observe mode ("act mode is off (A)"), for a machine whose target did not parse, and in `--demo` (nothing real to ssh to); `--plain` and `--json` never have it.

**Questions are answered only once they could be read.** A `y/N` question is not answered by a key that arrives before it has been on screen, whole, for 0.3 s (typeahead such as `Ayuy` in one burst, or a `y` typed by habit right behind the `u`, is discarded and the question stays open). A pane too small to show the whole question shows "terminal too small to ask" instead, and nothing answers it until the pane is large enough and the 0.3 s have passed. A paste is never a `y`: bracketed paste is on, so pasted text is not typed, and with a readable question open it answers no. A second `Ctrl-C` that arrives within that 0.3 s of the quit question is ignored for the same reason.

**`clan` deploys the working tree as it is at `y`**, not the evaluation auspex showed in the question, which may be hours old (auspex left open, the tree edited, `git pull`). auspex compares the pending system with the one it asked about: if a refresh changed it, `y` asks again about the new one and deploys nothing. Edits to the tree since the last evaluation are not seen by that check.

### Before an update: the checks and the confirmation

`u` does not ask at once. It first looks at what the update is about to do, and then shows
**one dialog** with everything it found: what **refuses** the update first (there is no way to
proceed; any key closes the dialog), then the **warnings**, then the question. While it looks
(the tree look, one git status, `HEAD` and a fingerprint of the changes, and a size query, each at most `timeouts.diff`) the dialog says so, and any key
cancels the look.

| Check | Refuses | Warns |
|---|---|---|
| **The fleet repository** (`git status` in the fleet directory, read-only: no hooks, no filters, no index rewrite) | never | uncommitted changes to tracked files (untracked files do not count: nix ignores them); `HEAD` ahead of its upstream; no upstream (or a deleted one, or a detached `HEAD`); the check could not run; the uncommitted changes could not be fingerprinted (see "What 'the same uncommitted changes' means") |
| **Free disk on the target's `/nix/store`** (the probe's `df -Pk /nix/store`, not `/`: with impermanence `/` is a tmpfs) | free is less than what the update adds plus 1 GiB. What it adds is the NAR size of the pending closure's paths that are **not** in the machine's own closure (the package preview fetches that one), or, when the machine's closure is not known, the whole pending closure; the dialog says which was counted. Sizes come from `nix path-info --recursive --size`, against the local store first and then the binary cache. **Refused too while the machine's probe is still out** (a refresh clears the probes): "not here yet" is not "not known" | the free space or the closure's size is not known (the probe answered without the field, or the size could not be had) |
| **Duplicate address** (the shared-IP lint) | the machine's target resolves to an address another machine's does too (the dialog names which), **or its address is not known** (the rule fails closed: a hostname that did not resolve cannot be told apart from another machine), **or address resolution is still out** (after a refresh, until it answers no collision can be ruled out) | other machines' addresses are not known because their resolution finished and failed |

A check that fails to *run* is a warning, not a refusal, except the duplicate-address rule.
The free space is the probe's, from the last scan. The estimate is still an overestimate (the
store holds older generations, and paths are shared between them), which is the safe direction.
`u` itself is refused ("still probing luna", "still resolving addresses") while either is out, and
the same rules decide again at `y`: a typed name that became required, or a warning that was not in
the dialog, asks again instead of deploying.

**The fleet tree at the `y`** is read once more, under the update lock, and the update starts only
on the same `HEAD` commit and the same uncommitted changes the dialog was answered about; otherwise
nothing is deployed and the notice names both commits (or says the uncommitted changes changed).

**What "the same uncommitted changes" means.** For a tree with changes to tracked files, auspex
hashes (SHA-256) what nix would evaluate: auspex evaluates the fleet as a `git+file` flake, which
reads the tracked files as they are in the **worktree**, so the digest covers each tracked
file's bytes and executable bit (a symlink's target) and which paths are tracked (the index's
paths, stages and assume-unchanged tags), and not the index's blob ids: `git add` of a file that
is already edited, a stash popped back, or the staging of a deletion (`git add -A`, `git add -u`, `git rm`: a tracked file that is missing is absent for nix either way) changes nothing nix reads and is not a change; a
further edit is, staged or not. It is bounded to 1 GiB read and 30 s, as the evaluation cache
key is, and the whole tree look (the status, `HEAD`, and this) to `timeouts.diff`. Untracked
files, ignored or not, are **not** in it: nix does not see them, so they cannot change what is
deployed. It is not `git diff HEAD`: that would run git's diff machinery (textconv, external
diff drivers, filters) which the checks never run in a repository they do not trust. The plan
(and `u`'s dialog) shows `HEAD`'s first 12 digits and, for a dirty tree, `dirty` with the
digest's first 8.

When the digest cannot be computed (too big, too slow, not readable) the plan says so in a
warning. If the **plan** had none, a turn compares the commit only and says so (in the machine's
log for `U`, in the status bar for `u`). If the plan **had** one and a turn cannot make one, the
changes are not known to be the ones confirmed, so that machine is refused ("could not be
checked"). Not covered, as for the status check itself: a tracked file marked assume-unchanged or
skip-worktree (git reports the tree clean, so there is no digest). A `clan` run that commits to
the fleet repository itself (generated vars) moves `HEAD` too: the next machine is then refused,
and running `U` again is the answer.

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

## Updating everything: `U`

`U` updates every machine whose row says `↑` (an update pending), minus the frozen ones, and asks
**one** question for all of it. Before it opens, auspex looks at the cluster (which one, and its
nodes) and at the fleet repository (one `git status`), and the question carries what it found:

```
update 4 machines:
cluster: context rke2 (https://10.0.0.1:6443); every kubectl call is pinned to it
fleet repository: HEAD 1a2b3c4d5e6f
rolling taghmata (one at a time; each node must be Ready before the next): luna → mars → sol → terra
in parallel, at most 2 at a time: alfrost, cerberus
at most 3 machines updating at once: one of the cluster's group + 2 of the machines in parallel
skipped genetor: frozen
warning: the fleet repository has uncommitted changes to 2 tracked files: the update deploys them, …
each machine is checked at its turn: a refusal skips it, a failure stops its rolling group, and a fleet repository that is not as it was at the y (a new warning, another commit, other uncommitted changes) refuses it
update 6 machines as listed above? y/N
```

The plan is shown whole or not asked: a pane too small for it says "terminal too small to ask".

**One auspex updates a fleet at a time.** The `y` takes an exclusive lock on the fleet (an
`flock` on a file of its own in the state directory, `update-<digest>.lock`, 0600), held for the
whole roll and for each `u`. A second `U` or `u` from another auspex is refused, naming what the
holder is doing (`not updating: another auspex is updating this fleet (…): pid 123: update
all`). The kernel drops the lock when the process ends, so a crash cannot leave it behind. A run
with no usable state directory has no lock to take, and then the freezes are unknown and nothing
can be updated anyway.

That lock is **per user, per machine and per checkout of the fleet**, not global: it lives in the
state directory of the user running auspex and is keyed by the fleet's canonical path (or by the
flake reference as written). Two operators, one operator on two machines, a second checkout of the
same fleet, and a remote flake reference beside a local checkout each take a different lock and are
**not** kept apart by it.

**Rolling is configuration, not tag names built into auspex.** Name the tags whose machines must
never update together:

```toml
[rolling.taghmata]     # machines with this tag update one at a time
kubernetes = true      # check node state in the cluster before and between machines
drain = false          # cordon and drain before, uncordon after (default false)
drain_timeout = 600    # kubectl drain's --timeout, seconds (default 600)
etcd = false           # RKE2 control plane: every etcd member must be healthy before a machine goes out (default false)

[update]
parallel = 2           # machines with no rolling tag, at once (1..16, default 2)
ready_timeout = 600    # wait for an updated machine to be healthy, seconds (default 600)
poll_interval = 5      # seconds between looks while waiting (1..60, default 5)

[kubernetes]
kubeconfig = "~/.kube/config"   # default: $KUBECONFIG, then ~/.kube/config
```

A `[rolling.<tag>]` with no keys is a rolling tag with the defaults (home-manager writes an empty
`rolling.<tag> = { };` as that table; it is not pruned like an empty section).

Machines that share **any** rolling tag are one group and never update at the same time, and the
relation chains: a machine tagged `a` and `b` joins everything tagged `a` with everything tagged
`b`. (On the fleet this was written for, `taghmata` is the RKE2 cluster, `fabricator` its control
plane and `skitarii` its workers; all four machines are etcd members, so two at once could cost
the quorum.) A group is one machine after another in name order; machines with no rolling tag go
`update.parallel` at a time, since `clan` builds locally and copies every closure. Groups run
beside each other and beside the free machines, **except** that every group with a machine that
is drained or waited for in the cluster shares it: at most one machine of all those groups is out
of service at a time (they have one kubeconfig, and one etcd). The plan says what runs at once.

**Each machine is checked at its turn, as `u` checks it** (the fleet tree as it is then, the
closure's size and the free disk, the duplicate-address rule, the freeze file once more right
before `clan` starts), and then:

| What happens | Result |
|---|---|
| the checks, or the freeze, refuse it | **skipped; the roll goes on** with the next machine |
| a warning about the fleet repository that was not in the plan the operator confirmed (someone edited the tree, or its upstream, after the `y`) | **refused**, the roll goes on |
| the fleet repository stands on another commit than the plan showed (a `git pull`, or a commit then pushed: no warning changes), or its uncommitted changes are not the ones it showed (the same files saved again), or the changes cannot be fingerprinted now though they could be at the plan | **refused**, with a sentence naming both commits, saying the uncommitted changes changed, or saying they could not be checked; the roll goes on (and the next machine is held to the same plan, so is refused the same way) |
| the checks warn about the machine itself (unknown closure size or free disk) | written into the machine's part of the Log tab; it goes on |
| it is marked `public` (its name has to be typed) | left out of the plan: update it with `u` |
| it does not answer a probe at its turn | refused: its health afterwards could not be told from before |
| the cluster is not healthy when its turn comes (see below) | **its rolling group stops** |
| `etcd = true` and a member's etcd is not ready, or cannot be read, when its turn comes (see below) | **its rolling group stops** |
| `clan` fails, a drain fails, or it does not become healthy | **its rolling group stops** (the rest is skipped); other groups carry on |

**Kubernetes** (`kubernetes = true`, or `drain` set). auspex talks to the API with your `kubectl`
(on `PATH`; a supervised child like every program it runs) and a kubeconfig on this machine, not a
client of its own and not `kubectl` over ssh.

- **One cluster, named.** Before the plan, `kubectl config view --minify` says which context the
  kubeconfig points at; the plan shows it and its API server, and **every later call is pinned to it**
  (`--context=<name>`): a `kubectl config use-context` in another terminal, or a shared kubeconfig
  whose current context is another cluster, cannot send a drain elsewhere.
- **Before the roll,** any node that is `NotReady` or cordoned refuses the group (listed as
  skipped, with the reason), and so does a machine that is no node of the cluster (a node is named
  like its machine). So does a node that says `Ready` but whose kubelet has gone quiet (below).
  A machine that **is** a node of the cluster but has no rolling group, or only a group that does not
  use the cluster (neither `kubernetes` nor `drain`), would update beside the cluster's group; the
  plan says so in a `warning:` line naming it (and its group), and does not refuse anything. It
  looks at the machines of this roll only, and at node names as the cluster lists them.
- **A kubelet that has gone quiet is not healthy.** A node whose kubelet stopped keeps
  `Ready=True` for about 40 s before the node controller flips it, so a kubelet that has missed
  three 10 s lease renewals still looks `Ready`. A `Ready` node whose **lease** (`renewTime`;
  the condition's own heartbeat is rewritten only every few minutes and says nothing about a live
  kubelet) is **more than 25 s older** than the newest lease of any node in the same read counts as
  not healthy (two missed renewals plus a margin for the read's own timing; the usual jitter of a
  renewal or two is well inside it): it refuses the group in the plan and stops it before the next
  machine, with a sentence naming the node, the age and the node that is the reference (`node mars
  is Ready, but its kubelet has been silent for 30 s by luna's clock (the newest lease in this
  read)`). The age is node against node, never against this machine's clock, so a skew between the
  desktop and the cluster does not matter. 25 s is fixed, not a setting. **The nodes' own clocks
  must agree within a few seconds**: a node whose clock runs ahead makes every other node look
  silent (when more than one is flagged the last sentence says that the reference clock may be the
  wrong one), and a node whose own clock runs ahead can be silent that much longer before it is
  flagged. A cluster in which every kubelet went quiet at once looks healthy by this rule alone.
  A `Ready` node with no lease while others have one is refused too (`has no lease, so its kubelet
  cannot be vouched for`). When no lease can be read at all the check is skipped, and the machine's
  Log line says so.
- **Before every machine** that uses the cluster, the nodes are read again (so also right after the
  `y`, for the first of each group, however long the plan sat on screen): any node that is `NotReady`
  or cordoned, **whoever cordoned it**, stops the group, and a node is drained only when it was
  schedulable a moment ago, which is what makes it auspex's to uncordon. A node someone else
  cordoned is never drained or uncordoned by auspex.
- **etcd** (`etcd = true`, off by default; implies `kubernetes`). On RKE2 a control-plane node is an
  etcd member, and taking one out while another member is down can cost the quorum even though every
  node is `Ready`. With `etcd = true` auspex reads the etcd static pods (`kubectl get pods -n
  kube-system -l component=etcd`, one per member) and the nodes' `node-role.kubernetes.io/etcd`
  label, in the plan and again **before every machine of the group**: every labelled node must have
  an etcd pod that is `Running` and `Ready`, otherwise the group is refused in the plan, or its next
  machine fails (**the group stops**) before anything is cordoned or drained. The rule is stricter
  than the quorum on purpose: *every* member, not "enough", and it applies to any machine of the
  group, members or not. What cannot be told counts as unhealthy: a pod list that cannot be read, no
  node with the label at all, an etcd pod on a node without it. After an update the machine is not
  done until its own etcd pod is `Ready` again (as well as its node), so the last machine of a
  group is checked too. Set `etcd = true` on the tag of an RKE2 control plane; leave it off on a
  cluster without etcd pods in `kube-system` (k3s with sqlite, a managed control plane, external
  etcd), where it could never pass.

  **Verify before you rely on it.** Pod `Ready` is the kubelet's view of the etcd container's
  *readiness probe*. Whether RKE2's probe reflects the member's health in the cluster, or only that
  the process answers, was not checked against a real cluster (auspex's tests use a fake `kubectl`).
  Check it once on your own control plane: `kubectl -n kube-system get pod etcd-<node> -o yaml` and
  read `readinessProbe` (and `startupProbe`), and compare with what happens to the pod when a member
  is stopped. Known gaps even then: a node whose kubelet died keeps a `Ready` pod for about 40 s;
  a member that is not a labelled node (a learner, a removed node) is invisible; and a member that
  goes down while the machine being updated is switching is noticed at the next turn.
- **Between machines** the roll waits until the updated machine runs the system that was deployed,
  has **no failed unit that it did not already have** when its turn began (a baseline probe, compared
  by unit name: a unit that was failed before is not the update's doing, a new one is, and
  `degraded` is accepted only when every failed unit is in the baseline), systemd is past `starting`,
  **and** its node is `Ready` **with a heartbeat newer than the one it had when the wait began**
  (a lease newer than its recorded lease, or the `Ready` condition's `lastHeartbeatTime` newer than
  its recorded one; `kubectl get leases -n kube-node-lease`): `Ready=True` stays true for about 40 s
  after a kubelet stops reporting, so a switch that restarts rke2 for 20 s would otherwise pass on the condition it
  had before. `Unknown` is not `Ready`. A kind of heartbeat that could not be recorded when the wait
  began (the lease read failed, or the node had none) is recorded by the first look that has it, and
  proves nothing itself: only a later one of the same kind does, because a lease and a condition are
  rewritten at very different rates and must never be compared with each other. It polls every `update.poll_interval` (default 5 s, never above
  `update.ready_timeout`) up to `update.ready_timeout`; a kubectl that fails or a node that is not in
  the list is "not yet", never "healthy". (Each node's heartbeat is compared with **its own** earlier one, recorded when the wait began, so no clock is
  compared with another: not this machine's with the kubelets', in either direction, and not one
  node's with another's. A node that was already back when the wait began shows its next heartbeat
  within about 10 s.)
- **Drain** (`drain = true`): `kubectl drain --ignore-daemonsets --delete-emptydir-data --timeout
  <t>s` before the update, uncordon after the wait. Off by default because a NixOS switch does not
  reboot the node and RKE2's containers survive a restart of the service.
- **When a node is put back and when it is not.** After a failed or timed-out drain, or a quit before
  the update started (that is, before `clan` is spawned: resolving the fleet's pinned `clan` can take minutes and counts as before), the node is **uncordoned** (nothing was done to the machine). After a failed
  update or a machine that never became healthy, and after a quit while its update or its wait was
  under way, the node **stays cordoned**: the machine may be half updated or broken, and workloads
  must not return to it unasked. That is said in the Log tab, the status bar (`LEFT CORDONED: luna`),
  the machine's row of the action log, and, whenever auspex leaves, on stderr after the terminal is
  restored, each with the command, pasteable as it stands (the kubeconfig auspex used, quoted when
  it needs it): `kubectl --kubeconfig /home/me/.kube/config --context rke2 uncordon luna`. If an
  uncordon that auspex owes cannot be made, the machine is failed and says the same.
- `kubernetes.kubeconfig` takes a leading `~/` like `flake`; with no setting `$KUBECONFIG` is used
  when it names exactly one absolute path, else `~/.kube/config`. auspex never reads the file:
  `kubectl` does.

**The Log tab (`L`)** has the roll's own run first (the plan, one line per machine as it ends: `luna:
done`, `mars: refused: …`, `sol: skipped: …`, the totals) and one run for each machine (the checks'
warnings, the drain, `clan`'s output, the wait, the uncordon). A machine that was **drained** or
started has one row of the action log (kind `deploy`; there is no kind for a drain), opened at its
drain, whose detail says it was drained, what `clan` said, and how the machine ended (`done [node
drained first]`, `failed: … node luna is left cordoned …`); one that was refused or skipped before
that was never tried and has none.

**Quit, signal, panic.** `q` asks (`an update of all machines is running (1 of 6 over; now: luna,
alfrost); quit and stop it? …`). On `y` nothing new starts, the running `clan` processes are
cancelled (their process groups are killed and reaped), a drain in progress is cancelled and its
node uncordoned, a node whose update was under way stays cordoned and is named; auspex exits when
every machine has ended. A second `q` and a termination signal (SIGTERM, SIGHUP, SIGINT, SIGQUIT)
do not wait for the question: the roll is stopped the same way within 20 s (a second signal cuts
that wait short), and then **auspex itself runs `kubectl uncordon` for every node it cordoned for
an update that had not started**, one at a time, each bounded at 15 s, with a plain process. That
runs after the 20 s and before auspex cancels what is still in flight, so the core's own uncordon
of the same node may be running beside it: two uncordons of one node do no harm, and a node the
core's one gave back is not reported as still cordoned. The action-log rows still open are closed
after that, naming only the nodes really left cordoned. What a panic does depends on the thread it
happens on. **On the loop thread** (the one that runs the interface) auspex is gone: the panic hook
gives the terminal back (bracketed paste off, raw mode off, the main screen), the message is
printed, and the hook runs the same last-resort uncordon, each node bounded at 10 s, with no wait
for the roll. If a panic in a task came first, its message is printed too (`a panic in a task came
first: …`). **On any other thread** (an adapter's task or a helper) the hook touches neither the
terminal nor a node and prints nothing over the screen. It records the first message and the run
ends **in order, as for a signal**:
the status bar says `a task panicked, so auspex is stopping the run`, the roll is stopped within the
same 20 s (the screen is not redrawn meanwhile, and in raw mode Ctrl-C is a key, not a signal: send
SIGINT or SIGTERM from another terminal to cut the wait short), a drained node is uncordoned, the
last resort runs as above, auspex gives the terminal back, and it prints `a panic in a task ended
the run: <message>` (one line, escape sequences removed, at most 240 characters) and exits 2. The
default panic output, with its backtrace, is not printed over the screen: the message is in that
line. A panic that arrives while a `q` or a signal is already ending the run is printed too (`a task
panicked during the run`); the exit is 2 if it would have been 0, and stays 128 plus the signal
after a signal. A panic after the exit report was made (a task that outlived the reap grace) is
printed as any panic is, and makes the exit 2. A panic inside the core itself, while a task delivers
an answer to it, takes the core down: auspex cannot ask it to stop the roll, so it cancels what is
running and uncordons what it holds itself, says so, and exits 2. Only the first panic's message is
kept. The request of a task that panicked is answered as a failure (`internal error: …`), so the
core is not left waiting for it and a cancelled drain gets its answer and its node is uncordoned,
for: the one-shot effects (probes, lookups, the machine list, previews, checks, waits), the
evaluation, the cluster calls, an update or build (a panic after `clan` was started is a failure,
and the node stays cordoned and is named; before, it is a refusal), the freeze file's reads and
writes and its last look before an update. **Not covered:** the synchronous part of acting on an
answer (taking the fleet lock, registering a node, finding ssh), the fleet lock, `s`'s request,
the timers and the action-log writes. A panic there still ends the run in order, but the core's
request is answered only by the stop, up to 20 s later, and then the shutdown gives back what was
cordoned. **While `s` has the terminal**, neither kind writes an escape sequence into your ssh
session; a panic on the loop thread still runs the last-resort uncordon and prints its message as
plain text, and one in a task is printed as text, recorded, and ends the run when ssh ends (ssh is
never killed for it). `--plain` and `--json` have no terminal and no cordon: the default hook
prints the panic to stderr, and a panicked task answers its request as a failure.
What could not be given back, and what is left cordoned on purpose, is printed with the commands.
While a roll runs, `u`, `b`, `r` and `A` say so and do nothing.

## Freezing

A frozen machine is never updated: `u` refuses it, and `b` (which changes nothing on any machine)
does not care. There are two ways to freeze one.

- **`f` in the TUI** keeps the freeze in `$XDG_STATE_HOME/auspex/state.toml` (else
  `~/.local/state/auspex/state.toml`), not in `config.toml`: a home-manager config is a read-only
  store symlink. The file is keyed by the fleet flake's canonical path, so two fleets do not share
  freezes, and is private (directory 0700, file 0600) and written atomically. Delete it to forget
  every freeze.
- **`machines.<m>.frozen = true` in `config.toml`** (home-manager: `programs.auspex.settings.machines.<m>.frozen`)
  freezes declaratively. **`f` cannot undo it** (the notice says so): remove the setting and
  switch.

A freeze is applied the moment `f` is pressed, and saved beside it; if the save fails the machine
stays frozen for this run and the notice says it was not kept. It stays frozen, whatever a later
save answers, until you unfreeze it (`f`, then `y`: the file never held it, so nothing has to be
written), and the next save that works writes it into the file too. An unfreeze is applied only once it
has been saved (the file is what the next run believes), so a failed unfreeze leaves the machine
frozen and says so.

**Two terminals on one fleet.** Each auspex reads the file when it starts, and reads it again at
the `y` of every update and once more right before `clan` is started: a machine frozen from the
other terminal in the meantime is refused (`luna is frozen; f to unfreeze`) and the ❄ appears here
too, and an update whose file cannot be read at that point is refused too. The list does not
redraw other terminals' freezes until then (or until your own next `f`). What a terminal knows
only grows: a freeze it has seen stays until its own operator unfreezes it, so a machine
unfrozen in another terminal stays frozen here until you restart or unfreeze it here too (which
fails safe).

**The key is the fleet's path.** Freezes are filed under the fleet flake's canonical path
(`path:` and `git+file://` spellings of a plain directory are the same fleet as the directory).
A fleet that is **moved or renamed is another fleet**: its freezes are not found under the new
path (they stay in the file, under the old one), so freeze again, or edit the key in
`state.toml`. A reference with a query (`?dir=`) or a remote host is keyed as written.

**When it is not known which machines are frozen, nothing is updated.** If the state file is
corrupt, was written by a newer auspex, or there is no usable state directory (neither
`XDG_STATE_HOME` nor `HOME` is absolute), the freezes made with `f` are *unknown*, not none: the
status bar says `freezes unknown: u and f refused`, `u` is refused for every machine (a guess
of "nothing is frozen" would update the machine you froze), and `f` is refused too, so a corrupt
file is not overwritten nor a newer one downgraded. Fix or delete the file and restart. Freezes in
`config.toml` still apply, and a build is unaffected.

`--json` carries `frozen` (a boolean) on every machine, and `frozen` among its `badges`, for both
kinds of freeze. **When the freezes made with `f` are unknown, `frozen` is only what the config
declares, and the report says so in band**: an entry in `errors` with `kind` `freezes_unknown`
(and the run exits 1, as for any fleet error). A script that updates "every machine whose `frozen`
is false" must check `errors` first. The JSON version stays 1: the kind is new, as an unknown
`kind` is for any reader of version 1.

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
[machines.luna]
frozen = true       # freeze it declaratively: `u` refuses it, `U` skips it, `f` cannot undo it (see Freezing)

[rolling.taghmata]  # `U`: machines with this tag update one at a time (see Updating everything)
kubernetes = true
drain = false
etcd = false

[update]
parallel = 2        # machines with no rolling tag updated at once by `U`

[kubernetes]
kubeconfig = "~/.kube/config"

[forge]             # the CI badge for the fleet's current commit (see CI badge); omit it for no badge
url = "https://forge.example.org"   # https only, no credentials
repo = "owner/name"                 # optional; default: from the fleet checkout's `origin` remote
token_file = "~/.config/auspex/forge-token"   # optional; a read-only API token, never inline

[runners]           # the CI runners tab and status line (see CI runners); omit [[runners.forge]] for none
unit_glob = "gitea-runner-*"
memory_warn_percent = 90

[[runners.forge]]
url = "https://forge.example.org"
scope = "admin"                   # admin | user | org:<name> | repo:<owner>/<name>
token_file = "~/.config/auspex/forge-admin-token"

[runners.hosts.alpha]             # optional caps for the Prometheus figures
cpus = 2
memory = "4GiB"

[runners.machines]                # optional: runner name -> machine name
"alpha-docker" = "alpha"
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

## CI badge

With `[forge]` configured, auspex shows one badge for the fleet flake's **current commit** (`HEAD`
of the fleet checkout): the forge's *combined commit status* (Forgejo:
`GET /api/v1/repos/{owner}/{repo}/commits/{sha}/status`). It is the second item of the status bar,
`ci ok 0123abcd` (the badge and the first eight characters of the commit), and the top-level
`fleet_ci` object of `--json`: `{"commit": …, "state": …, "url": …, "reason": …}`, every key
always present, `null` when unknown.

| `state` | status bar | meaning |
|---|---|---|
| `success` | `ci ok` | every status the forge holds for the commit passed |
| `failure` | `ci failed` | a status failed or errored (Forgejo's `warning` too: a commit that is not green must not look like one) |
| `pending` | `ci pending` | a status is still running |
| `none` | `ci none` | the forge holds no status for that commit |
| `not_pushed` | `ci not pushed` | `HEAD` is ahead of its upstream (the same `git status` the update's checks use), so the forge cannot have a status for it; **nothing is asked** |
| `unknown` | `ci unknown` | it could not be found out; `reason` says why, and so does the Log tab (once per reason) and, in `--plain`/`--json` runs, stderr |
| `off` | nothing | `[forge]` is not configured: nothing is read and nothing is asked |

A **dirty** working tree is not a CI state: `HEAD`'s status is shown as it is (the plan of an update
already warns about uncommitted changes). While the request is out the bar says `ci …`.

**One request per refresh, and it never delays a row** (nor does a row delay it). The badge is asked beside
the machine list, not after it: `git rev-parse HEAD`, the update checks' `git status` and, only
when `forge.repo` is not set, `git remote get-url origin`, then one `GET` with the HTTP adapter's
timeout (`timeouts.probe`), its no-redirect rule and a 1 MiB cap on the answer (at most 1,000
statuses). Headless mode waits for the badge like it waits for Prometheus, bounded by the same
global deadline, which makes it `unknown`. Only the *combined* status is read, not the Actions
runs API, and there is one badge for the fleet, not one per machine.

**Which repository.** `forge.repo` if set. Otherwise the fleet checkout's `origin`, in any of
`ssh://[user@]host[:port]/owner/name.git`, `git@host:owner/name.git` and
`https://[user[:password]@]host[:port]/owner/name.git`. If that remote's **host differs from
`forge.url`'s**, auspex does not guess (a mirror, another forge): the badge is `unknown: set
forge.repo`. A password in a remote is dropped by the parser and appears nowhere. A remote that
is not one of these forms, or whose path is not exactly `owner/name` (a forge under a sub-path
cannot be told from a group), says the same.

**The token.** `token_file` names a file holding a read-only API token, read **once, at start**
(a new token needs a restart), sent as `Authorization: token …`. auspex refuses the file, saying
why, when it is **accessible to group or others** (any of the `0o077` mode bits: `chmod 600`), is
not owned by you, is not a regular file, is empty or over 4 KiB, or holds anything but printable
ASCII besides one trailing newline (which is stripped). The check is on the open file, so the
file checked is the file read; a symlink is followed and its target checked. A refused token
makes the badge `unknown` and **no request is made**: asking anonymously instead would hide that
your token is not in use. Without `token_file` the forge is asked anonymously, which is how a
public repository is read. The token is held in a type whose `Debug` is `Token(<redacted>)`, with
no `Display` or `Serialize`; no error quotes the forge's answer (a forge can echo a credential);
every link, transport error (a TLS error names the certificate's names) and line of git's stderr
that reaches the badge is checked for the token, in any case and percent-encoded (repeatedly, up
to a bound), and replaced when it holds it, and git's lines are also sanitised and cut to 300
characters (a copy a forge disguises in a way of its own invention, base64 or a split, cannot be
recognised by any such check); the `Authorization` header is marked sensitive; redirects are not followed, so a forge cannot
send the token on to another host; and a token is sent only over `https` (or to a loopback
address, which only a test uses: `forge.url` itself must be `https`). The token never reaches
`config.toml` through Home Manager either: `programs.auspex.settings.forge.token_file` is a path,
and one inside the Nix store (world-readable) is refused at evaluation.

## CI runners

With a `[[runners.forge]]` configured, `R` opens the **Runners tab** and the status bar says
`runners: 7/8 online, 2 busy, 3 queued`. The tab is read-only: nothing in it acts on a runner (`q`
or `Esc` closes it, the scroll keys move it, every other key does nothing until it is closed).
`--json` carries the same data as `runners` (one entry per runner) and `runner_forges` (what each
forge gave beside rows: its queue, or why it failed).

```toml
[runners]
unit_glob = "gitea-runner-*"   # which systemd units are runner units (NixOS: the default)
memory_warn_percent = 90       # badge a row from this share of its host's memory cap

[[runners.forge]]
url = "https://forge.example.org"
scope = "admin"                # admin | user | org:<name> | repo:<owner>/<name>
token_file = "~/.config/auspex/forge-admin-token"   # read-only; `admin` needs read:admin

[runners.hosts.alpha]          # optional caps for the Prometheus figures
cpus = 2
memory = "4GiB"

[runners.machines]             # optional: runner name -> machine name
"alpha-docker" = "alpha"
```

**Three sources, joined.** A row is `(forge, runner name)`.

1. **The forge** (Forgejo 15 / gitea 1.22): `GET /api/v1/{admin,user,orgs/<org>,repos/<owner>/<repo>}/actions/runners`
   for each runner's `name`, `status` (`offline`, `idle`, `active`), `labels` and `version`, and
   `…/actions/runners/jobs` for the jobs that are waiting. The listing is paged (50 a page) and is
   read until a page is empty or has nothing it has not seen (a forge that ignores `page` is not
   read twice), and is asked with `visible=true`: the API's default is only the runners the scope
   owns directly, and `visible` is what makes `admin` list every runner of the instance (and `org:`
   and `repo:` the global ones that serve them too). A forge answers an empty list as `null`, which
   is read as the empty list it is.
2. **ssh**, NixOS only: the probe lists the systemd services matching `unit_glob`
   (`systemctl list-units --type=service --all -- <glob>`, loaded units only) in the round trip it
   makes anyway. The glob is checked (letters, digits and `: _ . @ - * ? [ ]`, not starting with `-`:
   it is written into a shell script, and systemctl reads options after the verb, so it also comes
   after `--`) and the unit names and states are checked like the failed-unit list's: one
   malformed entry makes the whole list unreadable, never half-believed. So does a list of more
   than 64 units, which is refused and not cut: a cut list read as the whole would hide the runner
   whose unit was cut. darwin reports nothing.
   The default matches what the NixOS module `services.gitea-actions-runner` names its units,
   `gitea-runner-<name>.service`; without a `[[runners.forge]]` the probe lists no units.
3. **Prometheus**, when `[prometheus]` is configured: the host's memory in use (total less
   available) against `[runners.hosts.<machine>] memory`, and its one-minute load against `cpus`.
   **No cap is assumed**: a host with no entry shows no figure and no badge, however full it is.
   The load average is a stand-in for CPU use (it is what the fleet-wide query already carries).

**Joining.** A runner is on the machine `[runners.machines]` names for it, else the machine whose
name it equals or begins with `<machine>-` (the longest such machine). Its unit is that machine's
runner unit whose instance name (`gitea-runner-<instance>.service`, the glob's literal prefix and
systemd's `\x2d` escapes removed) equals the runner's name or its name without `<machine>-`. Failing
that, a machine with **exactly one runner unit and exactly one runner** (the same runner listed by
two forges is one runner) has them be the same thing: that is the common NixOS layout, one
`instances.default` whose unit is `gitea-runner-default.service` and whose runner is named after the
host. With more of either, which is whose is not known and none is guessed. A machine whose probe
did not report units has no unit facts and is never called "unit down".

**Badges**, per row (the first three make it red; in `--json`, `badges`):

| badge | when |
|---|---|
| `offline` | the forge says the runner is offline |
| `unit down` (`forge_up_unit_down`) | the forge says online (idle or busy) and its unit is inactive, failed, deactivating or in maintenance |
| `unit up` (`forge_down_unit_up`) | the forge says offline and its unit is active or reloading |
| `memory` (`memory_near_cap`) | the host's memory in use is at or above `memory_warn_percent` of its cap (not red) |
| `forge error` | the forge could not be asked: one row for the whole entry, with the reason |

A unit that is `activating` (starting, or waiting to restart) is neither up nor down, so it earns
no mismatch badge. A forge status the API does not define is `unknown`: not online, not offline.

**What the forge's API does not say, and so the tab does not.** `ActionRunner` has no field for the
job a runner is running, and the waiting-jobs payload has no time at all, so:

- a busy runner is **busy**; which job it runs is not known and is not guessed;
- the queue is a **count** of waiting jobs (those with status `waiting`, or none; an empty list, or
  `null`, is no jobs); there is no "oldest has waited 12 minutes". The request carries no `labels`
  filter, which the API documents as "a comma separated list of labels to search for": **whether
  Forgejo then matches every waiting job or only those whose labels fit the ones given has not been
  checked against a real forge**, and if it is the second the count is of jobs no label filter
  excludes, which may be fewer than are waiting;
- there is **no "job running too long" badge**: the API does not say when a job began. It will be
  added when the API does.

**A failing forge** shows on its own row, with the reason, and never delays a machine row or the
refresh: every request is bounded (one timeout, `timeouts.probe`, for the whole listing and one
for the queue, side by side; 1 MiB a page, 4 MiB for the jobs list; at most 1,000 runners over 20 pages and 10,000 jobs,
counted while the body is read, and past a bound the answer is an error, never a truncated list
that would read as the truth; the jobs list may be 4 MiB, since 10,000 jobs of a few hundred bytes
each do not fit in a page's allowance). The runners and the queue fail independently: a token that may list
runners and not jobs still shows the runners, with the queue `?`. The forge's reason is written to
the Log tab once.

**The token** is read and guarded exactly as the CI badge's (see above): per entry, from its
`token_file`, refused with the reason when others can read it, never quoted, sent only over
`https`, never followed through a redirect. Every reason and every runner name, label and version
the forge sent is checked for it and replaced when it holds it. A refused token makes that entry
a `forge error` row and sends nothing to it.

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
