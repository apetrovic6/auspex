//! The half of the advisory bump that touches the forge.
//!
//! The bump itself is [`crate::deny`] with `--fix`: it runs in the checkout, rewrites `Cargo.lock`
//! in place, and leaves a record of what it did at [`crate::deny::ARTEFACT`]. This reads that
//! record and turns it into a branch and a pull request.
//!
//! Both files are read from where `deny --fix` wrote them, and there is no way to point it
//! elsewhere. It had one for a while, when this lived beside a pipeline that ran the bump inside
//! a container and had to hand the result out to the checkout the forge made; that pipeline is
//! gone, and with it the only caller that needed a second spelling of the paths. One path per
//! file, written down once in [`crate::deny`], is the whole interface.
//!
//! # What this is not
//!
//! **It does not merge, and nothing that calls it merges.** ADR 0023 settled that: the automation
//! opens, a person closes. The reason is in the bump report this carries into the pull request
//! body — the advisories gate is the only check that has run against the new lockfile, and a
//! semver-compatible `cargo update` can still break a build or a test. Everything the push
//! pipeline runs will run against the branch the moment it is pushed, and that is the check that
//! has not happened yet.
//!
//! It is also the only step in the nightly that holds a credential, which is why it is a step of
//! its own: `just deny --fix` — the bump, on a laptop or on the runner — never sees
//! `ADVISORY_PR_TOKEN`, and publishing is a deliberate second act.
//!
//! # Why `curl` rather than an HTTP client
//!
//! Two calls, and a subprocess each. `xtask` is the tool every gate and every dev shell builds,
//! and ADR 0022 relies on `cargo xtask check` being sub-second and offline; an HTTP stack and an
//! async runtime in its dependency graph would be paid on every one of those builds to serve a
//! command that runs once a night. `deny.rs` already shells out to `cargo deny` for the same
//! reason. `curl` is a dev-shell package (`nix/tooling.nix`) rather than a host assumption: a
//! runner's PATH has nix, devenv, git, bash, coreutils, cargo and cacert, and nothing else.
//!
//! The token never reaches argv, which is the property `forge.rs` had and a naive port would
//! lose. `curl -u user:token` puts it in the process's command line, where `ps` and anything
//! else on the runner can read it, and curl echoes it back in some error paths. `-K <file>` reads
//! options — including `user =` — from a file instead, so the token stays in this process's
//! memory and in one 0600 file that is deleted on the way out. The same reasoning as the
//! `GIT_ASKPASS` script below, applied to the other half of the round trip.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::deny::BumpArtefact;

/// The pull request's title shows at most this many advisory ids.
///
/// A cap because the forge limits a title — 255 characters in the forge this repository runs on —
/// and a nightly that clears thirty advisories would otherwise fail at the one call a human cannot
/// retry without noticing. The body carries every id unconditionally, and [`Forge::already_open`]
/// reads both.
const TITLE_IDS: usize = 4;

/// How many pull requests to look at before deciding none of them is this one.
///
/// Not a search: the forge's pull-request query has no reliable filter for "body contains this
/// string", and a nightly that miscounts opens a duplicate rather than losing information. A
/// repository with more pull requests than this is a repository where the dedupe gets weaker, which
/// is worth naming here rather than discovering.
const SEARCH_PULLS: usize = 100;

