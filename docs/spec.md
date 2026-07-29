# pgpushy Specification

**Version:** 0.2
**Date:** 2026-07-29
**Status:** Draft — all 0.x design decisions resolved (§15)

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
- **G5 — Fail before acting.** Every condition pgpushy can detect MUST be
  reported before any change reaches the target, and with a diagnostic that
  names the source of the problem. pgpushy MUST NOT let a condition it
  understands surface as an opaque error from a subprocess.

### 1.2 Non-goals

- pgpushy does not compute schema diffs or generate migration DDL; pgschema
  does (G3).
- pgpushy does not provide its own connection, locking, or apply-safety
  machinery beyond what it passes through to pgschema (§8.3).
- pgpushy is not a general SQL build system. It manages the object kinds in
  the statement allow-list (§4.3) and rejects everything else.

### 1.3 Relationship to pgschema

pgpushy invokes the `pgschema` binary as a subprocess (§8), obtaining that
binary through a provider (§8.5). It relies on pgschema behavior verified as of
pgschema **1.12.0**, in particular foreign-key deferral and deterministic
cycle-breaking during plan generation (pgschema PR #156, first released in
v1.4.2 on 2025-11-14). The minimum supported version is the version pgpushy is
tested against (§13).

## 2. Terminology

- **Source tree** — a directory whose `*.sql` files collectively describe the
  desired state of one database.
- **Source file** — a single `*.sql` file in the source tree that discovery
  retains (§4.1). Typically holds one table, but pgpushy imposes no such rule.
- **Managed schema** — a Postgres schema that pgpushy reconciles. The set of
  managed schemas is derived from the source tree, or declared (§4.4).
- **Default schema** — the schema to which an unqualified object belongs.
  Defaults to `public`; configurable (§10).
- **Desired state** — the single synthesized SQL document pgpushy produces
  from the source tree and passes to pgschema as `--file` (§5).
- **FK-lift** — the transform that moves foreign-key constraints out of table
  definitions into trailing `ALTER TABLE … ADD CONSTRAINT` statements (§5.3).
- **Cross-schema foreign key** — a foreign key whose referencing table and
  referenced table are assigned to different schemas.
- **Target database** — the Postgres database being reconciled.

## 3. Overview

pgpushy executes the following pipeline. Each stage is specified in the
referenced section.

```
   offline — requires no database connection
1. Discover      walk the source tree, apply excludes → ordered files   (§4.1)
2. Parse         parse each file; enforce the statement allow-list      (§4.2, §4.3)
3. Resolve       assign schemas; derive or verify the managed set       (§4.4)
4. Check         duplicate objects; unresolvable foreign-key referents  (§4.5)
5. Order         cross-schema FK graph; topological order; cycles       (§7)
6. Synthesize    FK-lift + qualify + category emission → desired state  (§5)

   online — requires the target database
7. Inspect       one read-only target query (§6)
8. Delegate      pgschema plan|apply --schema S --file <desired state>, (§8)
                 once per managed schema, in the order from stage 5
```

`pgpushy validate` (§8.2) runs stages 1–6 and stops; it never connects to
anything. `plan` and `apply` run the whole pipeline.

The desired state synthesized in stage 6 is **the same document for every
schema**; the per-schema loop in stage 8 varies only the `--schema` argument.
pgschema builds the full multi-schema desired state internally but diffs only
the named schema, so one synthesized document serves all schemas.

## 4. Source Tree

### 4.1 Discovery

pgpushy MUST recursively discover every file ending in `.sql` (case-insensitive)
under the source-tree root. Directory structure carries no ordering or
semantic meaning; it is organizational only (G1). pgpushy MUST NOT require any
manifest, index, or include file.

Hidden files and directories (names beginning with `.`) MUST be ignored.
Symbolic links to directories MUST NOT be followed, so that discovery
terminates and cannot escape the source tree.

Discovery MUST yield a deterministic order: files are sorted by their
source-root-relative path, compared as byte sequences, without locale
collation. Nothing in the synthesized output depends on discovery order
(§5.1), but a deterministic order makes diagnostics and reporting stable
(§11.3).

**Exclusions.** Configuration (§10) MAY supply a list of glob patterns matched
against source-root-relative paths. A file matching any pattern is not
discovered and is never parsed. pgpushy MUST report the number of excluded
files, and SHOULD report the count matched by each pattern, so that an
over-broad pattern silently swallowing real tables is visible rather than
mysterious.

An empty source tree is not an error; it describes an empty database.

### 4.2 Parsing

Each source file MUST be parsed with a Postgres-grammar parser (the
`pg_query` crate, wrapping libpg_query — the real server parser) so that
pgpushy understands statements structurally rather than textually. Parse
failures MUST abort the run, naming the file and the location.

### 4.3 Statement allow-list

pgpushy 0.x manages tables and foreign keys. A source file MUST contain only
the following statements:

| Statement | Category (§5.1) |
|---|---|
| `CREATE SCHEMA [IF NOT EXISTS] <name>` | 1 |
| `CREATE TABLE` (with inline constraints) | 2, FKs lifted to 4 |
| `CREATE [UNIQUE] INDEX` | 3 |
| `ALTER TABLE … ADD CONSTRAINT` — foreign key | 4 |
| `ALTER TABLE … ADD CONSTRAINT` — `CHECK`, `UNIQUE`, `PRIMARY KEY`, `EXCLUDE` | 3 |
| `COMMENT ON` an object of the kinds above | 5 |

