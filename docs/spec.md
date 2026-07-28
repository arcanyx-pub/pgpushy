# pgpushy Specification

**Version:** 0.1 (draft 2)
**Date:** 2026-07-28
**Status:** Draft — all 0.x design decisions resolved (§14)

This document specifies **pgpushy**, a declarative Postgres schema-management
tool that manages an entire database — all of its schemas — from a directory
tree of SQL files, delegating the actual diff-and-apply work to
[**pgschema**](https://github.com/pgplex/pgschema).

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY**
are to be interpreted as described in RFC 2119.

## 1. Introduction

pgschema is a Terraform-style declarative schema tool for Postgres: you
describe a desired schema state as SQL, it computes a migration plan by
diffing that state against a live database, and applies it. pgpushy does not
replace any of that. It sits in front of pgschema and removes two frictions:

- **Manual dependency ordering.** pgschema executes the desired-state SQL to
  build its comparison state, so the input must be ordered: an object that
  references another must appear after it, and mutually-referencing objects
  cannot be expressed with inline references at all. Authors maintain this
  order by hand (an ordered list of `\i` includes). pgpushy removes the
  requirement: authors organize files however they like, and pgpushy
  synthesizes a correctly-ordered desired state automatically.

- **Single-schema scope.** A pgschema invocation targets exactly one schema
  (`--schema`); objects in other schemas are ignored. Managing a whole
  database means running pgschema once per schema, in the right order, with
  the schemas pre-created. pgpushy performs this orchestration so the
  **database** is the unit of management, not the schema.

### 1.1 Goals

- **G1 — Order-free authoring.** The author MUST NOT need to order files, name
  them for ordering, or maintain an include list. File and directory
  organization is purely for human convenience.
- **G2 — Database-level management.** A single `pgpushy` invocation reconciles
  every managed schema in the target database.
- **G3 — Thin and transparent.** pgpushy is a preprocessor and orchestrator.
  pgschema remains the engine that computes and applies migrations; pgpushy
  MUST NOT reimplement diffing or migration generation.
- **G4 — Semantic transparency.** The desired state pgpushy synthesizes MUST
  describe exactly the same schema the author wrote. Any reordering or
  rewriting pgpushy performs MUST be behavior-preserving (§5.5).

### 1.2 Non-goals

- pgpushy does not compute schema diffs or generate migration DDL; pgschema
  does (G3).
- pgpushy does not provide its own connection, locking, or apply-safety
  machinery beyond what it passes through to pgschema (§8.3).
- pgpushy is not a general SQL build system. Its dependency handling covers
  the object kinds in §5; other kinds are subject to the limitations in §11.

### 1.3 Relationship to pgschema

pgpushy invokes the `pgschema` binary as a subprocess (§8), obtaining that
binary through a provider (§8.5). It relies on pgschema behavior verified as of
pgschema **1.12.0**, in particular foreign-key deferral and deterministic
cycle-breaking during plan generation (pgschema PR #156, first released in
v1.4.2 on 2025-11-14). The minimum supported version is the version pgpushy is
tested against (§12).

## 2. Terminology

- **Source tree** — a directory whose `*.sql` files collectively describe the
  desired state of one database.
- **Source file** — a single `*.sql` file in the source tree. Typically holds
  one table, but pgpushy imposes no such rule.
- **Managed schema** — a Postgres schema that pgpushy reconciles. The set of
  managed schemas is derived from the source tree (§4.3).
- **Default schema** — the schema to which an unqualified object belongs.
  Defaults to `public`; configurable (§9).
- **Desired state** — the single synthesized SQL document pgpushy produces
  from the source tree and passes to pgschema as `--file` (§5).
- **FK-lift** — the transform that moves foreign-key constraints out of table
  definitions into trailing `ALTER TABLE … ADD CONSTRAINT` statements (§5.3).
- **Target database** — the Postgres database being reconciled.

## 3. Overview

pgpushy executes the following pipeline. Each stage is specified in the
referenced section.

```
1. Discover      walk the source tree → set of source files            (§4)
2. Parse         parse each file (libpg_query) → objects + schemas      (§4, §5)
3. Synthesize    FK-lift + CREATE SCHEMA emission → one desired-state   (§5)
4. Precheck      verify every managed schema exists on the target (read-only) (§6)
5. Order         topologically order managed schemas by cross-schema FK (§7)
6. Delegate      pgschema plan|apply --schema S --file <desired-state>, (§8)
                 once per managed schema, in dependency order
```

The desired state synthesized in stage 3 is **the same document for every
schema**; the per-schema loop in stage 6 varies only the `--schema` argument.
pgschema builds the full multi-schema desired state internally but diffs only
the named schema, so one synthesized document serves all schemas.

## 4. Source Tree

### 4.1 Discovery

pgpushy MUST recursively discover every file ending in `.sql` (case-insensitive)
under the source-tree root. Directory structure carries no ordering or
semantic meaning; it is organizational only (G1). pgpushy MUST NOT require any
manifest, index, or include file.

Hidden files and directories (names beginning with `.`) SHOULD be ignored.
An empty source tree is not an error; it describes an empty database.

### 4.2 Parsing

Each source file MUST be parsed with a Postgres-grammar parser (the
`pg_query` crate, wrapping libpg_query — the real server parser) so that
pgpushy understands statements structurally rather than textually. Parse
failures MUST abort the run with the offending file and location.

### 4.3 Schema assignment and the managed-schema set

Every object is assigned to exactly one schema:

- An object written with a schema-qualified name (`billing.invoices`) belongs
  to that schema.
- An object written unqualified (`invoices`) belongs to the **default
  schema** (§9).

The **managed-schema set** is the union of:

1. every schema named in a `CREATE SCHEMA` statement in the source tree, and
2. every schema to which at least one discovered object is assigned.

The default schema (§9) governs only how *unqualified* objects are assigned; it
is **not** automatically a managed schema. A schema the source tree never
mentions — no assigned object and no `CREATE SCHEMA` — is left entirely alone:
pgpushy never reconciles it and therefore never empties it. (Were an
unmentioned schema treated as managed, its empty desired state would plan a
drop of everything the target holds in it.) pgpushy reconciles exactly the
managed-schema set and no other schema (§8.4).

## 5. Desired-State Synthesis

pgpushy transforms the discovered objects into a single desired-state
document. This section defines that document.

### 5.1 Statement categories

Discovered statements are grouped into ordered categories:

| Order | Category            | Contents                                              |
|-------|---------------------|-------------------------------------------------------|
| 1     | Schemas             | `CREATE SCHEMA` for every managed schema (§5.2)       |
| 2     | Tables and contents | `CREATE TABLE` and their inline column, `CHECK`, unique, and primary-key constraints; `CREATE INDEX`; comments |
| 3     | Foreign keys        | every FK as a trailing `ALTER TABLE … ADD CONSTRAINT` (§5.3) |

Within a category, pgpushy MAY emit statements in any order that is valid for
that category; a stable, deterministic order (§10.3) is REQUIRED. Category 2
requires no inter-table ordering because, once foreign keys are lifted to
category 3, table definitions in the same schema have no creation-time
dependencies on one another.

> **Non-normative.** Object kinds beyond those in category 2 — views,
> functions, triggers, user-defined types/domains, and policies — can have
> creation-time dependencies that FK-lift does not resolve. Version 0.x
> scopes managed dependency resolution to tables and foreign keys; see §11.2
> and §13.

### 5.2 Schema declarations

pgpushy MUST emit `CREATE SCHEMA IF NOT EXISTS <s>` for every managed schema
at the top of the desired state, before any object that references it.
pgschema executes the desired state in its **plan database** to build its
comparison model, and a schema-qualified object requires its schema to exist
in that model; emitting the declarations makes qualified objects in any schema
resolvable. These declarations run only in the plan database — never against
the target, which must already contain the schemas (§6).

`CREATE SCHEMA` statements authored in the source tree are honored and MUST
NOT be treated as errors. pgpushy MAY normalize them to the `IF NOT EXISTS`
form.

### 5.3 The FK-lift transform

For every foreign-key constraint in the source tree — whether written inline
in a `CREATE TABLE` or as a standalone `ALTER TABLE … ADD CONSTRAINT` —
pgpushy MUST emit the constraint as a trailing `ALTER TABLE … ADD CONSTRAINT`
statement in category 3, and MUST NOT emit it inline in category 2.

This mirrors how `pg_dump` separates foreign keys from table creation, and it
is what makes authoring order-free (G1): with all referenced tables created
before any foreign key is added, no ordering among tables can produce a
dangling reference, and **mutually-referencing tables — which have no valid
inline ordering at all — are expressed correctly.**

A foreign key whose constraint name is not given by the author MUST be
assigned a stable, deterministic name (§10.3) so that plans do not churn
between runs.

> **Non-normative — why lift rather than sort.** A topological sort of tables
> by their foreign keys fails on cycles: two tables that reference each other
> have no linear creation order. FK-lift has no such failure mode, because
> table creation and constraint creation are separated. It was verified to
> produce a plan byte-identical to correctly-ordered inline input, and to
> handle same-schema cycles that no sort can. This is why pgpushy lifts
> unconditionally rather than sorting.

### 5.4 Object identity and synthesis granularity

Objects from more than one managed schema coexist in the file handed to each
per-schema pgschema run. pgschema attributes an **unqualified** object to
whatever `--schema` the run targets — unqualified objects land in a scratch
schema that stands in for `--schema`. Because pgpushy runs the same file once
per schema, an unqualified object is thereby claimed by *every* schema in turn.

Verified: a combined file with `customers` unqualified (intended for `public`)
and `snowdrop.machine_ids` qualified, run with `--schema snowdrop`, plans
**both** `customers` and `machine_ids` into `snowdrop` — a misattribution.
Qualifying it as `public.customers` makes `--schema snowdrop` plan only
`machine_ids`, and `--schema public` plan only `customers`.

pgpushy MUST therefore **schema-qualify every emitted identifier with its
resolved schema, including the default schema** — `public.customers`, never
bare `customers` — and likewise qualify the referents of foreign keys and
other cross-object references. Objects whose schema a given run does not target
serve only to resolve references and are otherwise ignored by that run.

pgschema builds its desired-state model by executing the file it is given,
resolving every reference from that file alone and **not** from the target
database. Verified: a cross-schema foreign key to a table absent from the
file fails (`relation … does not exist`) even when that table exists on the
target. Therefore the file passed to a given `pgschema --schema S` run MUST
contain every table referenced by a foreign key in `S`, including tables in
other schemas — the transitive closure of cross-schema foreign-key
references.

Whether pgpushy synthesizes **one combined document** reused for every schema,
or a **per-schema document** trimmed to that closure, is an implementation
detail that does not affect observable behavior or this spec. A single
combined document satisfies the closure requirement trivially and is the
RECOMMENDED default; per-schema trimming is a possible optimization for very
large databases (§13). Whichever is chosen, the closure requirement above is
normative.

> **Non-normative.** The combined-document approach was verified end-to-end:
> one synthesized file, `pgschema … --schema S` per schema, correct per-schema
> plans and idempotent convergence. A `billing`-only file omitting a
> referenced `public.customers` failed to build desired state — the evidence
> behind the closure requirement.

### 5.5 Behavior preservation

The synthesized desired state MUST describe exactly the schema the source
tree describes (G4). FK-lift and schema-declaration emission are the only
permitted transformations, and both are behavior-preserving: they change the
textual order and constraint attachment site, never the resulting schema.
pgpushy MUST NOT add constraint attributes the author did not write (for
example, it MUST NOT inject `NOT VALID`), because such attributes are part of
the schema's meaning, not of its ordering.

## 6. Schema Precondition

pgschema cannot reconcile a schema absent from the target: reading current
state for a nonexistent schema fails, and an external plan database does not
change this — current state is always read from the target (verified). An
**existing but empty** schema reconciles normally.

For 0.x, pgpushy therefore makes schema existence a **hard precondition**:
**every managed schema MUST already exist on the target database.** Before any
delegation (§8), for both `plan` and `apply`, pgpushy MUST verify this with a
single read-only catalog query and MUST fail — naming every missing schema —
if any managed schema is absent, before issuing any change.

A consequence is that **pgpushy issues no DDL of its own to the target** in
0.x: schema *and* content changes all flow through pgschema (satisfying G3),
and the precondition check is read-only, so `plan` cannot mutate the target
even incidentally.

Note that the synthesized desired state still emits `CREATE SCHEMA IF NOT
EXISTS` for each managed schema (§5.2); that executes in pgschema's **plan
database** to make qualified references resolvable, never against the target.

> **Non-normative.** Creating managed schemas is thus the operator's
> responsibility in 0.x (a one-time `CREATE SCHEMA` per new schema). Automatic
> handling of absent schemas — reporting a new schema and its would-be
> contents without mutating the target — is deferred (§13, option B). pgpushy
> also never *drops* a schema (§11.3).

## 7. Cross-Schema Ordering

When a foreign key in one managed schema references a table in another
(a **cross-schema foreign key**), the referenced schema's table must exist in
the target before the referencing schema's foreign key is applied. Because
pgschema applies each schema in a separate transaction and cannot defer a
foreign key across schemas, pgpushy MUST order the per-schema delegations.

pgpushy MUST build a directed graph over managed schemas, with an edge
`A → B` when a table in `A` has a foreign key referencing a table in `B`, and
MUST process schemas in reverse-dependency order (a schema is processed after
every schema it references). Same-schema foreign keys do not create edges and
are handled within pgschema (§5.3).

If the graph contains a cycle (a **cross-schema foreign-key cycle**), no valid
order exists; pgpushy MUST fail with a clear diagnostic naming the schemas in
the cycle, before applying any changes. See §11.1.

## 8. Delegation to pgschema

### 8.1 Invocation

For each managed schema `S`, in the order of §7, pgpushy invokes the
`pgschema` binary with the subcommand corresponding to the pgpushy command
(§8.2), passing `--schema S` and `--file <synthesized desired state>`, plus
the connection and pass-through flags of §8.3.

pgpushy obtains the `pgschema` binary through a provider (§8.5). If no usable
binary can be resolved, pgpushy MUST fail with an actionable message.

### 8.2 Commands

pgpushy exposes commands mirroring pgschema:

Both commands first synthesize the desired state (§5) and enforce the schema
precondition (§6), failing before any delegation if a managed schema is absent
from the target.

- **`pgpushy plan`** — run `pgschema plan --schema S` for every managed schema
  and present each schema's plan. This never modifies the target: pgschema
  `plan` reads the target read-only, and its scratch objects live in the plan
  database, not the target. pgpushy itself issues no target DDL (§6).
- **`pgpushy apply`** — run `pgschema apply --schema S` for every managed
  schema in dependency order (§7). To fail fast, pgpushy SHOULD run the full
  `plan` pass first and abort before applying any schema if synthesis, the
  precondition, or any per-schema plan fails.

### 8.3 Pass-through flags

Connection and apply-tuning flags MUST be forwarded unchanged to pgschema,
including at least: `--host`, `--port`, `--db`, `--user`, `--password`,
`--sslmode`, `--lock-timeout`, and the `--plan-*` family. Standard `PG*`
environment variables MUST be honored as pgschema honors them.

pgpushy **owns** `--schema` (it loops over the managed set) and `--file` (it
synthesizes the desired state); these MUST NOT be settable by the user.

### 8.4 Unmanaged schemas

pgpushy reconciles exactly the managed-schema set (§4.3). Schemas present in
the target database but absent from the source tree are neither planned nor
modified nor dropped (§6, §11.3).

### 8.5 Resolving the pgschema binary

pgpushy obtains `pgschema` through a **provider** with two backends. The
managed backend is the intended default; the BYO backend is permanent (see
Rollout).

**Managed backend.** pgpushy downloads a pinned pgschema version — the version
its release was tested against (§12), overridable via configuration (§9) — from
pgschema's official GitHub release assets, which are standalone per-platform
binaries. It caches the binary per version under the user's cache home (e.g.
`$XDG_CACHE_HOME/pgpushy/pgschema/<version>/`), reusing it on later runs.
Downloads MUST be fetched over HTTPS and MUST be integrity-verified against a
SHA-256 that pgpushy ships for each version it pins — pgschema publishes no
checksums of its own, so pgpushy is the source of truth for integrity. A
version the operator pins that pgpushy has no shipped hash for MAY be used with
TLS-only trust or an operator-supplied hash, and pgpushy SHOULD say which.
The managed backend serves the platforms pgschema publishes binaries for:
Linux and macOS, amd64 and arm64.

