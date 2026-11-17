//! Machines, ssh targets, and the homelab-safety lints.
//!
//! The one security-sensitive part of the crate is here: a [`Target`] ends up in ssh's argv,
//! so its parser is a whitelist, and [`ssh_args`] is the only way to turn one into
//! arguments. A user or host that could be read as an option (`-oProxyCommand=…`), or that
//! carries whitespace or control characters, never makes it out of [`Target::parse`].

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::text::sanitize_line;

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
    /// The machine's name: the inventory key, and the last-resort ssh host. From
    /// [`parse_inventory`], always a plain name (see [`InventoryError::BadName`]).
    pub name: String,
    /// NixOS or darwin. The inventory's `machineClass`; `nixos` when missing or null, which
    /// is Clan's own default.
    pub class: MachineClass,
    /// The inventory's tags, in inventory order, each sanitised for the terminal (escape
    /// sequences, control and format characters removed). Empty when missing or null; a tag
    /// that sanitises to nothing is dropped.
    pub tags: Vec<String>,
    /// The inventory's `deploy.targetHost`, unparsed and unsanitised: it goes through
    /// [`Target::parse`], which refuses anything a sanitiser would remove, and only its
    /// canonical form is shown. `None` when missing, null or empty.
    pub target_host: Option<String>,
    /// The inventory's `deploy.buildHost`, unparsed, sanitised as `tags` are. Only ever
    /// shown, never handed to ssh. `None` when missing, null or empty.
    pub build_host: Option<String>,
}

/// The inventory JSON could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InventoryError {
    /// Not JSON, or not the expected shape (an object of objects, string fields).
    #[error("inventory is not valid: {0}")]
    Malformed(String),
    /// A machine name that is not a plain name: 1 to 63 ASCII letters, digits, `-` and `_`,
    /// starting with a letter or digit. The name is a Nix attribute name that is also used
    /// as a hostname and printed in every view, so anything else (a dot, `@`, `?`, an
    /// escape sequence, a bidi override) is refused rather than shown or sent to ssh.
    #[error(
        "machine name {machine:?} is not a plain name ([A-Za-z0-9_-], starting with a letter or digit, at most 63 characters)"
    )]
    BadName {
        /// The name as written.
        machine: String,
    },
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

