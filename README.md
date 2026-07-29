# pgpushy

Postgres declarative schema management at the database level, powered by
[pgschema](https://github.com/pgplex/pgschema).

> **Status: pre-release.** Nothing is published yet. The design is settled —
> see [`docs/spec.md`](docs/spec.md) — and implementation is in progress.

pgschema is a Terraform-style declarative schema tool for Postgres: describe
the schema you want, it diffs that against a live database and applies the
difference. pgpushy sits in front of it and removes two frictions.

**Order-free authoring.** pgschema executes your desired-state SQL to build its
comparison model, so the input has to be dependency-ordered by hand — an
ordered list of `\i` includes — and mutually-referencing tables cannot be
expressed at all. pgpushy walks a directory tree, organized however you like,
and synthesizes a correctly-ordered desired state for you. It does this by
lifting every foreign key out of its table definition into a trailing
`ALTER TABLE … ADD CONSTRAINT`, the way `pg_dump` does, so table order stops
mattering and reference cycles become expressible.

**Database-level management.** A pgschema invocation targets exactly one
schema. pgpushy reconciles every managed schema in the database in one command,
ordering them by their cross-schema foreign keys.

pgpushy does not compute diffs or generate migrations — pgschema does. pgpushy
is a preprocessor and an orchestrator.

## Commands

| Command | What it does |
|---|---|
| `pgpushy validate` | Check the source tree. No database connection at all. |
| `pgpushy plan` | Show what would change, per schema. Read-only. |
| `pgpushy apply` | Reconcile the database, after one approval. |

## Building

pgpushy parses SQL with [libpg_query](https://github.com/pganalyze/libpg_query)
— the real Postgres parser — via the `pg_query` crate, which builds C sources
through `bindgen`. That needs **libclang** at build time:

```sh
# Debian/Ubuntu
sudo apt-get install libclang-dev

# macOS: included with the Xcode command line tools
```

Then:

```sh
just ci      # fmt-check, clippy, test, doc
just test    # tests; integration tests skip without a database
```

Integration tests need a target database (`PGPUSHY_TEST_PG_URL`) and a
pgschema binary (`PGPUSHY_TEST_PGSCHEMA`); they skip when either is absent
rather than failing.

## Documentation

- [`docs/spec.md`](docs/spec.md) — the specification. Normative, and the place
  where every design decision and its reasoning is recorded.
- [`docs/impl-plan.md`](docs/impl-plan.md) — how it is being built, including
  everything verified about pgschema's behavior during design.

## License

Apache-2.0. See [LICENSE](LICENSE).
