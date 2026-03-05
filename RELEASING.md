# Releasing http-relay

## Prerequisites

- Push access to `main` branch
- `CARGO_REGISTRY_TOKEN` secret configured in GitHub repo settings
  (Settings → Secrets and variables → Actions → New repository secret)

## Release Steps

### 1. Bump the version and create version PR

Make sure you are working on the most recent main branch.

Update the version in `Cargo.toml`:

```toml
[package]
version = "0.7.0"  # bump appropriately
```

Then update the lockfile:

```bash
cargo check
```

Create the version PR:

```bash
git checkout -b chore/v0.7.0
git add Cargo.toml Cargo.lock
git commit -m "release: v0.7.0"
git push origin chore/v0.7.0
```

PR title: chore: v0.7.0
PR description: Changelogs

### 2. Tag and push

After the PR has been merged, tag the commit.

```bash
git checkout main
git pull origin main
git tag v0.7.0
git push origin v0.7.0
```

This triggers the release workflow. That's it — CI handles the rest.

## What CI Does Automatically

When a `v*` tag is pushed, the [release workflow](.github/workflows/release.yml) runs:

1. **Builds binaries** for 7 targets:
   - Linux: x86_64 (glibc), x86_64 (musl), aarch64
   - macOS: x86_64, aarch64 (Apple Silicon)
   - Windows: x86_64, aarch64

2. **Creates a GitHub Release** with auto-generated release notes and all binaries attached

3. **Publishes to crates.io** via `cargo publish`

## Post-Release Verification

- [ ] [GitHub Releases](https://github.com/pubky/http-relay/releases) — new release with 7 binary assets
- [ ] [crates.io/crates/http-relay](https://crates.io/crates/http-relay) — new version visible
- [ ] Install works: `cargo install http-relay`

## Versioning

Follow [Semantic Versioning](https://semver.org/):

- **Patch** (0.6.x): bug fixes, no API changes
- **Minor** (0.x.0): new features, backwards-compatible
- **Major** (x.0.0): breaking API changes
- **Release Candidate** (0.0.0-rc.x): Test releases

## Troubleshooting

**CI build fails?** Fix the issue, then delete and re-push the tag:

```bash
git tag -d v0.7.0
git push origin :refs/tags/v0.7.0
# fix the issue, commit, push
git tag v0.7.0
git push origin v0.7.0
```

**cargo publish fails?** Check that `CARGO_REGISTRY_TOKEN` is set and not expired. Generate a new token at [crates.io/settings/tokens](https://crates.io/settings/tokens).