/// The machine-name rule; see [`InventoryError::BadName`].
fn is_plain_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// Parses the inventory JSON.
///
/// That is the value of `clanInternals.inventoryClass.inventory.machines`, an object keyed by
/// machine name. Missing or null `deploy`, `targetHost`, `buildHost`, `tags` and
/// `machineClass` are all tolerated; unknown fields are ignored. Machines come back sorted
/// by name. Tags and `buildHost` are sanitised for the terminal.
///
/// # Errors
///
/// [`InventoryError::Malformed`] when the JSON does not have that shape,
/// [`InventoryError::BadName`] for a machine name that is not a plain name, and
/// [`InventoryError::UnknownClass`] for a class other than `nixos` or `darwin`.
pub fn parse_inventory(json: &str) -> Result<Vec<Machine>, InventoryError> {
    let raw: BTreeMap<String, Option<RawMachine>> =
        serde_json::from_str(json).map_err(|e| InventoryError::Malformed(e.to_string()))?;
    raw.into_iter()
        .map(|(name, m)| {
            if !is_plain_name(&name) {
                return Err(InventoryError::BadName { machine: name });
            }
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
                tags: m
                    .tags
                    .unwrap_or_default()
                    .iter()
                    .map(|t| sanitize_line(t))
                    .filter(|t| !t.is_empty())
                    .collect(),
                target_host: non_empty(target_host),
                build_host: non_empty(build_host.map(|b| sanitize_line(&b))),
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
    /// A host that is neither a hostname (`[A-Za-z0-9._-]`, not starting with `-` or `.`,
    /// and not an IPv4 address in a spelling other than the dotted quad) nor an IP literal.
    /// For a machine name used as the host, anything but a bare hostname.
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
    /// An option that is not `Name=value` with an alphanumeric name, or an allowed option
    /// whose value is not one it accepts (see [`Target::parse`] for the values).
    #[error("invalid ssh option {option:?}")]
    BadOption {
        /// The option as written.
        option: String,
    },
    /// An option auspex refuses: anything not on the allow list in [`Target::parse`], an
    /// option auspex sets itself (ssh keeps the *first* value of an option, so a target's
    /// copy would be ignored, and silently ignoring configuration is worse than refusing it),
    /// or an option given twice (for the same reason).
    #[error("ssh option {name} is not allowed in a target ({why})")]
    ForbiddenOption {
        /// The option name as written.
        name: String,
        /// Why it is refused.
        why: String,
    },
}

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
    if host.starts_with('-')
        || host.starts_with('.')
        || !host.chars().all(is_name_char)
        || is_other_ipv4_spelling(host)
    {
        return Err(TargetError::BadHost {
            host: host.to_owned(),
        });
    }
    Ok(())
}

/// A host that ssh would hand to `inet_aton` and read as an IPv4 address, but that is not
/// the dotted quad `Ipv4Addr` reads: `192.168.1.012` (octal, so `.10`), `3232235786` and
/// `0xc0a8010a` (one number), `127.1` (short form). Shown as written, a person reads a
/// different address from the one ssh connects to, and the lints would call it a hostname.
/// The test is the last label: no top-level domain is numeric, so a last label of digits,
/// or of `0x` and hex digits, means the whole name is an address.
fn is_other_ipv4_spelling(host: &str) -> bool {
    if host.parse::<Ipv4Addr>().is_ok() {
        return false;
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    let last = host.rsplit('.').next().unwrap_or(host);
    let hex = last
        .strip_prefix("0x")
        .or_else(|| last.strip_prefix("0X"))
        .is_some_and(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
    hex || (!last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()))
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

// The options rule. A target string comes from the fleet's inventory or from auspex's
// config, and it may say *how* to reach the host it names. It may not change *which* host
// (HostName, HostKeyAlias, every host-key and known-hosts option), *as whom* (User),
// *what runs locally* (ProxyCommand, PKCS11Provider, XAuthLocation, every option that takes
// a path), or *what the remote side can reach* (every forwarding option, SendEnv, Tunnel).
// Those belong in `~/.ssh/config`, which ssh reads and auspex honours without parsing.
//
// So the options are an allow list, each entry with the values it accepts, rather than a
// deny list: a deny list has to track every OpenSSH release and every undocumented alias
// (`SmartcardDevice` loads a library just like `PKCS11Provider`), and the one this replaced
// let all of those through.

/// The values an allowed option accepts.
enum Accepts {
    /// Exactly one of these words (ssh's own lowercase spelling).
    OneOf(&'static [&'static str]),
    /// A decimal integer in this inclusive range, no sign and no leading zero.
    Range(u32, u32),
}

const YES_NO: &[&str] = &["yes", "no"];

/// The DSCP code points `IPQoS` accepts. ssh's older type-of-service keywords (`lowdelay`,
/// `throughput`, `reliability`) are deprecated and left out.
const IPQOS: &[&str] = &[
    "af11", "af12", "af13", "af21", "af22", "af23", "af31", "af32", "af33", "af41", "af42", "af43",
    "cs0", "cs1", "cs2", "cs3", "cs4", "cs5", "cs6", "cs7", "ef", "le", "none",
];

/// The only options a target may carry, in their canonical spelling, with the values each
/// accepts. Each one acts only on the one ssh process auspex starts, with auspex's own
/// `BatchMode`, `ConnectTimeout` and control socket. Why each is safe:
///
/// - `AddressFamily`: IPv4, IPv6 or either, to the host the target names.
/// - `ConnectionAttempts` (1 to 3) and `ServerAliveCountMax` (1 to 5): bounded so that, with
///   auspex's `ConnectTimeout` and `ServerAliveInterval`, an unreachable host still fails
///   within seconds rather than holding its row.
/// - `IdentitiesOnly`, `PubkeyAuthentication`: which of the operator's keys are offered to
///   the host the target names. Neither can add a key, and `BatchMode` turns any prompt in
///   that ssh into a failure.
/// - `Compression`, `TCPKeepAlive`, `IPQoS`: transport tuning with no reach outside the
///   connection.
///
/// `ProxyJump` is deliberately not here. ssh runs each hop as a second `ssh` process that
/// gets none of auspex's options, so `BatchMode` does not cover it, and a bastion the
/// operator has never connected to would put a host-key (or password) prompt on the
/// terminal the TUI draws on. Jump hosts belong in `~/.ssh/config`, where the operator can
/// also set `BatchMode yes` for the hop; see [`ssh_args`] for what the adapter must do so
/// that such hops cannot prompt either.
const ALLOWED_OPTIONS: &[(&str, Accepts)] = &[
    ("AddressFamily", Accepts::OneOf(&["any", "inet", "inet6"])),
    ("ConnectionAttempts", Accepts::Range(1, 3)),
    ("ServerAliveCountMax", Accepts::Range(1, 5)),
    ("IdentitiesOnly", Accepts::OneOf(YES_NO)),
    ("PubkeyAuthentication", Accepts::OneOf(YES_NO)),
    ("Compression", Accepts::OneOf(YES_NO)),
    ("TCPKeepAlive", Accepts::OneOf(YES_NO)),
    ("IPQoS", Accepts::OneOf(IPQOS)),
];

/// Options [`ssh_args`] sets itself. Outside the allow list like everything else; named so
/// the refusal can say why.
const OWNED_OPTIONS: &[&str] = &[
    "BatchMode",
    "ConnectTimeout",
    "ServerAliveInterval",
    "ControlMaster",
    "ControlPath",
    "ControlPersist",
];

/// Options that make ssh execute a local command. Named only so the refusal says why.
const COMMAND_OPTIONS: &[&str] = &[
    "ProxyCommand",
    "LocalCommand",
    "PermitLocalCommand",
    "KnownHostsCommand",
    "RemoteCommand",
    "Match",
];

/// Options that would make ssh connect to a host, port or user other than the ones the
/// target names, so that the display, the hostname lint and the shared-IP check would all
/// be about the wrong machine. Named only so the refusal says why.
const DESTINATION_OPTIONS: &[&str] = &["HostName", "Port", "User", "HostKeyAlias"];

/// The longest option value accepted. Far above any allowed word or number.
const MAX_OPTION_VALUE: usize = 255;

/// The characters an option value may contain at all, before the option's own check:
/// ASCII letters, digits, `.`, `_` and `-`. Every allowed value is a word or a number, so
/// this is already wider than needed; it is a second fence, so that `%` (ssh's token
/// expansion), `$`, backtick, `;`, quotes, backslash, `/`, `,`, `:`, `@`, brackets, `=`,
/// `?`, `&`, whitespace, control characters and anything outside ASCII never reach ssh's
/// argv even if an entry above is widened by mistake.
const fn is_value_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

fn find_name<'a>(list: &[&'a str], name: &str) -> Option<&'a str> {
    list.iter().copied().find(|n| n.eq_ignore_ascii_case(name))
}

