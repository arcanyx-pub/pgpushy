# Migrating from pgschema

pgpushy does not replace pgschema. It sits in front of it: it reads a directory
tree of SQL, synthesizes one correctly-ordered desired-state document per
schema, and runs `pgschema plan` or `pgschema apply` once per schema against
those documents. The diffing and the DDL are still pgschema's, so the plans you
review and the migrations you apply are the ones you already trust.

What changes is what you hand it, and what you stop maintaining by hand.

This guide is for someone already running pgschema. It starts with the question
that decides whether you can move at all.

## Can you move yet? Check this first

pgpushy manages a strict **subset** of what pgschema manages, and a source
file containing anything outside it is rejected rather than passed through.
That no longer decides adoption on its own, though: **partial adoption is a
supported path**. Objects of kinds pgpushy cannot describe — views,
materialized views, functions, procedures, aggregates, triggers — are left
exactly as they are in a managed schema (spec §8.4), so you can move your
tables to pgpushy today and keep the rest wherever it lives, as long as the
rest is not *described in the source tree*. The one hard blocker is
row-level security: a policy or an RLS-enabled table in a managed schema is
refused by name (spec §6.5), so such a schema must stay out of the managed
set until the policies move or go. One nuance: *tables* are a managed kind,
so a table whose shape pgpushy rejects in source — partitioned, `INHERITS`,
`OF type` — cannot be described, and is therefore reconciled **away** on the
target like any table the tree omits, as an approved destructive change; it
is not left alone. Keep such a table's schema out of the managed set too.

| Object kind | pgschema | pgpushy 0.1 |
|---|---|---|
| `CREATE TABLE`, table constraints | yes | yes |
| `CREATE INDEX` | yes | yes |
| foreign keys, including cross-schema | yes | yes |
| `CREATE TYPE`, `CREATE DOMAIN` | yes | yes |
| `CREATE SEQUENCE` (standalone) | yes | yes |
| `COMMENT ON` a table, column, index or sequence | yes | yes |
| `COMMENT ON` a type, domain, schema or constraint | yes | **rejected** |
| `CREATE VIEW`, `CREATE MATERIALIZED VIEW` | yes | **rejected** |
| `CREATE FUNCTION`, `CREATE PROCEDURE`, `CREATE AGGREGATE` | yes | **rejected** |
| `CREATE TRIGGER` | yes | **rejected** |
| `CREATE POLICY` | yes | **rejected** |
| `GRANT` / `REVOKE`, `ALTER DEFAULT PRIVILEGES` | yes | **rejected** |
| `CREATE EXTENSION` | no | **rejected** |

Rejection rather than pass-through is deliberate, and the reason is
qualification. pgpushy schema-qualifies every identifier it emits, because
pgschema attributes an *unqualified* object to whichever `--schema` the run
targets — so an unqualified name in a multi-schema document is silently
misattributed. pgpushy can qualify a statement it models structurally; it
cannot reach inside one it does not, such as a table reference in a view body.
Passing such a statement through would emit exactly the construct qualification
exists to prevent. `docs/spec.md` §4.3 has the full rule and §14 the order in
which the remaining kinds are expected to arrive.

### Finding out in one command

`pgpushy validate` runs the entire offline pipeline: discovery, parsing, the
statement allow-list, schema assignment, the validity checks and cross-schema
ordering. It connects to no database and needs no pgschema binary, so you can
point it at the SQL you already have before installing or configuring anything
else.

It needs a `pgpushy.toml`, and for this purpose two lines are enough:

```toml
source_root    = "db"
default_schema = "public"
```

Then:

```console
$ pgpushy validate
  config: pgpushy.toml
  /home/joe/shop/db (3 files)

error: unsupported statement: CREATE VIEW
  at reports.sql:1
  help: pgpushy 0.1 manages tables, indexes, foreign keys, types, domains, sequences and comments; see spec §4.3 for the full list and §14 for what may come later

error: unsupported statement: CREATE FUNCTION
  at reports.sql:4
  help: pgpushy 0.1 manages tables, indexes, foreign keys, types, domains, sequences and comments; see spec §4.3 for the full list and §14 for what may come later

error: unsupported statement: CREATE TRIGGER
  at reports.sql:7
  help: pgpushy 0.1 manages tables, indexes, foreign keys, types, domains, sequences and comments; see spec §4.3 for the full list and §14 for what may come later

error: unsupported statement: GRANT or REVOKE
  at reports.sql:10
  help: pgpushy 0.1 manages tables, indexes, foreign keys, types, domains, sequences and comments; see spec §4.3 for the full list and §14 for what may come later

4 problems found in the source tree
```

Every unsupported statement is named, with its file and line, in one pass —
pgpushy does not stop at the first. That output is the complete answer to
whether your schema fits; if it lists a view, a function, a trigger, a policy or
an extension, stop here.

Two things that will not appear in that output, because they are absences
rather than statements:

- **An existing `.pgschemaignore` stops taking effect.** pgschema loads it from
  its working directory, and pgpushy runs pgschema in a directory pgpushy owns,
  writing its own file there covering privileges and nothing else. If yours
  excludes specific tables, columns or indexes from reconciliation, 0.1 has no
  equivalent — and the objects it was shielding become desired-state-absent,
  which is to say scheduled for deletion. Use `exclude` in `pgpushy.toml` if
  the answer is "do not read these files"; there is no answer yet if it is "do
  not touch these target objects".
- **`PGSCHEMA_PLAN_*` is stripped from pgschema's environment.** If you point
  the comparison model at a separate server that way today, configure it as
  `[env.<name>.plan_db]` instead (below). Unlike `PGSERVICE`, which pgpushy
  refuses loudly, this family is removed silently, so a working setup would
  otherwise be dropped on the floor.

The rest of this guide assumes neither applies.

## What changes conceptually

**pgschema.** One `--file` per invocation, holding one schema's desired state,
executed top to bottom. Because it is executed, the file must be
dependency-ordered by hand — typically an ordered list of `\i` includes that
someone has to keep correct. `--schema` targets exactly one schema, so a
database with three managed schemas is three invocations in the right order.
And because pgschema resolves references from the file alone and never from the
target, a cross-schema foreign key means the referenced table must be *repeated*
in the referencing schema's file:

```console
$ pgschema apply --schema billing --file billing.sql …
Error: failed to apply desired state: failed to apply schema SQL to temporary schema pgschema_tmp_20260818_150756_f17e7e4a: ERROR: schema "shop" does not exist (SQLSTATE 3F000)
```

The fix is to hand-copy `shop.orders` into `billing.sql` — and then to keep the
copy in step with the original for as long as the project lives.

**pgpushy.** A directory tree, organized however suits you: one table per file,
one file per schema, or anything in between. Directory structure carries no
ordering and no meaning. Every foreign key is lifted out of its `CREATE TABLE`
into a trailing `ALTER TABLE … ADD CONSTRAINT`, the way `pg_dump` does, so no
ordering among tables can produce a dangling reference. One command reconciles
every managed schema, in cross-schema foreign-key dependency order.

So you stop maintaining:

- the include list, and the ordering discipline behind it;
- the hand-copied closure of other schemas' tables;
- the shell script that runs pgschema once per schema in the right order.

And you gain two things ordering cannot give you. **Mutually-referencing tables
become expressible** — two tables that reference each other have no valid inline
order at all, and lifting the foreign keys removes the question:

```sql
-- db/schema/library.sql — no ordering problem, in either direction
CREATE TABLE authors (
    id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    best_id bigint REFERENCES books (id)
);

CREATE TABLE books (
    id        bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id bigint NOT NULL REFERENCES authors (id)
);
```

