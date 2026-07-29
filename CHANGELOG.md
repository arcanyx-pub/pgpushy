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