Any other statement MUST be a hard error naming the file, the line, and the
statement kind, and directing the reader to §14 for the object kinds under
consideration for later versions. This includes, non-exhaustively: `CREATE
VIEW`, `CREATE FUNCTION`, `CREATE TRIGGER`, `CREATE TYPE`, `CREATE DOMAIN`,
`CREATE POLICY`, `CREATE EXTENSION`, `GRANT`/`REVOKE`, every `ALTER TABLE`
subcommand other than `ADD CONSTRAINT`, every `DROP`, and all DML.

Two properties motivate rejection rather than pass-through:

- **Qualification cannot be honored.** §5.4 makes schema-qualifying every
  emitted identifier normative, because an unqualified identifier in the
  combined document is misattributed to every schema the document is run
  against. pgpushy can qualify identifiers in statements it models
  structurally. It cannot qualify the interior of a statement it does not
  model — a table reference inside a view body, say — so passing such a
  statement through would emit exactly the construct §5.4 forbids.
- **The desired state must describe schema, not data or actions.** pgschema
  *executes* the document to build its comparison model. A `DROP` or an
  `INSERT` riding along would run inside that model and silently distort the
  state everything is diffed against.

`CREATE SCHEMA` is accepted only in its bare form. The nested form
(`CREATE SCHEMA s CREATE TABLE t (…)`, whose elements Postgres permits inline)
and the name-from-role form (`CREATE SCHEMA AUTHORIZATION <role>`, whose schema
name pgpushy cannot resolve offline) MUST both be rejected, with a diagnostic
showing the accepted form. `AUTHORIZATION` on a *named* schema MUST likewise be
rejected: pgpushy does not manage ownership, and silently discarding the clause
would make the synthesized state say something the author did not write (G4).

Three further restrictions apply within the allowed statements, each for the
same reason the allow-list exists at all:

- `CREATE TABLE … INHERITS`, `… PARTITION OF`, `… PARTITION BY`, and
  `CREATE TABLE … OF <type>` MUST be rejected. Each makes a table's *creation*
  depend on another object, and FK-lift resolves table-to-table foreign-key
  ordering only — this is exactly the class of dependency §12.5 keeps out of
  0.x.
- `CREATE INDEX CONCURRENTLY` MUST be rejected. It cannot run inside a
  transaction block, and it describes a strategy for reaching a state rather
  than the state itself; how an index is built is pgschema's decision.
- `CREATE INDEX` without an explicit index name MUST be rejected. Postgres
  would generate one from the table and the indexed expressions, and pgpushy
  needs a stable name to detect duplicates against and to attach comments to.
  This is the opposite of the foreign-key rule in §5.3 only in appearance: a
  foreign key can be left unnamed precisely *because* pgpushy never needs to
  refer to it by name.

### 4.4 Schema assignment and the managed-schema set

Every object is assigned to exactly one schema:

- An object written with a schema-qualified name (`billing.invoices`) belongs
  to that schema.
- An object written unqualified (`invoices`) belongs to the **default
  schema** (§10).

**Derived set (default).** Absent an explicit declaration, the managed-schema
set is the union of:

1. every schema named in a `CREATE SCHEMA` statement in the source tree, and
2. every schema to which at least one discovered object is assigned.

The default schema governs only how *unqualified* objects are assigned; it is
**not** automatically a managed schema. A schema the source tree never
mentions — no assigned object and no `CREATE SCHEMA` — is left entirely alone:
pgpushy never reconciles it and therefore never empties it. (Were an
unmentioned schema treated as managed, its empty desired state would plan a
drop of everything the target holds in it.)

**Declared set.** Configuration (§10) MAY declare `managed_schemas`. When
present it is authoritative:

- A schema the source tree mentions but the declaration omits MUST be a hard
  error, naming the schema and the source file and line that assigned an
  object to it. This is the guardrail: without it, adding one file with a
  qualified object silently enlists a whole schema into reconciliation, and
  reconciliation drops things.
- A schema the declaration lists but the source tree never mentions **is**
  managed, with an empty desired state. This is the only way to express a
  managed-and-empty schema, and it is destructive by design: pgschema will
  plan to drop whatever the target holds there. pgpushy MUST call this out in
  the plan presentation.

pgpushy reconciles exactly the managed-schema set and no other schema (§8.4).

### 4.5 Source-tree validity

Beyond the allow-list, pgpushy MUST reject the following before synthesis, each
with a diagnostic naming the offending source locations (G5):

- **Duplicate objects.** Two definitions of the same object — the same table,
  index, or constraint in the same schema — MUST be an error naming **both**
  source files and lines. The freedom of §4.1 makes this easy to reach by
  accident (a copy-paste, a half-finished refactor, a backup file), and
  without this check it surfaces as `relation … already exists` from a
  subprocess, referring to a document the author never wrote.
- **Unresolvable foreign-key referents.** §5.4 requires the synthesized
  document to contain every table referenced by a foreign key. A foreign key
  whose referenced table is not defined anywhere in the source tree MUST be an
  error naming the constraint, the referencing table, and the unresolved
  referent. This is the diagnostic for a foreign key into a schema pgpushy
  does not manage — an extension schema, an externally-owned `auth` — which is
  future work (§14), not a supported 0.x configuration.

## 5. Desired-State Synthesis

