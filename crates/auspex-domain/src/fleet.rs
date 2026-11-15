//! Machines, ssh targets, and the homelab-safety lints.
//!
//! The one security-sensitive part of the crate is here: a [`Target`] ends up in ssh's argv,
//! so its parser is a whitelist, and [`ssh_args`] is the only way to turn one into
//! arguments. A user or host that could be read as an option (`-oProxyCommand=…`), or that
//! carries whitespace or control characters, never makes it out of [`Target::parse`].

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------------------
// Machines and the inventory
// ---------------------------------------------------------------------------------------

/// Which kind of system a machine runs. Decides which probe script and which flake output
/// (`nixosConfigurations` or `darwinConfigurations`) apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineClass {
    /// NixOS: systemd, `/run/booted-system`, `nixos-version`.
    Nixos,
    /// nix-darwin: no systemd, `darwin-version`.
    Darwin,
}

impl MachineClass {
    /// The flake output attribute set this class's configurations live in.
    #[must_use]
    pub const fn flake_output(self) -> &'static str {
        match self {
            Self::Nixos => "nixosConfigurations",
            Self::Darwin => "darwinConfigurations",
        }
    }
}

/// One machine, as the inventory describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// The machine's name: the inventory key, and the last-resort ssh host.
    pub name: String,
    /// NixOS or darwin. The inventory's `machineClass`; `nixos` when missing or null, which
    /// is Clan's own default.
    pub class: MachineClass,
    /// The inventory's tags, in inventory order. Empty when missing or null.
    pub tags: Vec<String>,
    /// The inventory's `deploy.targetHost`, unparsed. `None` when missing, null or empty.
    pub target_host: Option<String>,
    /// The inventory's `deploy.buildHost`, unparsed. `None` when missing, null or empty.
    pub build_host: Option<String>,
}

/// The inventory JSON could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InventoryError {
    /// Not JSON, or not the expected shape (an object of objects, string fields).
    #[error("inventory is not valid: {0}")]
    Malformed(String),
    /// A `machineClass` that is neither `nixos` nor `darwin`. Refused rather than guessed,
    /// because the class picks the probe: a darwin probe on NixOS reports no systemd state,
    /// and the other way round reports nothing at all.
    #[error("machine {machine}: unknown machineClass {class:?}")]
    UnknownClass {
        /// The machine whose class was not recognised.
        machine: String,
        /// The class as written.
        class: String,
    },
}

#[derive(Deserialize)]
struct RawMachine {
    #[serde(default)]
    deploy: Option<RawDeploy>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default, rename = "machineClass")]
    machine_class: Option<String>,
}

