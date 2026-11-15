# 0023. The nightly's pull request is opened by a scoped token and never merged

- **Status:** Accepted
- **Date:** 2026-10-24

## Context

`cargo xtask deny --fix` (RT-41 part A) turns a red advisories nightly into a rewritten
`Cargo.lock` whenever every finding is a vulnerability naming something to move to. That is
useful on a laptop with no credential anywhere. For the *nightly* to produce it, something has
to get the lockfile out of the scheduled job and into a pull request, and that needs a
credential this repository does not have.

**It does not have one at all.** `grep -rn "token\|TOKEN\|secrets\." .forgejo/` returns nothing:
neither `ci.yaml` nor `advisories.yaml` nor `preflight.sh` reads a secret, and the only things
the workflows take from the forge are the six `vars` mappings for the Dagger engine address and
the Nix substituters. Whatever is installed for this will be the first write credential in this
CI, which is why the record comes before the credential rather than after it — RT-41's own
acceptance criterion says so, and it is the cheaper order: a token's scope is almost impossible
to narrow once a job depends on it.

*(The state of the world this paragraph describes is gone, and the decision is unaffected —
which is the reason to say so rather than quietly leave it. [ADR 0024](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0024-the-pipeline-is-derivations-and-a-shell.md) retired that pipeline:
`preflight.sh` and the six `vars` mappings were deleted with it, and the workflows now named
`ci.yaml` and `advisories.yaml` are different files that took over the names. What survives
untouched is the part this record is about — one scoped secret, on the one step that pushes, in
the nightly and nowhere else — and the invariant that guards it
(`the_scheduled_checkout_persists_no_credentials`) reads the renamed `advisories.yaml`.)*

Three facts about this forge shape the decision, all measured against
`forge.ugalabugala.org`, Forgejo **15.0.9** (`GET /api/v1/version` → `{"version":"15.0.9"}`).

**Reading the open pull requests needs no credential.**
`GET /api/v1/repos/manjo/rust-app-template/pulls?state=open` answers `200` unauthenticated,
currently `[]`. So "do not open a second pull request for a finding a still-open one already
covers" — RT-41's dedupe criterion — is a read the job can do with no token, and the token is
needed only for the write.

**A merged AGit pull request does not delete its branch.** Learned while cleaning up #53, #54
and #55: an AGit pull request's head is `refs/pull/<n>/head`, a ref separate from whatever branch
was pushed, and all three pushed branches survived their merges. Branch existence is therefore
*not* a usable "already handled" signal, and anything keyed on it would open a second pull
request every night.

**GitLab's `merge_request.*` push options do nothing here.** Measured in RT-31's run, on this
forge: Forgejo accepts `git push -o merge_request.create` without complaint, ignores it, and
prints its ordinary "create a pull request" compare URL. An earlier automation in this repository
read that URL as evidence of a created pull request and reported success over a pull request that
did not exist. The URL is advice, not a receipt. The two facts above were re-measured for this
record; this one is RT-31's and is cited rather than repeated, because reproducing it means
pushing a branch.

And one fact from this repository's own history decides what the job is allowed to do. The zbus
trio — RUSTSEC-2026-0370, 0371 and 0372, `zvariant`, `zbus_names` and `zvariant_derive` — is a
case where the mechanically correct upgrade was the wrong answer. zbus 6 merged those crates;
`ashpd` pins `zbus = "^5.13"` and nothing in this tree can take zbus 6, so no `cargo update`
clears them. The answer was three `ignore` entries in `deny.toml` with the puller named by hand.
A gate cannot tell that case from the other one, which is exactly why RT-40 reports the two
categories and refuses to choose between them, and why `--fix` acts only when there is no
`unmaintained` finding anywhere in the verdict.

## Decision

**A Forgejo API token, held as a repository secret, used only to push a branch and `POST` a pull
request. The job never merges.**

**The credential is a token, not a deploy key.** Both can push. The token wins on the half that
is not pushing:

- Opening the pull request is `POST /api/v1/repos/{owner}/{repo}/pulls`, which answers with the
  created pull request's number and URL. That is a receipt. The deploy-key alternative is an
  AGit push to `refs/for/master`, whose result has to be read out of git's own stderr — and the
  push options that carry a title and body (`-o topic=`, `-o title=`, `-o description=`) are
  **single-line only**, so RT-41's required pull request body (RT-40's verdict line *and* the
  lockfile diff) cannot be sent that way at all. A second API call to fill the body needs the
  token anyway, at which point the key buys nothing.
- An AGit pull request's head is `refs/pull/<n>/head`. Combined with the branch surviving the
  merge, that makes the dedupe key awkward in exactly the direction that matters: the job has to
  decide "is this advisory already in flight?", and the API's open-PR list answers it directly
  while a ref under `refs/pull/` answers a different question.

