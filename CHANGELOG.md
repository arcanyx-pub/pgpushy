# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing released yet. See [`docs/spec.md`](docs/spec.md) for the specification
and [`docs/impl-plan.md`](docs/impl-plan.md) for the build plan.

### Added

- `pgpushy validate` — the whole offline pipeline, with no database connection
  and no pgschema binary. Discovers a source tree, parses it with the real
  Postgres grammar, enforces the statement allow-list, resolves the
  managed-schema set, checks for duplicate objects, unresolvable foreign-key
  referents and colliding unnamed foreign keys, orders schemas by their
  cross-schema foreign keys, and synthesizes the desired state. `--out` writes
  the synthesized document for inspection.
- `pgpushy-core` — the pure half: parsing, FK-lift, qualification, validity
  checks, cross-schema ordering and synthesis, with no IO of any kind.
- `pgpushy plan` — one pgschema plan per managed schema, in cross-schema
  dependency order. Read-only throughout: every statement pgpushy's own
  inspection issues is a `SELECT`, and pgschema builds its comparison model in
  a separate plan database.
- A bring-your-own pgschema provider: `--pgschema-path` or a `PATH` lookup,
  with the version floor enforced from the `Version:` line of `pgschema
  --help`. Below the floor is a hard error; an unreadable version warns.
- One connection resolution, not two: pgpushy resolves the target itself and
  hands pgschema every parameter explicitly, so pgschema resolves nothing and
  the two cannot reach different databases. `PG*` and the `PGSCHEMA_PLAN_*`
  family are stripped from the subprocess environment for the same reason, and
  `PGSERVICE`/`PGSERVICEFILE` are refused rather than silently dropped, since
  pgpushy cannot interpret them.
- `pgpushy apply` — plans every managed schema, presents them as one reviewable
  unit with destructive changes named individually, asks once, then applies the
  plans it just showed. Declining leaves the target untouched. `--auto-approve`
  for non-interactive use; without a terminal and without that flag, apply
  refuses rather than assuming yes.
- Detection of cross-schema foreign key removals the apply order cannot
  satisfy, with the two-step remedy spelled out and nothing applied.
- Failure reporting for a partial apply: what landed, what broke, what was
  never attempted, and that the applied schemas are not rolled back.
- A required `pgpushy.toml`, with `-c`/`--config` to name one explicitly. Holds
  everything that decides what gets reconciled: the source-tree layout,
  `managed_schemas`, `exclude`, the pgschema binary, and named environments.
  `source_root` defaults to the file's own directory, so the source tree is
  anchored to the project rather than to the working directory. Unknown keys
  are rejected rather than ignored.
- Named environments and a required `--env` for `plan` and `apply`. `PG*` does
  not override a named environment's target; `PGPASSWORD` remains, since a
  secret should not live in the file.
- A prominent warning when the password actually in use came from
  `pgpushy.toml` — and silence when something else overrode it.
- `pgpushy init`, which writes a starter configuration and guesses the source
  root from where the `*.sql` files are. It declines to guess when the answer
  is ambiguous, and never overwrites an existing file.
- An optional external plan database per environment (`[env.<name>.plan_db]`)
  and a `lock_timeout`, both forwarded to pgschema. `--lock-timeout` on
  `apply` overrides the environment. Neither password is ever passed on a
  command line: the target's goes through `PGPASSWORD`, the plan database's
  through `PGPUSHY_PLAN_PASSWORD`.
- `--verbose`, printing the pgschema command line, the synthesized document's
  path, and the files discovery kept.
- Colour is suppressed when output is not a terminal, and by `--no-color` or
  `NO_COLOR` — including pgschema's own, which previously left escape
  sequences in captured output.
- A managed pgschema provider, and it is the **default**: pgpushy downloads a
  pinned pgschema over HTTPS, verifies it against a SHA-256 it ships, and
  caches it per version and platform. No install step. The cache is re-verified
  on every run rather than trusted for existing, and a mismatch is reported and
  replaced. `backend = "byo"` or naming a binary opts out — required on
  Windows and in air-gapped environments.
- pgpushy no longer revokes privileges it does not manage. pgschema reads a
  desired state that mentions no grants as a request to have none, and planned
  `REVOKE` for every grant on the target; pgpushy now tells it to leave
  privileges alone, the same way an unmentioned schema is left alone.
- pgschema runs in a directory pgpushy owns, so a stray `.pgschemaignore` in
  the operator's shell directory cannot silently change what gets reconciled.
- A source tree with no managed schemas now says so rather than succeeding
  silently, and a `source_root` pointing at a file explains itself instead of
  failing with a bare "Not a directory".
