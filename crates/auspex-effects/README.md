# `auspex-effects`

auspex's adapters: everything that touches the outside world. The ssh probe, the machine
list (`nix eval`), the evaluation (`nix-eval-jobs`), the eval cache, Prometheus, the narinfo
"cached" badge, and DNS. This crate is the only one in auspex with tokio.

**Admission rule:** a function belongs here when it *obtains* something: it runs a process,
reads or writes a file, asks a server, or resolves a name. It takes everything as arguments,
hands what it got to `auspex-domain` to be parsed and judged, and returns the domain's value.
Parsing and status rules stay in `auspex-domain`, and deciding what to ask stays in the Crux
core. This crate never depends on the core: the TUI shell maps each core `Effect` to a
function here.

Every function has a timeout and takes a `CancellationToken`. A cancelled call kills the
whole process group of anything it spawned, reaps it, and returns `Err(Cancelled)`. No
process is run through a shell, and no name or path is spliced into argv or Nix source:
the crate rustdoc and each module say how.

Tests: `tests/ssh.rs` (fake `ssh` scripts, control-directory checks), `tests/nix_fake.rs`
(fake `nix` and `nix-eval-jobs`), `tests/nix_fixture.rs` (the real tools against
`tests/fixtures/flake`, skipped where `nix` or `nix-eval-jobs` is missing),
`tests/cache.rs` (temporary git repositories), and `tests/http.rs` (a server on 127.0.0.1).
None contacts a machine or the network beyond loopback.
