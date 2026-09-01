# Sketch: a pgpushy GitHub Action

**Status:** a sketch, not a plan. Written 2026-08-19 so the reasoning is not
re-derived. Delete this file when the action exists.

## Why it is worth doing

The plan-artifact flow in [spec §14](spec.md) — plan under a preview role,
persist the plan, approve *that artifact*, apply exactly it under a deploy role
— is not really a CLI shape. It is a CI shape, and GitHub already supplies
every piece of it:

- **`environment:` with required reviewers is the approval gate.** It stops a
  job until a human approves, which is precisely what §14's artifact wants and
  exactly the thing a CLI cannot do for itself.
- **Two environments give two credential sets**, so the preview/deploy role
  split falls out of GitHub's own model. pgpushy needs no feature for it.
- **Artifacts carry the plan between jobs**, which is what makes the approval
  apply to a *reviewed* object rather than to a re-computed one.

Atlas ships `ariga/atlas-action` and it is a large part of why Atlas is
adoptable at all; the CLI alone is not where teams meet a schema tool.

## What is buildable now, and what is not

| | |
|---|---|
| Run `validate` on a PR | now |
| Run `plan` and post the human output as a PR comment | now |
| Upload the plan as an artifact, apply exactly it | now — `plan --plan-out` / `apply --plan` (spec §8.9) |
| Label or gate a PR on destructive changes | now — exit 2 + `summary.json` (spec §9.1) |

Both halves now exist, so the action can be designed whole; release binaries
remain the real first task below.

## The prerequisite nobody would guess: release binaries

pgpushy publishes to crates.io and nowhere else. `cargo install pgpushy` inside
an action means installing **libclang** and compiling libpg_query's C sources
through `bindgen`, plus the whole dependency tree — minutes on every run, and a
toolchain the consuming repository otherwise has no reason to carry.

**An action needs prebuilt per-platform binaries**, published by a release
workflow the way pgschema publishes its own. There is a pleasing symmetry here:
pgpushy's managed provider already *is* a downloader-and-verifier of
per-platform binaries (§8.5), so the shape — asset naming, SHA-256 verification,
cache-by-version-and-platform — is one this project has already designed once
and can copy from itself.

This is the real first task. Without it the action is technically possible and
practically unpleasant.

## Shape

A **composite** action, not Docker and not JavaScript: it needs to fetch one
binary and run it, a container pull would cost more than the work, and there is
no reason to carry a Node runtime. Its own repository, so `@v1` tagging works
the way consumers expect.

```yaml
# .github/workflows/schema.yml, in a repository that uses pgpushy
on:
  pull_request:
  push:
    branches: [main]

jobs:
  plan:
    runs-on: ubuntu-latest
    environment: db-preview          # credentials that can read, not write
    steps:
      - uses: actions/checkout@v5
      - uses: arcanyx-pub/pgpushy-action@v1
        with:
          command: plan
          env: prod                  # the [env.prod] block in pgpushy.toml
          plan-out: ./plan           # needs spec §14
          comment: true              # post the human plan on the PR
      - uses: actions/upload-artifact@v4
        with:
          name: pgpushy-plan
          path: ./plan

  apply:
    needs: plan
    if: github.event_name == 'push'
    runs-on: ubuntu-latest
    environment: db-deploy           # required reviewers — this is the gate
    steps:
      - uses: actions/download-artifact@v4
        with:
          name: pgpushy-plan
          path: ./plan
      - uses: arcanyx-pub/pgpushy-action@v1
        with:
          command: apply
          env: prod
          plan: ./plan               # needs spec §14
```

Note what the apply job does *not* need: a checkout. §14's decision to put the
apply order in the artifact is what makes that true, and it is the difference
between a deploy job that carries a source tree and one that carries only what
was approved.

## Things to settle before building

- **Approval semantics.** A human approving the `db-deploy` environment *is*
  the approval, so `apply --plan` should not also want a terminal. Whether that
  means it implies `--auto-approve`, or whether the flag stays required and
  explicit, is an interface decision with a safety flavour — the second is
  noisier and harder to do by accident.
- **Comment behaviour.** Update one comment in place rather than appending on
  every push; a schema PR that gets ten pushes should not carry ten plans.
  Needs `permissions: pull-requests: write`. Decide what an empty plan does —
  probably a terse "no changes" rather than silence, since silence is
  indistinguishable from the action not running.
- **Wrong-target protection.** §14 has the artifact record the target's system
  identifier, which covers the case of a `plan` job and an `apply` job pointed
  at different `env` blocks. Worth testing deliberately, because it is the
  failure this whole shape exists to prevent.
- **Binary verification.** The action should verify the pgpushy binary it
  downloads against a shipped hash, for the same reason §8.5 verifies
  pgschema's — a downloaded-and-executed binary in CI is a supply-chain step
  whether or not anyone calls it that.
- **Version pinning.** `@v1` for the action, and an explicit `pgpushy-version`
  input, so a consuming repository's schema runs do not change under it when a
  new pgpushy ships.
