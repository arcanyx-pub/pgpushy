# pgpushy Implementation Plan

**Status:** Draft — build guidance (non-normative)
**Date:** 2026-07-28
**Companion to:** [`docs/spec.md`](./spec.md) (normative)

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

**Release/distribution facts (for the provider, §7)**
- Assets are **standalone per-platform binaries**: `pgschema-<ver>-{darwin,
  linux}-{amd64,arm64}` (~18 MB static Go), plus `.deb`/`.rpm`. **No Windows
  binary. No published checksums/signatures.** License **Apache-2.0**.

---

## 2. Tech stack & conventions

Mirror `snowdrop-id-rs`:
- **Rust edition 2024**, `rust-version = "1.85"` (bump only with reason),
  workspace `resolver = "3"`, shared `[workspace.package]`.
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
- `pg_query` — parse/deparse via libpg_query (the real PG parser). **Pin a
  version whose bundled Postgres grammar covers the PG features in use** (PG18
  in our spikes; pg_query trails PG majors — verify before relying on
  PG18-only syntax). This is the highest-risk dependency (see §14).
- `clap` (derive) — CLI. `serde` + `toml` — config. `semver` — version floor.
- `which` — PATH lookup (BYO). `tempfile` — the synthesized file.
- `tracing` + `tracing-subscriber` — logging and the password warning.
- `thiserror` (library errors) / `anyhow` (binary). `camino` (UTF-8 paths, opt).
- Later (managed provider): `ureq` or `reqwest` (blocking, rustls) + `sha2`.
- Dev: `insta` (snapshot the synthesized SQL), `assert_cmd` + `predicates`
  (CLI), `testcontainers` (hermetic Postgres) — with an env-URL fallback.

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
    src/discovery.rs        # walk a source tree → ordered set of .sql paths (§4)
    src/parse.rs            # pg_query parse → statements; classify (§5)
    src/model.rs            # Schema, Table, ForeignKey, Object ids (§4-data)
    src/synth.rs            # FK-lift + qualify + CREATE SCHEMA + deterministic order → String (§5-spec)
    src/graph.rs            # cross-schema FK graph, topo order, cycle detection (§7-spec)
    src/error.rs
  pgpushy/                  # the `pgpushy` binary — IO shell
    src/main.rs             # clap dispatch
    src/cli.rs              # arg/flag definitions; owns --schema/--file? NO (see §8-spec)
    src/config.rs           # pgpushy.toml load + precedence + password warning (§9-spec)
    src/provider/mod.rs     # trait PgschemaProvider
    src/provider/byo.rs     # PATH/explicit path + version check (ship first)
    src/provider/managed.rs # download+cache+verify (fast-follow)
    src/pgschema.rs         # build/run pgschema commands; stream output
    src/precondition.rs     # read-only: verify managed schemas exist on target
    src/run.rs              # orchestrates plan/apply over the schema order
    tests/                  # integration (real pgschema + Postgres)
```

Binary target name **`pgpushy`**; library crate **`pgpushy-core`**.

---

## 4. Data model (`pgpushy-core::model`)

Minimal for the tables+FK scope (spec §5.1). Grow later for views/functions.

```rust
struct SourceTree { root: Utf8PathBuf, files: Vec<SourceFile> }
struct SourceFile { path: Utf8PathBuf, statements: Vec<Statement> }

enum Statement {                 // classified from the parse (§5)
    CreateSchema(SchemaName),
    CreateTable(Table),          // FKs pulled out into `foreign_keys`
    ForeignKey(ForeignKey),      // from inline OR standalone ALTER … ADD CONSTRAINT
    Other(RawStmt),              // pass-through, kept in source order (v0.x: indexes, comments…)
}