**And the cross-schema closure is computed for you.** `--out <dir>` writes the
documents pgpushy hands pgschema, one `<schema>.sql` per managed schema, which
is where both effects become visible at once:

```sql
-- build/desired/billing.sql
-- Generated by pgpushy. Do not edit.
--
-- The desired state of schema billing, as pgschema diffs it against the
-- target. Objects from other schemas appear only so that references
-- into them resolve; pgschema does not compare them.

-- schemas
CREATE SCHEMA IF NOT EXISTS billing;
CREATE SCHEMA IF NOT EXISTS shop;

-- tables
CREATE TABLE billing.invoices (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, order_id bigint NOT NULL, total_cents bigint NOT NULL CHECK (total_cents >= 0));
CREATE TABLE shop.orders (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, customer_id bigint NOT NULL, placed_at timestamptz NOT NULL DEFAULT now());

-- indexes
CREATE INDEX orders_customer_idx ON shop.orders USING btree (customer_id);

-- foreign keys
ALTER TABLE billing.invoices ADD FOREIGN KEY (order_id) REFERENCES shop.orders (id);
```

`shop.orders` and its index are there so that `billing`'s foreign key resolves
when pgschema executes the document; pgschema diffs only `--schema billing`, so
they are scaffolding and are never compared. That is the copy you used to
maintain by hand.

## Getting a source tree

Two bootstrap routes, both with traps.

### `pgschema dump`

The natural first choice: it is per-schema, it emits table constraints
**inline** — primary keys, uniques, checks and same-schema foreign keys all live
in the `CREATE TABLE` — and inline is the shape pgpushy wants. A cross-schema
foreign key comes out as a standalone `ALTER TABLE … ADD CONSTRAINT`, which
pgpushy also accepts, because that is its own output form.

Two things to watch.

**It is lossy for a standalone sequence used as a column default.** Given a
sequence and a column defaulting to it:

```sql
CREATE SEQUENCE tmpseq.invoice_no START 1000;
CREATE TABLE tmpseq.inv (no bigint NOT NULL DEFAULT nextval('tmpseq.invoice_no'), body text);
```

`pgschema dump` renders the whole thing as a `BIGSERIAL` column and drops the
sequence — its start value with it:

```sql
CREATE TABLE IF NOT EXISTS inv (
    no BIGSERIAL,
    body text
);
```

Verified against pgschema 1.12.0 and 1.12.3. The dump will not tell you it
happened, so list the standalone sequences on the target first and check that
each one survived into the dump:

```sql
SELECT n.nspname AS schema, c.relname AS sequence
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind = 'S'
  AND NOT EXISTS (SELECT 1 FROM pg_depend d
                  WHERE d.objid = c.oid AND d.deptype IN ('a', 'i'))
ORDER BY 1, 2;
```

Excluding the `a` and `i` dependencies leaves out the sequences behind `serial`
and identity columns, which are not objects in their own right here. Note that
pgpushy rejects the original shape anyway (see below) — but rejecting it and
silently rewriting it are different outcomes, and only one of them is visible.

**It emits the target's `GRANT`s.** Anything granted on the schema comes out as
`GRANT SELECT ON TABLE customers TO reporting;`, which pgpushy rejects. Delete
those lines; pgpushy leaves privileges alone regardless (see *What pgpushy will
not touch*).

### `pg_dump --schema-only`

More work, but faithful, and it gets one thing exactly right that matters:
`pg_dump` sets `search_path` to `''` and **fully qualifies every name, including
inside string literals** — `nextval('tmpser.t_id_seq'::regclass)`, not
`nextval('t_id_seq'::regclass)`. That is precisely what pgpushy requires and
what nothing else gives you for free.

Everything else needs rewriting. A raw `pg_dump` of two schemas fed straight
into `pgpushy validate` produces a single parse failure, on the `\restrict`
meta-command pg_dump 18 writes at the top; delete those two lines and it
produces 27 errors, in these classes:

| In the dump | What to do |
|---|---|
| `\restrict` / `\unrestrict` (pg_dump 18) | Delete. These are psql meta-commands, not SQL; they fail the parse before anything else is reported. |
| `SET statement_timeout = 0;` and the rest of the preamble, `SELECT pg_catalog.set_config(…)` | Delete. |
| `ALTER TABLE … OWNER TO`, `ALTER SCHEMA … OWNER TO` | Delete. pgpushy does not manage ownership. |
| `GRANT USAGE ON SCHEMA …`, `GRANT SELECT ON TABLE …` | Delete. pgpushy leaves privileges alone, and rejects any attempt to express one. |
| `ALTER TABLE ONLY t ADD CONSTRAINT … PRIMARY KEY / UNIQUE` | Move inline into the `CREATE TABLE`. |
| `ALTER TABLE ONLY t ADD CONSTRAINT … FOREIGN KEY` | **Keep as is.** This is pgpushy's own canonical form. |
| `ALTER TABLE ONLY t ALTER COLUMN c ADD GENERATED ALWAYS AS IDENTITY (…)` | Fold back into the column: `c bigint GENERATED ALWAYS AS IDENTITY`. |
| `CREATE SEQUENCE t_id_seq` + `ALTER SEQUENCE … OWNED BY t.c` + `ALTER TABLE … ALTER COLUMN c SET DEFAULT nextval('s.t_id_seq'::regclass)` | Collapse all three back to `c serial` (or an identity column). This trio is how `pg_dump` spells a `serial`. |
| `CREATE SCHEMA s;` | Keep. The bare form is accepted. |
| `CHECK (…)` inside `CREATE TABLE` | Keep. Already inline. |

The rewrites are mechanical, and pgpushy's diagnostics do most of the work —
each one names the table to move the constraint into and the constraint name to
preserve:

```console
error: ALTER TABLE … ADD CONSTRAINT UNIQUE
  at dump.sql:137
  help: only a FOREIGN KEY may be added this way; write it inline in CREATE TABLE shop.customers instead, as `CONSTRAINT customers_email_key …` if you want to keep the name (spec §4.3)
```

Keeping the name matters: pgschema reads current state from the target catalog,
so a constraint that arrives under a different name is a drop and a recreate.

### The trap that is silent: one `default_schema` for the whole tree

`pgschema dump` output is **unqualified** — its objects carry no schema, because
a pgschema run supplies the schema through `--schema`. pgpushy has no such flag:
it assigns unqualified objects to the single `default_schema` from
`pgpushy.toml`, for the whole tree.

So dropping two per-schema dumps into one source tree assigns *both* schemas'
objects to the default schema, and nothing complains:

```console
$ pgpushy validate
  config: pgpushy.toml
  /home/joe/shop/db/schema (2 files)
  3 tables, 2 foreign keys, 2 indexes

  managed schemas: shop

  ok  no duplicate objects
  ok  all foreign key referents resolvable
  ok  no cross-schema foreign key cycles
  ok  no unsupported statements
```

`billing` is not in that list, and `billing.invoices` has quietly become
`shop.invoices`. Two defences:

- **Qualify everything except the one schema you nominate as `default_schema`.**
  A `pg_dump` source tree is qualified already and does not have this problem.
- **Always declare `managed_schemas`.** It is authoritative, so a schema you
  declare but the source tree never describes becomes visible rather than
  absent:

```console
  managed schemas (declared): billing, shop
  WARNING: no source file describes billing; applying would plan to drop everything the target holds there
```

That warning is the difference between noticing during `validate` and noticing
during `apply`.

## Source changes pgpushy requires

Beyond the object scope, seven rules.

