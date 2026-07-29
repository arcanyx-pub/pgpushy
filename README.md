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

`apply` plans every managed schema first and shows the lot, then asks once —
so declining leaves the target untouched. It then applies the plans it just
showed rather than recomputing them. Apply is not atomic across schemas; a
failure partway reports exactly what landed and what did not.

## Configuration

Everything works with flags and `PG*` environment variables alone. A
`pgpushy.toml` is optional convenience:

```toml
source_root    = "db/schema"      # relative to this file
default_schema = "app"            # schema for unqualified objects
exclude        = ["seeds/**", "**/*.test.sql"]

# Optional, and authoritative when present: a schema the source tree uses but
# this list omits becomes an error rather than being quietly reconciled.
managed_schemas = ["app", "billing"]

[pgschema]
path = "/usr/local/bin/pgschema"  # otherwise looked up on PATH

[connection]
host    = "localhost"
port    = 5432
db      = "myapp"
user    = "joe"
sslmode = "prefer"
# password = "..."   # permitted, but pgpushy warns loudly when it uses it
```

Precedence is **CLI flag → `PG*` environment → `pgpushy.toml` → default**. Note
that the environment beats the file, matching `psql`: an ambient `PGHOST`
outranks the project's configuration.

The file is read from the working directory and is *not* searched for in parent
directories; `--config <path>` names one explicitly, and paths inside it
resolve relative to the file itself. List settings given as flags **replace**
the file's rather than adding to them.

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
