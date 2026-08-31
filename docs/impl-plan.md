# pgpushy Implementation Plan

**Status:** Build guidance (non-normative)
**Date:** 2026-08-31
**Companion to:** [`docs/spec.md`](./spec.md) v0.5 (normative)

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
- Flags we pass through: `--host --port --db --user --sslmode`,
  `--lock-timeout` (apply), `--plan-host/--plan-db/--plan-user/--plan-port/
  --plan-sslmode` (external plan DB), `--auto-approve` (apply),
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
  the uniform safe rule.) The stripping applies to **identifiers only** — see
  the literal case below, which is why documents are per-schema.
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
  the target). → 0.1 makes "schemas pre-exist" a hard precondition.
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

**Verified while spiking sequences and grants (2026-08-16)**
- **`--lock-timeout` is apply-only.** `plan` rejects it: `unknown flag`.
- **pgschema reconciles privileges by default.** A desired state that mentions
  no grants reads as "there should be none", and pgschema plans `REVOKE` for
  every grant on the target, plus `ALTER DEFAULT PRIVILEGES … REVOKE`. This is
  the §8.4 hazard in a different costume, and is why pgpushy writes a
  `.pgschemaignore` until grants are a managed kind.
- **`.pgschemaignore` is TOML, auto-loaded from pgschema's working directory**,
  with no flag to point at it. Sections include `[privileges]` and
  `[default_privileges]`; `patterns = ["*"]` suppresses a kind entirely.
  Because it is ambient, pgpushy runs pgschema in a directory it owns.
- **Grants need their roles to exist in the plan database.** The embedded one
  fails with `role "x" does not exist`. Roles are **cluster-wide**, so an
  external plan database on the *same cluster* as the target has them — which
  is what makes managed grants workable at all.
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
  scratch schema whose name is unpredictable. → **per-schema documents carrying
  the closure** (spec §5.4).
- **pgschema's `dump` order** is TYPE → DOMAIN → SEQUENCE → TABLE. It is a
  convention rather than a guarantee — a domain over a domain, or a domain
  default calling `nextval`, inverts a pair of it — which is why spec §5.1
  sorts category 2 topologically instead of adopting this order.
- **`ALTER DEFAULT PRIVILEGES` requires `IN SCHEMA`**, so every such statement
  names its own schema — which answers the "how does a grant attribute to a
  schema" question the grants work was blocked on.
- Grants and ADP **attribute correctly per `--schema`**: a run for one schema
  plans only that schema's privileges.

**Verified while settling the per-schema document (2026-08-18)**
- **pgschema's `dump` emits every table constraint inline** — primary keys,
  uniques, checks **and foreign keys** — and only `CREATE INDEX` stands alone.
  `pg_dump` does the opposite for foreign keys, emitting standalone
  `ALTER TABLE … ADD CONSTRAINT`. Both shapes therefore arrive in real source
  trees, which is why spec §4.3 accepts the standalone foreign-key form, and
  pgpushy's own output is the `pg_dump` one (§5.3).
- **`dump` is lossy for a standalone sequence used as a column default.** It
  renders `DEFAULT nextval('invoice_no')` as `SERIAL`, drops the
  `CREATE SEQUENCE` entirely, and feeding its own dump back as desired state
  against the database it came from plans `DROP SEQUENCE invoice_no CASCADE`
  plus a new owned sequence. pgschema's `plan` path handles the same shape
  faithfully — only `dump` is lossy. This is why spec §4.3 rejects
  `CREATE SEQUENCE … OWNED BY`, and it is a caveat for anyone bootstrapping a
  source tree: `pg_dump` is the faithful starting point, `pgschema dump` is not
  where sequences are involved.
- **The literal-qualification failure, and its fix, reproduced in both
  directions.** `CREATE SEQUENCE w1.invoice_no` with
  `DEFAULT nextval('w1.invoice_no')`, run as `--schema w1`, fails
  `relation "w1.invoice_no" does not exist` — the identifier's qualifier was
  stripped into the scratch schema while the literal still names the real one.
  Spelling it `nextval('invoice_no')` gives `No changes detected.` against a
  target built from the same definition.
- **`pg_dump` sets `search_path` to `''` and fully qualifies inside string
  literals.** So spec §4.3's requirement that a name inside a literal carry its
  schema costs an imported tree nothing.
- **Postgres 18, on what can back a foreign-key reference.** A standalone
  `CREATE UNIQUE INDEX` and a standalone `ADD CONSTRAINT … UNIQUE` are both
  accepted as the referent's uniqueness. A **partial** unique index is not:
  `there is no unique constraint matching given keys for referenced table`. A
  `NULLS NOT DISTINCT` unique index is accepted. This is why a closure member
  brings **all** its indexes (spec §5.4) — the selection rule is exact, and a
  wrong answer fails silently in the unbuildable direction.
- **Postgres 18, two constraint details.** An inline table constraint can carry
  an explicit name, so spec §4.3 loses nothing by requiring `CHECK`, `UNIQUE`,
  `PRIMARY KEY` and `EXCLUDE` to be written inline. And `NOT VALID` is
  ALTER-only *in effect*: Postgres accepts it in `CREATE TABLE` syntactically
  and then silently ignores it (`convalidated = t`).
- **The Postgres driver models three `sslmode`s, not five.**
  `tokio-postgres` 0.7.18 — which the sync `postgres` crate wraps — accepts
  only `disable | prefer | require`; anything else is
  `InvalidValue("sslmode")`, so the two verifying modes cannot be delegated to
  it at all (spec §6.4). With rustls the *default* is verify-full behavior, so
  it is libpq's `require` and `prefer` that need a permissive verifier through
  rustls' dangerous API, not the verifying modes that need extra work.
  `tokio-postgres-rustls` 0.14 resolves against the current tree, and rustls is
  already there via `ureq`.

**Verified while building category 2 (2026-08-18)**
- **pgschema's `dump` is dependency-ordered, not fixed-order.** A composite
  type over a domain dumps the domain first, even though the type/domain
  category order would put the type first. Same conclusion spec §5.1 reaches:
  sort category 2, do not hardcode an order among the kinds.
- **A standalone sequence nothing defaults to is managed correctly.** Applied
  with its parameters, and the re-plan is empty.