**Bring-your-own (BYO) backend.** pgpushy uses an operator-provided binary — an
explicit path or a `PATH` lookup (§9). In this mode pgpushy MUST read the
binary's version and enforce the minimum (§12). Version is read by parsing the
`Version:` line of `pgschema --help`; there is no machine-readable version
output. A version **below the floor** MUST be a hard error naming both the
found and the required version. An **unparseable** version line MUST be a
warning, not a failure — the line is a human-readable string, not a stability
contract. BYO is the only backend available on **Windows** (no pgschema binary
is published) and in **air-gapped** environments, and is therefore a permanent
part of pgpushy, not a temporary measure.

**Rollout.** The first 0.x release ships the **BYO backend with the version
check** only. The managed backend is a fast-follow and becomes the default when
it lands; the provider seam keeps that change additive.

## 9. Configuration

pgpushy MUST be usable with CLI flags and `PG*` environment variables alone; a
configuration file is optional convenience, never required.

**File.** pgpushy reads an optional **`pgpushy.toml`** (TOML) from the project
root. It MAY hold:

- **Project structure** — the source-tree root and the default schema (§4.3).
- **pgschema provider** (§8.5) — the backend (managed vs. BYO), the pinned
  pgschema version (managed), and the binary path (BYO).
- **Connection defaults** — `host`, `port`, `db`, `user`, `sslmode`, and —
  permitted but discouraged — `password`.