pgpushy transforms the discovered objects into a single desired-state
document. This section defines that document.

### 5.1 Statement categories

Discovered statements are grouped into ordered categories. Every statement in
category *n* is emitted before every statement in category *n+1*.

| Order | Category | Contents |
|-------|----------|----------|
| 1 | Schemas | `CREATE SCHEMA IF NOT EXISTS` for every managed schema (§5.2) |
| 2 | Tables | `CREATE TABLE`, carrying their inline column, `CHECK`, `UNIQUE`, and `PRIMARY KEY` constraints — but not their foreign keys (§5.3) |
| 3 | Table-dependent objects | `CREATE INDEX`; standalone non-foreign-key `ALTER TABLE … ADD CONSTRAINT` |
| 4 | Foreign keys | every foreign key as a trailing `ALTER TABLE … ADD CONSTRAINT` (§5.3) |
| 5 | Comments | `COMMENT ON` any object above |

Within a category, pgpushy MAY emit statements in any order that is valid for
that category; a stable, deterministic order (§11.3) is REQUIRED.

The categories exist because emission order is execution order: pgschema
executes this document, so a statement that references an object must follow
the statement creating it. Category 2 is internally order-free — once foreign
keys are lifted to category 4, table definitions have no creation-time
dependencies on one another. Categories 3, 4, and 5 are not internally
order-free with respect to category 2, which is precisely why they are
separate categories rather than intermixed with it: an index, a constraint, or
a comment depends on its table existing. No category-3 object depends on
another category-3 object, and comments are emitted last so that they may
reference any object in the document.

### 5.2 Schema declarations

pgpushy MUST emit `CREATE SCHEMA IF NOT EXISTS <s>` for every managed schema
at the top of the desired state, before any object that references it.
pgschema executes the desired state in its **plan database** to build its
comparison model, and a schema-qualified object requires its schema to exist
in that model; emitting the declarations makes qualified objects in any schema
resolvable. These declarations run only in the plan database — never against
the target, which must already contain the schemas (§6.1).

`CREATE SCHEMA` statements authored in the source tree are honored and MUST
NOT be treated as errors. pgpushy MAY normalize them to the `IF NOT EXISTS`
form.

### 5.3 The FK-lift transform

For every foreign-key constraint in the source tree — whether written inline
in a `CREATE TABLE` or as a standalone `ALTER TABLE … ADD CONSTRAINT` —
pgpushy MUST emit the constraint as a trailing `ALTER TABLE … ADD CONSTRAINT`
statement in category 4, and MUST NOT emit it inline in category 2.

This mirrors how `pg_dump` separates foreign keys from table creation, and it
is what makes authoring order-free (G1): with all referenced tables created
before any foreign key is added, no ordering among tables can produce a
dangling reference, and **mutually-referencing tables — which have no valid
inline ordering at all — are expressed correctly.**

**Constraint naming.** A foreign key the author named MUST keep that name. A
foreign key the author left unnamed MUST be emitted **without a constraint
name**; pgpushy MUST NOT synthesize one.

The reason is idempotence (§11.1). The target holds an author-unnamed
constraint under the name *Postgres* generated for it. pgschema reads current
state from the target catalog, so any name pgpushy invents — however stable —
differs from the name already there, and every plan would show a drop and
recreate, forever. Emitting no name makes Postgres generate it inside
pgschema's plan database, by the same algorithm that named it on the target,
so the two agree by construction rather than by pgpushy imitating an internal
it does not own. Determinism (§11.3) is unaffected: the name is simply absent
from pgpushy's output.

Verified against Postgres 18: the inline and lifted forms generate identical
names for single-column, composite, quoted mixed-case, and 63-byte-truncated
cases, and for both collision cases (a competing foreign key, and a competing
constraint of another kind). One caveat follows from the same test — where two
constraints compete for a name, Postgres assigns the numeric suffix in
*creation* order. pgpushy MUST therefore detect the one configuration in which
that is observable; see §12.4.

> **Non-normative — why lift rather than sort.** A topological sort of tables
> by their foreign keys fails on cycles: two tables that reference each other
> have no linear creation order. FK-lift has no such failure mode, because
> table creation and constraint creation are separated. It was verified to
> produce a plan byte-identical to correctly-ordered inline input, and to
> handle same-schema cycles that no sort can. This is why pgpushy lifts
> unconditionally rather than sorting.

### 5.4 Object identity and qualification

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
bare `customers` — and likewise qualify the referents of foreign keys and the
targets of indexes, constraints, and comments. Objects whose schema a given run
does not target serve only to resolve references and are otherwise ignored by
that run.

pgschema builds its desired-state model by executing the file it is given,
resolving every reference from that file alone and **not** from the target
database. Verified: a cross-schema foreign key to a table absent from the
file fails (`relation … does not exist`) even when that table exists on the
target. Therefore the file passed to a given `pgschema --schema S` run MUST
contain every table referenced by a foreign key in `S`, including tables in
other schemas — the transitive closure of cross-schema foreign-key
references. §4.5 rejects a source tree that cannot satisfy this.

Whether pgpushy synthesizes **one combined document** reused for every schema,
or a **per-schema document** trimmed to that closure, is an implementation
detail that does not affect observable behavior or this spec. A single
combined document satisfies the closure requirement trivially and is the
RECOMMENDED default; per-schema trimming is a possible optimization for very
large databases (§14). Whichever is chosen, the closure requirement above is
normative.