- **A default calling `nextval` is applied as `SERIAL`.** `CREATE SEQUENCE
  m9.ticket_no` plus `t int DEFAULT nextval('ticket_no')` applies "successfully"
  and leaves `m9.people_t_seq` (deptype `a`) on the target — `ticket_no` is
  never created, and the re-plan shows `sequences: 1 to add, 1 to drop` forever.
  A **silent** non-convergence, which is why spec §4.3 rejects the shape.
- **A domain default calling `nextval` fails outright**: `relation "ticket_no"
  does not exist`, because pgschema applies domains before sequences. pgpushy's
  own category-2 sort cannot help — it orders the document pgschema *reads*,
  not the DDL pgschema *applies*.
- **Measure by applying, not by seeding with `psql`.** The first measurement of
  the sequence case used a hand-built target and concluded it worked. pgschema
  reads that target correctly; it just never builds one like it. Only an
  apply-then-replan measures what a user will hit.
- **libpg_query does not mark every built-in with `pg_catalog`.** `int` arrives
  as `pg_catalog.int4`, `text` arrives as plain `text`. So "not `pg_catalog`"
  does not mean "user-defined", and treating it that way rewrites `text` into
  `public.text`. Type references are resolved by matching what the source tree
  defines, after every file is parsed — which is also the only point at which
  the answer is knowable.

**Verified while answering adopter questions (2026-08-19)**
- **pgschema has no `hostaddr`, and ignores `PGHOSTADDR`.** Its only host input
  is `--host` (env `PGHOST`). Verified: `--host bogus.invalid` with
  `PGHOSTADDR=127.0.0.1` fails identically to the control —
  `hostname resolving error: lookup bogus.invalid`. So separating the name used
  for TLS verification from the address actually dialled — the normal reason to
  want `hostaddr`, and the usual need when reaching RDS through a tunnel or a
  pinned IP — is not expressible end to end, and cannot be fixed in pgpushy
  alone. `tokio-postgres` *does* support `hostaddr`, so pgpushy's own
  inspection could honour it, but pgschema would still resolve the name, and
  §6.3 forbids letting the two disagree. `/etc/hosts` is the only lever today.
- **pgschema classifies plan steps only as `create`, `drop` or `alter`.** A
  plan's JSON carries exactly `operation`, `path` and `type` per step, with no
  destructiveness flag, no risk level and no summary; the human output uses the
  same three words. A narrowing `ALTER COLUMN … TYPE varchar(10)` is therefore
  indistinguishable from the widening change that is safe. Spec §14 records
  what follows from that.
- **`pg_query`'s own node walk is incomplete.** `NodeEnum::nodes()` is
  hand-written, not generated, and does not descend into every field — for a
  `CreateStmt` it yields the statement and its `RangeVar` and stops, never
  entering `table_elts`, which is exactly where a column default lives. Any
  search over an AST must either walk the typed tree by hand or, as
  `literal.rs` does, run over the AST serialized to JSON, which covers every
  field by construction. `pg_query`'s protobuf types derive `Serialize` but
  **not** `Deserialize`, so a JSON round-trip cannot be used to *mutate* — the
  rewrite has to be a typed walk, which is why `literal.rs` checks afterwards
  that nothing it should have reached survived.

**pgschema's embedded plan database is a network dependency (2026-08-19)**
- It downloads a Postgres tarball at runtime and caches it in
  `~/.embedded-postgres-go/`, e.g.
  `embedded-postgres-binaries-linux-amd64-18.0.0.txz`, chosen to match the
  target's version. A developer machine only pays for this once, which is why
  it is invisible locally.
- **CI pays on every run, and it can fail.** Observed: an integration leg
  failing 16 tests in three seconds with `failed to start embedded PostgreSQL:
  no version found matching 18.0.0`, while the other matrix leg — same commit,
  same runner image, same target — passed. Nothing in pgpushy was involved; the
  commit was documentation only.
- A failure looks alarming rather than obvious: it surfaces as a wall of failed
  pgpushy tests, not as a step that could not reach the network.
- The remedy pgpushy already has is `[env.*.plan_db]` (spec §10.4): pointing the
  plan database at CI's own Postgres service means pgschema never fetches the
  embedded one. Not free, though — an external plan database accumulates state
  across runs (above), so tests sharing one need it cleaned or namespaced, and
  today only `plans_through_an_external_plan_database` exercises that path.

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
  `msrv`, `ci`, `bump <level>` (see [RELEASING.md](RELEASING.md)),
  `install-cli`, `package`, `publish`.
- `README.md`, `CHANGELOG.md`, `docs/` for prose. Optional `assets/` mascot.
- Integration tests **skip without an env URL** (snowdrop uses
  `SNOWDROP_TEST_PG_URL`); mirror with `PGPUSHY_TEST_PG_URL` and
  `PGPUSHY_TEST_PGSCHEMA` (path to a pgschema binary).

**Crate dependencies**

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
- `tokio-postgres-rustls` — TLS for that inspection. pgpushy interprets
  `sslmode` itself (spec §6.4) and hands the sync client the connector the mode
  calls for; `rustls` is already in the tree under `ureq`.
- `which` — PATH lookup (BYO). `tempfile` — the synthesized documents and the
  plans.
- `ureq` (rustls, no default features) + `sha2` — the managed provider's
  download and integrity check.
- `anyhow` — binary-level error context.

*Dev:* `assert_cmd` + `predicates` (CLI), `postgres` and `tempfile`
(integration), gated on env vars.

**Dropped from the original plan.** `tracing` + `tracing-subscriber`: pgpushy's
output is hand-formatted and user-facing rather than logs, and a subscriber
would add a layer over text that is already exactly what we want. What that
entry was really after is debuggability, which `--verbose` serves directly —
it prints the pgschema command line and where the synthesized documents went,
which is the only extra detail there is to want. `camino` was never needed;
`insta` was listed for snapshotting the synthesized SQL, but explicit golden
strings in the tests turned out to read better, since the interesting part is
*which* line changed rather than that something did. `walkdir` is a handful of
lines of `read_dir` given that discovery must not follow symlinked directories
anyway. `testcontainers` is not used either: integration tests take a database
URL and a binary path from the environment, and CI supplies both (§10).