/// Why an option outside the allow list is refused.
fn refusal(name: &str) -> String {
    if let Some(n) = find_name(OWNED_OPTIONS, name) {
        return format!("auspex sets {n} itself");
    }
    if let Some(n) = find_name(COMMAND_OPTIONS, name) {
        return format!("{n} runs a local command; set it in ~/.ssh/config");
    }
    if let Some(n) = find_name(DESTINATION_OPTIONS, name) {
        return format!(
            "{n} would make ssh connect somewhere other than the target names; put the user, \
             host and port in the target itself, or set it in ~/.ssh/config"
        );
    }
    if name.eq_ignore_ascii_case("ProxyJump") {
        return "a jump host runs as a second ssh without auspex's BatchMode, so it could \
                prompt on the terminal; set ProxyJump in ~/.ssh/config (with BatchMode yes for \
                the jump host), which auspex honours"
            .to_owned();
    }
    let allowed: Vec<&str> = ALLOWED_OPTIONS.iter().map(|(n, _)| *n).collect();
    format!(
        "a target may set only {}; set it in ~/.ssh/config, which auspex honours",
        allowed.join(", ")
    )
}

fn parse_bounded(value: &str, lo: u32, hi: u32) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && !(value.len() > 1 && value.starts_with('0'))
        && value.parse::<u32>().is_ok_and(|n| (lo..=hi).contains(&n))
}

fn parse_option(opt: &str) -> Result<(String, String), TargetError> {
    let bad = || TargetError::BadOption {
        option: opt.to_owned(),
    };
    let (name, value) = opt.split_once('=').ok_or_else(bad)?;
    let name_ok = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric());
    if !name_ok {
        return Err(bad());
    }
    // ssh option names are case-insensitive, so the allow list is too.
    let Some((canonical, accepts)) = ALLOWED_OPTIONS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
    else {
        return Err(TargetError::ForbiddenOption {
            name: name.to_owned(),
            why: refusal(name),
        });
    };
    let value_ok = !value.is_empty()
        && value.len() <= MAX_OPTION_VALUE
        && !value.starts_with('-')
        && value.chars().all(is_value_char)
        && match accepts {
            Accepts::OneOf(words) => words.contains(&value),
            Accepts::Range(lo, hi) => parse_bounded(value, *lo, *hi),
        };
    if !value_ok {
        return Err(bad());
    }
    Ok(((*canonical).to_owned(), value.to_owned()))
}