> **Non-normative.** The combined-document approach was verified end-to-end:
> one synthesized file, `pgschema … --schema S` per schema, correct per-schema
> plans and idempotent convergence. A `billing`-only file omitting a
> referenced `public.customers` failed to build desired state — the evidence
> behind the closure requirement. Note that the combined document is executed
> in full by every per-schema run, so plan-database work scales with the
> product of schema count and total DDL size; this is the cost the trimming
> optimization would address.

### 5.5 Behavior preservation

The synthesized desired state MUST describe exactly the schema the source
tree describes (G4). pgpushy performs exactly four transformations, and each
is behavior-preserving — they change the textual order, the attachment site,
and the spelling of names, never the resulting schema:

1. **FK-lift** (§5.3) — relocates a foreign key from a table definition to a
   trailing `ALTER TABLE`.
2. **Constraint-name omission** (§5.3) — drops a name the author never wrote,
   leaving Postgres to generate the same name it already generated.
3. **Qualification** (§5.4) — rewrites each identifier to name the schema the
   object was already resolved to.
4. **Schema-declaration emission** (§5.2) — adds `CREATE SCHEMA IF NOT EXISTS`
   for schemas the desired state requires.

pgpushy MUST NOT perform any other transformation. In particular it MUST NOT
add constraint attributes the author did not write (for example, it MUST NOT
inject `NOT VALID`), because such attributes are part of the schema's meaning,
not of its ordering.

## 6. Target Inspection

Before delegating, `plan` and `apply` MUST perform a single **read-only**
inspection of the target over pgpushy's own connection (§6.4). `validate` MUST
NOT connect at all.

The inspection gathers what §6.1–§6.3 need in one round trip. Two of those
checks conclude immediately and MUST abort before pgschema is invoked at all;
the third (§6.2) needs the plans, so it reads the target here and concludes
after the plan pass — still before any change reaches the target. Every check
MUST name everything that is wrong rather than only the first problem found
(G5).

A consequence of this section is that **pgpushy issues no DDL of its own to
the target** in 0.x: schema *and* content changes all flow through pgschema
(satisfying G3), and every query pgpushy issues directly is read-only, so
`plan` cannot mutate the target even incidentally.

### 6.1 Every managed schema must exist

pgschema cannot reconcile a schema absent from the target: reading current
state for a nonexistent schema fails, and an external plan database does not
change this — current state is always read from the target (verified). An
**existing but empty** schema reconciles normally.

For 0.x, pgpushy therefore makes schema existence a **hard precondition**:
every managed schema MUST already exist on the target. pgpushy MUST fail,
naming *every* missing schema, if any is absent.

Note that the synthesized desired state still emits `CREATE SCHEMA IF NOT
EXISTS` for each managed schema (§5.2); that executes in pgschema's **plan
database** to make qualified references resolvable, never against the target.

> **Non-normative.** Creating managed schemas is thus the operator's
> responsibility in 0.x (a one-time `CREATE SCHEMA` per new schema). Automatic
> handling of absent schemas — reporting a new schema and its would-be
> contents without mutating the target — is deferred (§14, option B). pgpushy
> also never *drops* a schema (§12.3).

### 6.2 Cross-schema foreign-key removals

pgpushy processes schemas in the order §7 derives from the **desired** state:
a referenced schema before the schema referencing it. That order is correct for
*creating* a cross-schema foreign key and is the reverse of what *removing* the
thing one points at requires.

Which removals actually break was established empirically against pgschema
1.12.0 and Postgres 18, and is narrower than it first appears, because
pgschema does not generate every drop the same way:

| The referenced schema's plan drops | pgschema emits | Referenced-schema-first |
|---|---|---|
| the referenced **table** | `DROP TABLE … CASCADE` | **succeeds** — the CASCADE takes the dependent foreign key with it |
| the referenced **column** | `ALTER TABLE … DROP COLUMN c` | **fails**: `cannot drop column … because other objects depend on it` |
| the referenced **unique or primary key constraint** | `ALTER TABLE … DROP CONSTRAINT k` | **fails**: `cannot drop constraint … because other objects depend on it` |

So dropping the whole table is safe in any order, and the hazard is precisely a
plan that removes a *column* a cross-schema foreign key points at, or the
unique constraint that foreign key depends on, while leaving the table in
place. Applying the referencing schema first resolves it — the foreign key goes
before the thing it depends on — which is exactly the order §7 does not
produce.

pgpushy MUST detect this and refuse before applying anything. Detection uses
the plan pass §8.6 already performs, and needs no diffing of its own (G3): for
each cross-schema foreign key the target holds, if the *referenced* schema's
plan contains a drop of the column or the constraint that foreign key depends
on, **and** the referencing schema is ordered after it, pgpushy MUST fail,
naming the constraint, both schemas, the object being dropped, and the two-step
resolution — remove the foreign key in one apply, then remove what it pointed
at in the next.

> **Non-normative.** Reordering the affected pair automatically is possible in
> principle, but a run that both creates and removes cross-schema references
> between the same two schemas has no single valid order at all, so refusing is
> the honest general answer; §14 records reordering the unambiguous cases as
> future work. Note also that the `DROP TABLE … CASCADE` above will remove
> dependent objects pgpushy does not manage — a foreign key from an unmanaged
> schema, for instance. That is pgschema's behavior and is no different without
> pgpushy in front of it.