---

## 3. Workspace & module layout

Two crates, following your "pure core + thin edge" philosophy — keep all
deterministic transformation in a library with no IO, push subprocess/network/
DB to the binary.

```
pgpushy/
  Cargo.toml                # [workspace] resolver=3, members, workspace.package
  justfile  README.md  CHANGELOG.md  LICENSE
  docs/spec.md  docs/impl-plan.md  docs/RELEASING.md
  docs/migrating-from-pgschema.md   assets/pgpushy.jpg  docs/RELEASING.md
  pgpushy-core/             # pure, deterministic, no IO — the heart
    src/lib.rs              # analyze(): the whole offline pipeline
    src/model.rs            # SchemaName, QualifiedName, Table, ForeignKey, Objects
    src/parse.rs            # pg_query parse → classify; allow-list enforcement (spec §4.2-4.3)
    src/resolve.rs          # schema assignment; managed-schema set derive/verify (spec §4.4)
    src/validate.rs         # duplicates, unresolvable referents, cross-schema refs (spec §4.5)
    src/synth.rs            # closure + FK-lift + qualify + 6-category emission,
                            #   one document per managed schema (spec §5)
    src/graph.rs            # cross-schema FK graph, topo order, cycle detection (spec §7)
    src/literal.rs          # object names inside string literals (spec §4.3, §5.4)
    src/seed.rs             # seed parse + allow-list + model checks (spec §4.6)
    src/error.rs            # diagnostics carrying file + line
  pgpushy/                  # the `pgpushy` binary — IO shell
    src/main.rs             # clap dispatch
    src/cli.rs              # arg/flag definitions
    src/config.rs           # pgpushy.toml load, named environments (spec §10)
    src/discovery.rs        # walk source tree, apply excludes, deterministic order (spec §4.1)
    src/conn.rs             # connection resolution → driver config + pgschema flags (spec §6.3-6.4)
    src/tls.rs              # what each sslmode means on the wire (spec §6.4)
    src/inspect.rs          # read-only target inspection: schemas, cross-schema FKs, identity (spec §6)
    src/provider/mod.rs     # trait PgschemaProvider
    src/provider/byo.rs     # PATH/explicit path + version check
    src/provider/managed.rs # download+cache+verify
    src/pgschema.rs         # build/run pgschema commands; own the working directory
    src/plan_file.rs        # read pgschema's --output-json plans
    src/hazard.rs           # cross-schema FK removal check, over those plans (spec §6.2)
    src/approve.rs          # plan presentation + single database-level prompt (spec §8.6)
    src/init.rs             # `pgpushy init`
    src/outdir.rs           # --out, a directory pgpushy owns (spec §8.7)
    src/output.rs           # verbosity and color, resolved once
    src/report.rs           # user-facing output, routed through one place
    src/run.rs              # orchestrates validate/plan/apply
    src/seeds.rs            # seed execution: per-file transaction + probe (spec §8.8)
    src/generate.rs         # `pgpushy generate` and --check (spec §4.7)
    tests/                  # integration (real pgschema + Postgres)
```

Binary target name **`pgpushy`**; library crate **`pgpushy-core`**.

Note the split: **discovery lives in the binary** (it touches the filesystem),
while everything from parsing onward is pure. `pgpushy-core` takes
`Vec<(RelPath, String)>` — path plus contents — and returns either an
`Analysis` (a document per managed schema, plus the schema order) or a list of
diagnostics. That makes the entire offline pipeline (spec §3 stages 2–6)
unit-testable from string literals with no fixtures on disk.

---

## 4. Data model (`pgpushy-core::model`)

Exactly the allow-list of spec §4.3 and nothing else. Grow per spec §14.

