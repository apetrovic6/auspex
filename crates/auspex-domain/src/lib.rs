//! auspex's pure rules: everything about a Clan fleet that can be decided without touching
//! the world.
//!
//! **Admission rule.** A function belongs here when it takes values and returns values: no
//! I/O, no async, no process spawning, no filesystem, no clock, no environment. Anything that
//! *obtains* an input (running ssh, reading `flake.lock`, asking Prometheus, resolving a
//! hostname, reading the time) is an effect and lives in `auspex-effects`; anything that
//! *sequences* effects lives in the Crux core. If a rule here needs "now", it takes the age
//! as an argument.
//!
//! The reason for the rule is that every parser and status rule here fails quietly when it is
//! wrong — a probe that reads `/run/current-system` as installed when it is missing, a target
//! that passes `-oProxyCommand=…` to ssh — and pure functions are the only kind that can be
//! tested exhaustively against fixtures.
//!
//! Modules, one per concern:
//!
//! - [`fleet`]: machines, the inventory parser, ssh targets, their resolution and the
//!   ssh argument vector, and the homelab-safety lints.
//! - [`observe`]: the probe scripts, their parser, and merging Prometheus samples.
//! - [`eval`]: `nix-eval-jobs` lines, the eval cache key and the cache entry.
//! - [`status`]: the status-badge rules.
//! - [`report`]: the versioned `--json` contract.
//! - [`text`]: making remote-controlled text safe for a terminal, for adapters too.

pub mod eval;
pub mod fleet;
pub mod observe;
pub mod report;
pub mod status;
pub mod text;