#[derive(Deserialize)]
struct RawDeploy {
    #[serde(default, rename = "targetHost")]
    target_host: Option<String>,
    #[serde(default, rename = "buildHost")]
    build_host: Option<String>,
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

/// Parses the inventory JSON.
///
/// That is the value of `clanInternals.inventoryClass.inventory.machines`, an object keyed by
/// machine name. Missing or null `deploy`, `targetHost`, `buildHost`, `tags` and
/// `machineClass` are all tolerated; unknown fields are ignored. Machines come back sorted
/// by name.
///
/// # Errors
///
/// [`InventoryError::Malformed`] when the JSON does not have that shape, and
/// [`InventoryError::UnknownClass`] for a class other than `nixos` or `darwin`.
pub fn parse_inventory(json: &str) -> Result<Vec<Machine>, InventoryError> {
    let raw: BTreeMap<String, Option<RawMachine>> =
        serde_json::from_str(json).map_err(|e| InventoryError::Malformed(e.to_string()))?;
    raw.into_iter()
        .map(|(name, m)| {
            let m = m.unwrap_or(RawMachine {
                deploy: None,
                tags: None,
                machine_class: None,
            });
            let class = match m.machine_class.as_deref() {
                None | Some("nixos") => MachineClass::Nixos,
                Some("darwin") => MachineClass::Darwin,
                Some(other) => {
                    return Err(InventoryError::UnknownClass {
                        machine: name,
                        class: other.to_owned(),
                    });
                }
            };
            let (target_host, build_host) = m
                .deploy
                .map_or((None, None), |d| (d.target_host, d.build_host));
            Ok(Machine {
                name,
                class,
                tags: m.tags.unwrap_or_default(),
                target_host: non_empty(target_host),
                build_host: non_empty(build_host),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------------------

/// An ssh target: `[user@]host[:port][?Opt=val&Opt2=val]`.
///
/// Only constructed by [`Target::parse`], so every value of this type has passed the
/// whitelist. `host` is stored without brackets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    user: Option<String>,
    host: String,
    port: Option<u16>,
    options: Vec<(String, String)>,
}

/// Why a target string was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetError {
    /// Nothing where the host should be.
    #[error("empty host")]
    EmptyHost,
    /// `user@` with nothing before the `@`.
    #[error("empty user")]
    EmptyUser,
    /// A user outside `[A-Za-z0-9._-]`, or starting with `-`.
    #[error("invalid user {user:?}")]
    BadUser {
        /// The user as written.
        user: String,
    },
    /// A host that is neither a hostname (`[A-Za-z0-9._-]`, not starting with `-` or `.`)
    /// nor an IP literal.
    #[error("invalid host {host:?}")]
    BadHost {
        /// The host as written.
        host: String,
    },
    /// A port that is not a number in 1..=65535.
    #[error("invalid port {port:?}")]
    BadPort {
        /// The port as written.
        port: String,
    },
    /// `[` without a matching `]`, or text after `]` other than `:port`.
    #[error("malformed bracketed host")]
    BadBrackets,
    /// An option that is not `Name=value` with an alphanumeric name and a value free of
    /// whitespace, control characters and a leading `-`.
    #[error("invalid ssh option {option:?}")]
    BadOption {
        /// The option as written.
        option: String,
    },
    /// An option auspex refuses: one that runs a local command, or one auspex sets itself
    /// (ssh keeps the *first* value of an option, so a target's copy would be ignored, and
    /// silently ignoring configuration is worse than refusing it).
    #[error("ssh option {name} is not allowed in a target ({why})")]
    ForbiddenOption {
        /// The option name as written.
        name: String,
        /// Why it is refused.
        why: String,
    },
}

/// Options that make ssh execute a local command. A target comes from the fleet's
/// inventory or from auspex's config, and observing a fleet must never run code from either:
/// set these in `~/.ssh/config`, which auspex honours and does not parse.
const COMMAND_OPTIONS: &[&str] = &[
    "ProxyCommand",
    "LocalCommand",
    "PermitLocalCommand",
    "KnownHostsCommand",
    "RemoteCommand",
    "Match",
];

/// Options [`ssh_args`] sets itself.
const OWNED_OPTIONS: &[&str] = &[
    "BatchMode",
    "ConnectTimeout",
    "ServerAliveInterval",
    "ControlMaster",
    "ControlPath",
    "ControlPersist",
];

const fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

fn validate_user(user: &str) -> Result<(), TargetError> {
    if user.is_empty() {
        return Err(TargetError::EmptyUser);
    }
    if user.starts_with('-') || !user.chars().all(is_name_char) {
        return Err(TargetError::BadUser {
            user: user.to_owned(),
        });
    }
    Ok(())
}

/// An IPv6 literal, optionally with a `%zone`.
fn is_ipv6_literal(host: &str) -> bool {
    let (addr, zone) = host
        .split_once('%')
        .map_or((host, None), |(a, z)| (a, Some(z)));
    if let Some(zone) = zone
        && (zone.is_empty() || zone.starts_with('-') || !zone.chars().all(is_name_char))
    {
        return false;
    }
    addr.parse::<Ipv6Addr>().is_ok()
}

fn validate_hostname(host: &str) -> Result<(), TargetError> {
    if host.is_empty() {
        return Err(TargetError::EmptyHost);
    }
    if host.starts_with('-') || host.starts_with('.') || !host.chars().all(is_name_char) {
        return Err(TargetError::BadHost {
            host: host.to_owned(),
        });
    }
    Ok(())
}

fn parse_port(port: &str) -> Result<u16, TargetError> {
    let bad = || TargetError::BadPort {
        port: port.to_owned(),
    };
    // `u16::from_str` accepts a leading `+`; a port is digits only.
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(bad()),
        Ok(p) => Ok(p),
    }
}

fn parse_option(opt: &str) -> Result<(String, String), TargetError> {
    let bad = || TargetError::BadOption {
        option: opt.to_owned(),
    };
    let (name, value) = opt.split_once('=').ok_or_else(bad)?;
    let name_ok = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric());
    let value_ok = !value.is_empty()
        && !value.starts_with('-')
        && !value.chars().any(|c| c.is_whitespace() || c.is_control());
    if !name_ok || !value_ok {
        return Err(bad());
    }
    // ssh option names are case-insensitive, so the deny lists are too.
    if let Some(n) = COMMAND_OPTIONS
        .iter()
        .find(|n| n.eq_ignore_ascii_case(name))
    {
        return Err(TargetError::ForbiddenOption {
            name: name.to_owned(),
            why: format!("{n} runs a local command; set it in ~/.ssh/config"),
        });
    }
    if let Some(n) = OWNED_OPTIONS.iter().find(|n| n.eq_ignore_ascii_case(name)) {
        return Err(TargetError::ForbiddenOption {
            name: name.to_owned(),
            why: format!("auspex sets {n} itself"),
        });
    }
    Ok((name.to_owned(), value.to_owned()))
}

impl Target {
    /// Parses `[user@]host[:port][?Opt=val&Opt2=val]`.
    ///
    /// - IPv6 with a port must be bracketed: `root@[fd00::1]:2222`. Brackets without a port
    ///   are accepted too.
    /// - Unbracketed, exactly one `:` means `host:port`; more than one means a bare IPv6
    ///   address (`fd00::1`), never an address with a port.
    /// - Users and hostnames are `[A-Za-z0-9._-]` and must not start with `-`; anything
    ///   else, including whitespace and control characters, is refused here so it can never
    ///   reach ssh's argv.
    ///
    /// # Errors
    ///
    /// A [`TargetError`] naming the part that was refused.
    pub fn parse(s: &str) -> Result<Self, TargetError> {
        let (dest, query) = s.split_once('?').map_or((s, None), |(d, q)| (d, Some(q)));
        let options = match query {
            None => Vec::new(),
            Some(q) => q
                .split('&')
                .map(parse_option)
                .collect::<Result<Vec<_>, _>>()?,
        };

        let (user, hostport) = match dest.split_once('@') {
            Some((u, h)) => {
                validate_user(u)?;
                (Some(u.to_owned()), h)
            }
            None => (None, dest),
        };

        let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
            let (host, after) = inner.split_once(']').ok_or(TargetError::BadBrackets)?;
            let port = match after {
                "" => None,
                _ => Some(parse_port(
                    after.strip_prefix(':').ok_or(TargetError::BadBrackets)?,
                )?),
            };
            if host.is_empty() {
                return Err(TargetError::EmptyHost);
            }
            if !is_ipv6_literal(host) {
                return Err(TargetError::BadHost {
                    host: host.to_owned(),
                });
            }
            (host, port)
        } else {
            match hostport.matches(':').count() {
                0 => {
                    validate_hostname(hostport)?;
                    (hostport, None)
                }
                1 => {
                    let (h, p) = hostport.split_once(':').unwrap_or((hostport, ""));
                    validate_hostname(h)?;
                    (h, Some(parse_port(p)?))
                }
                _ => {
                    if !is_ipv6_literal(hostport) {
                        return Err(TargetError::BadHost {
                            host: hostport.to_owned(),
                        });
                    }
                    (hostport, None)
                }
            }
        };

        Ok(Self {
            user,
            host: host.to_owned(),
            port,
            options,
        })
    }

    /// The user, if the target names one.
    #[must_use]
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// The host, without brackets.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port, if the target names one.
    #[must_use]
    pub const fn port(&self) -> Option<u16> {
        self.port
    }

    /// The query options, in the order written.
    #[must_use]
    pub fn options(&self) -> &[(String, String)] {
        &self.options
    }

    /// The host as an IP address, when it is a literal rather than a hostname.
    #[must_use]
    pub fn ip(&self) -> Option<IpAddr> {
        let addr = self.host.split_once('%').map_or(&*self.host, |(a, _)| a);
        addr.parse().ok()
    }

    /// What ssh is given as the destination: `user@host`, IPv6 unbracketed (the port goes
    /// in `-p`).
    #[must_use]
    pub fn destination(&self) -> String {
        self.user
            .as_ref()
            .map_or_else(|| self.host.clone(), |u| format!("{u}@{}", self.host))
    }
}

impl FromStr for Target {
    type Err = TargetError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for Target {
    /// The canonical spelling, which [`Target::parse`] reads back to an equal value. IPv6 is
    /// bracketed when, and only when, a port is present.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(u) = &self.user {
            write!(f, "{u}@")?;
        }
        match self.port {
            Some(p) if self.host.contains(':') => write!(f, "[{}]:{p}", self.host)?,
            Some(p) => write!(f, "{}:{p}", self.host)?,
            None => f.write_str(&self.host)?,
        }
        for (i, (k, v)) in self.options.iter().enumerate() {
            write!(f, "{}{k}={v}", if i == 0 { '?' } else { '&' })?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------------------

/// Where a machine's target came from. The detail panel shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetSource {
    /// The per-machine override in auspex's config.
    Override,
    /// The inventory's `deploy.targetHost`.
    Inventory,
    /// Nothing else was set, so the machine's name.
    MachineName,
}

/// A machine's target and which source won.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// The parsed target.
    pub target: Target,
    /// Which source it came from.
    pub source: TargetSource,
}

/// The winning source's value did not parse. Resolution does **not** fall back to the next
/// source: a broken override silently replaced by the inventory's host is how a deploy goes
/// to the wrong machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{target_source:?} target {raw:?}: {error}")]
pub struct ResolveError {
    /// Which source won.
    pub target_source: TargetSource,
    /// Its value as written.
    pub raw: String,
    /// Why it was refused.
    pub error: TargetError,
}

/// Picks a machine's target: `override_target` (from config), else the inventory's
/// `deploy.targetHost`, else the machine name.
///
/// # Errors
///
/// [`ResolveError`] when the winning value does not parse.
pub fn resolve_target(
    machine: &Machine,
    override_target: Option<&str>,
) -> Result<ResolvedTarget, ResolveError> {
    let (raw, source) = override_target.map_or_else(
        || {
            machine
                .target_host
                .as_deref()
                .map_or((machine.name.as_str(), TargetSource::MachineName), |t| {
                    (t, TargetSource::Inventory)
                })
        },
        |o| (o, TargetSource::Override),
    );
    Target::parse(raw)
        .map(|target| ResolvedTarget { target, source })
        .map_err(|error| ResolveError {
            target_source: source,
            raw: raw.to_owned(),
            error,
        })
}

// ---------------------------------------------------------------------------------------
// ssh argv
// ---------------------------------------------------------------------------------------

/// The control-socket directory was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "control path directory {dir:?} must be absolute and free of whitespace, control characters and '%'"
)]
pub struct ControlDirError {
    /// The directory as given.
    pub dir: String,
}