/// The bump, the branch, and the pull request.
///
/// Returns once the pull request exists. Every failure is a loud one, with the forge's own answer
/// in it, because the alternative — a scheduled job that exits 0 having opened nothing — is the
/// failure mode ADR 0023 names as the one to design against.
pub fn run() -> Result<()> {
    // Where `deny --fix` wrote, and the only spelling of either path: the artefact constant is
    // `deny.rs`'s own, and the lockfile is the checkout's, which is the file the bump rewrote.
    let artefact = Path::new(crate::deny::ARTEFACT);
    let lockfile = Path::new("Cargo.lock");

    let text = match fs::read_to_string(artefact) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            // Either the fix run found the gate already green, or it never ran at all. Both mean
            // there is nothing to open, and neither is this step's failure to report as one.
            println!(
                "nothing to open: {} is not there, so `cargo xtask deny --fix` wrote no bump",
                artefact.display()
            );
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", artefact.display()));
        }
    };
    let artefact: BumpArtefact =
        serde_json::from_str(&text).with_context(|| format!("parse {}", artefact.display()))?;

    if !artefact.lockfile_changed {
        println!(
            "nothing to open: the updates ran and left Cargo.lock byte-identical, so there is no change to review"
        );
        return Ok(());
    }

    let lock = fs::read_to_string(lockfile).with_context(|| {
        format!(
            "read {}: the artefact says the lockfile moved, and this cannot open a pull request \
             without the file that moved",
            lockfile.display()
        )
    })?;

    let forge = Forge::from_env()?;
    if let Some(existing) = forge.already_open(&artefact.advisories)? {
        println!(
            "nothing to open: {} already covers {} — {}",
            existing.title,
            artefact.advisories.join(", "),
            existing.html_url
        );
        return Ok(());
    }

    let branch = branch_name(&artefact);
    println!(
        "opening a pull request for {}",
        artefact.advisories.join(", ")
    );
    commit(&branch, &lock, &artefact)?;
    push(&forge, &branch)?;
    forge.open_pull_request(&branch, &title(&artefact), &body(&artefact))
}

/// The forge this step publishes to, from what the workflow provides.
///
/// Every field is a variable a Forgejo job already sets, except the token, which is the repository
/// secret `ADVISORY_PR_TOKEN` mapped into the step's `env:`. Absent means this is not a forge run —
/// a laptop, where the bump is real and the publishing is not — and that is said as an error rather
/// than skipped, because a scheduled job that quietly opens nothing is the failure this whole
/// design exists to avoid.
struct Forge {
    server: String,
    owner: String,
    repo: String,
    /// The account the token belongs to, and only a label: measured against the forge's own
    /// `services/auth/basic.go`, basic auth looks the user up from the *password*, and a valid
    /// access token used as one authenticates whatever username is paired with it.
    actor: String,
    token: String,
    /// The branch the schedule fired on, which for a scheduled or dispatched run is the
    /// repository's default branch. The bump branches from the checkout made of it.
    base: String,
}

