# Releasing

> **0.1.0 was published by hand and carries no `v0.1.0` tag** — Trusted
> Publishing could not be configured before the crates existed. That bootstrap
> is done; every release from 0.1.1 on goes through the flow below.

The two crates (`pgpushy-core`, `pgpushy`) are versioned in lockstep and
published to crates.io from CI using **Trusted Publishing (OIDC)** — GitHub
Actions mints a short-lived crates.io token per run, so there is no long-lived
`CARGO_REGISTRY_TOKEN` secret to leak or rotate.

Each crate's *Settings → Trusted Publishing* entry on crates.io names this
repository, the workflow filename `publish.yml` and the environment
`crates-io`. The environment name is checked against the OIDC token, so it must
match [`publish.yml`](../.github/workflows/publish.yml)'s `environment:` line
and must exist in this repository's settings; a job outside that environment,
or in one spelled differently, is refused at the token exchange. Both crates
carry the same entry — the workflow publishes both, and fails on whichever was
missed.

**Why lockstep:** one version, one CHANGELOG, one tag. `pgpushy` depends on
`pgpushy-core` at an exact version anyway, and while the two move together
there is nothing to gain from separate numbering. Worth revisiting if
`pgpushy-core` ever becomes useful on its own — it is a genuinely reusable
piece (parse a tree of DDL, lift foreign keys, emit a deterministic
desired-state document), and if anyone depends on it directly, dragging it
through a bump it did not earn stops being free.

The pgschema versions pgpushy names are promises about CI rather than about the
code (spec §13). A release means the matrix in
[`ci.yml`](../.github/workflows/ci.yml), `MIN_PGSCHEMA` in
[`provider/mod.rs`](../pgpushy/src/provider/mod.rs) and `PINNED_PGSCHEMA` in
[`provider/managed.rs`](../pgpushy/src/provider/managed.rs) must agree: the
matrix runs both, the floor is its low end, the pin is its high end. Moving one
without the others makes it a claim nothing tests.

## Bumping the pinned pgschema

The managed backend downloads a version pgpushy pins and verifies it against a
hash pgpushy ships, so bumping that version is a security-relevant change, not
a version-string edit:

```console
$ ver=1.13.0
$ for p in linux-amd64 linux-arm64 darwin-amd64 darwin-arm64; do
    curl -fsSL -o "pgschema-$ver-$p" \
      "https://github.com/pgplex/pgschema/releases/download/v$ver/pgschema-$ver-$p"
  done
$ sha256sum pgschema-$ver-*
```

Put all four rows in `HASHES` in
[`provider/managed.rs`](../pgpushy/src/provider/managed.rs) and commit them in
one change with the version bump, so the hashes are reviewed alongside the
version they belong to. A unit test fails if the pinned version lacks a hash
for any published platform — but nothing can check that a hash is the *right*
one except computing it from the real asset, so do that rather than copying
from anywhere.

Then move whichever of the two version constants this is (spec §13):

- **`PINNED_PGSCHEMA`** is what the managed backend downloads: the *newest*
  tested version. Raising it means the new version must be in the CI matrix.
- **`MIN_PGSCHEMA`** is the floor a bring-your-own binary must clear: the
  *oldest* tested version. Raising it makes existing BYO setups fail, so raise
  it only deliberately — when the old version is dropped from the matrix
  because pgpushy has come to rely on something newer.

The CI matrix in [`ci.yml`](../.github/workflows/ci.yml) must run both ends. A
constant that names a version the matrix does not test is a promise nothing
checks.

Re-verify before pinning, and verify by **applying**, not by seeding a target
with `psql` — pgschema reads a hand-built target correctly even where it would
never build one like it (impl-plan §1).

## Release flow

1. **On a feature branch, bump the version** (the commit rides along in a
   normal PR):
   ```console
   $ just bump patch     # or: minor | major
   ```
   This updates the workspace version, the internal dependency requirement,
   `Cargo.lock`, and stamps the `## [Unreleased]` CHANGELOG section, then
   commits `Release vX.Y.Z`.
2. **Open the PR and merge it** to `main`. CI must pass, including the MSRV job
   — `just ci` runs the same checks locally, `just msrv` just that one.
3. **Tag and trigger the publish** from an up-to-date `main`:
   ```console
   $ git switch main && git pull
   $ just publish
   ```
   `just publish` pushes the `vX.Y.Z` tag, and that tag is what starts
   [`publish.yml`](../.github/workflows/publish.yml). The workflow re-checks
   that the tag matches the workspace version before it sends anything, since
   a publish cannot be taken back; then it publishes `pgpushy-core`, waits for
   it to appear in the index, and publishes `pgpushy`. If it fails, the tag
   already exists and `just publish` will refuse the retry — delete the tag
   locally and on the remote first. Each publish step skips a version already
   in the index, so re-running after a half-finished release is safe.

## Checklist for a release that is not just code

- Both crates carry the Apache-2.0 text —
  `tar tzf target/package/pgpushy-*.crate | grep LICENSE`.
- `CHANGELOG.md` has a real `## [Unreleased]` section. `just bump` stamps it
  with the version and date; it does not write it.
- `docs/spec.md` version and date reflect any decisions the release changed.
  The spec is the record of *why*, so a release that changed behavior without
  changing the spec has lost something.
- The README's worked example still matches what the tool prints. It is the
  first thing anyone reads and the easiest thing to let drift.

## Verifying a release candidate by hand

Neither crate is much use without a real database, so before tagging:

```console
$ export PGPUSHY_TEST_PG_URL=postgres://postgres:pw@localhost:5432/pgpushy
$ export PGPUSHY_TEST_PGSCHEMA=/path/to/pgschema
$ just ci
$ just package        # the crates build as packaged, not just in the workspace
```

Without those two variables the integration tests **skip rather than fail**,
which is right for contributors but means a green `just test` alone does not
mean the tool works.