/// The remote command to append after [`ssh_args`] to run a probe.
///
/// The script goes on ssh's stdin. Feeding it on stdin, rather than as the command, means it never passes through the
/// remote login shell's quoting.
pub const PROBE_REMOTE_COMMAND: &str = "/bin/sh -s";

/// ssh's arguments for `target`, without the program name and without the remote command:
///
/// ```text
/// -T -o BatchMode=yes -o ConnectTimeout=5 -o ServerAliveInterval=5
///    -o ControlMaster=auto -o ControlPath=<control_dir>/%C -o ControlPersist=60s
///    [-o Opt=val]… [-p port] -- [user@]host
/// ```
///
/// auspex's own options come first because ssh keeps the first value it sees for an option.
/// `control_dir` is `$XDG_RUNTIME_DIR/auspex`; creating it with mode 0700 is the caller's
/// job.
///
/// # Errors
///
/// [`ControlDirError`] when `control_dir` is relative or contains whitespace, a control
/// character or `%` (which ssh would expand as a token).
pub fn ssh_args(target: &Target, control_dir: &str) -> Result<Vec<String>, ControlDirError> {
    if !control_dir.starts_with('/')
        || control_dir
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '%')
    {
        return Err(ControlDirError {
            dir: control_dir.to_owned(),
        });
    }
    let dir = control_dir.trim_end_matches('/');
    let mut args: Vec<String> = [
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ControlMaster=auto",
        "-o",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(format!("ControlPath={dir}/%C"));
    args.push("-o".to_owned());
    args.push("ControlPersist=60s".to_owned());
    for (k, v) in &target.options {
        args.push("-o".to_owned());
        args.push(format!("{k}={v}"));
    }
    if let Some(p) = target.port {
        args.push("-p".to_owned());
        args.push(p.to_string());
    }
    args.push("--".to_owned());
    args.push(target.destination());
    Ok(args)
}

