# Security review of http-relay

This runbook explains how to check http-relay for security issues before a release
and how to handle what you find.

A review has two parts:

1. **Dependency check:** `cargo deny check` compares `Cargo.lock` against the
   [RustSec advisory database](https://rustsec.org/) and enforces the license,
   ban, and source policy in [`deny.toml`](deny.toml).
2. **Code review:** an AI-assisted review of the relay's own code.

## When to review

- Before every release (step 1 of [RELEASING.md](RELEASING.md))
- After adding or upgrading dependencies
- When the [Security workflow](.github/workflows/security.yml) fails

## Prerequisites

Install cargo-deny:

```bash
cargo install --locked cargo-deny
```

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

Run an AI-assisted review of the code in `src/`.

<!-- TODO: Add the review prompt, scope, and what the reviewer should check. -->

## Handle findings

### A dependency has an advisory

1. Read the advisory linked in the output. Check which versions are fixed and
   whether http-relay uses the affected functionality.
2. Upgrade the dependency:

   ```bash
   cargo update -p <crate>
   ```

   Replace `<crate>` with the affected crate's name. If the fix needs a new major
   version, update `Cargo.toml` first.
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

## What CI does automatically

The [Security workflow](.github/workflows/security.yml) runs `cargo deny check`:

- On pull requests and pushes to `main` that change `Cargo.toml`, `Cargo.lock`,
  or `deny.toml`
- Every Monday at 06:00 UTC, to catch advisories published since the last change
- On demand from the Actions tab (**Run workflow**)

The workflow is separate from CI, so a newly published advisory doesn't block
unrelated pull requests.

The library and the binary declare `#![forbid(unsafe_code)]`, so the compiler
rejects any `unsafe` code in http-relay itself. Dependencies aren't affected.