### 6.3 Target identity

pgpushy inspects the target over its own connection while pgschema connects
independently. If the two resolve their connection settings differently — an
ambient `PGSERVICE`, a `.pgpass` entry, a default applied by one library and
not the other — pgpushy would report a clean inspection of one database while
pgschema reconciles another.

pgpushy MUST make this divergence impossible by construction rather than
detecting it after the fact: it MUST resolve every connection parameter itself
(§6.4) and pass all of them explicitly to pgschema, so that pgschema performs
no independent resolution. Where a parameter cannot be passed as a flag, it
MUST be supplied through the subprocess environment. pgpushy MUST NOT allow an
ambient variable in its own environment to reach pgschema unresolved.

pgpushy SHOULD additionally record the identity of the database it inspected —
database name, server address and port, and the cluster's system identifier —
and report it in `plan` and `apply` output, so that the target is visible in
the record of any change.

### 6.4 Connection resolution

pgpushy MUST resolve its connection from the sources and precedence of §10
into a single libpq-compatible connection string, and MUST rely on that
string's standard interpretation — including `.pgpass`, `sslmode`, and
`PGSERVICE` — rather than reimplementing those semantics. The resolved
parameters are what §6.3 forwards to pgschema.

This is the only place pgpushy accesses the target directly, and every
statement it issues there MUST be read-only.

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
are handled within pgschema (§5.3). Ties MUST be broken deterministically —
by schema name — so that the order is reproducible (§11.3).

If the graph contains a cycle (a **cross-schema foreign-key cycle**), no valid
apply order exists. pgpushy MUST report it, naming every schema in the cycle
and the foreign keys that form it. The consequence differs by command (§8.2):
`apply` and `validate` fail; `plan` reports the cycle and continues, because a
cycle does not prevent pgschema from *computing* per-schema plans and the
plans are what the operator needs in order to break it. See §12.1.

The graph is built from the desired state alone; §6.2 covers the removal case
the desired state cannot express.

## 8. Delegation to pgschema

### 8.1 Invocation

For each managed schema `S`, in the order of §7, pgpushy invokes the
`pgschema` binary with the subcommand corresponding to the pgpushy command
(§8.2), passing `--schema S` and `--file <synthesized desired state>`, plus
the connection and pass-through flags of §8.3.

pgpushy obtains the `pgschema` binary through a provider (§8.5). If no usable
binary can be resolved, pgpushy MUST fail with an actionable message.

### 8.2 Commands

- **`pgpushy validate`** — run the offline pipeline (§3, stages 1–6) and
  report. It MUST NOT connect to any database, so it is usable in a
  pre-commit hook and in CI with no Postgres service. It MUST report the
  managed-schema set, the file and object counts, the exclusions applied
  (§4.1), and the schema apply order, and MUST fail on any §4.3, §4.5, or §7
  condition. It SHOULD accept an option to write the synthesized desired state
  to a path for inspection.

- **`pgpushy plan`** — run the offline pipeline, inspect the target (§6), then
  run `pgschema plan --schema S` for every managed schema and present each
  schema's plan. This never modifies the target: pgschema `plan` reads the
  target read-only, its scratch objects live in the plan database, and pgpushy
  itself issues no target DDL (§6). A cross-schema foreign-key cycle is
  reported but does not suppress the plans; the command exits non-zero (§7).

- **`pgpushy apply`** — run the offline pipeline, inspect the target, then run
  `pgschema apply --schema S` for every managed schema in dependency order
  (§7). Approval is governed by §8.6. `apply` MUST abort before touching the
  target if synthesis, any §4 or §7 check, the target inspection, or any plan
  in the preceding plan pass fails.

`plan` and `apply` MUST fail before delegating if any managed schema is absent
from the target (§6.1).

### 8.3 Pass-through and owned flags

Connection and apply-tuning flags MUST be forwarded to pgschema, including at
least: `--host`, `--port`, `--db`, `--user`, `--password`, `--sslmode`,
`--lock-timeout`, and the `--plan-*` family. Standard `PG*` environment
variables MUST be honored as pgschema honors them, subject to §6.3: pgpushy
resolves them and forwards the resolved values, rather than letting pgschema
resolve them independently.

pgpushy **owns** the following, which MUST NOT be settable by the user on the
pgschema invocation:

- `--schema` — pgpushy loops over the managed set (§4.4).
- `--file` — pgpushy synthesizes the desired state (§5).
- `--auto-approve` — pgpushy always passes it, because approval happens once
  at the pgpushy level (§8.6). `pgpushy apply --auto-approve` controls
  pgpushy's own prompt, not pgschema's.

### 8.4 Unmanaged schemas

pgpushy reconciles exactly the managed-schema set (§4.4). Schemas present in
the target database but absent from that set are neither planned nor modified
nor dropped (§6.1, §12.3).

### 8.5 Resolving the pgschema binary

pgpushy obtains `pgschema` through a **provider** with two backends. The
managed backend is the intended default; the BYO backend is permanent (see
Rollout).

**Managed backend.** pgpushy downloads a pinned pgschema version — the version
its release was tested against (§13), overridable via configuration (§10) — from
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
explicit path or a `PATH` lookup (§10). In this mode pgpushy MUST read the
binary's version and enforce the minimum (§13). Version is read by parsing the
`Version:` line of `pgschema --help`; there is no machine-readable version
output. A version **below the floor** MUST be a hard error naming both the
found and the required version, with no override (§13). An **unparseable**
version line MUST be a warning, not a failure — the line is a human-readable
string, not a stability contract. BYO is the only backend available on
**Windows** (no pgschema binary is published) and in **air-gapped**
environments, and is therefore a permanent part of pgpushy, not a temporary
measure.