impl Forge {
    fn from_env() -> Result<Self> {
        let variable = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .with_context(|| format!("{name} is not set"))
        };
        let server = variable("GITHUB_SERVER_URL")?;
        let repository = variable("GITHUB_REPOSITORY")?;
        let (owner, repo) = repository.split_once('/').with_context(|| {
            format!("GITHUB_REPOSITORY is {repository:?}, which is not `owner/repo`")
        })?;
        let token = variable("ADVISORY_PR_TOKEN").context(
            "nothing is published. The bump itself is in Cargo.lock and its record under \
             target/, and publishing it needs ADVISORY_PR_TOKEN — a repository secret with \
             `write:repository`, mapped into the step's `env:`",
        )?;
        Ok(Self {
            server,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            actor: std::env::var("GITHUB_ACTOR").unwrap_or_else(|_| "advisory-bot".to_owned()),
            token,
            base: variable("GITHUB_REF_NAME")?,
        })
    }

    fn api(&self, path: &str) -> String {
        format!(
            "{}/api/v1/repos/{}/{}{path}",
            self.server.trim_end_matches('/'),
            self.owner,
            self.repo
        )
    }

    fn git_url(&self) -> String {
        format!(
            "{}/{}/{}.git",
            self.server.trim_end_matches('/'),
            self.owner,
            self.repo
        )
    }

    /// Is one of this repository's *open* pull requests already about exactly this set of
    /// advisories?
    ///
    /// The key is the advisory ids and not a branch name, which ADR 0023 measured rather than
    /// assumed: a pull request created by pushing to `refs/for/master` has its head at
    /// `refs/pull/<n>/head`, separate from whatever branch was pushed, and #53, #54 and #55 each
    /// kept their branch after merging. Branch existence is therefore not a usable "already
    /// handled" signal, and anything keyed on it would open a second pull request every night.
    ///
    /// **Open, and not `state=all`, which is the part worth stating because it has a consequence.**
    /// The question ADR 0023 asks is whether an advisory is *in flight*, and the open list answers
    /// it directly. A pull request closed without merging is not an advisory that stopped existing:
    /// the vulnerable version is still in the lock, so the next run reports the same ids and opens
    /// the same pull request again. That is the behaviour to want. The alternative — treating a
    /// closed pull request as handled — makes the nightly green with a live vulnerability and no
    /// open pull request, which is the silent failure this whole line of tickets exists to close.
    /// Closing is a human saying "not this bump"; it is not a human saying "no vulnerability".
    ///
    /// Every id has to be in one pull request rather than spread across several: the bump on this
    /// branch clears the whole set, and a pull request covering half of it is not the same change.
    ///
    /// Authenticated, although ADR 0023 measured that this read answers 200 with no credential at
    /// all. It is authenticated because the same token is already in hand and a private repository
    /// would not answer otherwise, and because a dedupe that silently sees an empty list is a
    /// dedupe that opens a duplicate every night.
    fn already_open(&self, advisories: &[String]) -> Result<Option<Pull>> {
        let url = format!("{}?state=open&limit={SEARCH_PULLS}", self.api("/pulls"));
        let text = self.get(&url, "listing open pull requests")?;
        let pulls: Vec<Pull> = serde_json::from_str(&text)
            .context("the forge's pull-request list is not the shape this reads")?;
        Ok(pulls.into_iter().find(|pull| covers(pull, advisories)))
    }

    /// Create the pull request. `head` is the bare branch name, not `owner:branch`: this repository
    /// writes its own branches, so there is no fork for the forge to disambiguate.
    fn open_pull_request(&self, branch: &str, title: &str, body: &str) -> Result<()> {
        let payload = serde_json::json!({
            "head": branch,
            "base": self.base,
            "title": title,
            "body": body,
        });
        let text = self.post_json(
            &self.api("/pulls"),
            &payload.to_string(),
            "opening the pull request",
        )?;
        let created: serde_json::Value = serde_json::from_str(&text).context(
            "the forge created a pull request and answered with something that is not JSON",
        )?;
        if let Some(url) = created["html_url"].as_str() {
            println!("opened: {url}");
        } else {
            println!(
                "opened: the forge answered {text} without an html_url — the branch is {branch}"
            );
        }
        Ok(())
    }

    /// One authenticated `GET`, and the forge's own words when it says no.
    fn get(&self, url: &str, what: &str) -> Result<String> {
        let (_, body) = self.curl(what, &["--get", url])?;
        Ok(body)
    }

    /// One authenticated `POST` of a JSON body.
    ///
    /// The body goes in a file rather than in `--data`, for the reason the token does: argv is
    /// world-readable on the runner through `/proc`, and a body this size would also brush the
    /// `ARG_MAX` a very long bump report could reach.
    fn post_json(&self, url: &str, payload: &str, what: &str) -> Result<String> {
        let scratch = Scratch::new()?;
        let body_path = scratch.dir.join("payload.json");
        fs::write(&body_path, payload).context("write the request body to a temporary file")?;
        let path = body_path.display().to_string();
        let (_, body) = self.curl(
            what,
            &[
                "--request",
                "POST",
                "--header",
                "Content-Type: application/json",
                "--data",
                &format!("@{path}"),
                url,
            ],
        )?;
        Ok(body)
    }

    /// Run curl, and carry the forge's answer into the error.
    ///
    /// A non-2xx is a failure that names the status and the body: `401` is what an expired or
    /// revoked token looks like, and ADR 0023 requires that to be loud rather than a nightly that
    /// quietly opened nothing. `invalid username or password` and `branch is not in this
    /// repository` are the other two that will actually happen here, and neither is worth guessing
    /// at from a status code.
    ///
    /// `-w '%{http_code}'` with the body written to a file is how one call yields both: curl's
    /// exit status covers transport, not the HTTP answer, and `--fail` would discard the very body
    /// that says what went wrong.
    fn curl(&self, what: &str, args: &[&str]) -> Result<(u16, String)> {
        let scratch = Scratch::new()?;
        let config = scratch.dir.join("curlrc");
        fs::write(&config, self.curl_config()).context("write curl's configuration")?;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600))
            .context("make curl's configuration unreadable to anyone else")?;

        let body_path = scratch.dir.join("body");
        let config_path = config.display().to_string();
        let body_arg = body_path.display().to_string();

        let mut command = Command::new("curl");
        command
            // Silent but not quiet: `-s` drops the progress meter, `-S` keeps the error line, so
            // a DNS failure or a refused connection is still reported.
            .args(["--silent", "--show-error"])
            .args(["--config", &config_path])
            .args(["--output", &body_arg])
            .args(["--write-out", "%{http_code}"])
            .args(args);

        let output = command
            .output()
            .context("`curl` could not be run — it is a dev-shell package (nix/tooling.nix)")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "{what}: curl exited {} — {}",
                output.status.code().unwrap_or(1),
                stderr.trim()
            );
        }
        let status: u16 = stdout.trim().parse().with_context(|| {
            format!("{what}: curl printed {stdout:?} where an HTTP status was expected")
        })?;
        let body = fs::read_to_string(&body_path)
            .with_context(|| format!("{what}: curl wrote no response body"))?;
        if !(200..300).contains(&status) {
            bail!("{what}: {status} — {}", clipped(&body, 2000));
        }
        Ok((status, body))
    }

    /// curl's `-K` file: the credential, and nothing else.
    ///
    /// Quoted the way curl's config parser wants — a value in double quotes, with `\` and `"`
    /// escaped — because a token is opaque and this is the one place its characters are
    /// interpreted rather than passed through.
    fn curl_config(&self) -> String {
        let escape = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
        format!(
            "user = \"{}:{}\"\n",
            escape(&self.actor),
            escape(&self.token)
        )
    }
}

