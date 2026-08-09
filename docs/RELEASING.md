# Releasing

> **Nothing has been released yet.** This documents the intended flow, which
> mirrors `snowdrop-id-rs`. The publish workflow itself lands with M6, since
> there is no point publishing a tool whose default pgschema provider is still
> "bring your own binary".

The two crates (`pgpushy-core`, `pgpushy`) are versioned in lockstep and
published to crates.io from CI using **Trusted Publishing (OIDC)** — GitHub
Actions mints a short-lived crates.io token per run, so there is no long-lived
`CARGO_REGISTRY_TOKEN` secret to leak or rotate.

**Why lockstep:** one version, one CHANGELOG, one tag. `pgpushy` depends on
`pgpushy-core` at an exact version anyway, and while the two move together
there is nothing to gain from separate numbering. Worth revisiting if
`pgpushy-core` ever becomes useful on its own — it is a genuinely reusable
piece (parse a tree of DDL, lift foreign keys, emit a deterministic
desired-state document), and if anyone depends on it directly, dragging it
through a bump it did not earn stops being free.

## Before the first release

The version floor is **the pgschema version pgpushy is tested against**
(spec §13), which is a promise about CI rather than about the code. So a
release means the CI matrix in [`ci.yml`](../.github/workflows/ci.yml) and
`MIN_PGSCHEMA` in [`provider/mod.rs`](../pgpushy/src/provider/mod.rs) must
agree. Raising one without the other makes the floor a lie in whichever
direction is worse.

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
   `just publish` pushes the `vX.Y.Z` tag, which triggers the publish workflow.

## Checklist for a release that is not just code

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