**Scope: write to this repository's code and pull requests, and nothing else.** On Forgejo that
is `write:repository` — which is also the coarsest thing in this decision and the reason the
next paragraph exists. `write:issue` is not needed: a pull request is created through the
`pulls` endpoint, and the job posts no comments. No `write:organization`, no `write:admin`, no
`write:user`. The token belongs to a machine account with access to this one repository, not to
a human account that happens to have the rights; a human's token carries every repository that
human can reach, and the blast radius of a leak should be one lockfile.

**A token that can push to `master` is the thing to avoid, and Forgejo's branch protection on
`master` is how that is enforced rather than hoped.** `write:repository` is a repository-wide
scope — it does not come in a per-branch flavour — so the push target cannot be narrowed by
scoping. It has to be narrowed by the forge: `master` protected, direct pushes refused, and the
only way in is a pull request a human approves. This is a brace-and-belt arrangement on purpose:
the job is written to push a side branch, and the protection is what makes that a *fact* rather
than a property of the job's current source.

**The job never merges, and that is a decision about the credential as much as about the code.**
A lockfile bump that passes `cargo deny check` has passed exactly one gate. `cargo update` can
move a semver-compatible version that changes behaviour, and the advisories check cannot see it —
the compile and test gates can, which is why the pull request is the unit here. And the zbus trio
is the case where the gate itself is not the authority: the mechanically correct upgrade was
refused by a transitive pin, and the right answer was an `ignore` entry, a `deny.toml` edit this
job must never make. Merging would mean the job deciding that a green advisories run is the whole
question. It is not, and nothing in the job knows that.

## Consequences

The nightly's write capability is one branch and one pull request. Everything else it needs —
the open-PR list for dedupe, the advisory database, the verdict — it already has or can read
unauthenticated.

`--fix` stays useful with no credential installed at all. That is the state after RT-41 part A
and it is the fallback if the token is ever revoked: the nightly reports, a person runs one
command. The part this record authorises is the round trip, not the fix.

The secret is a repository secret in `advisories.yaml`'s job, mapped into the step that needs it
and no other. `ci.yaml` does not get it: a per-push job with a write token is a credential
reachable from every pull request branch, including one an outside contributor wrote, and
advisories deliberately do not run there anyway (`advisories.yaml`'s own header says why).

Rotation is manual and the record is the reminder: a Forgejo token has no expiry by default, so
whoever installs it sets one. A job that silently stops opening pull requests when the token
expires looks exactly like a job with nothing to report, which is the failure mode this whole
line of tickets exists to close — so the step must fail loudly on a `401`, not skip.

## What was rejected, and why

**No credential at all — the nightly keeps reporting, a person runs `--fix`.** This is the status
quo after RT-41 part A, and part A makes it considerably more defensible than it was: the round
trip is now one command on a laptop rather than *read the advisory prose, work out which crate,
type the `cargo update`, re-run the gate*. What it does not fix is latency. An advisory published
on a Friday night waits for somebody to read a notification, and the whole point of the schedule
is that the finding is a function of the date rather than of anyone's attention. It is recorded
here because it is the option this record spends a credential to beat, and because it is where
the repository falls back to if the token is a mistake.

**An SSH deploy key with write access, pushing to `refs/for/master` (AGit).** No API token, no
secret beyond the key, and Forgejo creates the pull request from the push itself. Rejected on the
two measured facts above: the push options that would carry the pull request's body are
single-line only, so RT-41's "the body carries the verdict line and the lockfile diff" cannot be
satisfied by the push; and the resulting pull request's head is `refs/pull/<n>/head`, which makes
the dedupe key the awkward one. A deploy key is also not meaningfully narrower than the token —
both can push to any unprotected branch — so the narrowing that matters comes from branch
protection either way, and the key does not buy it.

**`git push -o merge_request.create`, GitLab's push options.** Zero credential surface beyond the
push, and it is what the ticket originally named. It does not work on this forge: Forgejo 15.0.9
ignores the option and prints a compare URL, and an automation here has already read that URL as
a created pull request. Rejected as a thing that does not exist, and recorded because the failure
mode is *silent success*, which is worse than an error.

**The job merges when the gate is green.** Rejected on the zbus trio, which is this repository's
own counterexample and the reason RT-40 stops at reporting: a green advisories run does not mean
the upgrade is right, it means one of four checks is happy. See the Decision above.

**A `GITHUB_TOKEN`-style automatic per-job token.** Forgejo does expose a job token, and leaning
on it would mean no secret to install or rotate. Not taken here: its permissions are the forge's
default for the actor that triggered the run, which for a `schedule` event is not something this
repository controls or can see from its own files, and a credential whose scope is decided
elsewhere is the opposite of what this record is for. If it turns out to be exactly
`write:repository` on this instance, that is a measurement and an amendment, not an assumption.