```rust
struct SchemaName(String);                    // always resolved, never empty
struct QualifiedName { schema: SchemaName, name: String }

// A shape for the allow-list rather than the literal type: `model.rs` holds
// an `Objects` with one typed vector per kind.
enum Statement {                              // the allow-list, and nothing else
    CreateSchema(SchemaName),
    CreateType(TypeDef),                      // enum, composite, range
    CreateDomain(DomainDef),
    CreateSequence(SequenceDef),              // standalone only; OWNED BY rejected
    CreateTable(Table),                       // FKs pulled out into ForeignKey
    CreateIndex(Index),
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

- **Every object and every referent carries an explicit schema**, resolved from
  a qualifier or from the default schema — including the names that appear
  inside string literals, which spec §4.3 requires the author to qualify rather
  than letting pgpushy infer.
- **Every statement carries an `Origin`.** Spec §4.5 and §4.3 diagnostics name
  file and line; a statement that cannot say where it came from cannot produce
  a compliant error message. Thread `Origin` from the start rather than
  retrofitting it.
- **There is no `Other` variant.** Spec §4.3 rejects everything outside the
  allow-list, so an unmatched statement produces an error, never a pass-through
  bucket. This simplifies synthesis considerably: no unmodelled text to place,
  order, or fail to qualify.
- **There is no standalone non-foreign-key constraint.** Spec §4.3 rejects
  every `ALTER` form but the foreign-key one, so a `CHECK`, `UNIQUE`,
  `PRIMARY KEY` or `EXCLUDE` constraint only ever arrives inline in its
  `CREATE TABLE` and travels inside `Table.ast`. That is also what makes
  category 4 `CREATE INDEX` alone, and therefore internally order-free.
- **Every object records what it references at execution time**, because §5's
  closure is a traversal over those edges. A foreign key's referent, a column's
  type or domain, a default's sequence.

---

## 5. Synthesis algorithm (`synth.rs`) — the heart

Produce **one document per managed schema** (spec §5.4). `synthesize` takes the
target schema as a parameter and is called once per managed schema; `Analysis`
carries a document per schema rather than a single string, `run.rs` writes one
tempfile per schema, and each pgschema invocation is handed the matching one.

The reason this is not an implementation detail is a single pgschema behavior:
it strips a schema qualifier from an **identifier** but cannot strip one from
inside a **string literal**, and a `nextval` in a column default is a string
literal. A reference to an object in `S` must therefore be spelled
*unqualified* in `S`'s own document and *qualified* in every other — two
requirements that contradict, so no single document is correct for every run.
§1 has the reproduction in both directions.

### What goes into the document for schema `S`

1. Every object assigned to `S`, in all six categories.
2. The **closure**: for every statement emitted, the objects it references at
   execution time — a foreign key's referent, a column's type or domain, a
   default's sequence — repeated until nothing new is added. Spec §4.5
   guarantees every such referent exists, so a missing one is a bug in
   `validate.rs`, not a case to handle here.
3. A closure member contributes **categories 1 through 4 only**: its schema
   declaration, its type/domain/sequence, its `CREATE TABLE`, and its indexes.
   Never a foreign key, never a comment.

Two rules that read like details and are not:

- **A closure member brings all its indexes, not a chosen subset.** A foreign
  key may reference a column set whose uniqueness is backed by a standalone
  unique index rather than an inline constraint, and Postgres accepts that
  (§1). Selecting only the indexes that *could* back a reference means encoding
  an exact rule — partial excluded, `NULLS NOT DISTINCT` included — whose wrong
  answer fails silently in the unbuildable direction.
- **A closure member brings none of its foreign keys**, which is what bounds
  the traversal. `S → X.t → Y.u` stops at `X.t`, because `Y.u` is needed only
  by a constraint `X`'s own document emits.

Since spec §4.5 caps cross-schema references at foreign keys, the closure stays
shallow in 0.1: `S`'s foreign-key referents, plus whatever those need within
their *own* schema — a column's domain, a default's sequence — because every
onward reference from a closure member is same-schema by construction. Write it
as a worklist over reference edges anyway; the spec specifies it that way, so
widening §12.6 later adds edge kinds rather than rewriting the traversal.

### Steps

1. **Bucket** every statement into the six categories of spec §5.1: schemas →
   types/domains/sequences → tables → indexes → foreign keys → comments. The
   category boundaries are what make the output executable; do not intermix.
2. **Sort category 2 topologically** by creation-time dependency, ties broken
   by `(schema, name)`. A domain over a domain, a composite type with a
   domain-typed field, and a domain default calling `nextval` each invert a
   different pair of the three kinds, so no fixed order works. A cycle is
   impossible — Postgres will not create one — but report it rather than
   emitting an arbitrary order if the sort finds one.
3. **FK-lift** (spec §5.3): move every FK — inline column constraint
   (`Constraint` with contype `CONSTR_FOREIGN`), table-level FK, and standalone
   `ALTER … ADD CONSTRAINT` — into category 5. Preserve the constraint
   definition **exactly** (no added `NOT VALID`, spec §5.5).
4. **Constraint names** (spec §5.3): keep the author's name if there is one;
   otherwise emit **no name** and let Postgres generate it in the plan DB. Do
   **not** synthesize names — a synthesized name differs from the one the
   target already holds and churns the plan forever.
5. **Qualify every identifier** (spec §5.4) — every emitted object, every FK
   referent, every index target and comment target, including `public` and
   including `S`'s own objects. Emit `CREATE SCHEMA IF NOT EXISTS <s>` for
   every schema the document names, its own and each closure member's (runs
   only in the plan DB — never the target; spec §5.2/§6.1).
6. **De-qualify literals naming `S`'s own objects** (spec §5.4). Inside a
   string literal, a name in `S` is emitted **without** its qualifier and a
   name anywhere else **with** it. This looks like the opposite of step 5 and
   is the same rule: match what pgschema does to identifiers, in the one place
   it cannot do it itself.
7. **Deterministic order** (spec §11.3): stable sort within each category by
   `(schema, name)`, except category 2, whose topological order is itself a
   deterministic function of the content. Every document must be
   byte-identical across runs and platforms, and must not depend on filesystem
   enumeration order.

**Implementation approach — AST-mutate + deparse.** `pg_query` gives a protobuf
AST and a `deparse()`. The transforms are targeted edits:
- FK-lift: remove FK `Constraint` nodes from `CreateStmt`; build
  `AlterTableStmt { cmds: [AT_AddConstraint(fk)] }` nodes for them, with
  `conname` left empty for author-unnamed constraints.
- Qualify: set `schemaname` on each relation `RangeVar` (the table, the index's
  table, the FK's `pktable`, the comment's object) and on the type name of a
  domain or a column.
- De-qualify a literal: the qualifier lives in an `A_Const` string inside the
  expression — a `FuncCall` argument for `nextval`, usually wrapped in a
  `TypeCast` to `regclass`. `parse.rs` already has to find these to enforce
  spec §4.3's rule that they carry a schema, so record their locations there
  and let synthesis rewrite rather than search twice.

Then `deparse()` each statement. **pgpushy's output is consumed by pgschema, a
machine — canonical/pretty form is irrelevant, only validity + correct desired
state.** So deparse output quality is not a concern beyond "parses and means
the same thing."

> ✅ **R1 is answered** — see `pgpushy-core/tests/spike_pg_query.rs` and §14.
> AST-mutate + deparse works on all 13 representative fixtures; the text-slice
> fallback is not needed.

---

## 6. Cross-schema ordering (`graph.rs`)

Spec §7. Build a digraph over managed schemas: edge `A → B` when a table in `A`
has a FK referencing a table in `B` (same-schema FKs create no edge — pgschema
handles those, §5.3/PR#156). Process schemas in **reverse-dependency order**
(a schema after every schema it references). Tie-break by schema name so the
order is reproducible.

A foreign key is the only edge kind because it is the only reference spec §4.5
lets cross a schema boundary. Widening that (§12.6) adds edge kinds here; it
does not change the shape of the graph.

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
struct PgschemaBin { path: PathBuf, version: Option<Version> }
```

- **`byo.rs`.** Resolve an explicit path (config/flag) or `which("pgschema")`.
  Run `pgschema --help`, parse the `Version:` line
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
supported floor. Bump `MIN_PGSCHEMA` as CI tests newer releases; the CI matrix
and this constant are the same decision written twice (§10).

---

## 8. Connection, inspection, orchestration

### `conn.rs` — one resolution, forwarded (spec §6.3, §6.4)