**Precedence** (highest first): CLI flag → `PG*` / pgschema environment
variable → `pgpushy.toml` → built-in default. The default schema defaults to
`public`.

**Password handling.** A `password` MAY be set in `pgpushy.toml`, but when the
effective connection password is *sourced from the file* — i.e. not overridden
by `PGPASSWORD` or `--password` — pgpushy MUST emit a prominent warning that a
password is being read from a file that is easily committed to version control.
Supplying it via `PGPASSWORD` or `--password` is the intended path and MUST NOT
warn. (The warning fires on actual use from the file, not on mere presence, so
an overridden file password is silent.)

Rich configuration (per-schema overrides, excludes, multiple target
environments) is future work (§13) and MUST NOT be required for the core
workflow.

## 10. Properties

### 10.1 Idempotence

Applying an already-reconciled database MUST produce no changes: every
per-schema `pgschema plan` is empty, and `pgpushy apply` is a no-op. (Verified
for the multi-schema loop in the design spike.)

### 10.2 Atomicity

pgschema applies each schema in its own transaction; pgpushy does **not** wrap
the whole database in one transaction. A failure partway through `apply`
therefore leaves already-processed schemas applied and the rest unapplied.
pgpushy MUST report which schemas were applied. The fail-fast plan pass (§8.2)
reduces, but does not eliminate, partial application.