**Rollout.** The first 0.x release ships the **BYO backend with the version
check** only. The managed backend is a fast-follow and becomes the default when
it lands; the provider seam keeps that change additive.

### 8.6 Approval

`apply` reconciles several schemas in sequence and is not atomic across them
(§11.2), so approval MUST be sought once, for the database, before any schema
is touched — not per schema as each apply begins.

pgpushy MUST:

1. run a full `plan` pass over every managed schema first, **retaining each
   plan** (pgschema writes one as JSON with `--output-json`);
2. present those plans together as one reviewable unit, including a summary of
   how many schemas change, and MUST call out destructive changes and any
   schema being reconciled to an empty desired state (§4.4);
3. perform the §6.2 check against those plans, and refuse if it finds a
   cross-schema removal the apply order cannot satisfy;
4. state that apply is not atomic across schemas;
5. prompt once, and abort without touching the target if the answer is no;
6. on approval, apply **the plans it just showed** — `pgschema apply --plan
   <plan.json>` — rather than recomputing them from the desired state.

Step 6 matters for more than efficiency: it makes the change that is applied
the same one that was reviewed, closing the window in which the target could
drift between the plan pass and the apply. pgschema fingerprints the state a
plan was computed against and refuses a plan whose target has since changed
(verified), so that window fails loudly rather than silently applying something
nobody approved.

A summary of "how many schemas change" and which changes are destructive is
read from those plans — each step carries its own operation — and not derived
by pgpushy comparing anything, which would be reimplementing the diffing G3
reserves for pgschema.

`pgpushy apply --auto-approve` skips step 5 for non-interactive use. When
standard input is not a terminal and `--auto-approve` was not given, pgpushy
MUST fail rather than proceed unapproved.

## 9. Failure Handling

`apply` MUST stop at the first schema whose apply fails; it MUST NOT continue
with later schemas, whose success may depend on the failed one (§7).

On any partial application, pgpushy MUST report which schemas were applied,
which failed, and which were not attempted, and MUST make clear that the
applied schemas are not rolled back (§11.2).

## 10. Configuration

pgpushy MUST be usable with CLI flags and `PG*` environment variables alone; a
configuration file is optional convenience, never required.

**File.** pgpushy reads an optional **`pgpushy.toml`** from the current working
directory. It is not searched for in parent directories; `--config <path>`
supplies an explicit path from anywhere. It MAY hold:

- **Project structure** — the source-tree root, the default schema (§4.4), and
  the `exclude` glob patterns (§4.1).
- **`managed_schemas`** (§4.4) — the authoritative managed-schema declaration.
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

Rich configuration (per-schema overrides, multiple target environments) is
future work (§14) and MUST NOT be required for the core workflow.

## 11. Properties

### 11.1 Idempotence

Applying an already-reconciled database MUST produce no changes: every
per-schema `pgschema plan` is empty, and `pgpushy apply` is a no-op. (Verified
for the multi-schema loop in the design spike.) §5.3's constraint-name rule
exists to preserve this property for author-unnamed foreign keys.

### 11.2 Atomicity

pgschema applies each schema in its own transaction; pgpushy does **not** wrap
the whole database in one transaction. A failure partway through `apply`
therefore leaves already-processed schemas applied and the rest unapplied
(§9). The plan pass and approval gate of §8.6 reduce, but do not eliminate,
partial application.

> **Non-normative.** Cross-database or all-or-nothing multi-schema application
> is not offered; it is not achievable through per-schema pgschema
> invocations. Deployments needing all-or-nothing semantics should treat a
> partial failure as a fix-forward condition.

### 11.3 Determinism

Given the same source tree, pgpushy MUST synthesize byte-identical desired
state across runs and platforms: category ordering (§5.1) and intra-category
ordering MUST be deterministic functions of the source content, never of
filesystem enumeration order (§4.1). The schema apply order (§7) MUST likewise
be deterministic. This keeps plans stable and reviewable.

## 12. Limitations

### 12.1 Cross-schema foreign-key cycles

Two managed schemas whose tables reference each other cannot be applied:
pgschema applies each schema separately and cannot defer a foreign key across
schemas, so no schema order works. pgpushy detects this (§7); `apply` refuses
rather than applying a partial result, while `plan` still shows the plans for
diagnosis. Same-schema cycles are fully supported (§5.3). *(This is expected
to be rare; the common deployment shares one schema across services.)*

### 12.2 Cross-schema foreign-key removal

Removing, in a single change, a cross-schema foreign key *and* the column or
unique constraint it points at cannot be applied in one pass: creation order
and removal order are reverses of each other (§6.2). pgpushy detects this and
directs the operator to a two-step apply. Dropping the referenced **table** is
not affected — pgschema drops tables with `CASCADE`, which takes the dependent
foreign key with it.

### 12.3 Managed schemas must pre-exist; pgpushy issues no schema DDL

In 0.x every managed schema MUST already exist on the target (§6.1); pgpushy
neither creates nor drops schemas. Introducing a new schema requires a
one-time manual `CREATE SCHEMA` before pgpushy can manage it, and removing a
schema from management is likewise manual. Automatic handling of absent
schemas is deferred (§14, option B).