/// A temporary directory that removes itself.
///
/// Three of these exist per call at most, and each holds something that must not outlive it: the
/// curl configuration carries the token, and the request and response bodies carry the bump
/// report. `std::env::temp_dir()` rather than the workspace, because a runner's workspace survives
/// between runs of a job and nothing here should leave a file a later run can read.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("advisory-pr-{}", std::process::id()));
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("make {} unreadable to anyone else", dir.display()))?;
        Ok(Self { dir })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A failure to remove is worth a line and nothing more: the directory is 0700 in the
        // system temp directory, and the step's own outcome must not turn on cleanup.
        if let Err(error) = fs::remove_dir_all(&self.dir) {
            println!("note: could not remove {}: {error}", self.dir.display());
        }
    }
}

/// One pull request, as far as this file cares.
#[derive(Debug, Deserialize)]
struct Pull {
    title: String,
    /// Absent from the forge's list response for a pull request with no description.
    #[serde(default)]
    body: String,
    html_url: String,
}

/// Does this pull request cover every advisory in the set?
///
/// Title and body together, because the title is capped ([`TITLE_IDS`]) and the body is not: a set
/// of six advisories is a pull request whose title names four of them, and a dedupe that read only
/// the title would open a duplicate every night for exactly the bumps most worth reviewing.
fn covers(pull: &Pull, advisories: &[String]) -> bool {
    let text = format!("{}\n{}", pull.title, pull.body);
    advisories.iter().all(|id| text.contains(id))
}

/// The branch, named for the set of advisories it clears.
///
/// The first id so the branch says what it is about, and a hash of the whole sorted set so it says
/// *which* set: two runs that clear a different set must not land on the same branch, and spelling
/// every id out makes a name as long as the list. The hash is FNV-1a rather than a dependency,
/// because this is a name and not a signature — nothing trusts it for anything else, and the dedupe
/// above deliberately does not look at it.
fn branch_name(artefact: &BumpArtefact) -> String {
    let mut sorted: Vec<&str> = artefact.advisories.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for id in &sorted {
        for byte in id.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }
    let first = sorted.first().copied().unwrap_or("unknown");
    format!("chore/advisories-{first}-{hash:06x}")
}

fn title(artefact: &BumpArtefact) -> String {
    let shown: Vec<&str> = artefact
        .advisories
        .iter()
        .take(TITLE_IDS)
        .map(String::as_str)
        .collect();
    let rest = artefact.advisories.len() - shown.len();
    let out = format!("Advisory bump: {}", shown.join(", "));
    match rest {
        0 => out,
        more => format!("{out} and {more} more"),
    }
}