**Constraints go inline, not through `ALTER`.** `CHECK`, `UNIQUE`,
`PRIMARY KEY` and `EXCLUDE` are written in the `CREATE TABLE`; an inline table
constraint can carry an explicit name, so nothing is lost but the spelling. A
source file says what exists, not the steps that reach it. The single exception
is `ALTER TABLE … ADD CONSTRAINT` for a **foreign key**, which is pgpushy's own
output form and what `pg_dump` emits — accepting it costs nothing, because
foreign keys are emitted last and cannot reintroduce an ordering problem.

**A name inside a string literal must say which schema it is in.**

```console
error: the table, index or sequence name 'things' does not say which schema it is in
  at t.sql:1
  help: pgschema strips a schema qualifier from an identifier but cannot reach inside a string literal, so pgpushy must know which schema this names; write it as 'schema.name' (spec §4.3)
```

pgschema strips the target schema's prefix from identifiers and cannot do the
same inside a literal, so a literal has to be spelled differently depending on
which schema's document it lands in — and pgpushy will not guess which schema
you meant. Write `'shop.things'::regclass`. `pg_dump` already does.

**No default calling `nextval`.** This is the rule most likely to bite an
existing schema, and the reason is not aesthetic:

```console
error: a default calling nextval on 'invoice_no'
  at t.sql:4
  help: pgschema applies this as SERIAL: it creates a sequence owned by the column instead of the one named here, and the plan never converges. Write the column as `serial` or `GENERATED BY DEFAULT AS IDENTITY`; a sequence nothing defaults to is managed normally (spec §4.3, §12.8)
```

pgschema models any `nextval` default as `SERIAL`. Applying `CREATE SEQUENCE s`
together with a column defaulting to it creates a *different*, column-owned
sequence, never creates `s`, reports success, and leaves every later plan showing
the same drop and add — silently, forever. On a **domain** default it fails
outright, because pgschema applies domains before sequences. Neither is
something pgpushy can order around: the apply order is pgschema's. Write
`serial` or `GENERATED … AS IDENTITY`. A sequence that nothing defaults to —
the common case, a sequence application code draws from — is managed normally.

**No `CREATE SEQUENCE … OWNED BY`.** Same root cause: a column-owned sequence is
part of the column as far as pgschema is concerned, so the shape does not
survive a dump-and-reapply. It also inverts the emission order, making a
sequence's creation depend on a table.

**No `LIKE`, `INHERITS`, `PARTITION OF`, `PARTITION BY`, or `OF <type>`.** Each
makes one table's *creation* depend on another. Lifting foreign keys resolves
foreign-key ordering and nothing else, so these are the one class of dependency
order-free authoring cannot absorb. `(LIKE t)` is the easiest to miss, because
it hides inside the column list rather than in a clause of its own.

**No `CREATE INDEX CONCURRENTLY`, and no unnamed `CREATE INDEX`.** How an index
is built is pgschema's decision, not the desired state's — it will use
`CONCURRENTLY` on its own where that is right:

```sql
-- Transaction Group #2
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS invoices_order_idx ON billing.invoices (order_id);
```

An index needs an explicit name because pgpushy needs a stable identity to
detect duplicates against and to attach comments to.

**Two unnamed foreign keys on one table over the same columns must be named.**
They compete for a single generated name, and Postgres gives the second a
numeric suffix in *creation* order — which pgpushy's emission order need not
match, so the two names can attach to opposite constraints and pgschema reads
that as two renames on every run. Naming either one resolves it. A
`pg_dump`-derived tree is safe, since pg_dump names every constraint; a tree
hand-edited from `pgschema dump` is not.

## Configuration

pgschema is configured by its flags. pgpushy requires a `pgpushy.toml`, and
everything that decides *what* gets reconciled lives in it rather than on the
command line — because pgpushy's blast radius is a whole database, and a flag
that silently narrows the desired state schedules everything outside it for
deletion.

`pgpushy init` scaffolds one. Here is a complete file for a two-schema project:

```toml
# Where the SQL lives. Relative paths resolve against this file's own
# directory, so the source tree is anchored to the project rather than to
# wherever you happen to be standing.
source_root = "db/schema"

# The schema an unqualified object belongs to. Objects in every other
# schema must be written schema-qualified.
default_schema = "shop"

# Authoritative when present: a schema the source tree uses but this list
# omits is an error, rather than a schema pgpushy quietly starts managing.
managed_schemas = ["shop", "billing"]

# Files under source_root that are not desired state.
exclude = ["seeds/**", "**/*.test.sql"]

[pgschema]
# pgpushy downloads a pinned, SHA-256-verified pgschema by default and
# caches it. To use the binary you already have instead:
# backend = "byo"
# path    = "/usr/local/bin/pgschema"

[env.local]
host = "localhost"
db   = "shop_dev"
user = "postgres"

[env.prod]
host         = "db.internal"
db           = "shop"
user         = "deploy"
sslmode      = "verify-full"
lock_timeout = "30s"
```

That is the file the worked example below runs against. `port` defaults to
5432, `host` to `localhost`, and `sslmode` to `prefer`.

If you point pgschema's comparison model at a separate server today, name it
here rather than through `PGSCHEMA_PLAN_*`, which pgpushy strips:

```toml
[env.prod.plan_db]
db   = "pgschema_plan"
user = "planner"
host = "plan.internal"
```

Its password comes from `PGPUSHY_PLAN_PASSWORD`. The database is scratch space
that pgschema writes to, so it must not be one that matters.

Notes for someone arriving from pgschema's flags:

- **`--env <name>` is required** on `plan` and `apply`, even with a single
  environment defined. Selecting the sole one automatically would make adding a
  second silently change what an existing command reconciles. `validate` takes
  no `--env`; it connects to nothing.
- **`PG*` does not override a named environment's target.** The point of
  `--env prod` is that it is unambiguous, and an ambient `PGHOST` that redirected
  it would defeat that exactly when it matters. `PGPASSWORD` is the exception,
  since a secret should not sit in a version-controlled file — and pgpushy warns
  loudly when the password it used came from the file instead.
- **All five libpq `sslmode` values work**, including `verify-ca` and
  `verify-full`, which pgpushy interprets itself.
- **pgschema is downloaded by default** — pinned to 1.12.3, verified against a
  SHA-256 pgpushy ships, and cached. Set `backend = "byo"` or a `path` to use
  your own; a BYO binary must be 1.12.0 or newer.
- **Unknown keys are rejected**, because a mistyped key is otherwise invisible
  from behavior. One TOML consequence worth knowing: the top-level keys must
  come *before* the first `[section]` header, or they are parsed as part of that
  section and rejected as unknown.

The only flags left are the ones that describe this run or this machine:
`--config`, `--env`, `--pgschema-path`, `--out`, `--auto-approve`,
`--lock-timeout`, `--verbose`, `--no-color`.

## What pgpushy will not touch

**Privileges are left exactly as found.** This is a real difference from running
pgschema yourself, and it is the safer default. pgschema reconciles privileges
by default, and reads a desired state that mentions no grants as a statement
that there should be none. Given the same document, raw pgschema plans this:

```console
$ pgschema plan --schema shop --file build/desired/shop.sql …
Plan: 1 to drop.

Summary by type:
  privileges: 1 to drop

Privileges:
  - reporting

DDL to be executed:
--------------------------------------------------

REVOKE SELECT ON TABLE customers FROM reporting;
```

pgpushy plans nothing, because it writes a `.pgschemaignore` suppressing
`[privileges]` and `[default_privileges]` for every run. Since pgpushy 0.1 has
no way to *express* a grant, the absence of grants from the source tree carries
no intent, and pgpushy will not let pgschema read intent into it. Permissions on
a pgpushy-managed database stay whatever something else made them.

