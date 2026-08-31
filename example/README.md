# The example project

A small shop, managed by pgpushy end to end. Every capability the tool has is
in this directory somewhere; this file says where to look, then walks through
running it.

## What is being shown, and where

| Capability | Where |
|---|---|
| Order-free authoring — file order never matters | `db/schema/app/orders.sql` references tables defined elsewhere; nothing is include-ordered |
| A same-schema reference cycle, inexpressible with ordered includes | `db/schema/app/teams.sql` — `teams` and `employees` reference each other |
| Whole-database management: three schemas, applied in dependency order | `app`, `billing`, `snowdrop`; the cross-schema foreign key in `db/schema/billing/invoices.sql` puts `billing` after `app` |
| The one `ALTER` pgpushy accepts — `ADD CONSTRAINT … FOREIGN KEY`, as `pg_dump` writes it | `db/schema/billing/invoices.sql` |
| Domains, standalone sequences, indexes, comments | `db/schema/app/types.sql`, `db/schema/billing/invoices.sql`, `db/schema/app/orders.sql` |
| A declared, authoritative `managed_schemas` list | `pgpushy.toml` |
| Exclusions keeping work-in-progress out of the desired state | `db/schema/app/wishlist.draft.sql` holds a view — rejected if discovered — parked by the `**/*.draft.sql` pattern |
| Seeds: idempotent baseline rows, applied by pgpushy after the schemas | `db/seeds/order_statuses.sql` — a guarded `DO UPDATE`, so the listed labels are authoritative and drift is corrected |
| Generated sources: a dependency's SQL, vendored and kept current | `db/schema/snowdrop/leases.sql` and `db/seeds/snowdrop_machine_ids.sql`, written by `pgpushy generate` from `snowdrop-id-postgres` via `xtask/` |

The snowdrop pieces are the motivating case for seeds and generated sources:
[`snowdrop-id-postgres`](https://crates.io/crates/snowdrop-id-postgres) leases
10-bit machine IDs for Snowdrop ID generators out of a 1024-row table. The
crate publishes its DDL and seed DML as library functions; `xtask/` prints
them, `[[generate]]` vendors them into the tree, and `Cargo.lock` — this
directory's own, not pgpushy's — pins the version they come from. The
application then runs with `auto_provision(false)` and needs nothing but DML
on the lease table: provisioning belongs to the deploy step, which is
pgpushy's step.

One honest footnote from measuring this project against a live database:
pgschema keeps storage parameters outside its model, so the
`WITH (fillfactor = 70)` in the vendored DDL is neither applied nor ever
diffed (impl-plan §1). Everything converges regardless; the fillfactor
itself, a HOT-update optimization, must be set by hand where it matters.

## Try it

Everything offline first — no database, no pgschema binary:

```console
$ pgpushy validate
```

Check the vendored SQL is current with the locked crate version (CI runs
this; it fails the moment a `cargo update` changes the emission):

```console
$ pgpushy generate --check
```

For `plan` and `apply`, point it at a scratch Postgres. pgpushy downloads and
verifies its own pgschema by default, so the database is the only thing to
supply:

```console
$ docker run -d --name pgpushy-example -e POSTGRES_PASSWORD=pw -p 5432:5432 postgres:18
$ docker exec pgpushy-example psql -U postgres -c 'CREATE DATABASE shop'
$ docker exec pgpushy-example psql -U postgres -d shop \
    -c 'CREATE SCHEMA app' -c 'CREATE SCHEMA billing' -c 'CREATE SCHEMA snowdrop'
```

The schemas are created by hand because pgpushy issues no DDL of its own to
the target: a managed schema must already exist (spec §6.1).

```console
$ export PGPASSWORD=pw
$ pgpushy plan --env local
$ pgpushy apply --env local
```

`apply` plans all three schemas, shows the lot — including the two seed
files — and asks once. On approval it applies the plans it just showed, then
runs the seeds: each file in its own transaction, executed twice, and the
second pass must touch nothing or the file rolls back whole. Run `apply`
again: every plan is empty, the seeds report zero rows affected, and the
probe passes — the whole project converges.

Two things worth trying from here:

- Edit a label in `db/seeds/order_statuses.sql` and re-apply: exactly one row
  is corrected, then the project converges again.
- Run `cargo update -p snowdrop-id-postgres` (when a newer release exists)
  and `pgpushy generate --check`: the vendored SQL is now stale, and the
  regeneration lands as a reviewable diff.