/// The pull request body.
///
/// Both halves of the gate's own words, verbatim out of the artefact: what it said about the old
/// lockfile, and what the bump did to it. Re-rendering either here would be a second renderer of a
/// report `deny.rs` already settled — and the one thing a reviewer of an automated lockfile change
/// needs is the diff's own account of what moved unasked.
fn body(artefact: &BumpArtefact) -> String {
    format!(
        "Automated dependency bump. `cargo xtask deny --fix` ran on the schedule, updated the crates \
         these advisories name, and `cargo deny check` is green against the result.\n\n\
         **Advisories cleared:** {}\n\n\
         ## What the gate said about the old lockfile\n\n> {}\n\n\
         ## What the bump did\n\n```text\n{}\n```\n\n\
         ## Read this before merging\n\n\
         The last line above is the report's own warning, and it is why this is a pull request rather \
         than a merge: **the advisories gate is the only check that has run against this lockfile.** \
         `cargo update` moves semver-compatible versions, and a semver-compatible version can still \
         break a build or a test. Everything the push pipeline runs — build, clippy, tests, the \
         contract check, the Android build — runs against this branch as soon as it is pushed, and \
         that is the check that has not happened yet.\n\n\
         ## What this is not\n\n\
         Nothing here merges, and nothing here will merge this. The automation opens and a person \
         decides (ADR 0023). Closing this is not the end of the advisory: the vulnerable version is \
         still in the lock, so the next run reports the same ids and opens a pull request again. What \
         ends it is a merge, or a change that makes the advisory stop applying — and writing a \
         `deny.toml` `ignore` entry for a vulnerability is not something this job will ever do.\n\n\
         Opened by the nightly: `cargo xtask deny --fix`, then `cargo xtask advisory-pr`.\n",
        artefact.advisories.join(", "),
        artefact.verdict.replace('\n', "\n> "),
        artefact.report,
    )
}

/// Write the new lockfile on a branch and commit it, alone.
///
/// **Alone, and that is the point.** The artefact it was derived from is under `target/`, which is
/// ignored, so the only thing this checkout can stage is `Cargo.lock` — and a bump that also
/// committed its own record would be a bump whose record a later run cannot regenerate.
fn commit(branch: &str, lock: &str, artefact: &BumpArtefact) -> Result<()> {
    fs::write("Cargo.lock", lock).context("write Cargo.lock in the checkout")?;

    git(&["checkout", "-B", branch], &[])?;
    git(&["add", "Cargo.lock"], &[])?;

    let message = format!(
        "chore(deps): bump {} for {}\n\n`cargo xtask deny --fix` on the schedule:\n\n{}\n",
        artefact.packages.join(", "),
        artefact.advisories.join(", "),
        indent(&artefact.report),
    );
    git(
        &[
            // The runner's checkout has no identity of its own, and git refuses a commit without
            // one. `.invalid` is RFC 2606 reserved, so this address reaches nobody: the pull
            // request is attributed to the token's user by the forge, not by this line.
            "-c",
            "user.name=advisory-bot",
            "-c",
            "user.email=advisory-bot@noreply.invalid",
            "commit",
            "-m",
            &message,
        ],
        &[],
    )?;
    Ok(())
}

/// Push the branch.
///
/// The token reaches git through `GIT_ASKPASS`, not through an argument. `git push
/// https://user:token@host/…` is the obvious way and it is worse than it looks: the token lands in
/// the process's argv, where `ps` and anything else on the runner can read it, and it reappears in
/// git's own error text when the push fails — which is the line somebody pastes into an issue. git's
/// documented hook for exactly this runs a named program with a prompt and reads the answer from its
/// stdout, so the token stays in this process's environment. The script goes in the system temp
/// directory rather than the workspace, because a runner's workspace survives between runs of a job
/// and nothing here should leave a file a later run can read.
///
/// `credential.helper=` with an empty value disables whatever helper the runner configured, so the
/// credentials git uses are the ones this names.
fn push(forge: &Forge, branch: &str) -> Result<()> {
    let scratch = Scratch::new()?;
    let askpass = scratch.dir.join("askpass.sh");
    fs::write(
        &askpass,
        "#!/bin/sh\ncase \"$1\" in\n  *[Uu]sername*) printf '%s\\n' \"$ADVISORY_GIT_USERNAME\" ;;\n  *) printf '%s\\n' \"$ADVISORY_GIT_PASSWORD\" ;;\nesac\n",
    )
    .with_context(|| format!("write {}", askpass.display()))?;
    fs::set_permissions(&askpass, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("make {} executable", askpass.display()))?;

    let askpass_path = askpass.display().to_string();
    let result = git(
        &[
            "-c",
            "credential.helper=",
            "push",
            &forge.git_url(),
            &format!("HEAD:refs/heads/{branch}"),
        ],
        &[
            ("GIT_ASKPASS", askpass_path.as_str()),
            ("ADVISORY_GIT_USERNAME", forge.actor.as_str()),
            ("ADVISORY_GIT_PASSWORD", forge.token.as_str()),
        ],
    );
    // `Scratch::drop` removes the script; the push's outcome must not depend on that.
    println!("{}", result?.trim());
    Ok(())
}