**Every managed schema must already exist on the target.** pgpushy issues no DDL
of its own — every change flows through pgschema, and pgschema cannot reconcile
a schema that is not there. Creating a schema is therefore the operator's one
manual step. Declaring a schema in `managed_schemas` before it exists on the
target gives this, with every missing schema named at once rather than only the
first:

```console
error: 1 managed schema is missing from the target
  reporting

  help: pgpushy does not create schemas — every change flows through pgschema,
        which cannot reconcile a schema that does not exist. Create them first:
          CREATE SCHEMA reporting;
```

pgpushy never drops a schema either, and schemas outside the managed set are
neither planned nor modified.

**Kinds pgpushy cannot describe are left exactly as found.** A managed
schema's views, materialized views, functions, procedures, aggregates and
triggers are suppressed in that same `.pgschemaignore`, and enforced the same
way privileges are not: any plan step that names a kind outside pgpushy's
model is refused outright (spec §8.4). This is what makes partial adoption
work — pgpushy reconciles the tables it is given and does not treat the
absence of your views from the source tree as a request to drop them.

**Policies and row-level security are the exception**, because pgschema's
ignore file has no section for them: they can be neither described nor left
alone. A managed schema holding either is refused, with every policy and
RLS-enabled table named (spec §6.5); `plan` still shows the plans and exits
non-zero, and `apply` refuses before touching anything.

## Running it the first time

`validate` → `plan` → read it → `apply`.

**`validate` first**, because it is free and offline. It reports the counts, the
managed set and the apply order, so it also confirms that your schema assignment
came out the way you meant:

```console
$ pgpushy validate
  config: pgpushy.toml
  /home/joe/shop/db/schema (3 files)
  3 tables, 2 foreign keys, 1 index

  managed schemas (declared): billing, shop

  ok  no duplicate objects
  ok  all foreign key referents resolvable
  ok  no cross-schema foreign key cycles
  ok  no unsupported statements

  schema apply order: shop, billing
```

**Then `plan` against the database you already have.** This is the real test of
the migration, and the reason to do it before changing anything: it compares
your rewritten source tree against the database pgschema has been maintaining,
and an **empty plan means the tree already describes exactly what is there**.

```console
$ pgpushy plan --env local
  config: pgpushy.toml
  /home/joe/shop/db/schema (3 files)
  3 tables, 2 foreign keys, 1 index

  managed schemas (declared): billing, shop

  pgschema 1.12.3 (/home/joe/.cache/pgpushy/pgschema/1.12.3/linux-amd64/pgschema)
  env local: shop_dev on 172.17.0.2:5432 (cluster 7668031834611146801)

── shop ──
No changes detected.

── billing ──
No changes detected.
```

(The address and cluster id are what the *server* reports about itself, so the
target is identifiable in the record of a change even when several routes reach
the same cluster.)

Anything other than "No changes detected" here is a transcription difference
between the old input and the new one, not a change you asked for. Read it as a
diff of your rewrite, and iterate on the source tree until both plans are empty.
Do this against a copy of production, or a development database restored from
it; the objects that differ are exactly the ones the rewrite got wrong.

**Then make a real change and apply it.** Adding a column and a unique index to
`billing.invoices`:

```console
$ pgpushy apply --env local
  config: pgpushy.toml
  /home/joe/shop/db/schema (3 files)
  3 tables, 2 foreign keys, 2 indexes

  managed schemas (declared): billing, shop

  pgschema 1.12.3 (/home/joe/.cache/pgpushy/pgschema/1.12.3/linux-amd64/pgschema)
  env local: shop_dev on 172.17.0.2:5432 (cluster 7668031834611146801)

── shop ──
No changes detected.

── billing ──
Plan: 1 to modify.

Summary by type:
  tables: 1 to modify

Tables:
  ~ invoices
    + issued_at (column)
    + invoices_order_idx (index)

DDL to be executed:
--------------------------------------------------

-- Transaction Group #1
ALTER TABLE invoices ADD COLUMN issued_at timestamptz DEFAULT now() NOT NULL;

-- Transaction Group #2
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS invoices_order_idx ON billing.invoices (order_id);

-- Transaction Group #3
… pgschema's progress query for the concurrent index, elided …

  Plan for 2 managed schemas, in apply order:

    shop      no changes
    billing   3 changes

  3 changes across 1 schema, 0 destructive.

  apply is not atomic across schemas: a failure partway leaves
  earlier schemas applied and the rest unapplied.

Apply? [y/N] y

── shop ──
No changes; nothing to apply.

── billing ──
… pgschema reprints the plan it is about to apply, elided …

Applying changes...
Set search_path to: billing, public

Executing group 1/3...
  Executing 1 statements in implicit transaction

Executing group 2/3...
  Executing 1 statements in implicit transaction

Executing group 3/3...
  Waiting: Creating index invoices_order_idx
    Completed immediately (< 1s)
Changes applied successfully!

  Applied 1 schema: billing
```

Every schema is planned first and shown as one reviewable unit; approval is
asked once, for the database, before anything is touched; declining leaves the
target untouched. On approval pgpushy applies **the plans it just showed**
rather than recomputing them, so what lands is what was reviewed — and pgschema
refuses a plan whose target has drifted since, so that window fails loudly.
`--auto-approve` skips the prompt for CI; without a terminal and without it,
pgpushy fails rather than proceeding unapproved.

Running it again reports no changes.

## What you give up

Honestly, and in rough order of how likely each is to matter:

- **Object scope.** No views, materialized views, functions, procedures,
  aggregates, triggers, policies, `GRANT`/`REVOKE` or `ALTER DEFAULT
  PRIVILEGES`. Parity with pgschema's own statement set is the main direction of
  travel, not a nice-to-have (`docs/spec.md` §14), but 0.1 is not there.
- **Cross-schema references other than foreign keys.** A foreign key is the only
  reference allowed to cross a schema boundary. A column typed by another
  schema's domain is rejected, naming both:

  ```console
  error: table billing.invoices uses the domain shop.money_cents, which is in another schema
    at a.sql:8
    at a.sql:1
    help: a foreign key is the only reference pgpushy 0.1 lets cross a schema boundary (spec §12.6); define the domain in billing, or the table in shop
  ```

  This is what keeps each document's closure shallow and its rule uniform, and
  it is the one place 0.1 trades a real capability for a smaller design.
- **Cross-schema foreign-key cycles.** Two schemas whose tables reference each
  other have no valid apply order, because pgschema applies each schema in its
  own transaction and cannot defer a foreign key across schemas. pgpushy detects
  this offline rather than failing halfway through an apply. *Same-schema* cycles
  are fully supported — that is what lifting the foreign keys buys, and no
  hand-ordered file of inline references can express one.
- **Removing a cross-schema foreign key and the column it points at in one
  change.** Creation order and removal order are reverses of each other, so this
  needs two applies: drop the foreign key, then drop what it pointed at. pgpushy
  refuses before touching anything and says so. Dropping the referenced *table*
  is unaffected — pgschema drops tables with `CASCADE`.
- **A sequence as a column default**, per the `nextval` rule above. `serial` and
  `GENERATED … AS IDENTITY` are the supported spellings; a shared sequence used
  as a default across several tables has no equivalent in 0.1.
- **All-or-nothing multi-schema apply.** pgschema applies each schema in its own
  transaction, and pgpushy does not wrap the database in one. A failure partway
  leaves earlier schemas applied; pgpushy reports exactly what landed, what
  failed and what was not attempted. Treat a partial failure as fix-forward.

## Further reading

- [`docs/spec.md`](spec.md) — normative, and the record of every design decision
  and its reasoning.
- [`docs/impl-plan.md`](impl-plan.md) §1 — everything verified about pgschema's
  behavior during design, including the measurements this guide's traps come
  from.