// ---------------------------------------------------------------------------------------
// Lints
// ---------------------------------------------------------------------------------------

/// A homelab-safety warning about one machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Lint {
    /// The target is a hostname, not an IP literal. A wildcard DNS record answers for every
    /// name, so a hostname target once deployed the wrong closure onto the main server.
    HostnameTarget {
        /// The hostname.
        host: String,
    },
    /// This machine's target resolves to an IP another machine's target also resolves to.
    /// Deploying is refused while this holds.
    SharedIp {
        /// The shared address.
        ip: IpAddr,
        /// The other machines with that address, sorted.
        with: Vec<String>,
    },
}

/// The hostname lint: `Some` when the target's host is not an IP literal.
#[must_use]
pub fn lint_hostname(target: &Target) -> Option<Lint> {
    target.ip().is_none().then(|| Lint::HostnameTarget {
        host: target.host.clone(),
    })
}

/// Machines whose targets share an IP address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedIp {
    /// The address.
    pub ip: IpAddr,
    /// The machines resolving to it, sorted, at least two.
    pub machines: Vec<String>,
}

/// The addresses more than one machine resolves to, sorted by address.
///
/// Takes each machine's resolved addresses, since resolution is an effect. IPv4-mapped IPv6 (`::ffff:a.b.c.d`) counts as
/// the IPv4 address, so a hostname returning the mapped form cannot hide a collision.
#[must_use]
pub fn shared_ips(resolved: &BTreeMap<String, Vec<IpAddr>>) -> Vec<SharedIp> {
    let mut by_ip: BTreeMap<IpAddr, Vec<String>> = BTreeMap::new();
    for (machine, ips) in resolved {
        for ip in ips {
            let names = by_ip.entry(ip.to_canonical()).or_default();
            if !names.contains(machine) {
                names.push(machine.clone());
            }
        }
    }
    by_ip
        .into_iter()
        .filter(|(_, m)| m.len() > 1)
        .map(|(ip, mut machines)| {
            machines.sort();
            SharedIp { ip, machines }
        })
        .collect()
}