### 12.4 Colliding unnamed foreign keys

Two foreign keys on the same table over the *identical* column set, both left
unnamed by the author, compete for one generated name; Postgres gives the
second a numeric suffix, assigned in creation order (§5.3). pgpushy's emission
order (§11.3) is derived from source content and need not match the order in
which the target's constraints were originally created, so the two names can
attach to opposite constraints — which pgschema reads as two renames, and
plans on every run.

pgpushy MUST reject this configuration, naming both constraints and asking the
author to name them explicitly, which resolves it completely. The check is
exact: it applies only when the column sets are identical and both names are
absent. Foreign keys differing in any column, or either one named, are
unaffected.

> **Non-normative.** Two unnamed foreign keys over the same columns to
> different tables is a pathological schema; the check exists because the
> failure is silent and permanent rather than because the case is common.

### 12.5 Object scope

pgpushy 0.x manages tables, indexes, table constraints, foreign keys, and
comments (§4.3). A source tree containing any other object kind — a view, a
function, a trigger, a user-defined type — is rejected, not partially managed.

This is a starting point, not the destination. **The goal is parity with the
statement set pgschema itself supports** (§14): `CREATE TABLE`, `CREATE INDEX`,
`CREATE VIEW`, `CREATE MATERIALIZED VIEW`, `CREATE FUNCTION`,
`CREATE PROCEDURE`, `CREATE AGGREGATE`, `CREATE TRIGGER`, `CREATE TYPE`,
`CREATE DOMAIN`, `CREATE SEQUENCE`, `CREATE POLICY`, `COMMENT ON`,
`GRANT`/`REVOKE`, and `ALTER DEFAULT PRIVILEGES`. Anything pgschema cannot
manage — `CREATE EXTENSION`, `CREATE ROLE`, `CREATE SCHEMA` itself — is
permanently out of scope for pgpushy too, since pgpushy delegates the work.

Widening is staged by how much new machinery each kind needs, not by how
useful it is (§14).

## 13. Dependencies and Compatibility