Resolve the named environment (spec §10.2) into a single parameter set, then
produce two things from it: a libpq connection string for pgpushy's own driver,
and an explicit flag list for pgschema. **pgschema must never resolve anything
itself** — pass `--host --port --db --user --sslmode` explicitly, supply the
password through the child's environment, and strip `PG*` and
`PGSCHEMA_PLAN_*` from that environment so nothing ambient reaches it.

This is how spec §6.3's identity guarantee is delivered: not by comparing two
resolutions afterwards, but by ensuring there is only one. The same reasoning
is why the process environment contributes nothing but `PGPASSWORD` (and
`PGPUSHY_PLAN_PASSWORD` for the plan database) — an ambient `PGHOST` that
redirected `--env prod` would defeat the point of naming it.

`PGSERVICE` and `PGSERVICEFILE` are **refused**, not ignored: pgpushy cannot
interpret them, and dropping one silently would mean connecting somewhere the
operator did not name.

**`sslmode` is pgpushy's to interpret**, across all five libpq modes (spec
§6.4). The driver models three and hard-errors on the other two (§1), so
delegating would mean refusing a connection string libpq accepts — or worse,
connecting in plaintext under a mode chosen for verification. Map the mode to a
TLS connector: `disable` → no TLS; `verify-full` → rustls over the platform
roots, which is simply rustls' default; `verify-ca` → the same with the
hostname check dropped; `require` and `prefer` → chain verification dropped as
well. Note the inversion that costs the work: everything *below* `verify-full`
needs a custom verifier through rustls' dangerous API, not the verifying modes.
An unrecognized mode is a hard error naming the value and all five accepted
ones.

### `inspect.rs` — one read-only round trip (spec §6)

A single connection answering three questions:

1. **Which managed schemas exist?** `SELECT nspname FROM pg_namespace` filtered
   to the managed set; report *all* missing ones (spec §6.1).
2. **What cross-schema FKs does the target hold?** Join `pg_constraint`
   (`contype = 'f'`) to `pg_class`/`pg_namespace` on both sides, keeping rows
   where the two namespaces differ and both are managed, and carrying the
   referenced columns and the unique constraint each depends on. This is
   *collection only*; the §6.2 decision is `hazard.rs`'s, over the plans.
3. **What database is this?** `current_database()`, `inet_server_addr()`,
   `inet_server_port()`, and `system_identifier` from `pg_control_system()`,
   for the identity line in output (spec §6.3).

Everything here is `SELECT`. Assert that in review: this module is the only
direct target access, and spec §6 hangs the "pgpushy issues no DDL" guarantee
on it.

### `hazard.rs` — the cross-schema removal check (spec §6.2)

Takes the target's cross-schema foreign keys from `inspect.rs`, the plans from
the plan pass, and the schema order. For each foreign key whose referencing
schema is ordered *after* the referenced one, look in the referenced schema's
plan for a `drop` step whose `path` names the column that foreign key points at
or the unique constraint it depends on. A dropped *table* is not a hazard —
pgschema CASCADEs it — and flagging one would block a legitimate change.

It reads pgschema's own conclusions rather than diffing anything, which is why
G3 survives the check: pgschema decided what to drop; pgpushy noticed that one
of those drops is load-bearing for a constraint in another schema.

### `pgschema.rs`

Build the argv:

```
pgschema plan  --schema <S> <connection flags> --file <S's document>
               --output-human stdout --output-json <plan.json>
pgschema apply --schema <S> <connection flags> --plan <plan.json>
               --auto-approve [--lock-timeout <d>]
```

pgpushy owns `--schema`, `--file`, `--plan` and `--auto-approve` (spec §8.3);
`--lock-timeout` is apply-only, because pgschema's `plan` rejects it (§1).
Stream pgschema's stdout/stderr through untouched — pgpushy does not reformat
plans (G3) — and pass `--no-color` when pgpushy is not colouring, since
pgschema colours unconditionally otherwise.

pgpushy also owns the **working directory**: pgschema auto-loads a
`.pgschemaignore` from wherever it runs, so the operator's shell directory
would otherwise be ambient input to what gets reconciled. Run it in a directory
pgpushy created, and write the `[privileges]` / `[default_privileges]`
suppression there (spec §8.4).

### `approve.rs` (spec §8.6)

Full plan pass → present all schemas' plans as one unit with a change summary
→ call out destructive changes and any schema reconciling to an empty desired
state → run the §6.2 check → state that apply is not atomic across schemas →
prompt once. Decline touches nothing. `--auto-approve` skips the prompt; a
non-TTY stdin without `--auto-approve` is a failure, not an implicit yes. Every
"N destructive" comes from a step pgschema labelled `drop`, never from pgpushy
comparing anything.

### `run.rs`

- `validate`: offline pipeline only, no connection. Report managed set, counts,
  exclusions, apply order. Fail on any §4.3/§4.5/§7 condition.
- `plan`: offline pipeline → inspect → per-schema `pgschema plan`. A cross-schema
  cycle is reported but does **not** suppress the plans; exit non-zero.
- `apply`: offline pipeline (cycle is fatal here) → inspect → plan pass →
  §6.2 check → approval → per-schema `pgschema apply --plan` in order. **Stop at
  the first failure**; report applied / failed / not-attempted, and say the
  applied ones are not rolled back (spec §9).

---

## 9. Config & CLI

- **`pgpushy.toml` is required** (spec §10.1), read from the working directory
  only and never searched for in parent directories; `--config <path>` names
  one anywhere, and relative paths inside it resolve against *its* directory.
  When no file is found, print the minimum a working one contains rather than
  falling back to defaults — a tool whose source root defaulted to the working
  directory would, run from the wrong place, treat a fragment of the tree as
  the whole desired state, and everything outside that fragment is then
  scheduled for deletion.
- **Project structure is not settable from the command line.** `source_root`,
  `default_schema`, `managed_schemas` and `exclude` live in the file and
  nowhere else, because each is a way to change what gets reconciled. There is
  therefore no precedence chain to get wrong: for anything about the project,
  the file is the only source.
- **Named environments** (spec §10.2): `[env.<name>]` blocks with `db` and
  `user` required, `host`/`port`/`sslmode` defaulting to
  `localhost`/`5432`/`prefer`, and optional `password`, `lock_timeout` and
  `[env.<name>.plan_db]`. `--env` is required for `plan` and `apply` **even
  when only one environment is defined**, and rejected by `validate`, which
  connects to nothing.
