# pgpushy

[![CI](https://github.com/arcanyx-pub/pgpushy/actions/workflows/ci.yml/badge.svg)](https://github.com/arcanyx-pub/pgpushy/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/pgpushy.svg)](https://crates.io/crates/pgpushy)
[![docs.rs](https://img.shields.io/docsrs/pgpushy-core)](https://docs.rs/pgpushy-core)

Declarative Postgres schema management at the database level, powered by
[pgschema](https://github.com/pgplex/pgschema).

<p align="center">
  <!-- Absolute URL: crates.io resolves relative paths against the crate's
       workspace subdirectory (path_in_vcs), not the repo root, and 404s. Both
       crates set readme = "../README.md", so this file is rendered from two
       subdirectories and a relative path would be wrong from both. -->
  <img src="https://raw.githubusercontent.com/arcanyx-pub/pgpushy/main/assets/pgpushy.jpg" alt="pgpushy, a baby elephant mascot, cheerfully pushing a boulder marked pg up a grassy hill">
</p>

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

## What pgpushy manages

The scope is narrow, and a source tree containing anything outside it is
**rejected**, not partially managed.

**Supported.** Tables, indexes, inline table constraints (`CHECK`, `UNIQUE`,
`PRIMARY KEY`, `EXCLUDE`), foreign keys, user-defined types, domains,
standalone sequences, and `COMMENT ON` a schema, table, column, index, table
constraint or sequence. A comment on a *type* or a *domain* is rejected, not
because pgpushy cannot express it but because pgschema drops it without
applying it and without saying so.

**Not supported.** Views, materialized views, functions, procedures, triggers,
policies, `GRANT`/`REVOKE`, `CREATE EXTENSION`, every `DROP`, and all DML.
Widening this towards parity with the statement set pgschema itself supports is
the main direction of travel; see [`docs/spec.md`](docs/spec.md) §14.

Rejection rather than pass-through is deliberate. pgpushy schema-qualifies
every identifier it emits, because an unqualified one is misattributed to
whichever schema's run reads it — and it cannot qualify the interior of a
statement it does not model, such as a table reference inside a view body. A
statement passed through would emit exactly the construct qualification exists
to prevent.

**`ALTER` is rejected too**, with one exception. A source file says what
exists, not the steps that reach it, so a `CHECK`, `UNIQUE`, `PRIMARY KEY` or
`EXCLUDE` constraint is written inline in its `CREATE TABLE`, where it can
still carry an explicit name — nothing is lost but the spelling. The exception
is `ALTER TABLE … ADD CONSTRAINT` for a **foreign key**, which is pgpushy's own
output form and what `pg_dump` emits, so an imported tree needs no rewriting.

A handful of further constructs are rejected inside otherwise-supported
statements, each because it would fail silently or never converge:

| Rejected | Why |
|---|---|
| `CREATE TABLE … (LIKE t)`, `INHERITS`, `PARTITION OF`, `PARTITION BY`, `OF <type>` | Each makes a table's *creation* depend on another table; lifting foreign keys resolves foreign-key ordering only. |
| `CREATE SEQUENCE … OWNED BY`, and any default calling `nextval` | pgschema models a column-owned sequence as `SERIAL`, so none of these round-trips. Use `serial` or `GENERATED … AS IDENTITY`. A sequence nothing defaults to is managed normally. |
| `CREATE INDEX CONCURRENTLY` | Cannot run in a transaction block, and describes a strategy rather than a state. How an index is built is pgschema's decision. |
| `CREATE INDEX` without a name | pgpushy needs a stable name to detect duplicates against and to attach comments to. |
| A bare object name in a string literal, `'x'::regclass` | pgpushy does not infer a schema inside a literal. Write `'public.x'::regclass`; `pg_dump` already does. |
| Two unnamed foreign keys on one table over identical columns | They compete for one generated name whose suffix follows creation order, so pgschema would plan two renames on every run. Name either one. |

Cross-schema **foreign keys** are supported, and are the only reference allowed
to cross a schema boundary in 0.1. A column typed by another schema's domain or
user-defined type is rejected, naming the referring table and the referenced
object.

- **Every managed schema must already exist on the target.** pgpushy issues no
  DDL of its own — every change flows through pgschema, and pgschema cannot
  reconcile a schema that is not there. A missing one fails early, naming all
  of them at once. A bare `CREATE SCHEMA s` in the source tree enlists `s` into
  the managed set; it does not create it.
- **Privileges are never touched.** pgschema would otherwise read a desired
  state mentioning no grants as a request to have none, and revoke every grant
  on the target. pgpushy has no way to express a grant, so it tells pgschema to
  leave privileges alone.

## Seeding baseline rows

Some rows are as load-bearing as the shape that holds them — lookup tables,
reference data, the 1024 machine-ID rows a
[`snowdrop-id-postgres`](https://crates.io/crates/snowdrop-id-postgres) lease
table needs before any worker can claim one. Provisioning them usually falls
either to a side script or to the application at boot, which needs rights a
production role should not hold. pgpushy owns the step instead: a `seed_root`
in `pgpushy.toml` names a directory of seed files, and `apply` executes them
itself after every schema has applied, one transaction per file, under the
deploy role.

Seeds are **not** desired state — they never reach pgschema — and they are
idempotent by construction: every statement must be `INSERT … ON CONFLICT`
(`DO UPDATE` needs a `WHERE … IS DISTINCT FROM` guard), with an explicit
column list, a schema-qualified table the source tree defines, and a source
that reads nothing — `VALUES`, or a `SELECT` over set-returning built-ins like
`generate_series`. `validate` checks the columns and the conflict target
against your tables, offline. Then `apply` proves convergence on every run:
inside each file's transaction the statements run **twice**, and the second
pass must affect zero rows, or the whole file rolls back and nothing from it
lands. Rows are never deleted: what no seed file describes, pgpushy does not
touch.

When the SQL is owned by a dependency rather than written by hand,
`pgpushy generate` vendors it: a `[[generate]]` entry names an argv command
whose output lands in the tree under a generated-source marker, and
`pgpushy generate --check` fails in CI the moment a dependency bump changes
the emission — so the change ships as a reviewed diff. `validate`, `plan` and
`apply` never execute a configured command; they read only files.

## A worked example

```console
$ pgpushy init
  wrote pgpushy.toml
  source_root: db/schema (found *.sql there)

  Next: set db and user in [env.local], then
    pgpushy validate          # checks your SQL, connects to nothing
    pgpushy plan --env local  # shows what would change
```

Fill in the target it scaffolded:

```toml
[env.local]
db   = "myapp_dev"
user = "joe"
```

Now write your schema, in whatever files and folders suit you. Nothing needs
ordering, and nothing needs qualifying:

```sql
-- db/schema/orders.sql   (references a table defined in another file)
CREATE TABLE orders (
    id          int PRIMARY KEY,
    customer_id int NOT NULL REFERENCES customers(id) ON DELETE CASCADE
);
CREATE INDEX orders_customer_idx ON orders (customer_id);

-- db/schema/customers.sql
CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);
```

`validate` runs the whole offline pipeline. It connects to nothing and needs no
pgschema binary, so it belongs in a pre-commit hook and in CI:

```console
$ pgpushy validate
  config: pgpushy.toml
  /home/joe/myapp/db/schema (2 files)
  2 tables, 1 foreign key, 1 index

  managed schemas: public

  ok  no duplicate objects
  ok  all foreign key referents resolvable
  ok  no cross-schema foreign key cycles
  ok  no unsupported statements
```

`--out <dir>` keeps the desired state it synthesized — one `<schema>.sql` per
managed schema — which is where the foreign-key lift becomes visible:

```console
$ pgpushy validate --out build/desired
  …
  wrote 1 document into build/desired
    public.sql
```

```sql
-- build/desired/public.sql, abridged
-- tables
CREATE TABLE public.customers (id int PRIMARY KEY, name text NOT NULL);
CREATE TABLE public.orders (id int PRIMARY KEY, customer_id int NOT NULL);

-- indexes
CREATE INDEX orders_customer_idx ON public.orders USING btree (customer_id);

-- foreign keys
ALTER TABLE public.orders ADD FOREIGN KEY (customer_id) REFERENCES public.customers (id) ON DELETE CASCADE;
```

`plan` is the first command that needs a database — and the first that needs
pgschema, which it fetches and caches itself:

```console
$ pgpushy plan --env local
  config: pgpushy.toml
  /home/joe/myapp/db/schema (2 files)
  2 tables, 1 foreign key, 1 index

  managed schemas: public
  downloading pgschema 1.12.3 (linux-amd64)...
  cached at /home/joe/.cache/pgpushy/pgschema/1.12.3/linux-amd64/pgschema

  pgschema 1.12.3 (/home/joe/.cache/pgpushy/pgschema/1.12.3/linux-amd64/pgschema)
  env local: myapp_dev on 127.0.0.1:5432 (cluster 7668031834611146801)

── public ──
Plan: 2 to add.

Summary by type:
  tables: 2 to add

Tables:
  + customers
  + orders
    + orders_customer_idx (index)

DDL to be executed:
--------------------------------------------------

CREATE TABLE IF NOT EXISTS customers (
    id integer,
    name text NOT NULL,
    CONSTRAINT customers_pkey PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS orders (
    id integer,
    customer_id integer NOT NULL,
    CONSTRAINT orders_pkey PRIMARY KEY (id),
    CONSTRAINT orders_customer_id_fkey FOREIGN KEY (customer_id) REFERENCES customers (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS orders_customer_idx ON orders (customer_id);
```

`apply` plans every managed schema, shows the lot, and asks once:

```console
$ pgpushy apply --env local
  config: pgpushy.toml
  /home/joe/myapp/db/schema (2 files)
  2 tables, 1 foreign key, 1 index

  managed schemas: public

  pgschema 1.12.3 (/home/joe/.cache/pgpushy/pgschema/1.12.3/linux-amd64/pgschema)
  env local: myapp_dev on 127.0.0.1:5432 (cluster 7668031834611146801)

── public ──
Plan: 2 to add.
… pgschema's plan for this schema, as above …

  Plan for 1 managed schema, in apply order:

    public    3 changes

  3 changes across 1 schema, 0 destructive.

  apply is not atomic across schemas: a failure partway leaves
  earlier schemas applied and the rest unapplied.

Apply? [y/N] y

── public ──
… pgschema applying that plan …

  Applied 1 schema: public
```

Ask again afterwards and there is nothing left to do — the target already
matches the source tree:

```console
$ pgpushy plan --env local
  …
── public ──
No changes detected.
```

## Commands

| Command | What it does |
|---|---|
| `pgpushy init` | Write a starter `pgpushy.toml`. |
| `pgpushy validate` | Check the source tree. No database, no pgschema binary. |
| `pgpushy plan` | Show what would change, per schema. Read-only. |
| `pgpushy apply` | Reconcile the database, after one approval; then apply the seeds. |
| `pgpushy generate` | Vendor each `[[generate]]` command's output into the tree; `--check` fails on stale output. |

`apply` plans every managed schema first and shows the lot, then asks once —
so declining leaves the target untouched. It then applies the plans it just
showed rather than recomputing them, which makes the change that lands the same
one that was reviewed. Destructive changes are named individually rather than
counted. `--auto-approve` skips the prompt; without a terminal and without that
flag, `apply` refuses rather than assuming yes.

Apply is not atomic across schemas. A failure partway stops the run and reports
what landed, what broke, what was never attempted, and that the applied schemas
are not rolled back.

Every diagnostic names every instance rather than the first, with file and
line:

```console
$ pgpushy validate
  config: pgpushy.toml
  /home/joe/myapp/db (2 files)

error: ALTER TABLE … ADD CONSTRAINT CHECK
  at orders.sql:9
  help: only a FOREIGN KEY may be added this way; write it inline in CREATE TABLE public.orders instead, as `CONSTRAINT orders_total_positive …` if you want to keep the name (spec §4.3)

error: unsupported statement: CREATE VIEW
  at reports.sql:2
  help: pgpushy 0.1 manages tables, indexes, foreign keys, types, domains, sequences and comments; see spec §4.3 for the full list and §14 for what may come later

2 problems found in the source tree
```

## Configuration

pgpushy requires a `pgpushy.toml`. It reconciles a whole database, so it will
not guess which files are desired state or which server to reconcile them
against — running from the wrong directory would otherwise treat part of a
source tree as all of it, and plan to drop everything else.

```toml
# source_root defaults to this file's directory
source_root    = "db/schema"
default_schema = "app"            # schema for unqualified objects
seed_root      = "db/seeds"       # optional: idempotent baseline rows
exclude        = ["seeds/**", "**/*.test.sql"]

# Optional, and authoritative when present: a schema the source tree uses but
# this list omits becomes an error rather than being quietly reconciled.
managed_schemas = ["app", "billing"]

[pgschema]
# pgpushy downloads and caches a pinned pgschema by default, verified against
# a checksum it ships. Point it at your own binary instead if you prefer:
# backend = "byo"
# path    = "/usr/local/bin/pgschema"
# version = "1.12.3"               # managed backend only

[env.local]
db   = "myapp_dev"
user = "joe"

[env.prod]
host    = "db.internal"
db      = "myapp"
user    = "deploy"
sslmode = "verify-full"

# How long Postgres waits for a lock before giving up. Worth setting on a busy
# production database, where an unbounded wait blocks everything behind it.
lock_timeout = "30s"

# Optional. pgschema builds its comparison model in an ephemeral embedded
# Postgres by default; name an external one if spawning a process is not
# possible where pgpushy runs. NOTE: pgschema WRITES here — it is scratch
# space, not a reference, so do not point it at anything that matters.
[env.prod.plan_db]
host = "plan.internal"
db   = "pgschema_scratch"
user = "planner"
```

Unknown keys are rejected rather than ignored: a mistyped key is invisible from
behavior, so silence is the one response that cannot be recovered from.

A `managed_schemas` entry the source tree never mentions **is** managed, with
an empty desired state — the only way to say "this schema should be empty", and
destructive by design. pgpushy warns about it in `validate` and again in the
approval summary.

`plan` and `apply` take `--env <name>`, and it is **required** — even when only
one environment is defined, because selecting the sole one automatically would
make adding a second silently change what an existing command reconciles.
`validate` takes no `--env`; it connects to nothing.

Everything that decides *what* gets reconciled lives in the file, not in flags.
The source root, default schema, managed-schema declaration and exclusions each
describe the project, and a flag that silently narrowed the desired state would
be the same hazard as guessing at a missing file. Paths inside the file resolve
against the file's own directory, so the source tree is anchored to the project
rather than to wherever you happen to be standing.

The file is read from the working directory and is **not** searched for in
parent directories; `-c`/`--config <path>` names one anywhere.

`PG*` deliberately does not override a named environment's target: the point of
`--env prod` is that it is unambiguous. `PGPASSWORD` is the exception, since a
secret should not live in a version-controlled file — and pgpushy warns loudly
if the password it ends up using came from the file. A plan database takes its
password from `PGPUSHY_PLAN_PASSWORD`, a separate variable because it is a
separate server. Neither password is ever passed on a command line.

`--lock-timeout` may also be given on `apply`, overriding the environment. It
is the one setting that works both ways, because it cannot change *what* gets
reconciled — only whether the apply gives up waiting.

## Connections and TLS

pgpushy resolves every connection parameter itself and passes all of them to
pgschema explicitly, so pgschema resolves nothing and the two cannot end up at
different databases. `PG*` and pgschema's `PGSCHEMA_PLAN_*` family are stripped
from the subprocess environment for the same reason; `PGSERVICE` and
`PGSERVICEFILE` are refused outright rather than silently dropped, since
pgpushy cannot interpret them and dropping one would mean connecting somewhere
you did not name. `plan` and `apply` report the database, server and cluster
system identifier they reached.

All five libpq `sslmode` values are honored, and pgpushy interprets them
itself rather than delegating to its Postgres driver, which models only the
first three:

| `sslmode` | Encryption | Certificate chain | Hostname |
|---|---|---|---|
| `disable` | no | — | — |
| `prefer` (default) | opportunistic | not verified | not verified |
| `require` | yes | not verified | not verified |
| `verify-ca` | yes | verified | not verified |
| `verify-full` | yes | verified | verified |

The verifying modes check against the platform trust store — the same place
pgschema's Go TLS stack looks — because a Postgres target usually sits behind a
private CA that no bundled public root list contains. `SSL_CERT_FILE` and
`SSL_CERT_DIR` are honored and resolved to absolute paths before pgschema sees
them, so a relative one cannot name different files for the two connections.

## Other flags

`--verbose` prints the pgschema command line as it will actually run, where the
synthesized desired state was written, and every file discovery kept; it is the
first thing to reach for when a run does something unexpected. Color is
suppressed automatically when output is not a terminal, and `--no-color` or
`NO_COLOR` forces it off.

## Where pgschema comes from

By default pgpushy **downloads pgschema for you** and caches it under
`$XDG_CACHE_HOME/pgpushy` — there is no install step. The pinned version is
**1.12.3**. The download is over HTTPS and verified against a SHA-256 that
pgpushy ships for each version it pins, because pgschema publishes no checksums
of its own. The cache is re-checked against that hash on every run rather than
trusted for existing; a mismatch is reported and re-fetched rather than
executed. This backend covers the platforms pgschema publishes binaries for —
Linux and macOS, amd64 and arm64.

To use your own binary instead — required on Windows, where pgschema publishes
none, and in air-gapped environments:

```toml
[pgschema]
backend = "byo"
path    = "/usr/local/bin/pgschema"   # or omit, to look on PATH
```

`--pgschema-path` does the same for a single run. A bring-your-own binary must
be **1.12.0 or newer**: that is the oldest version pgpushy's CI tests, and the
floor is not overridable. The two numbers are deliberately different — an
operator who brings their own binary should not be made to upgrade because
pgpushy prefers a newer release, while one who lets pgpushy fetch a binary
should get the newest release that has actually been tested.

## Installing

```console
$ cargo install pgpushy
```

There is no separate step for pgschema — pgpushy downloads and verifies its own
copy the first time it needs one. To build from a checkout instead, `just
install-cli`.

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
just ci      # fmt-check, clippy, test, doc, msrv
just test    # tests; integration tests skip without a database
```

`just ci` ends with the `msrv` step, which checks that the workspace still
builds on the minimum supported Rust version and installs that toolchain
through `rustup` when it is not already there.

Integration tests need a target database (`PGPUSHY_TEST_PG_URL`) and a
pgschema binary (`PGPUSHY_TEST_PGSCHEMA`); they skip when either is absent
rather than failing.

## Documentation

- [`docs/spec.md`](docs/spec.md) — the specification. Normative, and the place
  where every design decision and its reasoning is recorded.
- [`docs/migrating-from-pgschema.md`](docs/migrating-from-pgschema.md) — moving
  an existing pgschema project across: what fits and what must be rewritten.
- [`docs/impl-plan.md`](docs/impl-plan.md) — the build plan, and the record of
  everything verified about pgschema's own behavior along the way.
- [`docs/RELEASING.md`](docs/RELEASING.md) — the release flow, and what must
  agree with what before a release.

## License

Apache-2.0. See [LICENSE](LICENSE).
