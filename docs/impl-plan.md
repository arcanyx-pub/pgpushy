# pgpushy Implementation Plan

**Status:** Draft — build guidance (non-normative)
**Date:** 2026-07-29
**Companion to:** [`docs/spec.md`](./spec.md) v0.2 (normative)

This plan says *how* to build what `spec.md` defines. Where they disagree, the
spec wins. It is written to be read cold: §1 distills everything we learned
about pgschema from live spikes, so a fresh session need not re-derive it.

---

## 1. Essential pgschema knowledge (verified against pgschema 1.12.0)

All of this was confirmed empirically against `pgplex/pgschema:1.12.0` +
`postgres:18`. It is the ground truth the design rests on. Reproduction
commands are in [Appendix A](#appendix-a-reproduction-harness).

**CLI shape**
- Subcommands: `plan`, `apply`, `dump`. `plan`/`apply` take a single
  `--file <desired.sql>` and are `--schema`-scoped (**default `public`**, one
  schema per invocation). No `--version` flag and **no `version` subcommand**
  (`version` errors "unknown command"). Read version by parsing the
  `Version: X.Y.Z@<hash> <os>/<arch> <buildtime>` line printed by
  `pgschema --help` (also printed by a bare invocation).
- Flags we pass through: `--host --port --db --user --password --sslmode`,
  `--lock-timeout` (apply), `--plan-host/--plan-db/--plan-user/--plan-password/
  --plan-port/--plan-sslmode` (external plan DB), `--auto-approve` (apply),
  `--output-sql|--output-json|--output-human`. Honors `PG*` env vars.

**How it works (the mechanism that drives our whole design)**
- pgschema **executes** the desired-state `--file` into a temporary schema in a
  **plan database** (embedded ephemeral Postgres by default; external if
  `--plan-*` given) to build the "desired" model, reads "current" from the
  **target**, diffs, emits/apply DDL.
- Because it *executes* the file, **input order matters**: a reference to a
  not-yet-created table fails hard (`relation "x" does not exist`). `\i`
  include order matters too; a bare directory `\i dir/` includes
  **alphabetically**, not by dependency.
- It **"strips the target-schema prefix internally"** (per pgschema docs) and
  puts **unqualified** objects into the scratch schema that stands in for
  `--schema`. Consequence we verified: in a file feeding *many* per-schema
  runs, an **unqualified** object is attributed to **whichever `--schema` is
  running** → misattribution. **Fix: qualify every object with its real schema,
  including `public`.** (A qualifier that matches `--schema` is stripped, so
  qualified-matching == unqualified for a single-schema file; qualify-all is
  the uniform safe rule.)
- **Per-schema isolation:** with `--schema public`, objects in other schemas
  are silently ignored *for the diff* — but they must still be **present in the
  file** so cross-schema FKs resolve when the file executes in the plan DB.
  pgschema builds desired state **from the file only, never seeded from the
  target** (a `billing`-only file whose FK references `public.customers`
  *not in the file* fails, even if it exists on the target).
- **FK-lift is the key transform.** Emitting `CREATE TABLE` (no FKs) + trailing
  `ALTER TABLE … ADD CONSTRAINT` produces a plan **byte-identical** to inline
  FKs, makes table order irrelevant, and **handles same-schema cycles that no
  topological sort can** (pgschema itself defers one FK of a cycle via ALTER —
  PR #156, first in **v1.4.2**, 2025-11-14).
- **Absent target schema fails:** `plan`/`apply --schema X` errors
  `schema 'X' does not exist in the database` when X is absent on the target.
  An **empty-but-existing** schema is fine. **The external plan DB does NOT fix
  this** (it only builds *desired* state; *current* state is always read from
  the target). → v0.x makes "schemas pre-exist" a hard precondition.
- **Cross-schema FK cycles are unsupported by apply:** pgschema applies each
  schema in its own transaction and cannot defer a FK across schemas; both
  applies fail. Non-cyclic cross-schema FKs work **if schemas are applied in
  dependency order** (referenced schema first).
- `plan` is **read-only on the target** (scratch lives in the plan DB); `apply`
  writes only the diff (it does **not** emit `CREATE SCHEMA` — it assumes the
  schema exists).
- **`--output-human stdout --output-json <file>` work together** in one
  invocation, so the plan a human reads and the plan pgpushy summarizes are the
  same computation. The JSON is `{groups: [{steps: [{sql, type, operation,
  path}]}]}`, where `operation` is `create|drop|alter` and `path` is dotted
  (`public.orders.customer_id`). A plan with nothing to do has `groups: null`,
  not `[]`.
- **`apply --plan <file.json>` applies a previously computed plan**, and
  pgschema refuses one whose target has changed since (the plan carries a
  `source_fingerprint`). This is what lets pgpushy apply the plans the operator
  reviewed rather than recomputing them.
- **Drops are not generated uniformly**, which is the whole of spec §6.2:
  `DROP TABLE … CASCADE` for a table, but `ALTER TABLE … DROP COLUMN` and
  `ALTER TABLE … DROP CONSTRAINT` with no CASCADE. So dropping a table a
  cross-schema FK points at is safe in any order, and dropping the referenced
  *column* or *unique constraint* is not.

**What pgschema itself manages** (from its docs, 2026-07-29)
- Fifteen statement types: `CREATE TABLE`, `CREATE INDEX`, `CREATE VIEW`,
  `CREATE MATERIALIZED VIEW`, `CREATE FUNCTION`, `CREATE PROCEDURE`,
  `CREATE AGGREGATE`, `CREATE TRIGGER`, `CREATE TYPE`, `CREATE DOMAIN`,
  `CREATE SEQUENCE`, `CREATE POLICY`, `COMMENT ON`, `GRANT`/`REVOKE`,
  `ALTER DEFAULT PRIVILEGES`. This is pgpushy's eventual target (spec §12.5).
- **`CREATE SCHEMA` is on pgschema's own unsupported list**, independently
  confirming the §6.1 precondition. Also unsupported and therefore permanently
  out of pgpushy's scope: `CREATE EXTENSION`, `CREATE ROLE`, `CREATE DATABASE`,
  `CREATE TABLESPACE`, `CREATE CAST`, `CREATE COLLATION`, `CREATE OPERATOR`,
  `CREATE PUBLICATION`/`SUBSCRIPTION`, `CREATE SERVER`, event triggers,
  text search objects, and `RENAME`.
- **pgschema's own answer to ordering is a fixed category order.** Its `dump`
  emits *types → tables → views → functions → indexes*, and the docs say "most
  objects resolve regardless of order, but some objects depend on others
  existing **at creation time**" — e.g. a function whose parameter uses a
  table's row type must follow that table. That is the same mechanism as spec
  §5.1, so widening pgpushy's scope is mostly adding categories.
- **Function bodies are not parsed** — "intelligent dollar-quoting with
  automatic tag generation". So pgpushy can qualify a function's *name* and
  pass its body through untouched. Caveat to check: SQL-standard `BEGIN ATOMIC`
  bodies (PG14+) are resolved at creation time, unlike dollar-quoted ones.
- pgschema supports **PG 14–17**, which matches `pg_query` 6.1.1's PG17
  grammar; the PG18 target in our spikes is outside pgschema's stated range.
- pgschema has its own `.pgschemaignore` (for target objects) — unrelated to
  pgpushy's `exclude` (source files), but worth not confusing.

**Verified while spiking W1/W4 (2026-08-16)**
- **`--lock-timeout` is apply-only.** `plan` rejects it: `unknown flag`.
- **pgschema reconciles privileges by default.** A desired state that mentions
  no grants reads as "there should be none", and pgschema plans `REVOKE` for
  every grant on the target, plus `ALTER DEFAULT PRIVILEGES … REVOKE`. This is
  the §4.4 hazard in a different costume, and is why pgpushy writes a
  `.pgschemaignore` until grants are a managed kind.
- **`.pgschemaignore` is TOML, auto-loaded from pgschema's working directory**,
  with no flag to point at it. Sections include `[privileges]` and
  `[default_privileges]`; `patterns = ["*"]` suppresses a kind entirely.
  Because it is ambient, pgpushy runs pgschema in a directory it owns.
- **Grants need their roles to exist in the plan database.** The embedded one
  fails with `role "x" does not exist`. Roles are **cluster-wide**, so an
  external plan database on the *same cluster* as the target has them — which
  is what makes W4 workable at all.
- **An external plan database accumulates state across runs.** After a few
  spikes ours held seven leftover schemas, and stale objects made a *broken*
  desired state appear to work. It must be genuinely disposable; a spike that
  reuses one is not measuring what it thinks it is.
- **pgschema strips schema qualifiers from identifiers but not from string
  literals.** `CREATE SEQUENCE w1.s` becomes `CREATE SEQUENCE s` in the scratch
  schema, while `nextval('w1.s')` keeps its qualifier and then cannot resolve.
  `pg_dump` emits exactly that form (`nextval('s.seq'::regclass)`), so real
  source trees hit it immediately.
- **No single document satisfies every schema's run** once sequences are in
  scope: a same-schema `nextval` must be unqualified for its own run and
  qualified for every other. And a cross-schema reference *into* the target
  schema is unresolvable outright, because the target's objects live in a
  scratch schema whose name is unpredictable. → **per-schema documents trimmed
  to the closure** (spec §5.4), verified working.
- **pgschema's canonical order** (from `dump`): TYPE → DOMAIN → SEQUENCE →
  TABLE. An **owned** sequence renders as `SERIAL` on its column and does not
  appear as its own object; a **standalone** one does.
- **`ALTER DEFAULT PRIVILEGES` requires `IN SCHEMA`**, so every such statement
  names its own schema — which answers the "how does a grant attribute to a
  schema" question W4 was blocked on.
- Grants and ADP **attribute correctly per `--schema`**: a run for one schema
  plans only that schema's privileges.

**Release/distribution facts (for the provider, §7)**
- Assets are **standalone per-platform binaries**: `pgschema-<ver>-{darwin,
  linux}-{amd64,arm64}` (~18 MB static Go), plus `.deb`/`.rpm`. **No Windows
  binary. No published checksums/signatures.** License **Apache-2.0**.

---

## 2. Tech stack & conventions

Mirror `snowdrop-id-rs`:
- **Rust edition 2024**, `rust-version = "1.88"`, workspace `resolver = "3"`,
  shared `[workspace.package]`. 1.88 rather than snowdrop's 1.85 for a stated
  reason: **`let` chains** (`if let Some(x) = y && z`) are stable from 1.88,
  and writing around them costs readability in exactly the code that walks
  optional AST nodes. pgpushy is a CLI rather than a widely-depended-on
  library, so an older floor buys little. `just msrv` checks it before pushing
  — this is the one class of failure a modern local toolchain cannot see.
- **License:** Apache-2.0 (`LICENSE` is in the repo root), matching pgschema's
  own license. Set `license = "Apache-2.0"` in `[workspace.package]`.
- **justfile** recipes: `fmt`, `fmt-check`, `clippy` (`--all-targets
  --all-features -D warnings`), `test`, `test-fast`, `doc` (`-D warnings`),
  `ci = fmt-check clippy test doc`, `bump <level>` (see snowdrop's
  `docs/RELEASING.md`), `install-cli`.
- `README.md`, `CHANGELOG.md`, `docs/` for prose. Optional `assets/` mascot.
- Integration tests **skip without an env URL** (snowdrop uses
  `SNOWDROP_TEST_PG_URL`); mirror with `PGPUSHY_TEST_PG_URL` and
  `PGPUSHY_TEST_PGSCHEMA` (path to a pgschema binary).

**Crate dependencies (proposed)**

*Core (`pgpushy-core`, no IO):*
- `pg_query` — parse/deparse via libpg_query (the real PG parser). **Pin a
  version whose bundled Postgres grammar covers the PG features in use** (PG18
  in our spikes; pg_query trails PG majors — verify before relying on
  PG18-only syntax). This is the highest-risk dependency (see §14).
- `thiserror` — typed errors carrying source locations.

*Binary (`pgpushy`):*
- `clap` (derive) — CLI. `serde` + `toml` + `serde_json` — config, and reading
  pgschema's plan output. `semver` — version floor.
- `globset` — `exclude` patterns (spec §4.1).
- `postgres` (sync rust-postgres) — the read-only target inspection
  (spec §6). Sync, not async: pgpushy issues one query and shells out; a
  runtime would be pure overhead.
- `which` — PATH lookup (BYO). `tempfile` — the synthesized file and the plans.
- `anyhow` — binary-level error context.
- Later (managed provider): `ureq` or `reqwest` (blocking, rustls) + `sha2`.

*Dev:* `assert_cmd` + `predicates` (CLI), `postgres` and `tempfile`
(integration), gated on env vars rather than `testcontainers`.

**Dropped from the original plan.** `tracing` + `tracing-subscriber`: pgpushy's
output is hand-formatted and user-facing rather than logs, and a subscriber
would add a layer over text that is already exactly what we want. What that
entry was really after is debuggability, which `--verbose` serves directly —
it prints the pgschema command line and where the synthesized document went,
which is the only extra detail there is to want. `camino` was never needed;
`insta` was listed for snapshotting the synthesized SQL, but explicit golden
strings in the tests turned out to read better, since the interesting part is
*which* line changed rather than that something did. `walkdir` is a handful of
lines of `read_dir` given that discovery must not follow symlinks anyway.

---

## 3. Workspace & module layout

Two crates, following your "pure core + thin edge" philosophy — keep all
deterministic transformation in a library with no IO, push subprocess/network/
DB to the binary.

```
pgpushy/
  Cargo.toml                # [workspace] resolver=3, members, workspace.package
  justfile  README.md  CHANGELOG.md  LICENSE
  docs/spec.md  docs/impl-plan.md
  pgpushy-core/             # pure, deterministic, no IO — the heart
    src/lib.rs
    src/model.rs            # SchemaName, QualifiedName, Table, ForeignKey, Statement
    src/parse.rs            # pg_query parse → classify; allow-list enforcement (spec §4.2-4.3)
    src/resolve.rs          # schema assignment; managed-schema set derive/verify (spec §4.4)
    src/validate.rs         # duplicates, unresolvable FK referents (spec §4.5)
    src/synth.rs            # FK-lift + qualify + 5-category emission → String (spec §5)
    src/graph.rs            # cross-schema FK graph, topo order, cycle detection (spec §7)
    src/error.rs            # diagnostics carrying file + line
  pgpushy/                  # the `pgpushy` binary — IO shell
    src/main.rs             # clap dispatch
    src/cli.rs              # arg/flag definitions
    src/config.rs           # pgpushy.toml load + precedence + password warning (spec §10)
    src/discovery.rs        # walk source tree, apply excludes, deterministic order (spec §4.1)
    src/conn.rs             # connection resolution → conninfo; forward to pgschema (spec §6.3-6.4)
    src/inspect.rs          # read-only target inspection: schemas, cross-schema FKs, identity (spec §6)
    src/provider/mod.rs     # trait PgschemaProvider
    src/provider/byo.rs     # PATH/explicit path + version check (ship first)
    src/provider/managed.rs # download+cache+verify (fast-follow)
    src/pgschema.rs         # build/run pgschema commands; stream output
    src/approve.rs          # plan presentation + single database-level prompt (spec §8.6)
    src/run.rs              # orchestrates validate/plan/apply
    tests/                  # integration (real pgschema + Postgres)
```

Binary target name **`pgpushy`**; library crate **`pgpushy-core`**.

Note the split: **discovery lives in the binary** (it touches the filesystem),
while everything from parsing onward is pure. `pgpushy-core` takes
`Vec<(RelPath, String)>` — path plus contents — and returns either a
synthesized document plus a schema order, or a list of diagnostics. That makes
the entire offline pipeline (spec §3 stages 2–6) unit-testable from string
literals with no fixtures on disk.

---

## 4. Data model (`pgpushy-core::model`)

Minimal for the tables+FK scope (spec §4.3). Grow later per spec §14.

```rust
struct SchemaName(String);                    // always resolved, never empty
struct QualifiedName { schema: SchemaName, name: String }

enum Statement {                              // the allow-list, and nothing else
    CreateSchema(SchemaName),
    CreateTable(Table),                       // FKs pulled out into ForeignKey
    CreateIndex(Index),
    TableConstraint(TableConstraint),         // standalone non-FK ADD CONSTRAINT
    ForeignKey(ForeignKey),                   // from inline OR standalone ADD CONSTRAINT
    Comment(Comment),
}

struct Table { name: QualifiedName, ast: CreateStmt }   // ast: FKs already removed
struct ForeignKey {
    table: QualifiedName,                     // the referencing table
    name: Option<String>,                     // author's name, or None → emit unnamed
    referenced: QualifiedName,                // schema resolved
    ast: Constraint,                          // full FK definition, re-emitted unchanged
}
struct Origin { file: RelPath, line: u32 }    // on every statement, for diagnostics
```

Key invariants after parse + resolve:

- **Every object and every FK referent carries an explicit schema**, resolved
  from a qualifier or from the default schema.
- **Every statement carries an `Origin`.** Spec §4.5 and §4.3 diagnostics name
  file and line; a statement that cannot say where it came from cannot produce
  a compliant error message. Thread `Origin` from the start rather than
  retrofitting it.
- **There is no `Other` variant.** Spec §4.3 rejects everything outside the
  allow-list, so an unmatched statement produces an error, never a pass-through
  bucket. This is a change from the v0.1 plan and it simplifies synthesis
  considerably: no unmodelled text to place, order, or fail to qualify.

---

## 5. Synthesis algorithm (`synth.rs`) — the heart

Produce one desired-state document (spec §5). Steps:

1. **Bucket** every statement into the five categories of spec §5.1:
   schemas → tables → table-dependent objects (indexes, non-FK constraints) →
   foreign keys → comments. The category boundaries are what make the output
   executable; do not intermix.
2. **FK-lift** (spec §5.3): move every FK — inline column constraint
   (`Constraint` with contype `CONSTR_FOREIGN`), table-level FK, and standalone
   `ALTER … ADD CONSTRAINT` — into category 4. Preserve the constraint
   definition **exactly** (no added `NOT VALID`, spec §5.5).
3. **Constraint names** (spec §5.3): keep the author's name if there is one;
   otherwise emit **no name** and let Postgres generate it in the plan DB. Do
   **not** synthesize names — a synthesized name differs from the one the
   target already holds and churns the plan forever.
4. **Qualify everything** (spec §5.4): set the schema on every emitted object
   *and* every FK referent, including `public`. Emit
   `CREATE SCHEMA IF NOT EXISTS <s>` for each managed schema first (runs only
   in the plan DB — never the target; spec §5.2/§6.1).
5. **Deterministic order** (spec §11.3): stable sort within each category by
   `(schema, name)`. Output must be **byte-identical across runs and
   platforms**, and must not depend on filesystem enumeration order.

**Implementation approach — AST-mutate + deparse.** `pg_query` gives a protobuf
AST and a `deparse()`. The transforms are targeted edits:
- FK-lift: remove FK `Constraint` nodes from `CreateStmt`; build
  `AlterTableStmt { cmds: [AT_AddConstraint(fk)] }` nodes for them, with
  `conname` left empty for author-unnamed constraints.
- Qualify: set `schemaname` on each relation `RangeVar` (the table, the index's
  table, the FK's `pktable`, the comment's object).

Then `deparse()` each statement. **pgpushy's output is consumed by pgschema, a
machine — canonical/pretty form is irrelevant, only validity + correct desired
state.** So deparse output quality is not a concern beyond "parses and means
the same thing."

> ✅ **R1 is answered** — see `pgpushy-core/tests/spike_pg_query.rs` and §14.
> AST-mutate + deparse works on all 13 representative fixtures; the text-slice
> fallback is not needed.

**Synthesis-file granularity** (spec §5.4): emit **one combined document**
reused for every per-schema run (simplest, verified). Per-schema trimming to
the cross-schema closure is a possible large-DB optimization — not v0.x.

---

## 6. Cross-schema ordering (`graph.rs`)

Spec §7. Build a digraph over managed schemas: edge `A → B` when a table in `A`
has a FK referencing a table in `B` (same-schema FKs create no edge — pgschema
handles those, §5.3/PR#156). Process schemas in **reverse-dependency order**
(a schema after every schema it references). Tie-break by schema name so the
order is reproducible.

Cycle detection (Tarjan/Kahn) must report **the schemas in the cycle and the
foreign keys forming it** — enough for the operator to break it. The
*consequence* differs by command and belongs in `run.rs`, not here: `apply` and
`validate` fail; `plan` reports and continues. So `graph.rs` returns a
`Result<SchemaOrder, Cycle>`-shaped value where `Cycle` is data the caller
decides about, not an error the library raises.

---

## 7. pgschema provider (`provider/`)

Spec §8.5. Trait that yields a runnable binary:

```rust
trait PgschemaProvider { fn resolve(&self) -> Result<PgschemaBin>; }
struct PgschemaBin { path: Utf8PathBuf, version: Option<Version> }
```

- **`byo.rs` (ship first).** Resolve an explicit path (config/flag) or
  `which("pgschema")`. Run `pgschema --help`, parse the `Version:` line
  (`^Version:\s*(\d+\.\d+\.\d+)`), compare with the floor via `semver`.
  **Below floor → hard error** naming found vs required, with **no override**
  (spec §13). **Unparseable version → warn, proceed** (the line is not a
  stability contract) — hence `Option<Version>`.
- **`managed.rs` (the default).** Resolve version (pinned to tested version,
  config-overridable) → map `(version, os, arch)` to the GitHub release asset
  URL `…/releases/download/v<ver>/pgschema-<ver>-<os>-<arch>` → download over
  HTTPS to `$XDG_CACHE_HOME/pgpushy/pgschema/<ver>/<platform>/pgschema` →
  **verify against a pgpushy-shipped SHA-256** (const table, since pgschema
  ships none) → `chmod +x` via an atomic temp-file-and-rename. Linux/macOS
  only; Windows/air-gapped → BYO.

  Two departures from the sketch above, both from testing:

  **No lock is needed for concurrent runs.** The atomic rename is sufficient —
  four simultaneous cold-cache runs each download, each rename, and the last
  wins with an identical file. Verified: one cached binary, correct hash, no
  leftover temporaries. A lock would only save redundant bandwidth, and it
  would add a failure mode (a stale lock from a killed process) worse than the
  problem.

  **The cache is re-verified on every hit**, not trusted for existing. Atomic
  writes cover pgpushy's own interrupted downloads and nothing else, and
  `exists()` cannot distinguish a good cache from a tampered one. Hashing
  ~19 MB costs about 10 ms, which is nothing beside the network and database
  work that follows. On mismatch: report and re-download.

  The path also keys on platform as well as version, so a cache on a shared
  network home directory cannot serve one architecture's binary to another.

**Floor constant:** `MIN_PGSCHEMA = "1.12.0"` (the tested version). True
behavioral floor is v1.4.2 — headroom to lower later *with tests*, not the
supported floor. Bump `MIN_PGSCHEMA` as CI tests newer releases.

---

## 8. Connection, inspection, orchestration

### `conn.rs` — one resolution, forwarded (spec §6.3, §6.4)

Fold CLI flags, `PG*` env, and `pgpushy.toml` into a single resolved parameter
set, then produce two things from it: a libpq connection string for pgpushy's
own driver, and an explicit flag list for pgschema. **pgschema must never
resolve anything itself** — pass `--host --port --db --user --sslmode`
explicitly and supply the password through the child's environment. Do not let
ambient `PG*` reach the child unresolved.

This is how spec §6.3's identity guarantee is delivered: not by comparing two
resolutions afterwards, but by ensuring there is only one.

### `inspect.rs` — one read-only round trip (spec §6)

A single connection answering three questions:

1. **Which managed schemas exist?** `SELECT nspname FROM pg_namespace` filtered
   to the managed set; report *all* missing ones (spec §6.1).
2. **What cross-schema FKs does the target hold?** Join `pg_constraint`
   (`contype = 'f'`) to `pg_class`/`pg_namespace` on both sides, keeping rows
   where the two namespaces differ and both are managed. Compare against the
   desired state; a target FK absent from desired, whose removal the §7 order
   cannot accommodate, is a hard error with the two-step remedy (spec §6.2).
3. **What database is this?** `current_database()`, `inet_server_addr()`,
   `inet_server_port()`, and `system_identifier` from `pg_control_system()`,
   for the identity line in output (spec §6.3).

Everything here is `SELECT`. Assert that in review: this module is the only
direct target access, and spec §6 hangs the "pgpushy issues no DDL" guarantee
on it.

### `pgschema.rs`

Build the argv: `pgschema <plan|apply> --schema <S> --file <synth>
<connection flags> [--auto-approve]`. pgpushy owns `--schema`, `--file`, and
`--auto-approve` (spec §8.3). Stream pgschema stdout/stderr through — do
**not** parse plan output (thin wrapper). Write the synthesized doc to a
`tempfile`, or to a debuggable path under `--out`.

### `approve.rs` (spec §8.6)

Full plan pass → present all schemas' plans as one unit with a change summary
→ call out destructive changes and any schema reconciling to an empty desired
state → state that apply is not atomic across schemas → prompt once. Decline
touches nothing. `--auto-approve` skips the prompt; a non-TTY stdin without
`--auto-approve` is a failure, not an implicit yes.

### `run.rs`

- `validate`: offline pipeline only, no connection. Report managed set, counts,
  exclusions, apply order. Fail on any §4.3/§4.5/§7 condition.
- `plan`: offline pipeline → inspect → per-schema `pgschema plan`. A cross-schema
  cycle is reported but does **not** suppress the plans; exit non-zero.
- `apply`: offline pipeline (cycle is fatal here) → inspect → plan pass →
  approval → per-schema `pgschema apply` in order. **Stop at the first
  failure**; report applied / failed / not-attempted, and say the applied ones
  are not rolled back (spec §9).

---

## 9. Config & CLI

- **`pgpushy.toml`** (TOML, **current working directory only**, optional;
  `--config <path>` for an explicit path — it is *not* searched for in parent
  directories, spec §10). Sections: project structure (`source_root`,
  `default_schema`, `exclude`), `managed_schemas`, `[pgschema]` provider
  (backend, version, path), `[connection]` (host/port/db/user/sslmode, and
  `password`). Precedence **CLI > `PG*`/env > file > default**; default schema
  `public`.
- **`managed_schemas`** (spec §4.4): when present it is authoritative — a
  mentioned-but-unlisted schema is an error naming the file and line that
  enlisted it; a listed-but-unmentioned schema is managed **and empty**, which
  is destructive and must be called out in the plan presentation.
- **`exclude`** (spec §4.1): globs via `globset`, matched against source-root-
  relative paths, applied during discovery so excluded files are never parsed.
  Report the excluded count per pattern.
- **Password warning** (spec §10): when the *effective* password is sourced from
  the file (not overridden by `PGPASSWORD`/`--password`), emit a prominent
  `tracing::warn!` — "password read from pgpushy.toml, which is easily
  committed to version control; prefer PGPASSWORD or --password." Fires on
  actual use, not mere presence.
- **CLI** (`clap` derive): `pgpushy validate`, `pgpushy plan`, `pgpushy apply`,
  plus global connection flags (mirroring pgschema names), `--config`,
  `--source-root`, `--default-schema`, `--pgschema-path`, `-v/--verbose`.
  `--out <path>` to keep the synthesized document. `--auto-approve` for apply.
  (Future: `pgpushy dump`, spec §14.)

---

## 10. Testing strategy

- **Unit (`pgpushy-core`)** — parse/classify, allow-list rejection, schema
  resolution, duplicate detection, unresolvable-referent detection, FK-lift,
  qualification, category bucketing, graph ordering, cycle detection. Because
  core takes `(path, contents)` pairs, every one of these is a string-literal
  test. **Snapshot the synthesized SQL with `insta`** (golden files). Add a
  **determinism test**: synth twice → byte-identical, and synth with the input
  file list shuffled → byte-identical.
- **CLI (`assert_cmd`)** — `pgpushy validate` end-to-end on fixture trees, with
  no database anywhere. This covers most of the spec's diagnostics cheaply.
- **Integration (`pgpushy/tests`)** — against a **real pgschema + Postgres**.
  Use `testcontainers` to spin `postgres:18`, and a pgschema binary resolved
  from `PGPUSHY_TEST_PGSCHEMA` (download in a `just` setup step, or run the
  `pgplex/pgschema` image via a wrapper). Skip (don't fail) when neither is
  available, mirroring snowdrop.
- **Port the spike fixtures** (Appendix B) into `tests/` as the core cases —
  each is a regression we already know the answer to:
  1. unordered inline FK → pgpushy makes it succeed (was a raw-pgschema failure).
  2. FK-lift plan == correctly-ordered inline plan.
  3. same-schema FK cycle → succeeds.
  4. multi-schema qualification: no misattribution across `--schema` runs.
  5. cross-schema non-cyclic → applies in dependency order, converges.
  6. cross-schema cycle → `apply` and `validate` reject; `plan` shows plans and
     exits non-zero.
  7. absent target schema → inspection fails cleanly, naming every missing one.
  8. idempotence: second `apply` is a no-op / empty plans.
  9. BYO version check: below-floor pgschema → hard error; unparseable → warn.
  10. **author-unnamed FK → empty re-plan** (spec §5.3, §11.1). Create a table
      with an unnamed inline FK, apply, then plan again: must be empty. This is
      the test that proves omitting the name matches Postgres's own naming.
  11. **cross-schema FK removal** → detected before apply, with the two-step
      message; neither schema is touched.
  12. disallowed statement (a `CREATE VIEW`, an `INSERT`) → rejected with file
      and line, no connection attempted.

---

## 11. Milestones (phased; each shippable)

- **M0 — Skeleton & spike.** Workspace, justfile, CI. **Do the pg_query
  round-trip spike (R1) first** — it de-risks everything. R1 now also covers
  the unnamed-FK naming question (§14).
- **M1 — `pgpushy validate`. ✅ Done.** discover → parse → allow-list → resolve
  → duplicate/referent/collision checks → graph → synth, plus the CLI. **No
  database and no pgschema binary**, so it is fully testable in CI from day one
  and exercises every line of `pgpushy-core`. Fixtures 12 and the offline half
  of 6. Source-tree flags (`--source-root`, `--default-schema`,
  `--managed-schema`, `--exclude`) are CLI-only until M4 adds the file under the
  same precedence.
- **M2 — `plan` (BYO). ✅ Done.** BYO provider + version check, connection
  resolution, target inspection, and `pgschema plan` per managed schema in
  dependency order — the multi-schema loop came free, since `pgpushy-core`
  already produces the order. Fixtures 1–3, 5, 7, 8 (via plan), 9.
- **M3 — `apply`. ✅ Done.** Plan pass retaining each plan as JSON, single
  database-level approval, cross-schema FK removal detection, apply via
  `--plan`, stop-at-first-failure reporting. Fixtures 4–6, 8, 10, 11.
- **M4 — `pgpushy.toml`. ✅ Done.** A **required** configuration file holding
  everything that decides what gets reconciled, named environments with a
  required `--env`, and the password warning. Project structure is deliberately
  not settable by flag, which also means there is no CLI-vs-file precedence to
  get wrong — the earlier design had both, and the interaction between an
  optional file and a source root defaulting to the working directory was the
  hazard that prompted the change (spec §10.1).
- **M5 — Pass-through and polish. ✅ Done.** The §8.3 gap (`--lock-timeout`,
  the `--plan-*` family) plus `pgpushy init`, colour handling, `--verbose`, the
  empty-tree case, and dependency hygiene. Both pass-through settings live in
  the environment (spec §10.4, §10.5) because both describe *that target*;
  `--lock-timeout` is additionally a flag, since it cannot change what gets
  reconciled.
- **M6 — Managed provider. ✅ Done.** download/cache/verify; SHA-256 table;
  managed is now the default backend. Adding a pgschema version means adding
  four hashes — one per published platform — computed by downloading each asset
  and hashing it. A unit test asserts the pinned version has a hash for every
  platform, so forgetting one drops that platform to TLS-only trust loudly
  rather than silently.

**Widening to pgschema parity** (spec §12.5, §14). Deliberately *after* the
vertical slice: until `plan` and `apply` work, a new object kind cannot be
tested end to end, so every one added early is code resting on unverified
assumptions. Once M3 lands, each kind below gets a real regression test the day
it is written.

- **W0 — Per-schema trimmed synthesis.** *Prerequisite for W1, discovered by
  spiking it.* `synthesize` becomes per-target-schema: the target schema's
  objects plus the transitive closure of what they reference elsewhere, with
  string-literal references (`nextval`) unqualified for the target schema and
  qualified for the rest. `--out` becomes a directory of one file per schema
  (decided 2026-08-16). Also unblocks W3, where views need the same thing.
- **W1 — Sequences, types, domains.** Structured names, no bodies. Category
  order is pgschema's own: types and domains before sequences before tables.
  Owned sequences are part of their column (`SERIAL`) rather than separate
  objects, so only standalone ones get their own entry.
- **W2 — Functions, procedures, aggregates, triggers, policies.** Qualify the
  name, pass the body through. Spike `BEGIN ATOMIC` first.
- **W3 — Views and materialized views.** The real work: bodies are resolved at
  creation, and views need a topological sort within their category. Spike the
  per-schema-document question before writing an AST-walking qualifier — it may
  remove the need entirely (spec §14).
- **W4 — `GRANT`/`REVOKE`, `ALTER DEFAULT PRIVILEGES`.** Attribution is
  settled: `ALTER DEFAULT PRIVILEGES` requires `IN SCHEMA`, and a grant
  attributes to the schema of the object granted on — both verified to plan
  correctly per `--schema`. What remains is that grants need their roles in the
  plan database, so pgpushy must **refuse with an explanation** when a source
  tree grants and no plan database is configured (decided 2026-08-16), and an
  `ignore_grants` setting must keep today's leave-them-alone behaviour
  available as an explicit opt-out.

- **Later (spec §14):** `pgpushy dump`; references into unmanaged schemas via
  external plan DB; cross-schema-cycle and single-pass-removal support;
  schema-drop.

Note M1 comes before any pgschema or Postgres dependency — a deliberate change
from the v0.1 ordering. The offline pipeline is where all the spec's novel
logic lives, and `validate` makes it shippable and testable on its own.

---

## 12. Output & error conventions

- Human-first stderr via `tracing`; keep pgschema's own output visible
  (passthrough). Non-zero exit on any failure; distinguish pgpushy failures
  (validation, inspection, version) from pgschema failures in the message.
- Diagnostics name **file and line** for anything sourced from the tree
  (spec §4.3, §4.5), and **name every instance**, not just the first — missing
  schemas, duplicate objects, and disallowed statements are all reported as
  complete lists.
- On partial `apply` failure, list applied / failed / not-attempted schemas and
  state that applied schemas are not rolled back (spec §9, §11.2).
- Cross-schema cycle and removal errors must name the schemas *and* the foreign
  keys involved.

---

## 13. Non-obvious pitfalls (learned the hard way)

- **Do not leave objects unqualified** in the combined file — they leak into
  every `--schema` run (verified misattribution). Qualify all, incl. `public`.
- **Do not topologically sort tables** to fix ordering — FK-lift instead; sort
  can't express cycles, lift can.
- **Do not name author-unnamed FK constraints.** A stable name is not enough;
  it must be *Postgres's* name, or the plan churns forever. Emit no name.
- **Never `..Default::default()` a synthesized libpg_query node.** Its deparser
  maps protobuf enum values back to C enums through a `switch` that
  `Assert(false)`s on anything unrecognized, and every such enum reserves 0 for
  `Undefined` — which is exactly what `Default` supplies. The result is a
  `SIGABRT` that kills the process, not a `Result::Err` you can handle. Set
  every enum field on a synthesized node explicitly (`AlterTableCmd.behavior`
  is the one that bit us: it must be `DropBehavior::DropRestrict`). Nodes that
  came from a real parse are fine; only synthesized ones are at risk.
- **Do not put indexes or comments in the same category as tables.** They
  depend on their table existing, and a `(schema, name)` sort will happily
  place an index before it.
- **`CREATE SCHEMA` in the synth file is for the plan DB only.** It never
  reaches the target; the target schema must pre-exist (v0.x precondition).
- **External plan DB does not help absent target schemas** — current state is
  always read from the target.
- **Managed-schema set must exclude public unless it has objects** — otherwise
  an empty desired `public` plans a **drop of everything** in the target's
  public schema. The same hazard is what makes a listed-but-unmentioned
  `managed_schemas` entry destructive on purpose.
- **pgschema apply is per-schema-transactional** — no cross-schema atomicity;
  order schemas by cross-schema FKs; cross-schema cycles are unsupported, and
  cross-schema FK *removal* needs the reverse order (spec §6.2).
- **Let pgschema resolve nothing.** Two independent connection resolutions is a
  latent wrong-database bug; pass everything explicitly (spec §6.3).

---

## 14. Risks & open implementation questions

- **~~R1~~ — RESOLVED (2026-07-29).** Both halves confirmed; see below.
- **~~R2~~ — RESOLVED (2026-08-11).** pgschema ships no checksums, so pgpushy
  self-pins SHA-256 in `managed.rs`. The update process is part of
  [RELEASING.md](RELEASING.md): bumping the pinned version means downloading
  all four assets, hashing them, and committing the four rows in the same
  change as the version bump, so the hashes get reviewed alongside it. A unit
  test fails if the pinned version lacks a hash for any published platform.
- **~~R3~~ — RESOLVED (2026-07-29).** Cross-schema FK removal detection is
  precise because it reads pgschema's own plan rather than guessing: a hazard
  only when the referenced schema's plan drops the specific column or unique
  constraint the foreign key depends on, *and* the referencing schema is
  ordered after it. Table drops are explicitly not flagged, since pgschema
  CASCADEs them. One residual sharp edge, documented rather than fixed: when
  the desired state removes the foreign key entirely, the graph has no edge
  between the pair, so their relative order falls to the name tie-break — which
  may happen to be safe or may not. Either way pgpushy is correct (it refuses
  when unsafe), but whether a given change needs the two-step remedy depends on
  schema names. Ordering such pairs deliberately is the future work in spec
  §14.
- **R4 — testcontainers vs. image wrapper** for pgschema in CI, and how to get
  a pgschema binary into CI hermetically (download in `just setup`).

*Resolved since v0.1:* generated FK constraint names (spec §5.3 now omits them,
removing the churn risk entirely) and `Other`-statement ordering (spec §4.3 now
rejects unmodelled statements; confirmed acceptable — the real source trees
this targets contain only tables, indexes, comments and foreign keys).

### R1 spike results (2026-07-29)

**Deparse fidelity — passes.** `pgpushy-core/tests/spike_pg_query.rs` runs FK-lift
and qualification over 13 fixtures and checks that deparse output re-parses and
re-deparses identically. All pass, including composite FKs, `ON DELETE CASCADE`
/ `ON UPDATE RESTRICT`, `ON DELETE SET NULL (cols)`, `MATCH FULL DEFERRABLE
INITIALLY DEFERRED`, quoted mixed-case identifiers, reserved words as quoted
identifiers, generated and identity columns, arrays, and intervals with
qualifiers. **The text-slice fallback is not needed.** `synth.rs` can be
AST-mutate + deparse throughout.

Deparse also emits an unnamed lifted constraint as `ALTER TABLE t ADD FOREIGN
KEY (…)` — no `CONSTRAINT` clause — which is exactly what spec §5.3 requires.

**Constraint naming — passes.** Against Postgres 18, the inline and lifted
forms produce identical generated names in every case tried: single-column,
composite (`child_x_y_fkey`), quoted mixed case (`Orders_customerId_fkey`),
63-byte truncation, collision with another foreign key (`dual_ref_id_fkey` /
`dual_ref_id_fkey1`), and collision with a `CHECK` constraint already holding
the name (`blocked_ref_id_fkey1`). Zero divergences.

The truncation case is worth seeing, because it shows how much work option "B"
(replicating Postgres's algorithm in Rust) would have been: for a 65-character
table name Postgres produces
`a_very_long_table_name_that_will_a_rather_long_column_name_fkey` — it truncates
the *table-name component* to make room for the column and the suffix, rather
than truncating the finished string.

**One caveat, now spec §12.4.** Suffix assignment follows *creation* order.
Adding the same two competing foreign keys in the opposite order moves
`dual_ref_id_fkey` from referencing `t1` to referencing `t2`. Since pgpushy's
emission order comes from source content and need not match the order the
target's constraints were created in, two *unnamed* foreign keys on the same
table over the *identical* column set can swap names and churn the plan
forever. Spec §12.4 requires detecting and rejecting exactly that shape;
implement it in `validate.rs` alongside the duplicate check.

**Grammar version.** `pg_query` 6.1.1 bundles the **PG17** grammar while the
spikes ran PG18. Nothing in the tables-and-foreign-keys scope needs PG18-only
syntax, and every fixture above parses. Revisit if the scope widens (spec §14).

**Pitfall for synthesis:** see §13 on `Default::default()` — it aborts the
process, it does not return an error.

---

## Appendix A: Reproduction harness

Recreate the spike environment (the `pgplex/pgschema` + `postgres:18` images
are already cached locally):

```bash
# Postgres
docker run -d --name pgspike -e POSTGRES_PASSWORD=pw -e POSTGRES_DB=spike \
  -p 55432:5432 postgres:18
docker network create spikenet && docker network connect spikenet pgspike

# Run pgschema (mount a dir of .sql at /work)
pg() { docker run --rm --network spikenet -e PGPASSWORD=pw -v "$PWD":/work \
  pgplex/pgschema:latest "$@"; }

# Plan one schema against the target, show generated SQL
pg plan --host pgspike --db spike --user postgres --schema public \
  --file /work/desired.sql --output-sql stdout

# External plan DB (created as a separate DB on the same server):
#   --plan-host pgspike --plan-db plandb --plan-user postgres \
#   --plan-password pw --plan-sslmode disable

# psql to the target from the host
PGPASSWORD=pw psql -h localhost -p 55432 -U postgres -d spike
```

Read version: `docker run --rm pgplex/pgschema:latest --help | grep '^Version:'`

## Appendix B: Spike fixtures (embed as regression tests)

These SQL snippets and their verified outcomes (see §1 and §10). They were
authored under `~/workspace/pgspike-tests/` during design; reproduce them as
`pgpushy-core`/integration fixtures. Key ones:

```sql
-- Unordered inline FK (raw pgschema FAILS; pgpushy must make it PASS via lift)
CREATE TABLE orders (id int PRIMARY KEY, customer_id int NOT NULL REFERENCES customers(id));
CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);

-- FK-lifted form (pgschema plan == the correctly-ordered inline plan).
-- Note: pgpushy emits the constraint UNNAMED (spec §5.3); the name below is
-- what Postgres generates, shown for clarity.
CREATE TABLE orders (id int PRIMARY KEY, customer_id int NOT NULL);
CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);
ALTER TABLE orders ADD CONSTRAINT orders_customer_id_fkey
  FOREIGN KEY (customer_id) REFERENCES customers(id);

-- Same-schema cycle, lifted (SUCCEEDS; pgschema defers one FK):
CREATE TABLE husband (id int PRIMARY KEY, wife_id int);
CREATE TABLE wife    (id int PRIMARY KEY, husband_id int);
ALTER TABLE husband ADD CONSTRAINT husband_wife_id_fkey FOREIGN KEY (wife_id) REFERENCES wife(id);
ALTER TABLE wife    ADD CONSTRAINT wife_husband_id_fkey FOREIGN KEY (husband_id) REFERENCES husband(id);

-- Multi-schema: MUST qualify all incl. public, else --schema snowdrop wrongly
-- plans `customers` into snowdrop (verified misattribution):
CREATE SCHEMA IF NOT EXISTS snowdrop;
CREATE TABLE public.customers (id int PRIMARY KEY, name text NOT NULL);
CREATE TABLE snowdrop.machine_ids (machine_id int PRIMARY KEY, hostname text NOT NULL);

-- Cross-schema cycle (UNSUPPORTED — apply/validate reject; plan shows plans):
--   public.customers → billing.accounts AND billing.accounts → public.customers

-- Cross-schema FK removal (spec §6.2 — must be detected before apply):
--   target holds public.orders → billing.accounts;
--   source tree drops BOTH the FK and billing.accounts.
--   Creation order (billing first) cannot apply this; needs public first.
```

Verified outcomes are catalogued in §1; the memory file
`pgpushy-project.md` (agent memory) holds the same facts if available.