- **`PG*` does not override a named environment's target.** `PGPASSWORD` is the
  single exception and supplies only the password; the plan database's is
  `PGPUSHY_PLAN_PASSWORD`, a separate variable for separate credentials.
- **`managed_schemas`** (spec §4.4): when present it is authoritative — a
  mentioned-but-unlisted schema is an error naming the file and line that
  enlisted it; a listed-but-unmentioned schema is managed **and empty**, which
  is destructive and must be called out in the plan presentation.
- **`exclude`** (spec §4.1): globs via `globset`, matched against source-root-
  relative paths, applied during discovery so excluded files are never parsed.
  Report the count each pattern matched.
- **Unknown keys are rejected** (`deny_unknown_fields` throughout). A mistyped
  key is invisible from behavior — pgpushy would act as though the setting were
  absent — so silence is the one response that cannot be recovered from.
- **Password warning** (spec §10.2): when the *effective* password came from
  the file rather than `PGPASSWORD`, print a prominent warning to stderr saying
  which file it was read from and what to do instead. Plain `eprintln!` from
  `report.rs` — there is no logging framework (§2) — and it must never echo the
  password.
- **`seed_root` and `[[generate]]`** (spec §4.6, §4.7): both are project
  structure, so both live in the file and nowhere else. `seed_root` resolves
  like `source_root`; a nested seed root is excluded from desired-state
  discovery and reported like an exclusion. Each `[[generate]]` entry is an
  `output` path (must resolve under one of the two roots) and a `command`
  argv vector, run with no shell and the config file's directory as cwd.
- **CLI** (`clap` derive): `pgpushy init | validate | plan | apply |
  generate`. Global: `--config`, `--verbose`, `--no-color`. `--out <dir>` on
  all three of `validate`, `plan` and `apply`; `--env` and `--pgschema-path`
  on `plan` and `apply`; `--auto-approve` and `--lock-timeout` on `apply`
  alone; `--check` on `generate` alone, which takes no `--env` and connects to
  nothing. Nothing else — everything a flag could otherwise set is either
  project structure or the target. (Future: `pgpushy dump`, spec §14.)

---

## 10. Testing strategy

- **Unit (`pgpushy-core`)** — parse/classify, allow-list rejection, schema
  resolution, duplicate detection, unresolvable-referent detection, FK-lift,
  qualification, literal de-qualification, closure construction, category
  bucketing and the category-2 sort, graph ordering, cycle detection. Because
  core takes `(path, contents)` pairs, every one of these is a string-literal
  test. Assert the synthesized SQL against **explicit golden strings** rather
  than a snapshot library, so a diff shows *which* line changed. Add a
  **determinism test**: synth twice → byte-identical, and synth with the input
  file list shuffled → byte-identical.
- **CLI (`assert_cmd`)** — `pgpushy validate` end-to-end on fixture trees, with
  no database anywhere. This covers most of the spec's diagnostics cheaply.
- **Integration (`pgpushy/tests`)** — against a **real pgschema + Postgres**,
  resolved from `PGPUSHY_TEST_PG_URL` and `PGPUSHY_TEST_PGSCHEMA`, skipping
  (not failing) when either is absent, mirroring snowdrop. CI supplies both:
  a `postgres:<ver>-alpine` service container and the pgschema release asset
  fetched with `curl` in a step. `PGPUSHY_TEST_DOWNLOAD=1` additionally opts
  into the managed provider's real download, which CI sets only in that job so
  an ordinary `cargo test` never pulls ~19 MB from GitHub.
  The pgschema version in that matrix **is** the supported floor (spec §13):
  raising the matrix and raising `MIN_PGSCHEMA` are one action.
- **Seeds** (spec §4.6, §8.8) — the static rules are string-literal core
  tests: each allow-list rejection with its remedy — the data-modifying CTE,
  the `SELECT` over a table, the qualified user-function call, the WHERE-less
  `DO UPDATE`, the unqualified table, the implicit column list, the
  `GENERATED ALWAYS` column, the unmodeled table — plus the column and
  conflict-target checks against the model and the cross-file `DO UPDATE`
  collision warning. The probe is integration-only,
  against live Postgres: a well-formed seed applies once and re-applies as a
  no-op with the probe passing; a volatile seed (`random()` in a values list,
  which passes every static check) must roll back leaving zero rows; a
  `DO UPDATE` seed with the guard converges; the affected-count report matches
  what landed; and an empty schema plan with seeds present still prompts and
  still seeds. `generate` tests are CLI-level: marker written, overwrite of an
  unmarked file refused, `--check` fails on a stale output and passes after
  regeneration, argv commands get no shell.
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
  12. disallowed statement (a `CREATE VIEW`, an `INSERT`, an `ALTER TABLE …
      ADD CONSTRAINT … CHECK`) → rejected with file and line, no connection
      attempted.
  13. **literal de-qualification** (spec §5.4): a sequence in `S` referenced by
      a default in `S` appears unqualified in `S.sql` and qualified in every
      other document; both plans converge and re-plan empty.
  14. **closure contents** (spec §5.4): a schema whose FK points into `S` gets
      `S`'s table and *all* its indexes in its document, and none of `S`'s
      foreign keys or comments. Include the standalone-unique-index referent,
      which is the case that fails without the indexes.
  15. **`--out` as a directory** (spec §8.7): one `<schema>.sql` per managed
      schema, a hostile schema name percent-encoded, a foreign file in the
      directory refused by name, and a stale generated document pruned.
  16. **rejections added in 0.1**: `CREATE SEQUENCE … OWNED BY`,
      `CREATE TABLE … (LIKE t)`, an unqualified `nextval('s')`, and a
      cross-schema domain reference — each naming file, line and remedy.
  17. **`sslmode`**: all five modes resolve, and an unrecognized one is an
      error naming the value and listing the five.

---

## 11. Milestones (phased; each shippable)

- **M0 — Skeleton & spike. ✅ Done.** Workspace, justfile, CI. The pg_query
  round-trip spike (R1) came first — it de-risked everything. R1 also covers
  the unnamed-FK naming question (§14).