> **Non-normative.** Cross-database or all-or-nothing multi-schema application
> is not offered; it is not achievable through per-schema pgschema
> invocations. Deployments needing all-or-nothing semantics should treat a
> partial failure as a fix-forward condition.

### 10.3 Determinism

Given the same source tree, pgpushy MUST synthesize byte-identical desired
state across runs and platforms: category ordering (§5.1), intra-category
ordering, and generated constraint names (§5.3) MUST all be deterministic
functions of the source content. This keeps plans stable and reviewable.

## 11. Limitations

### 11.1 Cross-schema foreign-key cycles

Two managed schemas whose tables reference each other cannot be reconciled:
pgschema applies each schema separately and cannot defer a foreign key across
schemas, so no schema order works. pgpushy detects this (§7) and fails rather
than applying a partial result. Same-schema cycles are fully supported
(§5.3). *(This is expected to be rare; the common deployment shares one
schema across services.)*

### 11.2 Non-foreign-key cross-file dependencies

FK-lift resolves table-to-table foreign-key ordering only. Objects with other
creation-time cross-file dependencies — a view over a table, a trigger over a
function, a column of a user-defined type defined elsewhere — are emitted in
category 2/beyond without dependency ordering (§5.1) and MAY cause pgschema to
fail if misordered across files. Version 0.x does not manage these; §13.