struct Table { schema: SchemaName, name: String, /* body needed to re-emit */ ast: CreateStmt }
struct ForeignKey {
    schema: SchemaName, table: String,      // the referencing table
    name: Option<String>,                   // author name or None → generated (§5.3)
    referenced: QualifiedName,              // (schema, table) — schema resolved!
    ast: Constraint,                        // full FK definition to re-emit unchanged
}
// SchemaName resolves unqualified → default schema (config, default "public").
```

Key invariant: after parsing, **every object and every FK referent carries an
explicit schema** (resolved from qualifier or default). The managed-schema set
(spec §4.3) is the schemas that appear here — public is NOT auto-included.

---

## 5. Synthesis algorithm (`synth.rs`) — the heart

Produce one desired-state document (spec §5). Steps:

1. **Collect** all statements across files into three buckets in category
   order (spec §5.1): `CreateSchema` → `CreateTable` (FKs removed) → all
   `ForeignKey` as `ALTER TABLE … ADD CONSTRAINT`. `Other` statements ride with
   their table's category/position (v0.x: keep source order among them).
2. **FK-lift** (spec §5.3): move every FK — inline column constraint
   (`Constraint` contype `CONSTR_FOREIGN` on a column) and table-level FK and
   standalone `ALTER … ADD CONSTRAINT` — into the trailing category. Preserve
   the constraint definition **exactly** (no added `NOT VALID`, spec §5.5).
3. **Qualify everything** (spec §5.4): set the schema on every emitted object
   *and* every FK referent, including `public`. Emit
   `CREATE SCHEMA IF NOT EXISTS <s>` for each managed schema first (runs only
   in the plan DB — never the target; spec §5.2/§6).
4. **Deterministic order & names** (spec §10.3): stable sort within each
   category (e.g. by `(schema, name)`); generate missing FK names the pg_dump
   way — `<table>_<col1>[_<col2>…]_fkey`, de-duplicated with a numeric suffix —
   deterministically. Output must be **byte-identical across runs/platforms**.

**Implementation approach — AST-mutate + deparse.** `pg_query` gives a protobuf
AST and a `deparse()`. The transforms are targeted edits:
- FK-lift: remove FK `Constraint` nodes from `CreateStmt`; build
  `AlterTableStmt { cmds: [AT_AddConstraint(fk)] }` nodes for them.
- Qualify: set `schemaname` on each relation `RangeVar` (the table, and the FK
  `pktable`).
Then `deparse()` each statement. **pgpushy's output is consumed by pgschema, a
machine — canonical/pretty form is irrelevant, only validity + correct desired
state.** So deparse output quality is not a concern beyond "parses and means
the same thing."

> ⚠️ **Spike this first (§14, R1):** confirm `pg_query` parse → mutate → deparse
> round-trips representative DDL (inline & table FKs, composite FKs, multi-col
> PKs, `ON DELETE`, quoted/mixed-case idents, comments). Validate by
> re-parsing the deparsed output and comparing fingerprints. If deparse proves
> inadequate for some node, fall back to slicing original text via
> `stmt_location`/`stmt_len` for the *unchanged* statements and only
> deparse/synthesize the mutated ones.

**Synthesis-file granularity** (spec §5.4): emit **one combined document**
reused for every per-schema run (simplest, verified). Per-schema trimming to
the cross-schema closure is a possible large-DB optimization — not v0.x.

---

## 6. Cross-schema ordering (`graph.rs`)

Spec §7. Build a digraph over managed schemas: edge `A → B` when a table in `A`
has a FK referencing a table in `B` (same-schema FKs create no edge — pgschema
handles those, §5.3/PR#156). Process schemas in **reverse-dependency order**
(a schema after every schema it references). Detect cycles (Tarjan/Kahn); a
cross-schema FK cycle is a **hard error naming the schemas**, before any apply
(spec §11.1). Deterministic tie-break (e.g. name order) for reproducible plans.

---

## 7. pgschema provider (`provider/`)

Spec §8.5. Trait that yields a runnable binary:

```rust
trait PgschemaProvider { fn resolve(&self) -> Result<PgschemaBin>; }
struct PgschemaBin { path: Utf8PathBuf, version: Version }
```

- **`byo.rs` (ship first).** Resolve an explicit path (config/flag) or
  `which("pgschema")`. Run `pgschema --help`, parse the `Version:` line
  (`^Version:\s*(\d+\.\d+\.\d+)`), compare with the floor via `semver`.
  **Below floor → hard error** naming found vs required. **Unparseable version
  → warn, proceed** (the line is not a stability contract).
- **`managed.rs` (fast-follow, becomes default).** Resolve version (pinned to
  tested version, config-overridable) → map `(version, os, arch)` to the
  GitHub release asset URL `…/releases/download/v<ver>/pgschema-<ver>-<os>-<arch>`
  → download over HTTPS to `$XDG_CACHE_HOME/pgpushy/pgschema/<ver>/pgschema`
  (atomic: temp file + rename; lock for concurrent runs) → **verify against a
  pgpushy-shipped SHA-256** (const table `{version,platform} → hash`, since
  pgschema ships none) → `chmod +x`. Reuse if cached+verified. Linux/macOS
  only; Windows/air-gapped → BYO.

**Floor constant:** `MIN_PGSCHEMA = "1.12.0"` (the tested version). True
behavioral floor is v1.4.2 — headroom to lower later *with tests*, not the
supported floor. Bump `MIN_PGSCHEMA` as CI tests newer releases.

---

## 8. Invocation, precondition, orchestration

- **`precondition.rs`** — before delegating, open one target connection and
  read `pg_namespace` (or `information_schema.schemata`) to confirm **every
  managed schema exists**; else fail listing all missing ones (spec §6). This
  is pgpushy's only *direct* target access, and it is **read-only**.
- **`pgschema.rs`** — build the argv: `pgschema <plan|apply> --schema <S>
  --file <synth> <connection flags> [--auto-approve for apply]`. pgpushy
  **owns** `--schema` and `--file`; the user cannot set them (spec §8.3).
  Stream pgschema stdout/stderr through (do **not** parse plan output — thin
  wrapper). Write the synthesized doc to a `tempfile` (or a debuggable path
  under a `--keep`/`--out` flag).
- **`run.rs`** — `plan`: synth → precondition → for each schema in order, run
  `pgschema plan`, present each. `apply`: synth → precondition → run full plan
  pass (fail-fast) → for each schema in order, `pgschema apply`. Report which
  schemas were applied on partial failure (spec §10.2 — apply is **not**
  atomic across schemas; say so in output).

---

## 9. Config & CLI

- **`pgpushy.toml`** (TOML, project root, optional). Sections: project
  structure (source root, default schema), `[pgschema]` provider (backend,
  version, path), `[connection]` (host/port/db/user/sslmode, and `password`).
  Precedence **CLI > `PG*`/env > file > default**; default schema `public`.
- **Password warning** (spec §9): when the *effective* password is sourced from
  the file (not overridden by `PGPASSWORD`/`--password`), emit a prominent
  `tracing::warn!` — "password read from pgpushy.toml, which is easily
  committed to version control; prefer PGPASSWORD or --password." Fires on
  actual use, not mere presence.
- **CLI** (`clap` derive): `pgpushy plan`, `pgpushy apply`, plus global
  connection flags (mirroring pgschema names), `--config`, `--source-root`,
  `--default-schema`, `--pgschema-path`, `-v/--verbose`. `--auto-approve` for
  apply. (Future: `pgpushy dump`, spec §13.)

---

## 10. Testing strategy

- **Unit (`pgpushy-core`)** — parse/classify, FK-lift, qualification, name
  generation, graph ordering, cycle detection. **Snapshot the synthesized SQL
  with `insta`** (golden files). Add a **determinism test**: synth twice →
  byte-identical.
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
  6. cross-schema cycle → rejected with a clear error.
  7. absent target schema → precondition fails cleanly (lists the schema).
  8. idempotence: second `apply` is a no-op / empty plans.
  9. BYO version check: below-floor pgschema → hard error; unparseable → warn.

---

## 11. Milestones (phased; each shippable)

- **M0 — Skeleton & spike.** Workspace, justfile, CI, `LICENSE`. **Do the
  pg_query round-trip spike (R1) first** — it de-risks everything.
- **M1 — Single-schema `plan` (BYO).** discover → parse → FK-lift → qualify →
  synth → BYO provider + version check → `pgschema plan --schema <default>`.
  Covers fixtures 1–3, 9.
- **M2 — Multi-schema + `apply`.** qualify-all, graph ordering, cycle
  detection, precondition check, `apply` with fail-fast plan pass. Fixtures
  4–8.
- **M3 — `pgpushy.toml`.** config + precedence + password warning.
- **M4 — UX polish.** error messages, output formatting, `--keep`/`--out`,
  `--auto-approve` ergonomics, docs.
- **M5 — Managed provider (0.x fast-follow, then default).** download/cache/
  verify; SHA-256 table; make managed the default backend.
- **Later (spec §13):** non-table objects (views/functions/types) via general
  ordering or shadow-DB `pg_dump`; `pgpushy dump`; references into unmanaged
  schemas via external plan DB; cross-schema-cycle support; schema-drop.

---

## 12. Output & error conventions

- Human-first stderr via `tracing`; keep pgschema's own output visible
  (passthrough). Non-zero exit on any failure; distinguish precondition/version
  failures (pgpushy) from pgschema failures in the message.
- On partial `apply` failure, clearly list applied vs unapplied schemas
  (spec §10.2). Cross-schema cycle and missing-schema errors must name the
  schemas involved.

---

## 13. Non-obvious pitfalls (learned the hard way)

- **Do not leave objects unqualified** in the combined file — they leak into
  every `--schema` run (verified misattribution). Qualify all, incl. `public`.
- **Do not topologically sort tables** to fix ordering — FK-lift instead; sort
  can't express cycles, lift can.
- **`CREATE SCHEMA` in the synth file is for the plan DB only.** It never
  reaches the target; the target schema must pre-exist (v0.x precondition).
- **External plan DB does not help absent target schemas** — current state is
  always read from the target.
- **Managed-schema set must exclude public unless it has objects** — otherwise
  an empty desired `public` plans a **drop of everything** in the target's
  public schema.
- **pgschema apply is per-schema-transactional** — no cross-schema atomicity;
  order schemas by cross-schema FKs; cross-schema cycles are unsupported.

---

## 14. Risks & open implementation questions

- **R1 (highest) — pg_query deparse fidelity.** The whole synth approach
  assumes parse→mutate→deparse round-trips real DDL. **Spike in M0.** Fallback:
  text-slice unchanged statements, synthesize only mutated ones. Also confirm
  the pg_query crate's bundled PG grammar covers the PG version/features in use
  (PG18 seen in spikes; the crate may trail).
- **R2 — Generated FK constraint names** must match nothing pgschema would name
  differently, or plans churn. Verify our generated names produce empty
  re-plans (idempotence) against pgschema's own naming. Mirror pg_dump's scheme
  and test.
- **R3 — Managed download integrity.** pgschema ships no checksums; we self-pin
  SHA-256. Decide the update process when we bump the pinned version (recompute
  hashes for all four platforms). Consider verifying against GitHub's API
  digest too.
- **R4 — `Other` statements ordering.** v0.x keeps source order for indexes/
  comments/etc. Confirm none of Joe's real schemas rely on cross-file non-FK
  ordering (spec §11.2); if they do, pull M-later general ordering forward.
- **R5 — testcontainers vs. image wrapper** for pgschema in CI, and how to get
  a pgschema binary into CI hermetically (download in `just setup`).

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

-- FK-lifted form (pgschema plan == the correctly-ordered inline plan)
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

-- Cross-schema cycle (UNSUPPORTED — pgpushy must detect & reject before apply):
--   public.customers → billing.accounts AND billing.accounts → public.customers
```

Verified outcomes are catalogued in §1; the memory file
`pgpushy-project.md` (agent memory) holds the same facts if available.