- **M1 — `pgpushy validate`. ✅ Done.** discover → parse → allow-list → resolve
  → duplicate/referent/collision checks → graph → synth, plus the CLI. **No
  database and no pgschema binary**, so it is fully testable in CI from day one
  and exercises every line of `pgpushy-core`. Fixtures 12 and the offline half
  of 6.
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
- **M7 — Per-schema documents and the closure (spec §5.4, §5.5, §8.7).
  ✅ Done.** `synthesize` takes a target schema; `Analysis` carries a document
  per schema; `run.rs` writes one tempfile per schema and hands each pgschema
  run the matching one. `--out` is a **directory pgpushy owns**: it creates it,
  refuses it by name if it holds a file without the generated marker, prunes
  its own stale documents, and percent-encodes bytes outside `[A-Za-z0-9_-]` in
  the schema name so a legal-but-hostile name cannot escape the directory.
- **M8 — Types, domains and standalone sequences (spec §4.3, §5.1). ✅ Done.**
  New model variants and `parse.rs` arms for `CREATE TYPE` (enum, composite,
  range), `CREATE DOMAIN` and `CREATE SEQUENCE`; the topological sort within
  category 2; rejection of `CREATE SEQUENCE … OWNED BY`. Their names qualify
  exactly as tables do. It needed M7 first: a sequence in a column default is
  the reference that must be spelled differently in each schema's document.
- **M9 — `ALTER` rejection, and `CREATE TABLE … (LIKE t)` (spec §4.3, §5.1).
  ✅ Done.** `classify_alter_table` keeps only the foreign-key form, so the
  model holds no standalone non-foreign-key constraint — which is what makes
  category 4 `CREATE INDEX` alone and therefore internally order-free. The
  diagnostic shows the inline form to write instead — an inline constraint can
  carry an explicit name (§1), so nothing is lost but the spelling.

  `classify_create_table` guards a `TableLikeClause` beside `inh_relations`,
  `partspec` and `of_typename`; it arrives inside `table_elts` rather than in a
  clause of its own.
- **M14 — Seed files (spec §4.6, §8.8, §12.10–12.11).** Core: `seed.rs` —
  parse, the allow-list (INSERT + ON CONFLICT; no WITH or RETURNING; a
  database-free source; built-in functions only; guard required on DO UPDATE;
  explicit column list with no GENERATED ALWAYS column; qualified table), and
  the model checks (table, columns, conflict target), all string-literal
  testable. Binary: discovery under
  `seed_root`, `seeds.rs` (per-file transaction, empty `search_path`,
  `lock_timeout`, execute-record-probe-commit), the §8.6 summary line, §9
  reporting. Ships the snowdrop story with a hand-vendored seed file; M15 is
  not a prerequisite.