/// `[user@]host[:port]`, the destination part of a target.
fn parse_destination(dest: &str) -> Result<(Option<String>, String, Option<u16>), TargetError> {
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
    Ok((user, host.to_owned(), port))
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
    ///   reach ssh's argv. A hostname that ssh would read as an IPv4 address in another
    ///   spelling (`192.168.1.012`, `3232235786`, `0xc0a8010a`, `127.1`) is refused too.
    /// - Options are an allow list, matched case-insensitively and stored in their
    ///   canonical spelling, each at most once:
    ///
    ///   | Option | Accepted values |
    ///   |---|---|
    ///   | `AddressFamily` | `any`, `inet`, `inet6` |
    ///   | `ConnectionAttempts` | 1 to 3 |
    ///   | `ServerAliveCountMax` | 1 to 5 |
    ///   | `IdentitiesOnly`, `PubkeyAuthentication`, `Compression`, `TCPKeepAlive` | `yes`, `no` |
    ///   | `IPQoS` | one DSCP name: `af11`…`af43`, `cs0`…`cs7`, `ef`, `le`, `none` |
    ///
    ///   No value may be longer than 255 characters or contain anything outside ASCII
    ///   letters, digits, `.`, `_` and `-`. Every other option, including `ProxyJump`,
    ///   `HostName`, `Port`, `User`, the host-key and forwarding options and the ones
    ///   auspex sets itself, is refused with a reason that says to set it in
    ///   `~/.ssh/config`.
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
        for (i, (name, _)) in options.iter().enumerate() {
            if options[..i].iter().any(|(n, _)| n == name) {
                return Err(TargetError::ForbiddenOption {
                    name: name.clone(),
                    why: "given twice; ssh would use only the first".to_owned(),
                });
            }
        }
        let (user, host, port) = parse_destination(dest)?;
        Ok(Self {
            user,
            host,
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
/// An empty (or all-whitespace) override means no override, as an empty `targetHost` does:
/// a home-manager option that defaults to `""` must not mark every machine unreachable.
/// The machine name is used only as a bare hostname, with no user, port or options: it is
/// an inventory key, and a key is not a place to configure ssh.
///
/// # Errors
///
/// [`ResolveError`] when the winning value does not parse.
pub fn resolve_target(
    machine: &Machine,
    override_target: Option<&str>,
) -> Result<ResolvedTarget, ResolveError> {
    let override_target = override_target.filter(|o| !o.trim().is_empty());
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
    let parsed = if source == TargetSource::MachineName {
        validate_hostname(raw).and_then(|()| Target::parse(raw))
    } else {
        Target::parse(raw)
    };
    parsed
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
    "control path directory {dir:?} must be absolute and free of whitespace, control characters, '%' and '$'"
)]
pub struct ControlDirError {
    /// The directory as given.
    pub dir: String,
}

/// ssh's `ConnectTimeout`: the config's `timeouts.ssh_connect`, in whole seconds.
///
/// Only constructed through [`ConnectTimeout::new`], so every value is in range. The range
/// is 1 to 60 seconds: 0 means "the system's TCP timeout" to ssh, which is minutes, and
/// above a minute a dead host's row would sit on a spinner long past the point of telling
/// the operator anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectTimeout(u8);

/// The connect timeout was outside [`ConnectTimeout::MIN_SECONDS`] to
/// [`ConnectTimeout::MAX_SECONDS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("ssh connect timeout {seconds}s is outside 1..=60 seconds")]
pub struct ConnectTimeoutError {
    /// The value as given.
    pub seconds: u64,
}

impl ConnectTimeout {
    /// The smallest accepted value.
    pub const MIN_SECONDS: u64 = 1;
    /// The largest accepted value.
    pub const MAX_SECONDS: u64 = 60;
    /// The default when the config does not set one: 5 seconds, as the plan specifies.
    pub const DEFAULT: Self = Self(5);

    /// Validates a timeout in seconds.
    ///
    /// # Errors
    ///
    /// [`ConnectTimeoutError`] outside 1 to 60.
    pub const fn new(seconds: u64) -> Result<Self, ConnectTimeoutError> {
        if seconds < Self::MIN_SECONDS || seconds > Self::MAX_SECONDS {
            return Err(ConnectTimeoutError { seconds });
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "checked against MAX_SECONDS (60) just above"
        )]
        Ok(Self(seconds as u8))
    }

    /// The timeout in seconds.
    #[must_use]
    pub const fn seconds(self) -> u64 {
        self.0 as u64
    }
}

impl Default for ConnectTimeout {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Which SSH operation to perform, using the same destination and control socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SshOperation {
    /// Run a remote probe.
    #[default]
    Probe,
    /// Check for an existing master without opening a connection.
    Check,
    /// Authenticate, then detach a persistent master without a remote command.
    Connect,
}

/// What [`ssh_args`] needs besides the target: the effects layer's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SshOptions<'a> {
    /// The control-socket directory, `$XDG_RUNTIME_DIR/auspex`. Creating it with mode 0700
    /// is the caller's job.
    pub control_dir: &'a str,
    /// `ConnectTimeout`, from the config's `timeouts.ssh_connect`.
    pub connect_timeout: ConnectTimeout,
    /// `ControlPersist` in seconds, from `ssh.control_persist` (default 600).
    pub control_persist: u64,
    /// The operation, defaulting to a remote probe.
    pub operation: SshOperation,
}

impl<'a> SshOptions<'a> {
    /// `control_dir` with the default connect timeout.
    #[must_use]
    pub const fn new(control_dir: &'a str) -> Self {
        Self {
            control_dir,
            connect_timeout: ConnectTimeout::DEFAULT,
            control_persist: 600,
            operation: SshOperation::Probe,
        }
    }
}

/// The remote command to append after [`ssh_args`] to run a probe.
///
/// The script goes on ssh's stdin. Feeding it on stdin, rather than as the command, means it
/// never passes through the remote login shell's quoting.
pub const PROBE_REMOTE_COMMAND: &str = "/bin/sh -s";

/// ssh's arguments for `target`, without the program name and without the remote command:
///
/// ```text
/// -T -o BatchMode=yes -o ConnectTimeout=<connect_timeout> -o ServerAliveInterval=5
///    -o ControlMaster=auto -o ControlPath=<control_dir>/%C -o ControlPersist=<seconds>s
///    [-o Opt=val]… [-p port] -- [user@]host
/// ```
///
/// auspex's own options come first because ssh keeps the first value it sees for an option.
///
/// A live master lets any process running as the operator reuse the authenticated
/// connection for `control_persist` seconds. The control directory's ownership and 0700
/// permissions are what restrict that access to the operator.
///
/// [`SshOperation::Check`] adds `-O check`; [`SshOperation::Connect`] uses
/// `ControlMaster=yes -N -f`. Both use this same destination and safety options.
///
/// # What the caller must do
///
/// `BatchMode=yes` here covers the one ssh process auspex starts. It does not cover the
/// second ssh that a `ProxyJump` or `ProxyCommand` in `~/.ssh/config` starts for a hop: that
/// process reads only the operator's config and gets none of these options. So that neither
/// process can prompt (for a host key, a password or a passphrase) on the terminal the TUI
/// is drawing on, the adapter must run ssh
///
/// - with no controlling terminal: in a new session (`setsid`), with stdin a pipe (the probe
///   script) and stdout and stderr pipes, never inherited from the TUI; and
/// - with `SSH_ASKPASS_REQUIRE=never` set, so ssh cannot fall back to a graphical prompt.
///   Removing `SSH_ASKPASS` is optional, but not sufficient: ssh has a built-in default
///   askpass program even when that variable is absent.
///
/// The effects layer's tests should check both, because nothing in this crate can.
///
/// # Errors
///
/// [`ControlDirError`] when `options.control_dir` is relative or contains whitespace, a
/// control character, `%` (which ssh would expand as a token) or `$` (which ssh expands as
/// `${VAR}`, and an unset variable makes every connection fail with exit 255).
pub fn ssh_args(target: &Target, options: &SshOptions<'_>) -> Result<Vec<String>, ControlDirError> {
    let control_dir = options.control_dir;
    if !control_dir.starts_with('/')
        || control_dir
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '%' | '$'))
    {
        return Err(ControlDirError {
            dir: control_dir.to_owned(),
        });
    }
    let dir = control_dir.trim_end_matches('/');
    let mut args: Vec<String> = vec![
        "-T".to_owned(),
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "-o".to_owned(),
        format!("ConnectTimeout={}", options.connect_timeout.seconds()),
        "-o".to_owned(),
        "ServerAliveInterval=5".to_owned(),
        "-o".to_owned(),
        match options.operation {
            SshOperation::Connect => "ControlMaster=yes",
            SshOperation::Probe | SshOperation::Check => "ControlMaster=auto",
        }
        .to_owned(),
        "-o".to_owned(),
        format!("ControlPath={dir}/%C"),
        "-o".to_owned(),
        format!("ControlPersist={}s", options.control_persist),
    ];
    match options.operation {
        SshOperation::Probe => {}
        SshOperation::Check => args.extend(["-O".to_owned(), "check".to_owned()]),
        SshOperation::Connect => args.extend(["-N".to_owned(), "-f".to_owned()]),
    }
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
                "root@host?Compression=no&ConnectionAttempts=2",
                Some("root"),
                "host",
                None,
                &[("Compression", "no"), ("ConnectionAttempts", "2")],
            ),
            (
                "root@[fd00::1]:22?IPQoS=ef",
                Some("root"),
                "fd00::1",
                Some(22),
                &[("IPQoS", "ef")],
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
        assert_eq!(
            t("host:22?Compression=yes&TCPKeepAlive=no").to_string(),
            "host:22?Compression=yes&TCPKeepAlive=no"
        );
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
            ("host?Compression=", |e| {
                matches!(e, TargetError::BadOption { .. })
            }),
            ("host?=b", |e| matches!(e, TargetError::BadOption { .. })),
            ("host?Compression=yes&", |e| {
                matches!(e, TargetError::BadOption { .. })
            }),
            ("host?Compression=b c", |e| {
                matches!(e, TargetError::BadOption { .. })
            }),
            ("host?Compression=-x", |e| {
                matches!(e, TargetError::BadOption { .. })
            }),
            ("host?A-B=c", |e| matches!(e, TargetError::BadOption { .. })),
            ("host?Compression=b\nc", |e| {
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
        let args = ssh_args(
            &t("root@[fd00::1]:2222?Compression=yes"),
            &SshOptions::new(DIR),
        )
        .unwrap();
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
                "ControlPersist=600s",
                "-o",
                "Compression=yes",
                "-p",
                "2222",
                "--",
                "root@fd00::1",
            ]
        );
        let plain = ssh_args(&t("terra"), &SshOptions::new("/run/user/1000/auspex/")).unwrap();
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
                let args = ssh_args(&target, &SshOptions::new(DIR)).unwrap();
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
        // N6: `$` is refused like `%`: ssh expands `${VAR}` in ControlPath.
        for bad in [
            "",
            "relative",
            "/a b",
            "/a\nb",
            "/a%h",
            "/run/user/1000/${AUSPEX_UNSET}",
            "/run/user/1000/a${HOME}b",
            "/run/user/1000/${",
            "/run/$x",
        ] {
            assert!(ssh_args(&target, &SshOptions::new(bad)).is_err(), "{bad:?}");
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

    fn forbidden_why(s: &str) -> String {
        match Target::parse(s) {
            Err(TargetError::ForbiddenOption { why, .. }) => why,
            other => panic!("{s:?}: expected ForbiddenOption, got {other:?}"),
        }
    }

    /// F1 and F2: every option that loads code, forwards something, weakens or redirects
    /// host-key checking, or changes the host, port or user is refused, and the reason says
    /// where it belongs.
    #[test]
    fn options_outside_the_allow_list_are_refused() {
        let refused = [
            "PKCS11Provider=x",
            "SmartcardDevice=x",
            "SecurityKeyProvider=x",
            "XAuthLocation=x",
            "ForwardX11=yes",
            "ForwardX11Trusted=yes",
            "ForwardAgent=yes",
            "RemoteForward=1080",
            "LocalForward=8080",
            "DynamicForward=1080",
            "GatewayPorts=yes",
            "PermitRemoteOpen=any",
            "ClearAllForwardings=no",
            "ExitOnForwardFailure=no",
            "SendEnv=x",
            "SetEnv=A",
            "IdentityAgent=x",
            "IdentityFile=x",
            "CertificateFile=x",
            "AddKeysToAgent=yes",
            "Tunnel=yes",
            "TunnelDevice=any",
            "StrictHostKeyChecking=no",
            "UserKnownHostsFile=x",
            "GlobalKnownHostsFile=x",
            "RevokedHostKeys=x",
            "CheckHostIP=no",
            "VerifyHostKeyDNS=yes",
            "UpdateHostKeys=yes",
            "HashKnownHosts=no",
            "NoHostAuthenticationForLocalhost=yes",
            "HostKeyAlgorithms=ssh-rsa",
            "Ciphers=3des-cbc",
            "MACs=hmac-md5",
            "KexAlgorithms=diffie-hellman-group1-sha1",
            "PubkeyAcceptedAlgorithms=ssh-rsa",
            "PreferredAuthentications=password",
            "PasswordAuthentication=yes",
            "HostbasedAuthentication=yes",
            "CanonicalizeHostname=yes",
            "CanonicalDomains=x",
            "Tag=x",
            "Include=x",
            "Host=x",
            "SessionType=none",
            "StdinNull=yes",
            "ForkAfterAuthentication=yes",
            "LogLevel=QUIET",
            "RequestTTY=force",
            "EscapeChar=none",
            "BindAddress=x",
            "BindInterface=x",
            "ProxyUseFdpass=yes",
            "RequiredRSASize=1024",
        ];
        for opt in refused {
            let why = forbidden_why(&format!("host?{opt}"));
            assert!(why.contains("~/.ssh/config"), "{opt}: {why}");
        }
        // The options auspex sets itself say so.
        for n in OWNED_OPTIONS {
            for spelling in [n.to_string(), n.to_ascii_lowercase()] {
                let why = forbidden_why(&format!("host?{spelling}=x"));
                assert_eq!(why, format!("auspex sets {n} itself"), "{spelling}");
            }
        }
        // So do the ones that run a local command, the old deny list.
        for n in [
            "ProxyCommand",
            "LocalCommand",
            "PermitLocalCommand",
            "KnownHostsCommand",
            "RemoteCommand",
            "Match",
        ] {
            for spelling in [n.to_owned(), n.to_ascii_uppercase()] {
                let why = forbidden_why(&format!("host?{spelling}=x"));
                assert!(why.contains("runs a local command"), "{spelling}: {why}");
            }
        }
        // And the ones that would connect somewhere else.
        for opt in [
            "HostName=cerberus.ugalabugala.org",
            "Hostname=192.0.2.2",
            "Port=2222",
            "User=admin",
            "HostKeyAlias=cerberus",
        ] {
            let why = forbidden_why(&format!("root@192.168.1.10?{opt}"));
            assert!(why.contains("somewhere other than the target"), "{opt}");
        }
    }

    #[test]
    fn allowed_options_take_only_their_values() {
        let ok = [
            ("AddressFamily=inet6", ("AddressFamily", "inet6")),
            ("CONNECTIONATTEMPTS=3", ("ConnectionAttempts", "3")),
            ("ServerAliveCountMax=1", ("ServerAliveCountMax", "1")),
            ("IdentitiesOnly=yes", ("IdentitiesOnly", "yes")),
            ("PubkeyAuthentication=no", ("PubkeyAuthentication", "no")),
            ("Compression=yes", ("Compression", "yes")),
            ("TCPKeepAlive=no", ("TCPKeepAlive", "no")),
            ("ipqos=af21", ("IPQoS", "af21")),
        ];
        for (opt, (name, value)) in ok {
            let got = t(&format!("host?{opt}"));
            assert_eq!(
                got.options(),
                [(name.to_owned(), value.to_owned())],
                "{opt}"
            );
            assert_eq!(t(&got.to_string()), got, "{opt}");
        }
        for opt in [
            "AddressFamily=inet7",
            "ConnectionAttempts=0",
            "ConnectionAttempts=4",
            "ConnectionAttempts=02",
            "ConnectionAttempts=1000",
            "ServerAliveCountMax=6",
            "ServerAliveCountMax=1000000",
            "Compression=YES",
            "Compression=true",
            "IdentitiesOnly=",
            "IPQoS=lowdelay",
            "IPQoS=af21,cs1",
        ] {
            assert!(
                matches!(
                    Target::parse(&format!("host?{opt}")),
                    Err(TargetError::BadOption { .. })
                ),
                "{opt}"
            );
        }
        // Given twice: ssh would silently use the first.
        let why = forbidden_why("host?Compression=yes&compression=no");
        assert!(why.contains("twice"), "{why}");
    }

    #[test]
    fn allowed_option_value_sets_are_exact() {
        for (name, words) in [
            ("AddressFamily", &["any", "inet", "inet6"][..]),
            ("IdentitiesOnly", &["yes", "no"][..]),
            ("PubkeyAuthentication", &["yes", "no"][..]),
            ("Compression", &["yes", "no"][..]),
            ("TCPKeepAlive", &["yes", "no"][..]),
            (
                "IPQoS",
                &[
                    "af11", "af12", "af13", "af21", "af22", "af23", "af31", "af32", "af33", "af41",
                    "af42", "af43", "cs0", "cs1", "cs2", "cs3", "cs4", "cs5", "cs6", "cs7", "ef",
                    "le", "none",
                ][..],
            ),
        ] {
            let (_, Accepts::OneOf(accepted)) = ALLOWED_OPTIONS
                .iter()
                .find(|(option, _)| *option == name)
                .unwrap()
            else {
                panic!("{name} must accept only its enumerated words")
            };
            assert_eq!(*accepted, words, "{name}");
            for word in words {
                assert_eq!(t(&format!("host?{name}={word}")).options()[0].1, *word);
            }
            assert!(Target::parse(&format!("host?{name}=ask")).is_err());
        }
    }

    #[test]
    fn option_value_character_fence_is_independent() {
        for c in '\0'..='\u{7f}' {
            let expected =
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-".contains(c);
            assert_eq!(is_value_char(c), expected, "{c:?}");
        }
        for c in ['é', '\u{202e}', '\u{ff21}'] {
            assert!(!is_value_char(c), "{c:?}");
        }
    }

    #[test]
    fn option_values_are_a_conservative_character_set() {
        for v in [
            "a$(id)",
            "a;id",
            "a`id`",
            "%h.evil.example",
            "a%h",
            "a$b",
            "a\"b",
            "a'b",
            "a\\b",
            "a/b",
            "ssh://a",
            "a=b",
            "é",
            "a\u{202e}b",
            "a b",
            "a,b",
            "a:b",
            "a@b",
            "[fd00::1]",
        ] {
            let s = format!("host?IPQoS={v}");
            assert!(
                matches!(Target::parse(&s), Err(TargetError::BadOption { .. })),
                "{s:?}"
            );
        }
        let long = format!("host?IPQoS={}", "a".repeat(256));
        assert!(Target::parse(&long).is_err());
    }

    #[test]
    /// N1 (and with it N4, N5): a target may not carry `ProxyJump` in any form, because
    /// the hop runs as a second ssh without auspex's `BatchMode`.
    fn proxy_jump_is_refused() {
        for v in [
            "jump",
            "me@jump:2200",
            "bastion.example.org",
            "none",
            "NONE",
            "[fd00::1]:22,bastion",
            "a,[fd00::1]:22,last",
            "a,b@c:22,d",
        ] {
            for name in ["ProxyJump", "proxyjump", "PROXYJUMP"] {
                let why = forbidden_why(&format!("root@192.0.2.10?{name}={v}"));
                assert!(
                    why.contains("~/.ssh/config") && why.contains("BatchMode"),
                    "{name}={v}: {why}"
                );
            }
        }
    }

    /// The allow list is the security boundary, so its exact contents are pinned: widening
    /// it (a new name, or `ProxyJump` back) must fail a test.
    #[test]
    fn allow_list_is_exactly_this() {
        let names: Vec<&str> = ALLOWED_OPTIONS.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "AddressFamily",
                "ConnectionAttempts",
                "ServerAliveCountMax",
                "IdentitiesOnly",
                "PubkeyAuthentication",
                "Compression",
                "TCPKeepAlive",
                "IPQoS",
            ]
        );
    }

    /// F7: a host that ssh reads as an IPv4 address in another spelling.
    #[test]
    fn other_ipv4_spellings_are_refused() {
        for s in [
            "root@192.168.1.012",
            "root@3232235786",
            "root@0xc0a8010a",
            "root@0XC0A8010A",
            "root@127.1",
            "root@10.0.0.1.",
            "root@1.2.3.4.5",
            "0x",
            "0",
            "host.0x1f",
            "terra.10",
        ] {
            assert!(
                matches!(Target::parse(s), Err(TargetError::BadHost { .. })),
                "{s}"
            );
        }
        for s in [
            "root@192.168.1.12",
            "0.0.0.0",
            "host1",
            "1host",
            "123.example.com",
            "a.b1",
            "0xfoo",
        ] {
            t(s);
        }
    }

    /// F5: the machine name is used only as a bare hostname.
    #[test]
    fn machine_name_is_a_bare_hostname() {
        for name in [
            "terra?ForwardAgent=yes",
            "terra?Compression=yes",
            "admin@terra",
            "terra:2222",
            "terra?HostName=10.0.0.2",
            "fd00::1",
            "3232235786",
        ] {
            let e = resolve_target(&machine(name, None), None).unwrap_err();
            assert_eq!(e.target_source, TargetSource::MachineName, "{name}");
            assert!(
                matches!(e.error, TargetError::BadHost { .. }),
                "{name}: {e}"
            );
        }
        let r = resolve_target(&machine("terra", None), None).unwrap();
        assert_eq!(r.target.to_string(), "terra");
        // The inventory's own target may still carry an allowed option.
        let r = resolve_target(
            &machine("terra", Some("root@10.0.0.1?Compression=yes")),
            None,
        )
        .unwrap();
        assert_eq!(r.target.options().len(), 1);
    }

    /// F11: an empty override is no override.
    #[test]
    fn empty_override_is_no_override() {
        let m = machine("terra", Some("root@192.168.1.10"));
        for o in ["", "  ", "\t"] {
            let r = resolve_target(&m, Some(o)).unwrap();
            assert_eq!(r.source, TargetSource::Inventory, "{o:?}");
            assert_eq!(r.target.host(), "192.168.1.10");
        }
        let r = resolve_target(&machine("terra", None), Some("")).unwrap();
        assert_eq!(r.source, TargetSource::MachineName);
    }

    #[test]
    fn connect_timeout_is_an_input() {
        assert_eq!(ConnectTimeout::default().seconds(), 5);
        assert_eq!(ConnectTimeout::DEFAULT, ConnectTimeout::default());
        for bad in [0, 61, 3600, u64::MAX] {
            assert_eq!(
                ConnectTimeout::new(bad),
                Err(ConnectTimeoutError { seconds: bad })
            );
        }
        assert_eq!(ConnectTimeout::new(1).unwrap().seconds(), 1);
        assert_eq!(ConnectTimeout::new(60).unwrap().seconds(), 60);
        let opts = SshOptions {
            control_dir: DIR,
            connect_timeout: ConnectTimeout::new(12).unwrap(),
            control_persist: 1200,
            operation: SshOperation::Probe,
        };
        let args = ssh_args(&t("terra"), &opts).unwrap();
        assert!(args.contains(&"ConnectTimeout=12".to_owned()), "{args:?}");
        assert!(
            args.contains(&"ControlPersist=1200s".to_owned()),
            "{args:?}"
        );
        assert!(!args.contains(&"ConnectTimeout=5".to_owned()));
        assert_eq!(
            args.iter()
                .filter(|a| a.starts_with("ConnectTimeout="))
                .count(),
            1
        );
    }

    /// F10: machine names are plain names; tags and buildHost are sanitised.
    #[test]
    fn inventory_names_and_text() {
        for bad in [
            "te\u{1b}]0;pwned\u{7}rra",
            "",
            "-terra",
            "_terra",
            "terra.omnissiah",
            "root@terra",
            "terra?ForwardAgent=yes",
            "ter ra",
            "terr\u{202e}a",
            "térra",
            &"a".repeat(64),
        ] {
            let json = serde_json::json!({ bad: {} }).to_string();
            assert_eq!(
                parse_inventory(&json),
                Err(InventoryError::BadName {
                    machine: bad.to_owned()
                }),
                "{bad:?}"
            );
        }
        let e = InventoryError::BadName {
            machine: "a\u{1b}[2Jb\u{202e}".to_owned(),
        }
        .to_string();
        assert!(!e.contains('\u{1b}') && !e.contains('\u{202e}'), "{e:?}");

        let json = serde_json::json!({
            "terra": {"tags": ["base", "\u{1b}[2J", "k8s\u{202e}"],
                      "deploy": {"buildHost": "\u{1b}[31mbuilder\u{1b}[0m"}},
            "luna": {"deploy": {"buildHost": "\u{1b}[2J", "targetHost": "  \t "}},
            "k8s-master": {}, "web_1": {}, "9lives": {}
        })
        .to_string();
        let ms = parse_inventory(&json).unwrap();
        let get = |n: &str| ms.iter().find(|m| m.name == n).unwrap();
        assert_eq!(get("terra").tags, ["base", "k8s"]);
        assert_eq!(get("terra").build_host.as_deref(), Some("builder"));
        assert_eq!(get("luna").build_host, None);
        // M25: a whitespace-only targetHost is no target.
        assert_eq!(get("luna").target_host, None);
        assert_eq!(
            resolve_target(get("luna"), None).unwrap().source,
            TargetSource::MachineName
        );
    }
}