### 11.3 Managed schemas must pre-exist; pgpushy issues no schema DDL

In 0.x every managed schema MUST already exist on the target (§6); pgpushy
neither creates nor drops schemas. Introducing a new schema requires a
one-time manual `CREATE SCHEMA` before pgpushy can manage it, and removing a
schema from management is likewise manual. Automatic handling of absent
schemas is deferred (§13, option B).

## 12. Dependencies and Compatibility

- **pgschema** — required at runtime, resolved through the provider (§8.5):
  downloaded by the managed backend or supplied by the operator (BYO). The
  minimum supported version is **the version pgpushy is tested against** —
  currently **v1.12.0** — expressed as a `>=` floor that tracks pgpushy's CI
  matrix and rises as newer releases are tested; newer-than-floor is accepted.
  The relied-upon behavior (foreign-key deferral, deterministic cycle-breaking,
  PR #156) is technically present from **v1.4.2** (2025-11-14), which bounds how
  far the floor could be lowered later with testing, but is not itself the
  supported floor. The BYO backend enforces the floor (§8.5); the managed
  backend controls the version and so needs no check.
- **libpg_query** (via the `pg_query` crate) — used for parsing (§4.2).
- **Postgres** — the target and pgschema's plan database. Minimum version
  follows pgschema's own requirement.

## 13. Future Work (non-normative)

- **Absent-schema handling (option B)** — lift the §6 precondition by, for a
  managed schema missing from the target, reporting it as new and its contents
  as fully-created (without mutating the target), rather than failing. This is
  the preferred future direction over having pgpushy create schemas itself.
- **General dependency resolution** for non-table objects (§11.2), e.g. a
  topological sort over all object kinds, or normalization through a throwaway
  Postgres + `pg_dump` (which orders all object kinds for free).
- **References into unmanaged schemas** — for foreign keys targeting schemas
  pgpushy does not manage (extension schemas, an externally-owned `auth`,
  etc.), support pgschema's **external plan database** seeded with those
  external objects. Not needed for cross-schema references *among managed
  schemas* (the combined document already covers those, §5.4), so it is not
  mandated in 0.x.
- **`pgpushy dump`** — the inverse: read an existing database and emit a
  per-object source tree, to bootstrap adoption.
- **Cross-schema FK cycle support** by managing the cycle-breaking foreign
  keys directly against the target after all pgschema runs.
- **Richer configuration** (§9): per-schema overrides, exclude patterns.
- **Schema-drop management** (§11.3), behind an explicit opt-in.

## 14. Open Decisions

**Resolved (recorded in the body):**

- **Object scope for 0.x** — tables and foreign keys only; views, functions,
  and user-defined types deferred to §13. (§5.1, §11.2)
- **Schema-assignment mechanism** — schema-qualify **every** emitted
  identifier with its resolved schema, including `public`; an unqualified
  object would be misattributed to every schema the combined file is run
  against (verified). Synthesis-file granularity (one combined document vs.
  per-schema documents trimmed to the cross-schema closure) remains an
  implementation detail; the combined document is the recommended default.
  (§5.4)
- **Absent schemas / `plan` mutation** — 0.x makes schema existence a hard
  precondition: every managed schema MUST pre-exist on the target, checked
  read-only, else pgpushy fails before delegating. pgpushy thus issues no DDL
  of its own to the target, and `plan` cannot mutate it. Automatic absent-schema
  handling is deferred (§13, option B). (§6, §8.2)
- **Cross-schema FK cycles** — detected and rejected in 0.x. (§7, §11.1)
- **pgschema version & resolution** — floor is the tested version (`>= v1.12.0`
  today), tracking pgpushy's CI matrix. pgpushy resolves the binary through a
  provider (§8.5): managed download (intended default, pinned + SHA-256-verified)
  with a permanent BYO override that parses `pgschema --help` and enforces the
  floor. First 0.x release ships BYO + version check; managed is a fast-follow.
  (§8.5, §12)

- **Configuration file** — optional `pgpushy.toml` (TOML) at the project root,
  holding project structure, pgschema-provider, and connection defaults;
  precedence CLI > env > file > default. `password` is permitted in the file
  but triggers a prominent warning when it is the effective source. (§9)

All decisions identified for 0.x are now resolved; this section stands as the
decision log.