- **M15 — `pgpushy generate` (spec §4.7).** `[[generate]]` config, argv
  execution with captured stdout, the generated-source marker (distinct from
  §4.1's document marker — opposite discovery polarity), the refusal to
  overwrite unmarked files, and `--check`. Small by design: everything
  downstream of discovery is untouched, and no other command executes a
  configured generator.
- **M10 — Names inside string literals (spec §4.3, §5.4). ✅ Done.** A bare
  name in a literal is rejected, naming the file, the line and the qualified
  form to write. `literal.rs` also carries the walk that §5.4's de-qualification
  pass runs over each statement.
- **M11 — Cross-schema references other than foreign keys (spec §4.5, §12.6).
  ✅ Done.** Rejected in `validate.rs`, naming the referring object, the
  referenced object and both schemas. This is what keeps the closure shallow,
  so it belonged with M7 in review even though it is a separate change.
- **M12 — `sslmode` in full and TLS (spec §6.4). ✅ Done.** All five modes are
  parsed in `conn.rs` and `inspect.rs` connects through the connector each one
  calls for (§8). The driver's own `ssl_mode` carries the fallback semantics —
  `disable`, `prefer`, `require` for `require`/`verify-ca`/`verify-full` —
  while the connector carries the verification, so `prefer` does not collapse
  into `require`.
- **M13 — Discovery follows symlinked files (spec §4.1). ✅ Done.** A symlink
  to a `.sql` file is followed; a symlink to a directory is not, which is what
  stops the walk escaping the source tree.

### Future work

The object kinds spec §12.5 defers past 0.1, staged by the machinery each
needs rather than by how useful it is. They come after the vertical slice for a
reason: a new kind cannot be tested end to end until `plan` and `apply` work,
so one added earlier would be code resting on unverified assumptions. Each now
gets a real regression test the day it is written.

- **Functions, procedures, aggregates, triggers, policies.** Qualify the name,
  pass the body through — pgschema treats bodies as opaque dollar-quoted text
  and Postgres does not resolve a plpgsql body at creation time. Spike
  `BEGIN ATOMIC` (PG14+) first: those bodies *are* resolved at creation.
- **Views and materialized views.** A view's query is resolved at creation, and
  views need a topological sort *within* their category — the machinery
  category 2 already has. The per-schema document answered the harder half: an
  unqualified reference inside a view body resolves to the scratch schema
  standing in for `S`, which is the right answer, so an AST-walking qualifier
  is needed only if cross-schema references widen past §12.6.
- **`GRANT`/`REVOKE` and `ALTER DEFAULT PRIVILEGES`.** Attribution is settled
  (§1). Three things remain. Grants need their **roles in the plan database**,
  which the embedded one has none of, so pgpushy must **refuse with an
  explanation** — naming the file and line and showing the `[env.*.plan_db]`
  block to add — rather than letting pgschema fail on a missing role. An
  **`ignore_grants` opt-out** must keep today's leave-them-alone behavior
  available for projects whose permissions are managed elsewhere: when set,
  pgpushy keeps writing the `.pgschemaignore` sections and keeps rejecting
  `GRANT` in source; when unset and grants are present, it manages them. And
  **`GRANT … ON SCHEMA` appeared to be silently ignored** by pgschema in a
  spike — confirm, and reject it in the allow-list if so, rather than accepting
  a statement that does nothing.
- **Later (spec §14):** `pgpushy dump`; references into unmanaged schemas via
  an external plan DB; plan-database hygiene; cross-schema-cycle and
  single-pass-removal support; schema-drop.

Note that M1 came before any pgschema or Postgres dependency, deliberately:
the offline pipeline is where all the spec's novel logic lives, and `validate`
makes it shippable and testable on its own.

---

## 12. Output & error conventions

- Human-first text, hand-formatted, and all of it in `report.rs` so the shape
  of pgpushy's output is visible in one place. Progress and results to stdout,
  diagnostics to stderr, so that piping one does not interleave the other.
  pgschema's own output passes through untouched; pgpushy only tells it not to
  colour when pgpushy is not colouring. Non-zero exit on any failure;
  distinguish pgpushy failures (validation, inspection, version) from pgschema
  failures in the message.
- Diagnostics name **file and line** for anything sourced from the tree
  (spec §4.3, §4.5), and **name every instance**, not just the first — missing
  schemas, duplicate objects, and disallowed statements are all reported as
  complete lists.
- A rejection says what to write instead. `ALTER TABLE … ADD CONSTRAINT …
  CHECK` shows the inline form; a bare `nextval('s')` shows the qualified one.
  The allow-list is strict enough that a diagnostic without a remedy reads as
  an arbitrary refusal.
- On partial `apply` failure, list applied / failed / not-attempted schemas and
  state that applied schemas are not rolled back (spec §9, §11.2).
- Cross-schema cycle and removal errors must name the schemas *and* the foreign
  keys involved.

---

## 13. Non-obvious pitfalls (learned the hard way)

- **Qualify every identifier, including `public` and including the document's
  own schema.** pgschema strips a matching prefix, so qualify-all is one rule
  rather than two; an unqualified object is attributed to whichever `--schema`
  is running (verified misattribution).
- **Inside a string literal, do the opposite — but only for the document's own
  schema.** A name in `S` is emitted unqualified in `S`'s document and
  qualified everywhere else. It looks like a contradiction of the rule above
  and is the same rule: pgschema cannot strip a literal, so pgpushy strips it
  for pgschema.
- **A closure member brings all its indexes and none of its foreign keys.**
  Omitting the indexes produces a document whose foreign key cannot be created,
  because a standalone unique index is a legal referent; including the foreign
  keys unbounds the closure.
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
- **`CREATE SCHEMA` in the synth documents is for the plan DB only.** It never
  reaches the target; the target schema must pre-exist (0.1 precondition).
- **External plan DB does not help absent target schemas** — current state is
  always read from the target.
- **Never reuse an external plan database between measurements.** It
  accumulates schemas across runs, and stale objects make a *broken* desired
  state look like it works — which sent one spike's conclusion the wrong way
  for a while. Drop and recreate it.
- **Managed-schema set must exclude public unless it has objects** — otherwise
  an empty desired `public` plans a **drop of everything** in the target's
  public schema. The same hazard is what makes a listed-but-unmentioned
  `managed_schemas` entry destructive on purpose.
- **pgschema apply is per-schema-transactional** — no cross-schema atomicity;
  order schemas by cross-schema FKs; cross-schema cycles are unsupported, and
  cross-schema FK *removal* needs the reverse order (spec §6.2).
- **Let pgschema resolve nothing.** Two independent connection resolutions is a
  latent wrong-database bug; pass everything explicitly (spec §6.3).
- **Do not bootstrap a source tree from `pgschema dump`** where a standalone
  sequence backs a column default — it comes back as `SERIAL` with the sequence
  gone (§1). `pg_dump` is the faithful source, and it already qualifies inside
  string literals the way spec §4.3 wants.
- **`NOT VALID` in a `CREATE TABLE` is accepted and then ignored** by Postgres.
  Do not read its acceptance as its working, in a fixture or in a diagnostic.

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
- **~~R4~~ — RESOLVED (2026-08-18).** Neither testcontainers nor an image
  wrapper. `.github/workflows/ci.yml` runs a `postgres:<ver>-alpine` **service
  container** and fetches the pgschema release asset with `curl` in a step,
  exporting `PGPUSHY_TEST_PG_URL` and `PGPUSHY_TEST_PGSCHEMA` for the job. Both
  are matrix dimensions, which is what makes the supported floor (spec §13) a
  thing CI states rather than a claim in prose. Tests skip rather than fail
  when the variables are absent, so a plain `cargo test` on a laptop with no
  Postgres still passes.

*Resolved since v0.1:* generated FK constraint names (spec §5.3 now omits them,
removing the churn risk entirely) and `Other`-statement ordering (spec §4.3 now
rejects unmodelled statements; confirmed acceptable — the real source trees
this targets contain only the allow-listed kinds).

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
spikes ran PG18. Nothing in the 0.1 object scope needs PG18-only syntax, and
every fixture above parses. Revisit if the scope widens (spec §14).

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

**Local development.** A longer-lived container serves the integration tests:
Postgres as `pgpushy-dev` on port 55434 (`postgres`/`pw`), plus a pgschema
1.12.0 binary anywhere on disk. Export `PGPUSHY_TEST_PG_URL` and
`PGPUSHY_TEST_PGSCHEMA` to run them, `PGPUSHY_TEST_DOWNLOAD=1` to include the
managed provider's real download, and run `just msrv` before pushing — a modern
toolchain cannot see MSRV breakage, and CI has caught exactly that.

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

-- Source form (spec §4.3: the literal carries its schema):
CREATE SEQUENCE billing.invoice_no;
CREATE TABLE billing.invoices (
  id bigint PRIMARY KEY DEFAULT nextval('billing.invoice_no')
);
-- billing.sql MUST spell it nextval('invoice_no'); any other schema's document
-- MUST keep the qualifier. The qualified form under --schema billing fails
-- `relation "billing.invoice_no" does not exist`.

-- Cross-schema cycle (UNSUPPORTED — apply/validate reject; plan shows plans):
--   public.customers → billing.accounts AND billing.accounts → public.customers

-- Cross-schema FK removal (spec §6.2 — must be detected before apply):
--   target holds public.orders → billing.accounts;
--   source tree drops BOTH the FK and billing.accounts.
--   Creation order (billing first) cannot apply this; needs public first.
```

Verified outcomes are catalogued in §1; the memory file
`pgpushy-project.md` (agent memory) holds the same facts if available.