- **pgschema** — required at runtime, resolved through the provider (§8.5):
  downloaded by the managed backend or supplied by the operator (BYO). The
  minimum supported version is **the version pgpushy is tested against** —
  currently **v1.12.0** — expressed as a `>=` floor that tracks pgpushy's CI
  matrix and rises as newer releases are tested; newer-than-floor is accepted.
  The floor is not overridable: a below-floor binary is a hard error, and the
  remedy is to upgrade pgschema or use the managed backend. The relied-upon
  behavior (foreign-key deferral, deterministic cycle-breaking, PR #156) is
  technically present from **v1.4.2** (2025-11-14), which bounds how far the
  floor could be lowered later with testing, but is not itself the supported
  floor. The BYO backend enforces the floor (§8.5); the managed backend
  controls the version and so needs no check.
- **libpg_query** (via the `pg_query` crate) — used for parsing (§4.2).
- **A Postgres client driver** — used for the read-only target inspection
  (§6). pgpushy connects to the target directly, in addition to the
  connections pgschema makes.
- **Postgres** — the target and pgschema's plan database. Minimum version
  follows pgschema's own requirement.

## 14. Future Work (non-normative)

- **Absent-schema handling (option B)** — lift the §6.1 precondition by, for a
  managed schema missing from the target, reporting it as new and its contents
  as fully-created (without mutating the target), rather than failing. This is
  the preferred future direction over having pgpushy create schemas itself.
- **Parity with pgschema's statement set** (§12.5) — the main direction of
  travel. pgschema's own `dump` orders its output by category — *types → tables
  → views → functions → indexes* — and its documentation states that "most
  objects resolve regardless of order, but some objects depend on others
  existing at creation time". That is the same mechanism as §5.1, which is why
  widening is mostly adding categories rather than redesigning synthesis.
  Staged by the machinery each kind needs:

  1. **Sequences, types, domains.** Structured names that qualify exactly as a
     table's does, and no body referencing other objects. Category additions
     and nothing more.
  2. **Functions, procedures, aggregates, triggers, policies.** pgschema treats
     function bodies as opaque dollar-quoted text rather than parsing them, and
     Postgres does not resolve a plpgsql body at creation time — so the name is
     qualified and the body passes through byte-for-byte. Trigger and policy
     references are structured AST fields. One question to settle first:
     SQL-standard `BEGIN ATOMIC` bodies (PG14+) *are* resolved at creation,
     unlike dollar-quoted ones.
  3. **Views and materialized views.** A view's query *is* resolved at creation
     time, so unqualified references inside it are a live problem, and views
     need a topological sort *within* their category, because a view over a
     view is a genuine creation-time dependency that category order cannot
     express. Before building an AST-walking qualifier — which would have to
     track scope, since CTE names and subquery aliases are `RangeVar`s too and
     qualifying those would break the view — settle whether a **per-schema
     document** removes the need. With objects of schema `S` qualified as `S`,
     which pgschema strips, an unqualified reference inside a view body would
     resolve to the scratch schema standing in for `S`, which is the correct
     answer. If that holds, only genuinely cross-schema references need
     rewriting, and an author must write those qualified regardless.
  4. **`GRANT`/`REVOKE` and `ALTER DEFAULT PRIVILEGES`.** Permissions rather
     than shape; needs a decision on how they attribute to the managed-schema
     set, since a grant is not owned by a schema the way a table is.
- **References into unmanaged schemas** — for foreign keys targeting schemas
  pgpushy does not manage (extension schemas, an externally-owned `auth`,
  etc.), support pgschema's **external plan database** seeded with those
  external objects. §4.5 rejects these today. Not needed for cross-schema
  references *among managed schemas* (the combined document already covers
  those, §5.4).
- **`pgpushy dump`** — the inverse: read an existing database and emit a
  per-object source tree, to bootstrap adoption.
- **Cross-schema FK cycle support** (§12.1). **Single-pass cross-schema FK
  removal** (§12.2) has a cheaper route than the cycle case: where a run only
  *removes* references between a pair of schemas and adds none, the pair can
  simply be applied in the reverse order, with no target DDL from pgpushy at
  all. Refusing is only the general answer, for runs that both add and remove
  between the same pair.
- **Per-schema synthesis** trimmed to the cross-schema closure (§5.4), for
  databases where re-executing the full document per schema is too costly.
- **Richer configuration** (§10): per-schema overrides, multiple environments.
- **Schema-drop management** (§12.3), behind an explicit opt-in.

## 15. Decision Log

All decisions identified for 0.x are resolved. Decisions marked **[0.2]** were
made after draft 2 of v0.1.

- **Object scope for 0.x** — tables, indexes, table constraints, foreign keys,
  and comments. **[0.2]** Anything else is **rejected** with a diagnostic
  rather than passed through, because pgpushy cannot qualify the interior of a
  statement it does not model, and §5.4 makes qualification normative.
  (§4.3, §12.5)
- **Schema-assignment mechanism** — schema-qualify **every** emitted
  identifier with its resolved schema, including `public`; an unqualified
  object would be misattributed to every schema the combined file is run
  against (verified). Synthesis-file granularity remains an implementation
  detail; the combined document is the recommended default. (§5.4)
- **Absent schemas / `plan` mutation** — 0.x makes schema existence a hard
  precondition, checked read-only, else pgpushy fails before delegating.
  pgpushy issues no DDL of its own to the target, and `plan` cannot mutate it.
  (§6.1, §8.2)
- **Cross-schema FK cycles** — detected and rejected. **[0.2]** Fatal for
  `apply` and `validate`; `plan` reports the cycle, still shows the plans, and
  exits non-zero, because the plans are what the operator needs to break the
  cycle. (§7, §12.1)
- **[0.2] Cross-schema FK removal** — detected before apply and rejected with
  a two-step resolution, rather than failing mid-apply on a Postgres dependency
  error. Verified against pgschema 1.12.0: the hazard is *not* dropping the
  referenced table, which pgschema does with `CASCADE`, but dropping the
  referenced **column** or **unique constraint**, which it does without. The
  check reads the drop steps of the plan pass rather than diffing anything.
  (§6.2, §12.2)
- **[0.2] Constraint naming** — author-unnamed foreign keys are emitted with
  **no name**, so Postgres generates the same name in the plan database that
  it generated on the target. pgpushy does not synthesize constraint names.
  Verified against Postgres 18 across single-column, composite, quoted,
  truncated, and both collision cases. The one residual hazard — two unnamed
  foreign keys competing for a name, whose suffix follows creation order — is
  detected and rejected. (§5.3, §12.4)
- **[0.2] Approval** — one database-level approval after a full plan pass;
  pgschema is always invoked with `--auto-approve`. Declining touches nothing.
  (§8.6)
- **[0.2] Offline validation** — a `pgpushy validate` command runs the whole
  offline pipeline with no database connection. (§8.2)
- **[0.2] Managed-schema set** — derived from the source tree by default; an
  optional `managed_schemas` declaration in `pgpushy.toml` is authoritative
  when present, turning accidental enlistment into an error and providing the
  only way to express a managed-and-empty schema. (§4.4)
- **[0.2] File exclusion** — `exclude` glob patterns in `pgpushy.toml`, needed
  because the strict allow-list makes a stray seed or fixture file fatal.
  (§4.1)
- **[0.2] Target connection** — pgpushy needs its own Postgres driver for the
  read-only inspection. It resolves every connection parameter itself and
  passes them explicitly to pgschema, making divergent resolution impossible
  by construction rather than detecting it afterwards. (§6.3, §6.4, §13)
- **pgschema version & resolution** — floor is the tested version
  (`>= v1.12.0` today), tracking pgpushy's CI matrix, **not overridable**.
  pgpushy resolves the binary through a provider: managed download (intended
  default, pinned + SHA-256-verified) with a permanent BYO override that
  parses `pgschema --help` and enforces the floor. First 0.x release ships
  BYO + version check; managed is a fast-follow. (§8.5, §13)
- **Configuration file** — optional `pgpushy.toml` at the **current working
  directory** (**[0.2]** not searched upward; `--config` for an explicit
  path), holding project structure, `managed_schemas`, exclusions,
  pgschema-provider, and connection defaults; precedence CLI > env > file >
  default. `password` is permitted in the file but triggers a prominent
  warning when it is the effective source. (§10)
- **[0.2] `CREATE SCHEMA` forms** — only the bare
  `CREATE SCHEMA [IF NOT EXISTS] <name>` form is accepted; the nested-element
  form and the `AUTHORIZATION`-only form are rejected. (§4.3)