/// Run git, and carry its stderr into the error.
///
/// git writes the interesting half of a push to stderr even when the push succeeds, which is why the
/// success path returns it rather than swallowing it.
fn git(args: &[&str], extra_env: &[(&str, &str)]) -> Result<String> {
    let command = format!("git {}", args.join(" "));
    let output = Command::new("git")
        .args(args)
        .envs(extra_env.iter().copied())
        .output()
        .with_context(|| format!("`{command}` could not be run — is git in the dev shell?"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        bail!(
            "`{command}` exited {}: {}",
            output.status.code().unwrap_or(1),
            stderr.trim()
        );
    }
    Ok(stderr)
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A response body, shortened for an error line without cutting the end that names the problem.
fn clipped(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= max {
        return trimmed.to_owned();
    }
    format!("…{}", &trimmed[trimmed.len() - max..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An artefact with these advisory ids and reports that say something distinguishable.
    fn artefact(ids: &[&str]) -> BumpArtefact {
        BumpArtefact {
            advisories: ids.iter().map(|id| (*id).to_owned()).collect(),
            packages: ids.iter().map(|id| format!("crate-{}", id.len())).collect(),
            updates: ids.iter().map(|id| format!("{id}@1.0.0")).collect(),
            lockfile_changed: true,
            verdict: "advisories: 4 vulnerabilities, 0 unmaintained; every one names a fix"
                .to_owned(),
            report: "advisory bump: 4 vulnerabilities, every one of them naming a fix; one `cargo update` with 3 of them, and `cargo deny check` is green after it".to_owned(),
        }
    }

    fn pull(title: &str, body: &str) -> Pull {
        Pull {
            title: title.to_owned(),
            body: body.to_owned(),
            html_url: "https://forge.invalid/pulls/1".to_owned(),
        }
    }

    /// The dedupe is the thing that decides whether a nightly writes to the repository, and it is
    /// the half no laptop can exercise: it reads a forge. So the predicate is a function rather
    /// than a closure inside a request, and these are its cases.
    #[test]
    fn a_pull_request_covers_the_set_only_when_it_names_every_advisory() {
        let ids: Vec<String> = ["RUSTSEC-2026-0111", "RUSTSEC-2026-0112"]
            .iter()
            .map(|id| (*id).to_owned())
            .collect();

        assert!(covers(
            &pull(
                "Advisory bump",
                "cleared RUSTSEC-2026-0111 and RUSTSEC-2026-0112"
            ),
            &ids
        ));
        // Half a set is not the same change: this branch bumps both crates at once, and a pull
        // request about one of them is a different diff.
        assert!(!covers(
            &pull("Advisory bump", "cleared RUSTSEC-2026-0111"),
            &ids
        ));
        // And two pull requests that between them name both are still not one that names both.
        assert!(!covers(&pull("RUSTSEC-2026-0111", ""), &ids));
    }

    #[test]
    fn the_body_is_what_the_dedupe_reads_because_the_title_is_capped() {
        let ids: Vec<String> = (0..6).map(|n| format!("RUSTSEC-2026-02{n}")).collect();
        let many: Vec<&str> = ids.iter().map(String::as_str).collect();
        let artefact = artefact(&many);
        let titled = pull(&title(&artefact), "");

        // The title alone cannot carry six ids, so a dedupe keyed on titles would open a duplicate
        // every night for exactly the largest bumps.
        assert!(!covers(&titled, &ids));
        assert!(covers(&pull(&title(&artefact), &body(&artefact)), &ids));
    }

    #[test]
    fn a_branch_name_says_which_set_it_clears() {
        let one = branch_name(&artefact(&["RUSTSEC-2026-0111", "RUSTSEC-2026-0112"]));
        let same = branch_name(&artefact(&["RUSTSEC-2026-0112", "RUSTSEC-2026-0111"]));
        let other = branch_name(&artefact(&["RUSTSEC-2026-0111", "RUSTSEC-2026-0399"]));

        // Order is cargo-deny's emission order, which is not a stable identity: the same set
        // reported in a different order must not produce a second branch.
        assert_eq!(one, same);
        assert_ne!(one, other);
        assert!(one.starts_with("chore/advisories-RUSTSEC-2026-0111-"));
    }

    #[test]
    fn a_title_names_what_fits_and_counts_what_does_not() {
        let two = title(&artefact(&["RUSTSEC-2026-0111", "RUSTSEC-2026-0112"]));
        assert_eq!(two, "Advisory bump: RUSTSEC-2026-0111, RUSTSEC-2026-0112");

        let six = title(&artefact(&["a", "b", "c", "d", "e", "f"]));
        assert_eq!(six, "Advisory bump: a, b, c, d and 2 more");
    }

    /// What the ticket asks the body to carry, asserted rather than eyeballed: the verdict line and
    /// the lockfile diff, both verbatim out of the artefact, plus the sentence that says nobody here
    /// merges.
    #[test]
    fn the_body_carries_both_halves_of_the_gate_and_the_decision_that_keeps_it_open() {
        let artefact = artefact(&["RUSTSEC-2026-0111", "RUSTSEC-2026-0112"]);
        let text = body(&artefact);

        assert!(text.contains(artefact.verdict.as_str()), "{text}");
        assert!(text.contains(artefact.report.as_str()), "{text}");
        assert!(text.contains("RUSTSEC-2026-0112"), "{text}");
        assert!(
            text.contains("the advisories gate is the only check"),
            "{text}"
        );
        assert!(text.contains("Nothing here merges"), "{text}");
        // The consequence of deduping against open pull requests, said where a person decides.
        assert!(text.contains("opens a pull request again"), "{text}");
    }

    /// The credential is what must not leak, and argv is where a naive port would put it.
    ///
    /// `curl -u` is the obvious spelling and lands the token in `/proc/<pid>/cmdline`, which is
    /// world-readable on the runner; `-K <file>` is why [`Forge::curl`] exists. This asserts the
    /// file's contents are the credential and that nothing in an invocation carries it — the two
    /// halves of the property, pinned rather than left to a reader noticing.
    #[test]
    fn the_token_reaches_curl_through_its_configuration_and_not_its_arguments() {
        let forge = Forge {
            server: "https://forge.invalid".to_owned(),
            owner: "manjo".to_owned(),
            repo: "rust-app-template".to_owned(),
            actor: "advisory-bot".to_owned(),
            token: "sekrit".to_owned(),
            base: "master".to_owned(),
        };

        assert_eq!(
            forge.curl_config(),
            "user = \"advisory-bot:sekrit\"\n",
            "curl reads `user = \"name:password\"` from its -K file; a change here changes how the \
             credential is delivered"
        );

        // A token with the two characters curl's config parser treats specially must survive the
        // round trip rather than terminating the value early.
        let quoted = Forge {
            token: r#"a"b\c"#.to_owned(),
            ..Forge {
                server: "https://forge.invalid".to_owned(),
                owner: "manjo".to_owned(),
                repo: "rust-app-template".to_owned(),
                actor: "advisory-bot".to_owned(),
                token: "sekrit".to_owned(),
                base: "master".to_owned(),
            }
        };
        assert_eq!(
            quoted.curl_config(),
            "user = \"advisory-bot:a\\\"b\\\\c\"\n",
            "an unescaped quote would end the value and leave the rest of the token as a curl \
             option"
        );

        // And the URL builder, which is the other half of what a call is made of.
        assert_eq!(
            forge.api("/pulls"),
            "https://forge.invalid/api/v1/repos/manjo/rust-app-template/pulls"
        );
        assert_eq!(
            forge.git_url(),
            "https://forge.invalid/manjo/rust-app-template.git"
        );
    }
}
