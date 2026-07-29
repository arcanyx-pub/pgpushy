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
  dependency order. Read-only throughout: pgpushy's own inspection is a single
  `SELECT`, and pgschema builds its comparison model in a separate plan
  database.
- A bring-your-own pgschema provider: `--pgschema-path` or a `PATH` lookup,
  with the version floor enforced from the `Version:` line of `pgschema
  --help`. Below the floor is a hard error; an unreadable version warns.
- Connection resolution that pgschema does not repeat: pgpushy folds flags and
  `PG*` into one answer and passes every parameter explicitly, so the two
  cannot reach different databases.
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
