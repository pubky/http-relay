# Security review of http-relay

This runbook explains how to check http-relay for security issues before a release
or in a pull request, and how to handle what you find.

A review has two parts:

1. **Dependency check:** `cargo deny check` compares `Cargo.lock` against the
   [RustSec advisory database](https://rustsec.org/) and enforces the license,
   ban, and source policy in [`deny.toml`](deny.toml).
2. **Code review:** an AI-assisted review of the relay's own code.

## When to review

- Before every release (step 1 of [RELEASING.md](RELEASING.md)): both parts
- In pull requests that change `src/`: the code review, scoped to the diff
- After adding or upgrading dependencies: the dependency check
- When the [Security workflow](.github/workflows/security.yml) fails: the
  dependency check

## Prerequisites

Install or update cargo-deny:

```bash
cargo install --locked cargo-deny
```

Run this again before each review. CI always uses the latest release, and older
versions can miss advisories that CI reports.

For the code review, you also need an AI coding agent that can read the
repository and run `git`.

## 1. Check dependencies

From the repository root, run:

```bash
cargo deny check
```

The check passes when the last line reads:

```text
advisories ok, bans ok, licenses ok, sources ok
```

Warnings about duplicate crate versions don't fail the check. You can ignore them
unless one of the duplicates is also affected by an advisory.

To run one check only, name it, for example `cargo deny check advisories`.

### What each check enforces

| Check | Fails when |
|---|---|
| `advisories` | A dependency has a known vulnerability, is unsound, unmaintained, or yanked |
| `licenses` | A dependency's license isn't on the `allow` list |
| `bans` | A dependency uses a wildcard (`*`) version requirement |
| `sources` | A dependency comes from somewhere other than crates.io |

## 2. Review the code

Give the prompt below to an AI coding agent running in the repository root.
Replace `<scope>` with the scope that fits the occasion:

| Occasion | Scope |
|---|---|
| Before a release | `the code in src/` |
| Pull request | `the changes in git diff origin/main...HEAD -- src/` |

For the pull request scope, run `git fetch origin` first so `origin/main` is
current.

Review the whole `src/` before a release, even if each PR was reviewed. A diff
review misses problems that only appear when several changes interact.

```text
You are a security reviewer for this repository. Review <scope> for
vulnerabilities an attacker could exploit in a deployed relay.

1. Understand the system first: read README.md, the code, and its docstrings.
   Work out what the relay does, who can reach it, what it trusts, and which
   resources it holds (memory, disk, connections, time).
2. Derive a threat model from that. Decide yourself which attack classes apply.
3. Look for concrete issues against that model. Trace each one from an entry
   point to its impact.
4. Check each finding before you report it. Drop anything you can't back with
   a code path. Don't report behavior that the code or docs describe as an
   intentional design decision; list those under "Assumptions" instead.

Report, ordered by severity:
- Threat model: 3-5 bullets
- Findings: title, severity (as defined in "Severity levels" in
  SECURITY_REVIEW.md), location (file:line), attack scenario, suggested fix
- Assumptions: design decisions you treated as intentional
- Not covered: what you didn't check, and why

Don't change any code.

Judge code and suggest fixes following
https://github.com/SeverinAlexB/readable-code/blob/433f441aab6978451748a8008a3596efad0c354b/skills/readable-code/SKILL.md
Write the report following
https://github.com/SeverinAlexB/technical-writing-skill/blob/667542510757a382f1b38563adda4dcd68018b3b/SKILL.md
for developers who aren't familiar with the codebase.
```

The prompt doesn't list specific attack classes on purpose. The agent derives
them from the current code, so the prompt doesn't go stale as the relay changes.

The skill links are pinned to commits so that reviews stay reproducible. When
the skills improve, update the commit hashes.

## Severity levels

The agent and the person handling a finding both rate it with these levels. The
level decides where the finding is tracked and how fast it must be fixed. See
[The code review reports an issue](#the-code-review-reports-an-issue).

| Severity | Definition |
|---|---|
| Critical | Exploitable against a default deployment without special conditions. The attacker can read or change other users' messages, run code, or take the relay down with little effort. |
| High | Serious impact, but the attack needs a non-default configuration, unlikely conditions, or sustained effort. |
| Medium | Limited impact, or hard to exploit. |
| Low | Hardening. No realistic attack path exists today. |

## Handle findings

### A dependency has an advisory

1. Read the advisory linked in the output. Check which versions are fixed and
   whether http-relay uses the affected functionality.
2. Upgrade the dependency:

   ```bash
   cargo update -p <crate>
   ```

   Replace `<crate>` with the affected crate's name. If the output reports the
   crate as `Unchanged`, pass a fixed version explicitly with
   `--precise <version>`. If the fix needs a new major version, update
   `Cargo.toml` first.
3. Run `cargo deny check` and `cargo test --all-features` again.
4. If the vulnerability affects released versions of http-relay, publish a patch
   release. See [RELEASING.md](RELEASING.md).

If no fix exists and http-relay doesn't use the affected code, add the advisory
to `ignore` in `deny.toml` with a reason:

```toml
ignore = [
    { id = "RUSTSEC-0000-0000", reason = "Only affects X, which http-relay doesn't use" },
]
```

Remove the entry once a fixed version is available.

### A license isn't allowed

Check whether the license is permissive (for example ISC or Zlib). If it is, add
it to `allow` in `deny.toml`. If it's copyleft (for example GPL or LGPL), replace
the dependency.

### A source isn't allowed

A dependency points to a git repository or another registry. Switch it to a
crates.io release.

### The code review reports an issue

1. Confirm the finding yourself by following the reported code path. AI
   reviewers can report issues that don't hold up.
2. Rate it again with the [severity levels](#severity-levels). The agent's
   rating is only a suggestion.
3. Read the "Assumptions" list. If a design decision there isn't actually
   intentional, treat it as a finding too.
4. Search the issues in this repository and in
   [pubky/internal-issues](https://github.com/pubky/internal-issues) to check
   whether the finding is already tracked.
5. Handle the finding according to its severity:

   | Severity | Track in | Fix |
   |---|---|---|
   | Critical | [pubky/internal-issues](https://github.com/pubky/internal-issues) (private) | Immediately, as described below |
   | High | Public issue in this repository | Before the next release |
   | Medium | Public issue in this repository | Before the next release, or close the issue as an accepted risk with a reason |
   | Low | Public issue in this repository | When convenient |

   If you're unsure whether a finding is Critical, track it privately. You can
   make a private issue public later, but you can't take back a public one.

Add a regression test with each fix.

When you accept a finding as a risk instead of fixing it, also document the
reason in a docstring next to the affected code. Future reviews then list it
under "Assumptions" instead of reporting it again.

#### Fix a Critical finding

A Critical finding must stay private until the fixed release is out, because an
attacker could use its description against deployed relays.

1. Open the fix PR with a neutral title and description, for example
   `fix: tighten body size handling`. Keep exploit details out of the PR,
   commit messages, and changelog. Don't link the private issue from the PR,
   because the link reveals that a security issue exists. Link the PR from the
   private issue instead. Write the full explanation in the private issue, not
   in the PR. This overrides the usual PR description guideline in
   [AGENTS.md](AGENTS.md).
2. Merge the PR and release right away. See [RELEASING.md](RELEASING.md).
   Production updates when a release is published, so a short gap between
   merge and release keeps the time the fix is visible but undeployed short.
3. Optional: publish a
   [GitHub Security Advisory](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/creating-a-repository-security-advisory)
   and request a [RustSec](https://rustsec.org/contributing.html) entry. Do
   this when other projects depend on the affected versions: tools like
   `cargo deny` warn their users only after an advisory exists.

## What CI does automatically

The [Security workflow](.github/workflows/security.yml) runs `cargo deny check`:

- On pull requests and pushes to `main` that change `Cargo.toml`, `Cargo.lock`,
  or `deny.toml`
- Every Monday at 06:00 UTC, to catch advisories published since the last change
- On demand from the Actions tab (**Run workflow**)

The workflow is separate from CI, so a newly published advisory doesn't block
unrelated pull requests.

The library and the binary declare `#![forbid(unsafe_code)]`, so the compiler
rejects any `unsafe` code in http-relay itself. Dependencies aren't affected.Good