/// The [`Lint::SharedIp`] entries for one machine, from the output of [`shared_ips`].
#[must_use]
pub fn shared_ip_lints(machine: &str, shared: &[SharedIp]) -> Vec<Lint> {
    shared
        .iter()
        .filter(|s| s.machines.iter().any(|m| m == machine))
        .map(|s| Lint::SharedIp {
            ip: s.ip,
            with: s
                .machines
                .iter()
                .filter(|m| *m != machine)
                .cloned()
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Target {
        Target::parse(s).unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }

    #[test]
    fn target_forms() {
        type Case<'a> = (
            &'a str,
            Option<&'a str>,
            &'a str,
            Option<u16>,
            &'a [(&'a str, &'a str)],
        );
        let cases: &[Case<'_>] = &[
            ("host", None, "host", None, &[]),
            ("root@host", Some("root"), "host", None, &[]),
            ("host:2222", None, "host", Some(2222), &[]),
            ("root@host:22", Some("root"), "host", Some(22), &[]),
            ("10.0.0.1", None, "10.0.0.1", None, &[]),
            (
                "root@10.0.0.1:2222",
                Some("root"),
                "10.0.0.1",
                Some(2222),
                &[],
            ),
            ("fd00::1", None, "fd00::1", None, &[]),
            ("root@fd00::1", Some("root"), "fd00::1", None, &[]),
            (
                "root@[fd00::1]:2222",
                Some("root"),
                "fd00::1",
                Some(2222),
                &[],
            ),
            ("[fd00::1]", None, "fd00::1", None, &[]),
            ("fe80::1%eth0", None, "fe80::1%eth0", None, &[]),
            (
                "root@host?ProxyJump=jump&StrictHostKeyChecking=no",
                Some("root"),
                "host",
                None,
                &[("ProxyJump", "jump"), ("StrictHostKeyChecking", "no")],
            ),
            (
                "root@[fd00::1]:22?ProxyJump=me@jump:2200",
                Some("root"),
                "fd00::1",
                Some(22),
                &[("ProxyJump", "me@jump:2200")],
            ),
            ("terra.omnissiah", None, "terra.omnissiah", None, &[]),
        ];
        for (input, user, host, port, opts) in cases {
            let got = t(input);
            assert_eq!(got.user(), *user, "{input}");
            assert_eq!(got.host(), *host, "{input}");
            assert_eq!(got.port(), *port, "{input}");
            let want: Vec<(String, String)> = opts
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            assert_eq!(got.options(), want.as_slice(), "{input}");
            // Display reads back to the same value.
            assert_eq!(t(&got.to_string()), got, "{input}");
        }
    }

    #[test]
    fn display_brackets_ipv6_only_with_port() {
        assert_eq!(t("root@[fd00::1]:2222").to_string(), "root@[fd00::1]:2222");
        assert_eq!(t("[fd00::1]").to_string(), "fd00::1");
        assert_eq!(t("fd00::1").to_string(), "fd00::1");
        assert_eq!(t("host:22?A=b&C=d").to_string(), "host:22?A=b&C=d");
    }

    #[test]
    fn one_colon_is_a_port_more_is_ipv6() {
        assert_eq!(t("fd00::22").port(), None);
        assert_eq!(t("fd00::22").host(), "fd00::22");
        // `fd00:22` is host `fd00`, port 22 — one colon, so host:port, as the plan says.
        assert_eq!(t("fd00:22").host(), "fd00");
        assert_eq!(t("fd00:22").port(), Some(22));
        // Not an address and not host:port.
        assert!(matches!(
            Target::parse("a:b:c"),
            Err(TargetError::BadHost { .. })
        ));
    }

    #[test]
    fn rejected_inputs() {
        type Check = fn(&TargetError) -> bool;
        let bad: &[(&str, Check)] = &[
            ("", |e| *e == TargetError::EmptyHost),
            ("root@", |e| *e == TargetError::EmptyHost),
            ("@host", |e| *e == TargetError::EmptyUser),
            (":22", |e| *e == TargetError::EmptyHost),
            ("[]:22", |e| *e == TargetError::EmptyHost),
            ("-oProxyCommand=touch /tmp/x", |e| {
                matches!(e, TargetError::BadHost { .. })
            }),
            ("-host", |e| matches!(e, TargetError::BadHost { .. })),
            ("-l@host", |e| matches!(e, TargetError::BadUser { .. })),
            ("-oProxyCommand=x@host", |e| {
                matches!(e, TargetError::BadUser { .. })
            }),
            ("root@-host", |e| matches!(e, TargetError::BadHost { .. })),
            ("ro ot@host", |e| matches!(e, TargetError::BadUser { .. })),
            ("root@ho st", |e| matches!(e, TargetError::BadHost { .. })),
            ("root@host\n", |e| matches!(e, TargetError::BadHost { .. })),
            ("root@host\tx", |e| matches!(e, TargetError::BadHost { .. })),
            ("root\u{0}@host", |e| {
                matches!(e, TargetError::BadUser { .. })
            }),
            ("root@host\u{1b}[2J", |e| {
                matches!(e, TargetError::BadHost { .. })
            }),
            ("root@\u{a0}host", |e| {
                matches!(e, TargetError::BadHost { .. })
            }),
            ("a@b@c", |e| matches!(e, TargetError::BadHost { .. })),
            ("root@.host", |e| matches!(e, TargetError::BadHost { .. })),
            ("host:", |e| matches!(e, TargetError::BadPort { .. })),
            ("host:0", |e| matches!(e, TargetError::BadPort { .. })),
            ("host:65536", |e| matches!(e, TargetError::BadPort { .. })),
            ("host:+22", |e| matches!(e, TargetError::BadPort { .. })),
            ("host:-22", |e| matches!(e, TargetError::BadPort { .. })),
            ("host:2x", |e| matches!(e, TargetError::BadPort { .. })),
            ("[fd00::1]:", |e| matches!(e, TargetError::BadPort { .. })),
            ("[fd00::1]:99999", |e| {
                matches!(e, TargetError::BadPort { .. })
            }),
            ("[fd00::1", |e| *e == TargetError::BadBrackets),
            ("[fd00::1]22", |e| *e == TargetError::BadBrackets),
            ("[host]:22", |e| matches!(e, TargetError::BadHost { .. })),
            ("[-oX=y]:22", |e| matches!(e, TargetError::BadHost { .. })),
            ("fd00::1%-x", |e| matches!(e, TargetError::BadHost { .. })),
            ("fd00::1%", |e| matches!(e, TargetError::BadHost { .. })),
            ("host?", |e| matches!(e, TargetError::BadOption { .. })),
            ("host?A", |e| matches!(e, TargetError::BadOption { .. })),
            ("host?A=", |e| matches!(e, TargetError::BadOption { .. })),
            ("host?=b", |e| matches!(e, TargetError::BadOption { .. })),
            ("host?A=b&", |e| matches!(e, TargetError::BadOption { .. })),
            ("host?A=b c", |e| matches!(e, TargetError::BadOption { .. })),
            ("host?A=-x", |e| matches!(e, TargetError::BadOption { .. })),
            ("host?A-B=c", |e| matches!(e, TargetError::BadOption { .. })),
            ("host?A=b\nc", |e| {
                matches!(e, TargetError::BadOption { .. })
            }),
            ("host?ProxyCommand=nc", |e| {
                matches!(e, TargetError::ForbiddenOption { .. })
            }),
            ("host?proxycommand=nc", |e| {
                matches!(e, TargetError::ForbiddenOption { .. })
            }),
            ("host?LocalCommand=x", |e| {
                matches!(e, TargetError::ForbiddenOption { .. })
            }),
            ("host?KnownHostsCommand=x", |e| {
                matches!(e, TargetError::ForbiddenOption { .. })
            }),
            ("host?ControlPath=/tmp/x", |e| {
                matches!(e, TargetError::ForbiddenOption { .. })
            }),
            ("host?BatchMode=no", |e| {
                matches!(e, TargetError::ForbiddenOption { .. })
            }),
        ];
        for (input, ok) in bad {
            match Target::parse(input) {
                Ok(t) => panic!("{input:?} parsed as {t:?}"),
                Err(e) => assert!(ok(&e), "{input:?}: unexpected {e:?}"),
            }
        }
    }

    const DIR: &str = "/run/user/1000/auspex";

    #[test]
    fn ssh_args_shape() {
        let args = ssh_args(&t("root@[fd00::1]:2222?ProxyJump=jump"), DIR).unwrap();
        assert_eq!(
            args,
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=/run/user/1000/auspex/%C",
                "-o",
                "ControlPersist=60s",
                "-o",
                "ProxyJump=jump",
                "-p",
                "2222",
                "--",
                "root@fd00::1",
            ]
        );
        let plain = ssh_args(&t("terra"), "/run/user/1000/auspex/").unwrap();
        assert_eq!(&plain[plain.len() - 2..], ["--", "terra"]);
        assert!(plain.contains(&"ControlPath=/run/user/1000/auspex/%C".to_owned()));
        assert!(!plain.contains(&"-p".to_owned()));
    }

    /// Whatever a target string is, if it parses, then in argv: the destination is the
    /// element right after the one and only `--`, nothing after `--` other than it, and no
    /// element before `--` that came from the target starts with `-` unless it is an
    /// argument of `-o`/`-p`.
    #[test]
    fn ssh_args_never_carry_injected_options() {
        let hostile = [
            "-oProxyCommand=sh",
            "root@-oProxyCommand=sh",
            "-oProxyCommand=sh@host",
            "host?ProxyCommand=sh",
            "host?A=b&-oProxyCommand=sh",
            "host?A=-oProxyCommand=sh",
            "host -oProxyCommand=sh",
            "host\t-oProxyCommand=sh",
            "host\n-oProxyCommand=sh",
            "root@host:22 -oX=y",
            "[fd00::1 -oX=y]:22",
            "[-oProxyCommand=sh]:22",
            "host?ProxyJump=-oProxyCommand=sh",
            "host?ProxyJump=a\u{0}b",
            "host?A=b%20c",
        ];
        for h in hostile {
            if let Ok(target) = Target::parse(h) {
                let args = ssh_args(&target, DIR).unwrap();
                let dd = args.iter().position(|a| a == "--").unwrap();
                assert_eq!(dd, args.len() - 2, "{h:?}: {args:?}");
                assert!(!args[dd + 1].starts_with('-'), "{h:?}: {args:?}");
                for a in &args {
                    assert!(
                        !a.chars().any(|c| c.is_whitespace() || c.is_control()),
                        "{h:?}: {a:?}"
                    );
                    assert!(!a.to_ascii_lowercase().contains("proxycommand"), "{h:?}");
                }
            }
        }
    }

    #[test]
    fn control_dir_is_validated() {
        let target = t("host");
        for bad in ["", "relative", "/a b", "/a\nb", "/a%h"] {
            assert!(ssh_args(&target, bad).is_err(), "{bad:?}");
        }
    }

    fn machine(name: &str, target_host: Option<&str>) -> Machine {
        Machine {
            name: name.to_owned(),
            class: MachineClass::Nixos,
            tags: vec![],
            target_host: target_host.map(str::to_owned),
            build_host: None,
        }
    }

    #[test]
    fn resolution_precedence() {
        let m = machine("terra", Some("root@192.168.1.10"));
        let r = resolve_target(&m, Some("root@10.0.0.10")).unwrap();
        assert_eq!(r.source, TargetSource::Override);
        assert_eq!(r.target.host(), "10.0.0.10");

        let r = resolve_target(&m, None).unwrap();
        assert_eq!(r.source, TargetSource::Inventory);
        assert_eq!(r.target.host(), "192.168.1.10");

        let r = resolve_target(&machine("terra", None), None).unwrap();
        assert_eq!(r.source, TargetSource::MachineName);
        assert_eq!(r.target.to_string(), "terra");
    }

    #[test]
    fn resolution_does_not_fall_back_past_a_bad_value() {
        let m = machine("terra", Some("root@192.168.1.10"));
        let e = resolve_target(&m, Some("root@-x")).unwrap_err();
        assert_eq!(e.target_source, TargetSource::Override);
        let e = resolve_target(&machine("terra", Some("ro ot@h")), None).unwrap_err();
        assert_eq!(e.target_source, TargetSource::Inventory);
        let e = resolve_target(&machine("-terra", None), None).unwrap_err();
        assert_eq!(e.target_source, TargetSource::MachineName);
    }

    #[test]
    fn hostname_lint() {
        assert!(lint_hostname(&t("root@192.168.1.10")).is_none());
        assert!(lint_hostname(&t("root@[fd00::1]:22")).is_none());
        assert!(lint_hostname(&t("fe80::1%eth0")).is_none());
        assert_eq!(
            lint_hostname(&t("root@cerberus.ugalabugala.org")),
            Some(Lint::HostnameTarget {
                host: "cerberus.ugalabugala.org".to_owned()
            })
        );
        assert!(lint_hostname(&t("terra")).is_some());
    }

    #[test]
    fn shared_ip_detection() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let mut resolved = BTreeMap::new();
        resolved.insert("cerberus".to_owned(), vec![ip("192.168.1.2")]);
        resolved.insert("terra".to_owned(), vec![ip("192.168.1.10")]);
        // A wildcard DNS name answering with cerberus's address, in the mapped form.
        resolved.insert(
            "luna".to_owned(),
            vec![ip("::ffff:192.168.1.2"), ip("fd00::5")],
        );
        resolved.insert("sol".to_owned(), vec![ip("fd00::5"), ip("fd00::5")]);
        let shared = shared_ips(&resolved);
        assert_eq!(
            shared,
            vec![
                SharedIp {
                    ip: ip("192.168.1.2"),
                    machines: vec!["cerberus".to_owned(), "luna".to_owned()]
                },
                SharedIp {
                    ip: ip("fd00::5"),
                    machines: vec!["luna".to_owned(), "sol".to_owned()]
                },
            ]
        );
        assert_eq!(
            shared_ip_lints("sol", &shared),
            vec![Lint::SharedIp {
                ip: ip("fd00::5"),
                with: vec!["luna".to_owned()]
            }]
        );
        assert_eq!(shared_ip_lints("terra", &shared), vec![]);
    }

    #[test]
    fn inventory_tolerates_missing_and_null() {
        let json = r#"{
            "terra": {"deploy": {"targetHost": "root@192.168.1.10", "buildHost": "localhost"},
                      "tags": ["base", "k8s-base"], "machineClass": "nixos", "extra": 1},
            "nodeploy": {"tags": ["base"]},
            "nulltarget": {"deploy": {"targetHost": null}, "tags": null, "machineClass": null},
            "emptytarget": {"deploy": {"targetHost": ""}},
            "genetor": {"deploy": {"targetHost": "admin@192.168.1.109"}, "machineClass": "darwin"},
            "bare": {},
            "nullmachine": null
        }"#;
        let ms = parse_inventory(json).unwrap();
        let names: Vec<&str> = ms.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "bare",
                "emptytarget",
                "genetor",
                "nodeploy",
                "nullmachine",
                "nulltarget",
                "terra"
            ]
        );
        let get = |n: &str| ms.iter().find(|m| m.name == n).unwrap();
        assert_eq!(
            get("terra").target_host.as_deref(),
            Some("root@192.168.1.10")
        );
        assert_eq!(get("terra").build_host.as_deref(), Some("localhost"));
        assert_eq!(get("terra").tags, ["base", "k8s-base"]);
        assert_eq!(get("nodeploy").target_host, None);
        assert_eq!(get("nulltarget").target_host, None);
        assert_eq!(get("nulltarget").tags, Vec::<String>::new());
        assert_eq!(get("nulltarget").class, MachineClass::Nixos);
        assert_eq!(get("emptytarget").target_host, None);
        assert_eq!(get("genetor").class, MachineClass::Darwin);
    }

    #[test]
    fn inventory_errors() {
        assert!(matches!(
            parse_inventory("[]"),
            Err(InventoryError::Malformed(_))
        ));
        assert!(matches!(
            parse_inventory("not json"),
            Err(InventoryError::Malformed(_))
        ));
        assert!(matches!(
            parse_inventory(r#"{"a": {"tags": "base"}}"#),
            Err(InventoryError::Malformed(_))
        ));
        assert_eq!(
            parse_inventory(r#"{"a": {"machineClass": "windows"}}"#),
            Err(InventoryError::UnknownClass {
                machine: "a".to_owned(),
                class: "windows".to_owned()
            })
        );
    }
}
