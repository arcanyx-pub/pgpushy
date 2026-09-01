# pgpushy Specification

**Version:** 0.7
**Date:** 2026-08-31
**Status:** All design decisions resolved (§15).

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
  machinery for schema changes beyond what it passes through to pgschema
  (§8.3). Seed execution (§8.8) is the one deliberate exception, and carries
  its own safety machinery: one transaction per file, and a convergence probe.
- pgpushy is not a general SQL build system. It manages the object kinds in
  the statement allow-list (§4.3), plus baseline rows through the seed file
  class (§4.6), and rejects everything else.

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
  Defaults to `public`; configurable (§10.1).
- **Desired state** — the synthesized SQL documents pgpushy produces from the
  source tree: one per managed schema, each passed as `--file` to that
  schema's pgschema run (§5).
- **FK-lift** — the transform that moves foreign-key constraints out of table
  definitions into trailing `ALTER TABLE … ADD CONSTRAINT` statements (§5.3).
- **Cross-schema foreign key** — a foreign key whose referencing table and
  referenced table are assigned to different schemas.
- **Closure member** — an object from another schema that appears in a
  schema's document only so that references into it resolve. It is never
  diffed, only executed (§5.4).
- **Target database** — the Postgres database being reconciled.
- **Seed file** — a `*.sql` file under the seed root (§4.6) holding idempotent
  baseline rows. Not desired state: never synthesized, never shown to
  pgschema; executed by pgpushy itself after apply (§8.8).
- **Generated source** — a source or seed file written by `pgpushy generate`
  from a configured command (§4.7), carried in the repository like any
  hand-written file.

## 3. Overview

pgpushy executes the following pipeline. Each stage is specified in the
referenced section.

```
   offline — requires no database connection
1. Discover      walk the source tree, apply excludes → ordered files   (§4.1)
                 walk the seed root, when configured → ordered seeds    (§4.6)
2. Parse         parse each file; enforce the statement allow-list      (§4.2, §4.3)
                 parse each seed; enforce the seed allow-list           (§4.6)
3. Resolve       assign schemas; derive or verify the managed set       (§4.4)
4. Check         duplicate objects; unresolvable foreign-key referents  (§4.5)
                 seed targets, columns and conflict targets, against    (§4.6)
                 the model
5. Order         cross-schema FK graph; topological order; cycles       (§7)
6. Synthesize    FK-lift + qualify + category emission → one document   (§5)
                 per managed schema

   online — requires the target database
7. Inspect       one read-only target query (§6)
8. Delegate      pgschema plan|apply --schema S --file <S's document>,  (§8)
                 once per managed schema, in the order from stage 5
9. Seed          execute each seed file, prove convergence, commit      (§8.8)
                 (apply only)
```

`pgpushy validate` (§8.2) runs stages 1–6 and stops; it never connects to
anything. `plan` runs stages 1–8; only `apply` reaches stage 9. `pgpushy
generate` (§8.2) sits upstream of stage 1: it writes files that discovery then
reads, and no other command executes a configured generator (§4.7).

Stage 6 produces **one document per managed schema**, not one document reused
for every schema. Each document holds that schema's objects plus the closure
of what they reference elsewhere (§5.4). The two are not interchangeable: a
schema reference inside a *string literal* must be spelled differently
depending on which schema's run reads it, so no single document can be correct
for all of them (§5.4).

## 4. Source Tree

### 4.1 Discovery

pgpushy MUST recursively discover every file ending in `.sql` (case-insensitive)
under the source-tree root. Directory structure carries no ordering or
semantic meaning; it is organizational only (G1). pgpushy MUST NOT require any
manifest, index, or include file.

Hidden files and directories (names beginning with `.`) MUST be ignored.
Symbolic links to **directories** MUST NOT be followed, so that discovery
terminates and cannot escape the source tree. Symbolic links to **files** MUST
be followed: they introduce neither risk, and a `.sql` file silently missing
from the desired state is a file scheduled for deletion.

Discovery MUST yield a deterministic order: files are sorted by their
source-root-relative path, compared as byte sequences, without locale
collation. Nothing in the synthesized output depends on discovery order
(§5.1), but a deterministic order makes diagnostics and reporting stable
(§11.3).

**Exclusions.** Configuration (§10.1) MAY supply a list of glob patterns matched
against source-root-relative paths. A file matching any pattern is not
discovered and is never parsed. pgpushy MUST report the number of excluded
files, and SHOULD report the count matched by each pattern, so that an
over-broad pattern silently swallowing real tables is visible rather than
mysterious.

**Generated documents.** pgpushy MUST begin every document it writes with a
generated-document marker, and MUST skip any discovered file that begins with
it, reporting the count. Writing the synthesized documents into the source
root with `--out` (§8.7) is a natural thing to do, and without this every
later run would report every object in them as a duplicate of itself — a
failure that outlives the run that caused it and gives no hint where the extra
files came from.

This marker marks pgpushy's **output**. The output of `pgpushy generate`
(§4.7) is **input** — a source file like any other — so it carries a distinct
generated-*source* marker that discovery treats as ordinary content. The two
markers MUST be distinguishable from a file's opening bytes, because they
demand opposite behavior: one exists so pgpushy never re-reads what it wrote,
the other so `generate` knows which files it may overwrite (§4.7).

An empty source tree is not an error; it describes an empty database.

### 4.2 Parsing

Each source file MUST be parsed with a Postgres-grammar parser (the
`pg_query` crate, wrapping libpg_query — the real server parser) so that
pgpushy understands statements structurally rather than textually. Parse
failures MUST abort the run, naming the file and the location.

### 4.3 Statement allow-list

pgpushy 0.1 manages tables, indexes, foreign keys, user-defined types,
domains and standalone sequences. A source file MUST contain only the
following statements:

| Statement | Category (§5.1) |
|---|---|
| `CREATE SCHEMA [IF NOT EXISTS] <name>` | 1 |
| `CREATE TYPE` | 2 |
| `CREATE DOMAIN` | 2 |
| `CREATE SEQUENCE` (standalone) | 2 |
| `CREATE TABLE` (with inline constraints) | 3, FKs lifted to 5 |
| `CREATE [UNIQUE] INDEX` | 4 |
| `ALTER TABLE … ADD CONSTRAINT` — foreign key | 5 |
| `COMMENT ON` a table, column, index or sequence | 6 |

Any other statement MUST be a hard error naming the file, the line, and the
statement kind, and directing the reader to §14 for the object kinds under
consideration for later versions. This includes, non-exhaustively: `CREATE
VIEW`, `CREATE FUNCTION`, `CREATE TRIGGER`, `CREATE POLICY`, `CREATE
EXTENSION`, `GRANT`/`REVOKE`, every `DROP`, and all DML.

**`ALTER` is not a declarative statement, with one exception.** A source file
says what exists, not what steps reach it, so every `ALTER` form MUST be
rejected — including `ALTER TABLE … ADD CONSTRAINT` for a `CHECK`, `UNIQUE`,
`PRIMARY KEY` or `EXCLUDE` constraint, which MUST instead be written inline in
its `CREATE TABLE`. Verified against Postgres 18: an inline table constraint
can carry an explicit name, so nothing is lost but the spelling.

The exception is `ALTER TABLE … ADD CONSTRAINT` for a **foreign key**, which
is accepted because it is pgpushy's own canonical output form — §5.3 lifts
every foreign key into exactly that shape — and because it is what `pg_dump`
emits, so a source tree derived from one needs no rewriting there. Accepting
it costs nothing: foreign keys are category 5, so they cannot reintroduce a
creation-time dependency into any earlier category.

Rejecting the non-foreign-key forms also removes the only way a category-4
object could depend on another (§5.1). `ALTER TABLE … ADD CONSTRAINT …
UNIQUE USING INDEX i` requires `i` to exist first; with it gone, category 4 is
`CREATE INDEX` alone, and no two indexes can depend on each other.

Two further properties motivate rejection rather than pass-through:

- **Qualification cannot be honored.** §5.4 makes schema-qualifying every
  emitted identifier normative, because an unqualified identifier is
  misattributed to whichever schema's run reads it. pgpushy can qualify
  identifiers in statements it models structurally. It cannot qualify the
  interior of a statement it does not model — a table reference inside a view
  body, say — so passing such a statement through would emit exactly the
  construct §5.4 forbids.
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

Five further restrictions apply within the allowed statements, each for the
same reason the allow-list exists at all:

- `CREATE TABLE … INHERITS`, `… PARTITION OF`, `… PARTITION BY`,
  `CREATE TABLE … OF <type>`, and `CREATE TABLE … (LIKE <table>)` MUST be
  rejected. Each makes a table's *creation* depend on another table, and
  FK-lift resolves table-to-table foreign-key ordering only — this is exactly
  the class of dependency §12.5 keeps out of 0.1. `LIKE` is the easiest of the
  five to miss, because it hides inside the column list rather than in a
  clause of its own, and category 3 is emitted in name order: a `LIKE` that
  sorts before the table it copies produces a document that cannot execute.
- `CREATE SEQUENCE … OWNED BY` MUST be rejected. It makes a sequence's
  creation depend on a table, inverting category 2 and category 3, and it is
  not a shape pgschema round-trips: pgschema models a column-owned sequence as
  `SERIAL` rather than as an object of its own, so an explicitly-owned
  sequence does not survive a dump-and-reapply (verified against pgschema
  1.12.0 — the standalone sequence is dropped and an owned one created in its
  place). An owned sequence is spelled `serial` or `GENERATED … AS IDENTITY`.
- **`COMMENT ON` a type, a domain, a schema or a table constraint** MUST be
  rejected, though a comment on a sequence is accepted. pgschema generates no
  DDL for any of the four: verified against pgschema 1.12.3 by applying and
  re-planning, it emits the sequence's comment, omits the others, applies
  everything else, and then reports no changes — so the comment never reaches
  the target and nothing ever says so. A comment that silently does not exist
  is worse than one that is refused (§12.9).
- A **default calling `nextval`** — on a column or on a domain — MUST be
  rejected. pgschema models any such default as `SERIAL`: verified against
  pgschema 1.12.0, applying `CREATE SEQUENCE s` together with a column
  defaulting to it creates a sequence *owned by that column* instead, never
  creates `s`, reports success, and leaves every later plan showing the same
  drop and add. A **domain** default calling `nextval` fails outright, because
  pgschema applies domains before sequences. Neither is something pgpushy can
  order around — the apply order is pgschema's. The remedy is `serial` or
  `GENERATED … AS IDENTITY`; a sequence nothing defaults to is managed normally
  (§12.8).
- A schema-qualifying name inside a **string literal** — `'x'::regclass`,
  `'x'::regtype` — MUST name its schema explicitly; a bare name MUST be
  rejected. pgpushy does not infer a schema inside a literal. §4.4's rule for
  identifiers cannot simply be reused, because the two would then disagree
  silently the moment cross-schema references are supported: `nextval('s')`
  inside a table in `billing` legally means `public.s` under one reading and
  `billing.s` under the other, with no error either way. Requiring the schema
  keeps that choice open, and costs imported trees nothing — `pg_dump`
  qualifies inside literals already (verified), and `serial` and `IDENTITY`
  produce no literal at all.
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
  schema** (§10.1).

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

**Declared set.** Configuration (§10.1) MAY declare `managed_schemas`. When
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
  future work (§14), not a supported 0.1 configuration.
- **Cross-schema references other than foreign keys.** A foreign key is the
  only reference pgpushy 0.1 permits to cross a schema boundary. A column
  typed by a domain or type in another schema, and a default calling
  `nextval` on a sequence in another schema, MUST be errors naming the
  referring object, the referenced object, and both schemas.

  The restriction is what keeps the closure of §5.4 shallow and its rule
  uniform: a foreign key is not a creation-time dependency — FK-lift is what
  bought that — so it is the one reference kind that can cross a schema
  without dragging a transitive closure of another schema's objects behind
  it. Widening this is future work (§14) and is additive; the closure is
  specified over reference edges generally, not over foreign keys
  specifically.
- **Colliding unnamed foreign keys.** Two foreign keys on the same table over
  the identical column set, both left unnamed by the author, MUST be an error
  naming both. They compete for one generated name whose numeric suffix
  follows creation order, which pgpushy's emission order need not match; §12.4
  gives the full reasoning.

### 4.6 Seed files

A source tree describes shape. Some baseline **rows** are as load-bearing as
the shape that holds them: reference data the application requires in order to
function at all. The motivating example is a library that provisions its own
table — `snowdrop-id-postgres` cannot lease a machine ID until the lease table
in its `snowdrop` schema holds its 1024 seed rows — but every lookup table an
application joins against is the same situation. Provisioning such rows has
traditionally fallen either to a side script run by hand, or to the
application at boot (`auto_provision`-style), which needs DDL rights a
production application role should not hold. Both are
the kind of friction pgpushy exists to remove, so pgpushy owns the step — as a
**separate file class** with its own rules, not as a relaxation of §4.3, whose
reasons for rejecting DML are untouched: a statement in the desired state
executes inside pgschema's comparison model, and a seed file never goes
anywhere near it.

**Discovery.** Configuration MAY name a `seed_root` (§10.1); when it is
absent, there are no seed files and this section does not apply. Discovery
under the seed root follows §4.1's rules — recursive `*.sql`, hidden entries
ignored, the same symlink policy, the same deterministic byte-order — and its
files are seed files, never desired state; the `exclude` patterns of §4.1
apply to the source tree only, never under the seed root (§14). The two roots
MUST NOT be the same directory. A seed root inside the source root is excluded from desired-state
discovery, reported the way exclusions are; a source root inside the seed root
MUST be an error, because every schema file would then also be read as a seed
and rejected by the allow-list below.

**The seed allow-list.** Every statement in a seed file MUST be an `INSERT`
carrying an `ON CONFLICT` clause — `DO NOTHING` or `DO UPDATE` — and anything
else MUST be rejected, naming the file, the line and the statement kind, with
the `ON CONFLICT` form to add as the remedy for a bare `INSERT`. The rule is
idempotence by construction (§11.1): a bare `INSERT` is a duplicate row or a
unique-violation on the second apply, and either is this section's one
failure mode. Four refinements, each rejected with the remedy shown:

- **The source MUST NOT read the database**: `VALUES`, or a `SELECT` that
  references no table or view — a set-returning built-in such as
  `generate_series` is the useful case — and **no `WITH` clause at all**. A
  data-modifying CTE is a `DELETE` wearing an `INSERT`'s statement kind, and
  would quietly break §12.10's guarantee; even a read-only source query makes
  the seeded rows a function of target state rather than of the repository,
  the row-level version of the ambient input §4.7 rejects. `RETURNING` is
  rejected with the rest; nothing consumes it.
- **Expressions may call only built-in functions.** Under §8.8's empty
  `search_path` an unqualified function resolves in `pg_catalog` or not at
  all, so unqualified calls are safe; a call qualified with any other schema
  MUST be rejected — a user-defined function can do arbitrary work, including
  exactly the deletes §12.10 forecloses. User-defined **types and domains**
  MAY appear but MUST be schema-qualified, since an unqualified one cannot
  resolve at apply; a cast cannot write. validate enforces the rule for the
  types the tree defines; a bare name it cannot recognize fails inside the
  seed's transaction at apply, landing nothing.
- **`DO UPDATE` without a `WHERE` guard MUST be rejected.** Whenever the
  statement seeds a row at all, the probe pass finds every one of those rows
  conflicting, every conflicting row takes the update arm, and Postgres
  counts a row updated to its existing values as affected — so the pass
  cannot report zero. (A source yielding no rows converges vacuously, and
  seeds nothing.) The diagnostic MUST show the guard to add
  (`WHERE t.col IS DISTINCT FROM excluded.col`), which is also simply good
  practice: without it every apply rewrites every seeded row — dead tuples,
  WAL, and triggers firing over no-ops.
- **The column list MUST be explicit**, and MUST NOT name a column the model
  declares `GENERATED ALWAYS` — as identity, which this form cannot insert
  into without `OVERRIDING SYSTEM VALUE`, or as an expression, which nothing
  can insert into. An `INSERT` without a column list binds values to
  positions, and breaks silently the day the table gains a column; with one,
  validate can check every named column against the model.

**Qualification and the model.** The inserted-into table MUST be written
schema-qualified. Seed files are executed verbatim (§8.8) under an empty
`search_path`, so an unqualified name would fail at apply; validate rejects it
earlier, showing the qualified form. The table MUST be one the source tree
describes — assigned to a managed schema, present in the model. Seeding a
table pgpushy does not manage is writing rows into shape it cannot see, the
row-level version of what §8.4 exists to prevent — and the model is what makes
the offline checks possible at all. validate MUST check that the table and
every named column exist, and that the conflict target is checkable: a plain
column list equal — as a set; order carries no meaning — to the table's
primary key or to a unique constraint or unique index over exactly those
columns, or `ON CONFLICT ON CONSTRAINT` naming a modeled constraint.
`DO NOTHING` MAY omit the conflict target, arbitrating on any conflict, which
converges trivially; `DO UPDATE` cannot, and Postgres itself refuses it. A
conflict target validate cannot check statically — an expression target, or
one carrying a partial-index `WHERE` — MUST be rejected, with the
`ON CONSTRAINT` form as the remedy; admitting them is possible later if
wanted. validate SHOULD additionally warn when two `DO UPDATE` statements
anywhere in the tree share a table and conflict target: the per-file probe
cannot see two files converging against each other (§11.1, §12.11).

Seed files never enter synthesis, never appear in any document, are never
shown to pgschema, and never reach the plan database (§5, §8, §10.4). Their execution is specified in §8.8; what they
cannot do is stated in §12.10 and §12.11.

### 4.7 Generated sources

Some source files are owned by a dependency rather than an author. A library
that provisions its own table publishes the DDL — and the seed DML — as an
API, and the correct copy of that SQL is a function of the **pinned dependency
version**, not of anything a human maintains by hand
(`snowdrop-id-postgres::schema_sql()` is the motivating case). Vendoring the
output by hand works until the dependency is bumped, at which point the copy
is stale and nothing says so. `pgpushy generate` closes that loop.

**Configuration.** The file MAY declare generators (§10.1): each `[[generate]]`
entry names an `output` path — relative, `..`-free, resolved against the
configuration file's own directory without following symlinks — and a
`command`, an argv vector executed **without a shell**, so there is no
quoting or injection surface. The output MUST land under the source root or
the seed root and MUST be a file discovery will retain (`*.sql`, not hidden,
not excluded), because a generated file nothing will discover is a
configuration mistake. Two entries MUST NOT share one output. With no entries
configured, `generate` and `--check` succeed, and say so. The command runs with the configuration file's directory
as its working directory; its stdout is the file's content; its stderr passes
through; a nonzero exit or empty output MUST be an error.

**`pgpushy generate`** (§8.2) runs each configured command and writes its
output under a generated-**source** marker (§4.1) that names the command,
states that the file is not to be edited, and says how to regenerate it. It
MUST refuse to overwrite an existing file that does not begin with that
marker — `generate` can create a new file where none exists, but it can never
overwrite a file it cannot prove is its own: neither an operator's SQL nor a
§8.7 document. **`pgpushy generate --check`** re-runs every
generator, writes nothing, and MUST fail — naming each stale output, with
running `generate` as the remedy — if any output file differs from what would
be written or is absent; a failing command is its own error, not staleness. That check is the freshness
guarantee: run in CI, it forces a dependency bump that changes the emitted SQL
to land as a reviewed `.sql` diff in the same change.

**Vendored output is the only mode.** `validate`, `plan` and `apply` MUST NOT
execute a configured command; generation is upstream of discovery, and
everything downstream reads only files. Output that is not persisted is
ambient input: `plan` and `apply` could disagree when the tool changes between
them, review never sees the schema a dependency bump changed, and a persisted
plan (§14) stops being reproducible from the tree. This is the same family of
hazard as the working-directory `.pgschemaignore` (§8.4) and `PG*` overrides
(§10.2), and it gets the same answer. Running generators at plan or apply time
is listed in §14, to be built only against demonstrated need.

A command's output MUST be byte-stable for the same inputs; `--check` is the
enforcement.

> **Non-normative.** Point `command` at a tool the repository's own lockfile
> governs — `cargo run -p xtask` printing a dependency's published SQL, an
> `npm exec` script, a pinned container — rather than at a globally installed
> binary. The lockfile then pins the SQL's provenance, and `--check` fails the
> moment an upgrade changes it. Two operational notes: `generate` executes
> repository-configured commands, so CI should never run it on untrusted input
> (the `pull_request_target` hazard in
> [`github-action-sketch.md`](./github-action-sketch.md)); and removing a
> `[[generate]]` entry leaves its marked output behind as an ordinary source
> file — still discovered, still applied — so delete the file in the same
> change.

- **Qualified references into a managed schema that the tree does not
  define.** A column type, a domain base type, or a name literal written
  `S.name`, where `S` is managed but nothing in the tree defines `name`, MUST
  be an error: in the plan database a managed schema holds only what the
  source tree defines, so the reference cannot resolve there, and the failure
  would otherwise surface mid-plan-loop as pgschema's error (G5). An
  **unqualified** unknown name is left as written — `text` and an extension's
  type are indistinguishable offline — and a qualified reference into an
  unmanaged schema remains §12.6's and §14's business. For a literal, a
  tree-defined table or index also satisfies the reference, since `regclass`
  legitimately names either.

## 5. Desired-State Synthesis

pgpushy transforms the discovered objects into one desired-state document per
managed schema. This section defines those documents. §5.1 through §5.3 and
§5.5 describe how any one document is built; §5.4 defines which objects go
into which document, and how their names are spelled there.

### 5.1 Statement categories

Discovered statements are grouped into ordered categories. Every statement in
category *n* is emitted before every statement in category *n+1*.

| Order | Category | Contents |
|-------|----------|----------|
| 1 | Schemas | `CREATE SCHEMA IF NOT EXISTS` for every schema the document names (§5.2) |
| 2 | Types, domains and sequences | `CREATE TYPE`, `CREATE DOMAIN`, `CREATE SEQUENCE`, topologically sorted among themselves |
| 3 | Tables | `CREATE TABLE`, carrying their inline column, `CHECK`, `UNIQUE`, and `PRIMARY KEY` constraints — but not their foreign keys (§5.3) |
| 4 | Indexes | `CREATE INDEX` |
| 5 | Foreign keys | every foreign key as a trailing `ALTER TABLE … ADD CONSTRAINT` (§5.3) |
| 6 | Comments | `COMMENT ON` any object above |

Within a category, pgpushy MAY emit statements in any order that is valid for
that category; a stable, deterministic order (§11.3) is REQUIRED.

The categories exist because emission order is execution order: pgschema
executes this document, so a statement that references an object must follow
the statement creating it.

Categories 1, 3, 4 and 6 are internally order-free, each for a reason:

- **Category 3** — once foreign keys are lifted to category 5, no table
  definition depends on another. This is what FK-lift buys, and §4.3's
  rejection of `INHERITS`, `PARTITION OF`, `OF <type>` and `LIKE` is what
  keeps it true, since each of those is a table-to-table creation dependency
  FK-lift does not resolve.
- **Category 4** — an index depends only on its own table. §4.3's rejection of
  standalone `ADD CONSTRAINT` removes the one form that could depend on
  another category-4 object, `… ADD CONSTRAINT … UNIQUE USING INDEX`.
- **Category 6** — comments are emitted last, so they may reference anything.

**Category 2 is the exception, and is sorted.** A domain can be defined over
another domain, a composite type can have a domain-typed field, and a domain
default can call `nextval` on a sequence — so no fixed order among the three
kinds is correct in general, and pgschema's own `dump` order (types, then
domains, then sequences) is a convention rather than a guarantee. pgpushy
therefore orders category 2 topologically by creation-time dependency, with
ties broken by `(schema, name)` for determinism (§11.3). A cycle is impossible
here — Postgres will not create one — but if the sort encounters one, it MUST
be reported rather than emitted in an arbitrary order.

### 5.2 Schema declarations

pgpushy MUST emit `CREATE SCHEMA IF NOT EXISTS <s>` at the top of each
document for every schema that document names — its own target schema, and the
schema of every closure member in it (§5.4) — before any object that
references it. pgschema executes the desired state in its **plan database** to
build its comparison model, and a schema-qualified object requires its schema
to exist in that model; emitting the declarations makes qualified objects in
any schema resolvable. These declarations run only in the plan database —
never against the target, which must already contain the schemas (§6.1).

`CREATE SCHEMA` statements authored in the source tree are honored and MUST
NOT be treated as errors. pgpushy MAY normalize them to the `IF NOT EXISTS`
form.

### 5.3 The FK-lift transform

For every foreign-key constraint in the source tree — whether written inline
in a `CREATE TABLE` or as a standalone `ALTER TABLE … ADD CONSTRAINT` —
pgpushy MUST emit the constraint as a trailing `ALTER TABLE … ADD CONSTRAINT`
statement in category 5, and MUST NOT emit it inline in category 3.

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

Objects from more than one managed schema coexist in a single document,
because a document carries closure members from other schemas alongside the
schema's own objects (below). pgschema attributes an **unqualified** object to
whatever `--schema` the run targets — unqualified objects land in a scratch
schema that stands in for `--schema`. An unqualified closure member would
therefore be silently claimed by the schema being reconciled.

Verified: a file with `customers` unqualified (intended for `public`)
and `snowdrop.machine_ids` qualified, run with `--schema snowdrop`, plans
**both** `customers` and `machine_ids` into `snowdrop` — a misattribution.
Qualifying it as `public.customers` makes `--schema snowdrop` plan only
`machine_ids`, and `--schema public` plan only `customers`.

pgpushy MUST therefore **schema-qualify every emitted identifier with its
resolved schema, including the target schema's own objects and the default
schema** — `public.customers`, never bare `customers` — and likewise qualify
the referents of foreign keys and the targets of indexes and comments.
Qualifying an object with the schema the run targets is safe precisely because
pgschema strips that prefix; qualifying everything is therefore one rule
rather than two.

#### One document per managed schema

pgpushy MUST synthesize **one document per managed schema**, and MUST NOT
reuse a single document across runs. This is a correctness requirement, not an
optimization.

The reason is that pgschema strips a schema qualifier from an **identifier**
but cannot strip one from inside a **string literal**. So a name inside a
literal — `'X.t'::regclass` — that refers to an object in schema `X` must be
spelled:

| In the document for… | as | because |
|---|---|---|
| `X` itself | **unqualified** | pgschema moved `X`'s objects into the scratch schema, and the real `X` does not hold them |
| any other schema | **qualified** | `X`'s objects were left in `X`, exactly as written |

The same reference therefore has two correct spellings, chosen by which
document it appears in — so no single document can serve every run. Verified
against pgschema 1.12.0: `CREATE SEQUENCE w1.invoice_no` plus
`DEFAULT nextval('w1.invoice_no')` fails with `relation "w1.invoice_no" does
not exist`, because the sequence was created in the scratch schema while the
literal still names the real one; de-qualifying the literal yields `No changes
detected.` against a target built from the same definition.

> **Non-normative — how often this bites in 0.1.** Narrowly, and the rule is
> here for what comes next rather than for what is here now. The clearest
> example of a name in a literal is a `nextval` default, and §4.3 rejects those
> outright for an unrelated pgschema limitation, so 0.1 reaches this rule only
> through a `regclass` or `regtype` cast — legal, and uncommon. Views (§14) are
> what make it routine: a view's body is full of names, it is resolved at
> creation time, and there is no lifting it out the way a foreign key is
> lifted. Building the per-schema document before views rather than during them
> is deliberate.

Accordingly, in the document for schema `S`, a schema-qualifying name inside a
string literal MUST be emitted **without its qualifier when it names an object
in `S`**, and **with its qualifier otherwise**. This is transformation 4 of
§5.5.

#### What each document contains

The document for schema `S` MUST contain every object assigned to `S`, and the
**closure** of what those objects reference, and nothing else.

pgschema builds its desired-state model by executing the file it is given,
resolving every reference from that file alone and **not** from the target
database. Verified: a cross-schema foreign key to a table absent from the file
fails (`relation … does not exist`) even when that table exists on the target.
The closure is what makes every such reference resolvable.

The closure is defined over **execution-time references**:

1. Seed it with every object assigned to `S`, emitted in all six categories.
2. For every statement emitted, add each object it references at execution
   time — a foreign key's referent, a column's domain or type, a default's
   sequence. §4.5 rejects a source tree in which such a referent does not
   exist.
3. Objects added this way are **closure members**. A closure member
   contributes to **categories 1 through 4 only**: its schema declaration, its
   type, domain or sequence, its `CREATE TABLE`, and its indexes. It MUST NOT
   contribute a foreign key (category 5) or a comment (category 6).
4. Repeat from step 2 until no new object is added.

Two consequences are worth stating outright, because getting either wrong
produces a document that cannot execute:

- **A closure member brings its indexes.** A foreign key may reference a
  column set whose uniqueness is backed by a standalone `CREATE UNIQUE INDEX`
  rather than by an inline constraint, and Postgres accepts that as a
  referent (verified against Postgres 18; a *partial* unique index is not
  accepted). Emitting a closure member's table without its indexes therefore
  produces a document in which `S`'s foreign key cannot be created. pgpushy
  emits all of a closure member's indexes rather than selecting the ones that
  could back a reference, because the selection rule is exact and fiddly —
  partial excluded, `NULLS NOT DISTINCT` included — and a wrong answer fails
  silently in the unbuildable direction.
- **A closure member does not bring its foreign keys**, which is what bounds
  the closure. A foreign key is not a creation-time dependency — that is what
  FK-lift bought — so `S → X.t → Y.u` stops at `X.t`: `Y.u` is only needed by
  a constraint that `X`'s document emits and `S`'s does not. Since §4.5 admits
  no cross-schema reference other than a foreign key, every onward reference
  from a closure member is within its own schema, and the closure terminates
  quickly.

Objects a given run does not target serve only to resolve references and are
otherwise ignored by that run — pgschema diffs only `--schema`.

> **Non-normative — why not a combined document.** Earlier drafts of this spec
> made a single combined document the recommended form, on the evidence that
> it worked end to end for tables and foreign keys: correct per-schema plans
> and idempotent convergence. That evidence was real but incomplete. It held
> only because nothing in that object scope puts a schema name inside a string
> literal; adding sequences broke it immediately, and no amount of care in
> choosing *one* document's spelling can fix a requirement that two runs
> disagree about. The cost of the combined form was also never free: it is
> executed in full by every per-schema run, so plan-database work scaled with
> the product of schema count and total DDL size. The per-schema form is both
> the correct one and the cheaper one.

### 5.5 Behavior preservation

For every managed schema `S`, `S`'s document MUST describe exactly the part of
the schema the source tree assigns to `S` (G4). Closure members in that
document are scaffolding rather than description: pgschema diffs only
`--schema`, so a closure member is executed and never compared, and its
deliberate incompleteness (§5.4 — no foreign keys, no comments) is not a
divergence from what the author wrote. Every managed schema gets its own
document, so every object the source tree describes is described exactly once,
in the one document that is diffed against it.

pgpushy performs exactly six transformations, and each is behavior-preserving
— they change the textual order, the attachment site, the spelling of names,
and which document an object appears in, never the resulting schema:

1. **FK-lift** (§5.3) — relocates a foreign key from a table definition to a
   trailing `ALTER TABLE`.
2. **Constraint-name omission** (§5.3) — drops a name the author never wrote,
   leaving Postgres to generate the same name it already generated.
3. **Qualification** (§5.4) — rewrites each identifier to name the schema the
   object was already resolved to.
4. **Literal de-qualification** (§5.4) — removes the schema qualifier from a
   name inside a string literal when that name refers to an object in the
   document's own target schema, matching what pgschema does to identifiers
   and cannot do to literals.
5. **Schema-declaration emission** (§5.2) — adds `CREATE SCHEMA IF NOT EXISTS`
   for schemas the document requires.
6. **Partition and closure** (§5.4) — places each object in the document for
   its own schema, and copies closure members into the documents that need
   them.

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
the target**: schema changes all flow through pgschema (satisfying G3), and
outside `apply`'s seed execution (§8.8) every statement pgpushy issues
directly is read-only, so `plan` cannot mutate the target even incidentally.

### 6.1 Every managed schema must exist

pgschema cannot reconcile a schema absent from the target: reading current
state for a nonexistent schema fails, and an external plan database does not
change this — current state is always read from the target (verified). An
**existing but empty** schema reconciles normally.

For 0.1, pgpushy therefore makes schema existence a **hard precondition**:
every managed schema MUST already exist on the target. pgpushy MUST fail,
naming *every* missing schema, if any is absent.

Note that the synthesized desired state still emits `CREATE SCHEMA IF NOT
EXISTS` for each managed schema (§5.2); that executes in pgschema's **plan
database** to make qualified references resolvable, never against the target.

> **Non-normative.** Creating managed schemas is thus the operator's
> responsibility in 0.1 (a one-time `CREATE SCHEMA` per new schema). Automatic
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

pgpushy MUST resolve its connection from the named environment (§10.2) into a
single set of parameters. The resolved parameters are what §6.3 forwards to
pgschema.

Settings that would let the *environment* rather than the configuration decide
the target — `PGSERVICE`, `PGSERVICEFILE` — MUST be refused rather than
silently ignored, since pgpushy cannot interpret them and a dropped one would
mean connecting somewhere the operator did not name.

**`sslmode` MUST be honored in full**, across all five libpq modes:

| `sslmode` | Encryption | Certificate chain | Hostname |
|---|---|---|---|
| `disable` | no | — | — |
| `prefer` | opportunistic | not verified | not verified |
| `require` | yes | not verified | not verified |
| `verify-ca` | yes | verified | not verified |
| `verify-full` | yes | verified | verified |

pgpushy MUST interpret `sslmode` itself rather than delegating to its Postgres
driver, because the driver models only `disable`, `prefer` and `require` and
rejects the two verifying modes outright. Delegating would mean either
refusing a connection string libpq accepts, or — worse — connecting in
plaintext under a mode the operator chose for verification while pgschema,
which does implement all five, connects encrypted to the same database. §6.3
exists to make exactly that kind of divergence impossible.

An unrecognized `sslmode` MUST be a hard error naming the value and listing
the five accepted modes.

Inspection and seed execution (§8.8) are the only places pgpushy accesses the
target directly — §8.8 reuses this section's resolution — and every statement
outside §8.8 MUST be read-only.

### 6.5 Policies and row-level security

Inspection MUST also read, for every managed schema, the policies it holds
and the tables with row-level security enabled (`pg_policy`,
`pg_class.relrowsecurity`). Neither can be suppressed: pgschema's ignore file
has no section for them, and §4.3 admits no way to describe them — so a
managed schema holding either would have its policies dropped and its RLS
disabled by reconciliation, which §8.4's rule forbids.

The response follows §7's cycle semantics: fatal for `apply`, before anything
is touched; reported by `plan`, which still shows the plans — the operator
needs them to decide what to move — and exits non-zero. Every policy and
every RLS-enabled table MUST be named, with the choices stated: drop the
policy, or leave the schema out of the managed set. (Their plan steps,
`table.policy` and `table.rls`, are also caught by §8.4's enforcement — the
fail-closed net behind this check's friendlier message.)

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

The graph is complete because a foreign key is the only reference §4.5 permits
to cross a schema boundary. Widening that (§14) means adding edge kinds here,
not a different graph: a cross-schema `nextval` would need the same ordering
for the same reason.

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

- **`pgpushy init`** — write a starter `pgpushy.toml`, guessing the source
  root from where the `*.sql` files are. It is the one command that runs
  *without* a configuration file, since its purpose is to produce one; it MUST
  decline to guess when the answer is ambiguous, and MUST NOT overwrite an
  existing file. It connects to nothing.

- **`pgpushy validate`** — run the offline pipeline (§3, stages 1–6) and
  report. It MUST NOT connect to any database, so it is usable in a
  pre-commit hook and in CI with no Postgres service. It MUST report the
  managed-schema set, the file and object counts, the exclusions applied
  (§4.1), the seed file and statement counts when a seed root is configured
  (§4.6), and the schema apply order, and MUST fail on any §4.3, §4.5, §4.6,
  or §7 condition. It SHOULD accept an option to write the synthesized documents for
  inspection (§8.7).

- **`pgpushy plan`** — run the offline pipeline, inspect the target (§6), then
  run `pgschema plan --schema S` for every managed schema and present each
  schema's plan. This never modifies the target: pgschema `plan` reads the
  target read-only, its scratch objects live in the plan database, and pgpushy
  itself issues no target DDL (§6). A cross-schema foreign-key cycle is
  reported but does not suppress the plans; the command exits non-zero (§7).

- **`pgpushy apply`** — run the offline pipeline, inspect the target, then run
  `pgschema apply --schema S` for every managed schema in dependency order
  (§7), then execute the seed files (§8.8). Approval is governed by §8.6.
  `apply` MUST abort before touching the target if synthesis, any §4 or §7
  check, the target
  inspection — including §6.5 — or any plan in the preceding plan pass
  fails, or if any plan carries a step outside pgpushy's model (§8.4).

- **`pgpushy generate`** — run each configured generator and (re)write its
  output file; with `--check`, write nothing and fail if any output is stale
  (§4.7). It connects to nothing and runs none of the pipeline: it is the one
  command upstream of discovery, and the only one that executes a configured
  command.

`plan` and `apply` MUST fail before delegating if any managed schema is absent
from the target (§6.1).

### 8.3 Pass-through and owned flags

The connection pgpushy resolved (§6.4) MUST be forwarded to pgschema in full —
`--host`, `--port`, `--db`, `--user`, `--sslmode`, with the password through
the environment — so that pgschema resolves nothing for itself (§6.3). The plan
database (§10.4) and the lock timeout (§10.5) MUST likewise be forwarded when
configured, as `--plan-*` and `--lock-timeout`.

pgpushy MUST NOT let pgschema resolve any of these from *its* environment
either: `PGSCHEMA_PLAN_HOST` and the rest of that family MUST be stripped from
the subprocess environment alongside `PG*` (§6.3), so that the plan database
pgpushy configured is the one pgschema uses.

Note that pgpushy does **not** honor `PG*` for the target's identity: the
target comes from the named environment and nowhere else (§10.2). `PGPASSWORD`
is the exception, and is forwarded as the resolved password.

pgpushy **owns** the following, which MUST NOT be settable by the user on the
pgschema invocation:

- `--schema` — pgpushy loops over the managed set (§4.4).
- `--file` — pgpushy synthesizes the desired state (§5).
- `--auto-approve` — pgpushy always passes it, because approval happens once
  at the pgpushy level (§8.6). `pgpushy apply --auto-approve` controls
  pgpushy's own prompt, not pgschema's.
- **The working directory.** pgschema reads a `.pgschemaignore` from wherever
  it happens to be run, with no flag to point at one. That makes the operator's
  shell directory ambient input to what gets reconciled, so pgpushy MUST run
  pgschema in a directory pgpushy creates and controls.
- **The contents of that `.pgschemaignore`**, per §8.4.

### 8.4 What pgpushy does not manage, pgschema must not touch

Three axes of the target lie outside the managed set, and each needs pgpushy
to say so explicitly rather than let silence be read as intent.

**Unmanaged schemas.** pgpushy reconciles exactly the managed-schema set
(§4.4). Schemas present in the target database but absent from that set are
neither planned nor modified nor dropped (§6.1, §12.3).

**Privileges.** pgschema reconciles them **by default**, and reads a desired
state that
mentions no grants as a statement that there should be none: verified against
pgschema 1.12.0, a target granting `SELECT, INSERT` to a role has both revoked,
along with the schema's default privileges.

pgpushy 0.1 does not manage privileges — §4.3 rejects `GRANT` and `REVOKE` in
source — so their absence from the desired state carries no intent, and
pgpushy MUST NOT let pgschema read intent into it. pgpushy therefore writes a
`.pgschemaignore` into the working directory of §8.3 suppressing `[privileges]`
and `[default_privileges]` for every run.

**Unmanaged kinds.** A managed schema may hold objects of kinds pgpushy
cannot describe — views, materialized views, functions, procedures,
aggregates, triggers — installed by a migration pgpushy did not make, or
owned by something else. §4.3 rejects them in *source*; their absence from
the desired state therefore carries no intent, exactly as with privileges,
and pgpushy MUST NOT let pgschema read one. Two layers deliver that, because
one cannot:

- The `.pgschemaignore` pgpushy writes suppresses the kinds — `[views]`,
  `[materialized_views]`, `[functions]`, `[procedures]`, `[aggregates]`,
  `[triggers]` (verified at 1.12.3: `[views]` alone also covers materialized
  views; the extra section is insurance against an upstream split).
- pgschema silently accepts ignore sections it does not know (verified), so
  the file is ergonomics, not enforcement. pgpushy therefore also checks the
  plans: any step whose type names a kind outside pgpushy's model MUST be
  refused — reported by `plan`, which exits non-zero, and fatal for `apply`
  before anything is touched (§8.6). An upstream rename of an ignore section
  then fails loudly instead of re-arming the drops.

This axis is what makes **partial adoption a supported path**: a database
whose tables pgpushy manages keeps its views and functions wherever they came
from. The two exceptions are policies and row-level security, which have no
ignore section at all and are refused by name instead (§6.5).

One rule covers all three axes: **what the source tree does not describe,
pgpushy does not touch.** The consequence of getting either wrong is the same, and it
is the reason the rule is stated rather than assumed — reconciliation drops
things.

The suppression is unconditional in 0.1 because there is no way to express a
grant, so there is nothing for an opt-out to select between. When privileges
become a managed kind (§14), this becomes an explicit opt-out rather than the
only mode.

### 8.5 Resolving the pgschema binary

pgpushy obtains `pgschema` through a **provider** with two backends. The
managed backend is the intended default; the BYO backend is permanent (see
Rollout).

**Managed backend.** pgpushy downloads a pinned pgschema version — the version
its release was tested against (§13), overridable via configuration (§10.1) — from
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
explicit path or a `PATH` lookup (§10.3). In this mode pgpushy MUST read the
binary's version and enforce the minimum (§13). Version is read by parsing the
`Version:` line of `pgschema --help`; there is no machine-readable version
output. A version **below the floor** MUST be a hard error naming both the
found and the required version, with no override (§13). An **unparseable**
version line MUST be a warning, not a failure — the line is a human-readable
string, not a stability contract. BYO is the only backend available on
**Windows** (no pgschema binary is published) and in **air-gapped**
environments, and is therefore a permanent part of pgpushy, not a temporary
measure.

pgpushy MUST NOT trust a cached binary merely because the file is present: a
cache hit MUST be re-verified against the shipped hash where one exists. An
atomic write protects against pgpushy's own interrupted downloads and says
nothing about what else may have touched the cache since. A mismatch MUST be
reported and the binary re-fetched rather than executed.

**Selection.** The managed backend is the default. An explicitly configured
binary path selects BYO regardless of the configured backend, since naming a
binary and then downloading a different one cannot be what the operator meant.

### 8.6 Approval

`apply` reconciles several schemas in sequence and is not atomic across them
(§11.2), so approval MUST be sought once, for the database, before any schema
is touched — not per schema as each apply begins.

pgpushy MUST:

1. run a full `plan` pass over every managed schema first, **retaining each
   plan** (pgschema writes one as JSON with `--output-json`);
2. present those plans together as one reviewable unit, including a summary of
   how many schemas change, and MUST call out destructive changes and any
   schema being reconciled to an empty desired state (§4.4); when seed files
   exist the summary MUST list them with their statement counts, because the
   approval covers the seed writes too (§8.8);
3. perform the §6.2 check and §8.4's kind check against those plans, and
   refuse on a cross-schema removal the apply order cannot satisfy or a step
   outside pgpushy's model;
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
reserves for pgschema. One refinement is required, and it is scoped
precisely: in the summary's **destructive classification**, a `drop` step
paired with a `create` of the same kind and path is a modification, not a
destructive change — pgschema's own rendering of a widened UNIQUE constraint
is "1 to modify" over exactly such a pair (verified at 1.12.3). Step counts
still count steps. The §6.2 check MUST NOT pair: the drop half executes
first regardless, and Postgres refuses to drop a constraint a foreign key
depends on (verified — SQLSTATE 2BP01, mid-apply), so a recreated referenced
constraint is precisely the removal that check exists to catch.

`pgpushy apply --auto-approve` skips step 5 for non-interactive use. When
standard input is not a terminal and `--auto-approve` was not given, pgpushy
MUST fail rather than proceed unapproved.

### 8.7 Writing the synthesized documents

`validate`, `plan` and `apply` MAY be given `--out <dir>` to keep the
documents §5 synthesized. Because there is one document per managed schema
(§5.4), `--out` names a **directory**, not a file: pgpushy writes one
`<schema>.sql` per managed schema. The differences between those documents —
which closure members each carries, and how each spells a literal — are
exactly what someone reaching for `--out` is trying to see.

A schema name is a Postgres identifier and may contain characters that are not
safe in a filename. pgpushy MUST emit bytes outside `[A-Za-z0-9_-]`
percent-encoded, so that a legal but hostile schema name yields a legal
filename inside the directory and cannot escape it.

**pgpushy owns the directory it is given.** It MUST create it if absent. If it
exists, every file in it MUST carry pgpushy's generated-document marker
(§4.1),
otherwise pgpushy MUST refuse and name a file it did not write — so `--out`
can never overwrite an operator's own SQL. On success pgpushy MUST remove
generated files the current run did not write, so that a schema dropped from
`managed_schemas` leaves no document behind that reads as current. pgpushy
MUST NOT delete a file it cannot prove it wrote.

### 8.8 Seed execution

Seed files run inside `apply`, after every managed schema has applied (§3,
stage 9): their tables are guaranteed to exist and to have the modeled shape
only once stage 8 has finished. If any schema's apply failed, the seed files
MUST be reported as not attempted (§9). When the plan pass found no schema
changes but seed files exist, `apply` MUST still proceed to them — seeds are
convergent, and a no-op schema plan says nothing about missing rows — and
approval (§8.6) is still required, because a write is a write.

This is pgpushy's own write path to the target — the only one. It is bounded
on every axis: DML only, from seed files only, after apply only, one
transaction per file. §6's guarantee is intact — pgpushy still issues **no
DDL** of its own to the target — and `plan` remains read-only in effect: it
lists the seed files and their statement counts and MUST NOT execute them.

For each seed file, in seed-root-relative byte order, pgpushy MUST:

1. open one transaction on its own target connection (§6.3, §6.4), with
   `search_path` set empty — a qualified statement cannot be diverted, and
   §4.6 guarantees every statement is qualified — and the environment's
   `lock_timeout` applied (§10.5), transaction-locally;
2. execute the file's statements in file order, recording each statement's
   affected-row count; these counts are what the report shows;
3. execute the file's statements **again**, in the same transaction — the
   convergence probe. If any statement of the probe pass reports affected
   rows, the file is not idempotent: pgpushy MUST roll back and fail the
   file, naming the statement and its count;
4. otherwise commit.

The probe's placement is the point. A probe that passes touched nothing, so
the commit commits exactly the first pass; a probe that fails is rolled back
together with the first pass, so a non-idempotent seed lands **nothing**. The
failure path and the undo path are the same path — the check costs nothing
when it passes and cannot itself do damage when it fails. It is the dynamic
half of §11.1's guarantee, catching what §4.6's static rules cannot see: a
volatile expression in a values list, a `DO UPDATE` that never converges.

> **Non-normative.** Two probe side effects are unavoidable and harmless
> enough to document rather than design around: sequence values consumed
> inside the transaction are not returned by a rollback, and a trigger on a
> seeded table fires on both passes — its transactional effects roll back with
> everything else, but a trigger doing non-transactional work (an HTTP call
> from an extension, say) will have done it twice.

### 8.9 The plan artifact

`plan` MAY be given `--plan-out <dir>`: a directory pgpushy owns, into which
the plan pass is written as a reviewable, applicable artifact — **only when
the run is not refused**. A plan that exits 1 (a cycle, a §6.2, §6.5 or §8.4
refusal) MUST NOT write one: an artifact's checks re-run at apply where they
can — §6.2, §6.5, §8.4 — but a cross-schema cycle cannot be re-detected
without the source tree, so the only place to stop a cyclic artifact is to
never mint it. A destructive plan (exit 2, §9.1) DOES write the artifact:
the artifact is precisely what its reviewer gates on. `apply` MAY be
given `--plan <dir>`, and then applies **exactly that artifact**: no
discovery, no parsing, no synthesis — the deploy environment carries no
source tree, because the apply order lives in the artifact and source drift
after planning is not the apply step's business. This is the surface for the
deployment shape a CLI prompt cannot serve: plan under a preview role,
persist the plan, review and approve *that artifact*, apply exactly it under
a deploy role.

The artifact holds:

- **`manifest.json`** — the artifact format version; the managed schemas in
  apply order (the order is read from here, never re-derived); the schemas
  whose desired state is empty, so §8.6's loudest warning survives the
  boundary; each schema's plan file name with its SHA-256, so a plan file and
  its manifest entry cannot disagree silently; the target's identity (§6.3:
  `system_identifier` and database name); the pgschema version that planned;
  and the checked seed statements (§4.6), carried verbatim. The manifest is
  what marks the directory as pgpushy's, since JSON cannot carry §4.1's
  marker.

  The hashes are scoped honestly: they catch corruption and mixups — a plan
  file swapped, truncated, or paired with the wrong entry — not a hostile
  editor, who could rewrite the hashes too. What *is* enforced against
  editing is the executable content: the manifest's seed statements are
  **re-checked at apply time** against every §4.6 form rule — `INSERT … ON
  CONFLICT` only, the `DO UPDATE` guard, no `WITH`, a database-free source,
  built-in functions only, an explicit column list, a qualified table — and a
  seed whose table lies outside the manifest's own managed schemas is
  refused. The model checks ran at plan time; the form rules are what
  §12.10's guarantee rests on, so they run again at the point of use.
- **one `<schema>.json` per managed schema** — pgschema's plan, byte for
  byte, named by §8.7's percent-encoding rule.
- **`summary.json`** — the §9.1 machine-readable summary.

Ownership follows §8.7's rules: pgpushy creates the directory, refuses one
holding anything it cannot prove is an artifact of its own, and prunes files
a fresh write does not produce.

`apply --plan <dir>` MUST, in order:

1. verify each plan file against its manifest hash;
2. inspect the target (§6) and refuse if its identity differs from the
   manifest's. pgschema's fingerprint covers target *drift*, not target
   *identity* — it is silent exactly when two databases are kept identical,
   which is what a promote-through-environments pipeline does on purpose;
   each environment plans its own artifact;
3. re-run the §6.2 removal check against a **fresh** inspection — the
   per-schema fingerprint cannot see a relationship between two schemas — and
   the §6.5 check, and §8.4's kind check over the artifact's plans;
4. re-check the manifest's seed statements against §4.6's form rules, and
   refuse a seed whose table is outside the manifest's managed schemas;
5. seek §8.6 approval, summarized from the artifact — including the
   manifest's empty-desired-state schemas, called out per §8.6;
6. apply each non-empty plan, in manifest order, via `pgschema apply
   --plan`, from a working directory carrying §8.4's ignore file. **The
   ignore file participates in pgschema's fingerprint** (verified at 1.12.3:
   the same plan is refused when applied from a directory without it), so it
   is part of a plan's identity, not decoration;
7. execute the re-checked seed statements, per §8.8.

A pgschema version differing from the manifest's SHOULD be warned about; the
§8.5 floor applies regardless. And one consequence stated plainly: **an
approved artifact is not a promise that it will apply.** The target can move
underneath it, pgschema's fingerprint refuses (verified), and a pipeline
needs a story for approved-then-refused — which is re-plan and re-approve.
The same story covers a **partial** artifact apply: the apply itself moves
the target, so the already-applied schemas' fingerprints no longer match and
the artifact is spent; pgpushy MUST say so when reporting the partial
failure.

## 9. Failure Handling

`apply` MUST stop at the first schema whose apply fails; it MUST NOT continue
with later schemas, whose success may depend on the failed one (§7).

On any partial application, pgpushy MUST report which schemas were applied,
which failed, and which were not attempted, and MUST make clear that the
applied schemas are not rolled back (§11.2).

Seed files (§8.8) run only after every schema has applied; when any schema
failed, every seed file MUST be reported as not attempted. Among the seed
files themselves, `apply` MUST stop at the first that fails — on execution or
on the probe — and report which were applied, which failed, and which were not
attempted. Each file is atomic (§11.2): the failing file lands nothing, but
files committed before it stay committed.

### 9.1 Exit codes, and the destructive gate

Exit codes are a contract a pipeline routes on: **0**, success; **1**,
pgpushy refused the run or something failed — a rejected source tree, a §6.2
or §6.5 or §8.4 refusal, a cycle, a pgschema failure; **2**, from `plan`
alone: the plans are valid and would apply, but they contain destructive
changes and the environment does not allow them. A broken tree and a dropped
column route to different people, which is why 2 MUST NOT be 1.

`plan` classifies destructiveness per §8.6 — a recreated pair is a
modification — and, when any destructive change remains, exits 2 unless the
environment sets `allow_destructive = true` (§10.2). Following Atlas, the
opt-out lives in configuration rather than on the command line: §10.1's
reasoning, since a flag that disables a safety check per invocation is the
hazard configuration exists to prevent. Destructive tolerance is a property
of the *target* — a development database says yes, production says no — which
is why the key is per-environment. `apply` is not gated by it: approval
(§8.6) is apply's gate, and the §8.6 summary names every destructive change
before asking.

With `--plan-out`, `plan` also writes the classification as
**`summary.json`** (§8.9): a format version, the target's identity,
per-schema step and destructive counts, and every destructive step as
`drop.` plus pgschema's own type, with its path — `drop.table.column`,
`drop.table` — so a pipeline can allowlist a specific finding rather than
accept or reject a boolean.

## 10. Configuration

pgpushy reconciles an entire database. Everything it does — which files are
desired state, which schemas are managed, which server is reconciled against
them — determines what gets dropped as much as what gets created, and a wrong
answer to any of them is destructive. So configuration is **required and
explicit**, not inferred from where the command happened to be run.

Two consequences follow, and both cost convenience deliberately.

### 10.1 A configuration file is required

pgpushy MUST refuse to run without a **`pgpushy.toml`**. It is read from the
current working directory and MUST NOT be searched for in parent directories;
`--config <path>` names one anywhere. When no file is found, pgpushy MUST say
so plainly and show the minimum a working one contains, rather than falling
back to defaults.

The reason is the interaction, not the file. A tool whose source root defaulted
to the working directory *and* whose configuration was optional would, when run
from the wrong directory, silently treat a fragment of the source tree as the
whole desired state — and every object outside that fragment is then
desired-state-absent, which is to say scheduled for deletion. Failing is the
only safe response to not knowing what the source tree is.

**Project structure MUST NOT be settable from the command line.** The source
root, the seed root, the default schema, the managed-schema declaration, the
exclusions, and the generators all describe *the project*, and each is a way
to change what gets reconciled or executed.
They live in the file, where they are reviewable and version-controlled,
because a flag that silently narrows the desired state is the same hazard in a
different shape. `--config` selects which project; nothing else about the
project is a flag.

The file MAY hold:

- **Project structure** — `source_root`, defaulting to the directory containing
  the file itself; `default_schema` (§4.4), defaulting to `public`; and
  `exclude` glob patterns (§4.1). Relative paths resolve against the file's own
  directory.
- **`managed_schemas`** (§4.4) — the authoritative managed-schema declaration.
- **`seed_root`** (§4.6) — the directory of seed files, resolved like
  `source_root`; there are no seed files when it is absent.
- **Generators** (§4.7) — `[[generate]]` entries, each an output path and an
  argv command.
- **pgschema provider** (§8.5) — the backend, the pinned version (managed), and
  the binary path (BYO).
- **Environments** (§10.2).

Unknown keys MUST be rejected. A mistyped key is invisible from behavior —
pgpushy would act as though the setting were absent — so silence is the one
response that cannot be recovered from.

### 10.2 The target is named, never inferred

Connection settings live in **named environments**, and `plan` and `apply` MUST
require `--env <name>` to select one. `validate` MUST NOT accept it, because it
connects to nothing.

```toml
[env.local]
db   = "myapp_dev"
user = "joe"

[env.prod]
host = "db.internal"
db   = "myapp"
user = "deploy"
```

`--env` is required **even when only one environment is defined**. Selecting
the sole environment automatically would make adding a second one silently
change what an existing command reconciles.

An environment MUST specify `db` and `user`, which have no safe default.
It MAY also carry a plan database (§10.4) and a lock timeout (§10.5).
`host` defaults to `localhost`, `port` to `5432`, and `sslmode` to `prefer`.

**`PG*` environment variables MUST NOT override a named environment's target.**
This reverses the ordinary precedence, and deliberately: the entire purpose of
`--env prod` is to name a target unambiguously, and an ambient `PGHOST` that
silently redirected it would defeat that at exactly the moment it matters.
pgpushy already strips `PG*` from pgschema's environment (§6.3); it now
declines to read them for its own targeting too.

The password is the exception, because a secret does not belong in a
version-controlled file. `PGPASSWORD` MAY supply it, and takes precedence over
the environment's `password`.

**Password handling.** A `password` MAY be set in an environment, but when the
effective password is *sourced from the file* — not overridden by `PGPASSWORD`
— pgpushy MUST emit a prominent warning that a password is being read from a
file that is easily committed to version control. The warning fires on actual
use, so an overridden file password is silent, and it MUST NOT echo the
password.

**`allow_destructive`** MAY be set per environment. When absent or false,
a `plan` whose classification finds any destructive change exits 2 (§9.1);
when true, it exits 0 and the destructive changes are simply listed. It
gates nothing at `apply`, where §8.6's approval is the gate.

### 10.3 What remains a flag

Only things that describe *this run* or *this machine*, never the project or
the target: `--config`, `--env`, `--pgschema-path` (which differs per machine
and cannot affect what is reconciled), `--out`, `--plan-out` and `--plan`
(§8.9 — where an artifact is written or read is about *this run*; what it
contains never is), `--auto-approve`, `--lock-timeout` (§10.5), `--verbose`,
`--no-color`, and `generate`'s `--check`, which selects a mode of that
command rather than anything about the project.

### 10.4 The plan database

pgschema builds its comparison model by executing the desired state into a
**plan database** — an ephemeral embedded Postgres by default, or an external
one given `--plan-*`. An environment MAY name an external one:

```toml
[env.prod.plan_db]
host = "plan.internal"
db   = "pgschema_plan"
user = "planner"
```

`db` and `user` are required, as in the environment itself; `host`, `port` and
`sslmode` default the same way (§10.2), and `password` behaves as it does
there, except that the environment variable supplying it is
`PGPUSHY_PLAN_PASSWORD` — a separate variable, because the plan database is a
separate server with separate credentials.

An external plan database is working space, and it **accumulates**. Each run
executes every document into it, and a closure member (§5.4) lands in its
real, named schema, which pgschema never cleans — so a project with
cross-schema references cannot plan twice against the same plan database: the
second run fails on the first run's leftovers, midway through the loop
(verified at 1.12.3). Following pgschema's own lead, pgpushy does not clean it
either — it issues no DDL to the plan database, as to the target (§6) — the
database is **single-use for such projects**, dropped and recreated between
runs by the operator.

What pgpushy MUST do is refuse early and by name rather than let the failure
surface as pgschema's mid-loop error: before delegating, it checks the plan
database, and if any managed schema there is non-empty, it refuses, naming
the schemas and the remedy. An **empty** leftover schema MUST NOT be refused:
a project with no cross-schema references executes its objects into
pgschema's scratch schema and re-plans against the same plan database
indefinitely (verified), and refusing it would break what works.

> **Non-normative.** The plan database MUST NOT be one that matters. The
> embedded default needs none of this; an external plan database exists for
> environments where spawning a Postgres is not possible, and for seeding
> objects pgpushy does not manage (§14).

### 10.5 Lock timeout

`apply` MAY carry a `lock_timeout`, forwarded to pgschema as
`--lock-timeout`. It bounds how long Postgres waits for a lock before giving
up, which matters on a busy production table where an unbounded wait blocks
every query behind it.

Unlike project structure (§10.1), this MAY be set both in an environment and
by `--lock-timeout` on the command line, with the flag winning. Precedence is
safe here precisely because a lock timeout **cannot change what is
reconciled** — only whether the apply gives up waiting. `plan` does not accept
it, because pgschema's `plan` does not (verified).

The same value bounds each seed-file transaction (§8.8), applied
transaction-locally. A seed insert contending with application traffic
deserves the same bound as DDL.

## 11. Properties

### 11.1 Idempotence

Applying an already-reconciled database MUST produce no changes: every
per-schema `pgschema plan` is empty, and `pgpushy apply` is a no-op. (Verified
for the multi-schema loop in the design spike.) §5.3's constraint-name rule
exists to preserve this property for author-unnamed foreign keys.

Seed files extend the property to rows, and prove it on every apply: each
seed file MUST converge, and §8.8's probe refuses to commit any file whose
second pass in the same transaction touched anything. The probe's scope is
the file — two files `DO UPDATE`-ing the same row toward different values
each converge alone while together rewriting the row on every apply, which
validate warns about where it is statically visible (§4.6) and a whole-set
probe could close (§14). §4.6's static rules exist so the common violations fail at
`validate`, offline, with a file and a line; the probe is the backstop for
what parsing cannot see. "Converged" is defined as **zero affected rows** on
the probe pass — stricter than semantic idempotence for `DO UPDATE`, and
deliberately so, since the `WHERE … IS DISTINCT FROM` guard that satisfies it
(§4.6) is also what stops every apply from rewriting every seeded row.

### 11.2 Atomicity

pgschema applies each schema in its own transaction; pgpushy does **not** wrap
the whole database in one transaction. A failure partway through `apply`
therefore leaves already-processed schemas applied and the rest unapplied
(§9). The plan pass and approval gate of §8.6 reduce, but do not eliminate,
partial application. Each seed file likewise applies in one transaction of its
own (§8.8): a failing file lands nothing, while files committed before it stay
committed.

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

Seed execution order — files in seed-root-relative byte order, statements in
file order — is deterministic for the same reason (§8.8). Generated sources
add one requirement upstream of all of this: a generator command MUST emit
byte-identical output for the same inputs, and `generate --check` is the
enforcement (§4.7).

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

In 0.1 every managed schema MUST already exist on the target (§6.1); pgpushy
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

pgpushy 0.1 manages tables, indexes, table constraints, foreign keys,
user-defined types, domains, standalone sequences, and comments (§4.3). A
source tree containing any other object kind — a view, a function, a trigger,
a policy — is rejected, not partially managed. `ALTER` is rejected throughout,
except for the foreign-key form that is pgpushy's own output (§4.3), so a
constraint is written where the object is defined rather than bolted on
afterwards.

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

### 12.6 Cross-schema references other than foreign keys

A foreign key is the only reference pgpushy 0.1 permits to cross a schema
boundary (§4.5). A column in one schema typed by a domain in another, or a
default calling `nextval` on another schema's sequence, is rejected — write
the referenced object in the same schema, or the referring object in the
schema that owns it.

The restriction is what keeps §5.4's closure shallow and its rule uniform, and
it is the one place where 0.1 trades a real capability for a smaller design.
Lifting it is future work (§14) and is additive: the closure is already
specified over reference edges generally, and §7's graph would gain edge kinds
rather than change shape.

### 12.7 Privileges are not managed, and not touched

pgpushy 0.1 neither reconciles `GRANT`/`REVOKE` nor lets pgschema reconcile
them on its behalf (§8.4). Permissions on a pgpushy-managed database are
whatever something else made them, and an `apply` leaves them exactly as it
found them. This is a deliberate non-feature rather than an oversight: the
alternative available today is not "pgpushy manages grants" but "pgschema
revokes every grant pgpushy cannot see."

### 12.8 A sequence cannot be a default

pgschema models any default calling `nextval` as `SERIAL`, so a sequence named
in one is not a sequence it will manage: on a column it silently creates a
different, column-owned sequence and never converges, and on a domain it fails
to apply at all. §4.3 rejects both rather than letting either reach a database.

A sequence nothing defaults to is managed normally — created with its
parameters, and idempotent afterwards (verified). That covers a sequence drawn
from by application code, which is the common reason to declare one; it does
not cover using a shared sequence as a column default, for which `serial` or
`GENERATED … AS IDENTITY` is the supported spelling.

### 12.9 A type, a domain, a schema or a constraint cannot be commented

pgschema drops `COMMENT ON TYPE` and `COMMENT ON DOMAIN` without applying them
and without reporting it — the surrounding statements apply, the plan
afterwards is empty, and the comment simply is not there. §4.3 rejects both
rather than letting a comment quietly not exist.

`COMMENT ON SCHEMA` and `COMMENT ON CONSTRAINT` fail the same way, found the
same way — by applying and re-planning against 1.12.3: neither produces a
plan step, neither lands in the catalog, and the plan afterwards is empty.
Both were accepted through 0.1, which made its comment support for those two
targets hollow; §4.3 now rejects them too, the honest narrowing. `COMMENT ON
SEQUENCE` is unaffected and is managed normally (verified — its step folds
under the sequence's own type rather than a `.comment` suffix).

### 12.10 Seeds ensure rows exist; they never delete

A seed file can only insert a row, or hold a listed row at its declared
values. A row absent from every seed file is invisible to pgpushy: it is never
deleted, and no form of row-level reconciliation is offered. This is §8.4's
rule applied to data — what the source does not describe, pgpushy does not
touch — and it is what keeps seeds from growing into a data-diffing engine, a
thing no declarative schema tool offers and G3's reasoning declines one level
down. A row that must be removed is an operational change, made by the
operator or the application.

### 12.11 The seed statement form is narrow

`INSERT … ON CONFLICT`, with a database-free source, built-in functions
only, an explicit column list and a statically checkable conflict target, is
the whole seed allow-list (§4.6): no `COPY`, no `UPDATE`-only fixups, no
`WITH` clause, no reading the target in a source query, no expression
conflict targets. The narrowness is not incidental — it is what makes
§12.10's guarantee and §11.1's convergence checkable at all, statically at
`validate` and dynamically by §8.8's probe. Each exclusion is additive later
if a real tree needs it.

The probe's one blind spot is scope: it proves each file converges, not that
the set does. Two files `DO UPDATE`-ing the same row toward different values
each pass their own probe while the row is rewritten on every apply; validate
warns on the statically visible shape (§4.6), and a whole-set probe is future
work (§14).

## 13. Dependencies and Compatibility

- **pgschema** — required at runtime, resolved through the provider (§8.5):
  downloaded by the managed backend or supplied by the operator (BYO).

  Two versions matter, and they are **not** the same number. The **floor** is
  the oldest version pgpushy is tested against — currently **v1.12.0** —
  expressed as a `>=` requirement; newer is accepted. The **pin** is the
  newest version pgpushy is tested against — currently **v1.12.3** — and is
  what the managed backend downloads. Both ends MUST appear in pgpushy's CI
  matrix, or one of them is a claim nothing tests.

  Keeping them apart answers two different questions. An operator who brings
  their own binary should not be made to upgrade because pgpushy prefers a
  newer release; an operator who lets pgpushy fetch one should get the most
  fixed release that has actually been tested. Collapsing them forces one of
  those two to lose.

  The floor is not overridable: a below-floor binary is a hard error, and the
  remedy is to upgrade pgschema or use the managed backend. The relied-upon
  behavior (foreign-key deferral, deterministic cycle-breaking, PR #156) is
  technically present from **v1.4.2** (2025-11-14), which bounds how far the
  floor could be lowered later with testing, but is not itself the supported
  floor. The BYO backend enforces the floor (§8.5); the managed backend
  controls the version and so needs no check — but the floor still applies to
  a version the operator pins, since it is about what pgpushy is tested
  against rather than about how the binary arrived.
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

  1. **Functions, procedures, aggregates, triggers, policies.** pgschema treats
     function bodies as opaque dollar-quoted text rather than parsing them, and
     Postgres does not resolve a plpgsql body at creation time — so the name is
     qualified and the body passes through byte-for-byte. Trigger and policy
     references are structured AST fields. One question to settle first:
     SQL-standard `BEGIN ATOMIC` bodies (PG14+) *are* resolved at creation,
     unlike dollar-quoted ones.
  2. **Views and materialized views.** A view's query *is* resolved at creation
     time, so unqualified references inside it are a live problem, and views
     need a topological sort *within* their category, because a view over a
     view is a genuine creation-time dependency that category order cannot
     express — the same machinery §5.1 already applies to category 2. The
     per-schema document (§5.4) has since answered the harder half: with `S`'s
     objects qualified as `S`, which pgschema strips, an unqualified reference
     inside a view body resolves to the scratch schema standing in for `S`,
     which is the correct answer. So an AST-walking qualifier — which would
     have to track scope, since CTE names and subquery aliases are `RangeVar`s
     too and qualifying those would break the view — is needed only if
     cross-schema references are widened past §12.6.
  3. **`GRANT`/`REVOKE` and `ALTER DEFAULT PRIVILEGES`.** Permissions rather
     than shape. Attribution is settled: `ALTER DEFAULT PRIVILEGES` requires
     `IN SCHEMA`, so every such statement names its own schema, and a grant
     attributes to the schema of the object granted on — both verified to plan
     correctly per `--schema`. What remains is that grants need their **roles
     to exist in the plan database**; the embedded one has none and fails with
     `role "x" does not exist`. Roles are cluster-wide, so an external plan
     database on the same cluster as the target has them, which makes
     `[env.*.plan_db]` (§10.4) load-bearing rather than optional. pgpushy
     should refuse with an explanation — naming the file and line and showing
     the block to add — rather than letting pgschema fail on a missing role.
     This is also where §8.4's unconditional privilege suppression becomes an
     explicit opt-out. One open question: `GRANT … ON SCHEMA` appeared to be
     silently ignored by pgschema in a spike; confirm, and reject it in the
     allow-list if so, rather than accepting a statement that does nothing.
- **References into unmanaged schemas** — for foreign keys targeting schemas
  pgpushy does not manage (extension schemas, an externally-owned `auth`,
  etc.), support pgschema's **external plan database** seeded with those
  external objects. §4.5 rejects these today. Not needed for cross-schema
  references *among managed schemas*, which §5.4's closure already covers.
- **Cross-schema references other than foreign keys** (§12.6) — a column typed
  by another schema's domain, a default calling another schema's sequence.
  Additive: §5.4's closure is already specified over reference edges rather
  than over foreign keys, and §7's graph gains edge kinds rather than changing
  shape. What it needs first is verification that pgschema resolves a
  cross-schema *type* reference correctly under a per-schema run — the
  identifier must survive un-stripped while the referring table's own
  qualifier is stripped to the scratch schema.
- **Comprehensive destructive-change detection.** The shallow-but-honest
  version shipped (§9.1): what pgschema labels a drop, paired per §8.6, with
  a distinct exit code and a machine-readable summary. What remains is the
  *comprehensive* question — `ALTER COLUMN wide TYPE varchar(10)` truncates
  or fails and is reported identically to the safe widening, and pgschema is
  the only party that can classify that cheaply, since it alone holds both
  states. Atlas's coded analyzers (DS102 table dropped, MF104 nullable to
  non-nullable) are the model worth studying, and Atlas does not flag a
  narrowing type either. Whether pgschema grows the classification, another
  tool has it ([pgmold](https://github.com/fmguerreiro/pgmold) is one to look
  at), or the diffing belongs in pgpushy after all — that last reverses G3 —
  is a decision rather than a task.
- **Release binaries, and a GitHub Action.** pgpushy publishes to crates.io
  only, so installing it in CI means libclang, `bindgen` and compiling
  libpg_query's C sources on every run. Per-platform release binaries would fix
  that, and pgpushy has already designed the shape once — its managed provider
  downloads, verifies and caches exactly such binaries for pgschema (§8.5).

  They are the prerequisite for an action, which is where the §8.9 plan
  artifact stops being a CLI feature and starts being useful: GitHub's
  `environment:` with required reviewers is the approval gate that design
  needs, two environments give the preview and deploy roles their separate
  credentials, and an uploaded artifact is what makes the approval apply to a
  reviewed object rather than a recomputed one. See
  [`github-action-sketch.md`](./github-action-sketch.md).
- **Plan-database hygiene** — an external plan database accumulates state
  across runs (§10.4), and stale objects can make a broken desired state
  appear to work. With grants making an external plan database mandatory for
  some projects, this stops being spike hygiene and becomes something pgpushy
  should either clean, namespace, or check.
- **`pgpushy dump`** — the inverse: read an existing database and emit a
  per-object source tree, to bootstrap adoption.
- **Cross-schema FK cycle support** (§12.1). **Single-pass cross-schema FK
  removal** (§12.2) has a cheaper route than the cycle case: where a run only
  *removes* references between a pair of schemas and adds none, the pair can
  simply be applied in the reverse order, with no target DDL from pgpushy at
  all. Refusing is only the general answer, for runs that both add and remove
  between the same pair.
- **Richer configuration** (§10): per-schema overrides, and variable
  interpolation in environments so that a target can be assembled from the
  process environment (Atlas does this with `--var`) rather than written out
  statically.
- **Schema-drop management** (§12.3), behind an explicit opt-in.
- **A plan-time seed probe, and a whole-set probe.** `plan` lists seed files
  without executing them (§8.8). A `BEGIN … ROLLBACK` double-run against the
  target would report real would-insert counts and prove convergence before
  the deploy gate — but it takes locks, burns transaction IDs, and fires
  triggers on what should be a read, so it waits for someone to want it. The
  same double-run over **all** files in one transaction would close §11.1's
  per-file scope (§12.11).
- **Ephemeral generators** — running `[[generate]]` commands at plan or apply
  time without vendoring the output. Rejected for now because unvendored
  output is ambient input (§4.7); revisit only against demonstrated need.
- **Seed exclusions, and generator pruning.** `exclude` globs do not apply
  under `seed_root`, and `generate` has no way to remove an output whose
  entry was deleted from configuration (§4.7 says delete the file in the same
  change). Both are small; neither has a user yet.

## 15. Decision Log

All decisions identified for 0.1 are resolved. Decisions marked **[0.2]** were
made after draft 2 of v0.1.

- **Object scope** — tables, indexes, table constraints, foreign keys,
  and comments. **[0.2]** Anything else is **rejected** with a diagnostic
  rather than passed through, because pgpushy cannot qualify the interior of a
  statement it does not model, and §5.4 makes qualification normative.
  (§4.3, §12.5)
- **Schema-assignment mechanism** — schema-qualify **every** emitted
  identifier with its resolved schema, including `public`; an unqualified
  object would be misattributed to whichever schema's run reads it (verified).
  **[0.4]** Synthesis-file granularity is *not* an implementation detail, and
  the combined document — recommended through v0.3 — is wrong. See the
  per-schema entry below. (§5.4)
- **Absent schemas / `plan` mutation** — 0.1 makes schema existence a hard
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
- **pgschema version & resolution** — **[0.4]** the floor and the pin are
  separate numbers: the floor is the *oldest* tested version (`>= v1.12.0`
  today) and the pin is the *newest* (`v1.12.3`), with both ends in the CI
  matrix. A BYO operator is not made to upgrade for a release pgpushy merely
  prefers, and a managed one gets the most fixed release that was tested. The
  floor tracks pgpushy's CI matrix and is **not overridable**.
  pgpushy resolves the binary through a provider: managed download (intended
  default, pinned + SHA-256-verified) with a permanent BYO override that
  parses `pgschema --help` and enforces the floor. The plan was for the first
  release to ship BYO + version check only; **[0.3]** managed landed early and
  is the default, with BYO selected by naming a binary or setting
  `backend = "byo"`. (§8.5, §13)
- **Configuration file** — **[0.3]** `pgpushy.toml` is **required**, read from
  the working directory only (not searched upward; `--config` for an explicit
  path), and holds everything about the project: structure, `managed_schemas`,
  exclusions, pgschema-provider, and named environments. Project structure is
  deliberately **not** settable by flag.

  This replaces the earlier "optional convenience" decision. The two defaults
  interacted badly: an optional file plus a source root defaulting to the
  working directory meant that running from the wrong directory silently
  treated a fragment of the tree as the whole desired state, scheduling
  everything outside it for deletion. Atlas and pgschema both read their config
  from the working directory only, and get away with it because their input is
  explicitly named — pgschema requires `--file`. pgpushy's blast radius is the
  whole database, so it names its input explicitly too. (§10.1)
- **[0.3] Plan database and lock timeout** — both live in the environment
  (§10.4, §10.5), because both describe *that target*. `--lock-timeout` is
  additionally a flag, since it cannot change what gets reconciled — the test
  that separates a safe flag from an unsafe one. The plan database is not:
  pointing it somewhere else changes where the desired state gets executed.
- **[0.3] Named environments** — connection settings live in `[env.<name>]`
  blocks and `--env` is required for `plan` and `apply`, even with only one
  environment defined. `PG*` no longer overrides a named environment's target,
  because an ambient `PGHOST` silently redirecting `--env prod` would defeat
  the point of naming it; `PGPASSWORD` remains, since a secret should not be in
  the file. `password` is permitted in an environment but triggers a prominent
  warning when it is the effective source. (§10.2)
- **[0.2] `CREATE SCHEMA` forms** — only the bare
  `CREATE SCHEMA [IF NOT EXISTS] <name>` form is accepted; the nested-element
  form and the `AUTHORIZATION`-only form are rejected. (§4.3)
- **[0.4] One document per managed schema** — reverses the combined-document
  recommendation of v0.1–v0.3. pgschema strips a schema qualifier from an
  identifier but not from inside a string literal, and a `nextval` reference
  to an object in `S` must be spelled *unqualified* in `S`'s own document and
  *qualified* in every other. Those two requirements contradict, so no single
  document can be correct for every run. The earlier evidence was sound but
  incomplete: it held only because tables and foreign keys put no schema name
  inside a literal. Verified against pgschema 1.12.0 in both directions —
  the qualified literal fails with `relation … does not exist`, the
  de-qualified one converges. The per-schema form is also the cheaper one.
  (§5.4, §5.5)
- **[0.4] Closure contents** — a document holds its schema's objects plus the
  transitive closure of what they reference at execution time. A closure
  member contributes categories 1–4 (schema, type/domain/sequence, table,
  indexes) and never a foreign key or a comment. It brings **all** its
  indexes, not a selected subset: a foreign key may reference a column set
  whose uniqueness is backed by a standalone unique index (verified against
  Postgres 18; a *partial* unique index is not accepted), and the selection
  rule is fiddly enough that a wrong answer fails silently in the unbuildable
  direction. Omitting a closure member's foreign keys is what bounds the
  closure — FK-lift is what makes a foreign key not a creation-time
  dependency. (§5.4)
- **[0.4] Cross-schema references capped at foreign keys** — a foreign key is
  the only reference permitted to cross a schema boundary in 0.1; a
  cross-schema type, domain or sequence reference is rejected. This keeps the
  closure shallow and its rule uniform, and it avoids shipping on an
  unverified assumption about how pgschema resolves a cross-schema type
  reference under a per-schema run. Additive to widen later: the closure is
  specified over reference edges generally. (§4.5, §12.6, §14)
- **[0.4] `ALTER` is not a declarative statement** — every `ALTER` form is
  rejected in source except `ALTER TABLE … ADD CONSTRAINT` for a foreign key.
  A `CHECK`, `UNIQUE`, `PRIMARY KEY` or `EXCLUDE` constraint is written inline
  in its `CREATE TABLE`; verified against Postgres 18 that an inline table
  constraint can carry an explicit name, so only the spelling changes. The
  foreign-key form stays because it is pgpushy's own output shape and what
  `pg_dump` emits, and because category 5 cannot reintroduce an earlier
  category's ordering problem. This also removes `… ADD CONSTRAINT … UNIQUE
  USING INDEX`, the one form that made category 4 not internally order-free.
  (§4.3, §5.1)
- **[0.4] Names inside string literals must be qualified** — `nextval('s')` is
  rejected; write `nextval('public.s')`. §4.4's rule for identifiers is
  deliberately *not* reused, because the two readings — the default schema
  versus the owning object's schema — diverge silently once cross-schema
  references are supported, and both are legal. Requiring the schema keeps the
  choice open and costs imported trees nothing: `pg_dump` qualifies inside
  literals already (verified), and `serial`/`IDENTITY` produce no literal.
  (§4.3, §5.4)
- **[0.4] A sequence may not be a default** — pgschema models any default
  calling `nextval` as `SERIAL`. Verified against pgschema 1.12.0 by applying
  and then re-planning: on a column it creates a sequence owned by that column,
  never creates the one named, reports success, and leaves the plan showing the
  same drop and add on every run afterwards; on a domain it fails to apply,
  because pgschema orders domains before sequences. The apply order is
  pgschema's, so pgpushy cannot order around either. Both are rejected. A
  sequence nothing defaults to applies and converges normally, which is the
  common reason to declare one. An earlier measurement missed this by building
  the target with `psql` rather than letting pgschema apply it — the two
  disagree, and only one of them is what a user will run. (§4.3, §12.8)
- **[0.4] Object scope for 0.1** — adds user-defined types, domains and
  standalone sequences to tables, indexes, constraints, foreign keys and
  comments. `CREATE SEQUENCE … OWNED BY` is rejected: it inverts the category
  order, and pgschema models a column-owned sequence as `SERIAL` rather than
  an object of its own, so the shape does not survive a dump-and-reapply
  (verified — the standalone sequence is dropped and an owned one created).
  `CREATE TABLE … (LIKE t)` is rejected alongside `INHERITS`, `PARTITION OF`
  and `OF <type>`, being the same table-to-table creation dependency hiding
  inside the column list. (§4.3, §12.5)
- **[0.4] Category 2 is sorted, not fixed** — types, domains and sequences
  share one category ordered topologically by creation-time dependency, rather
  than following pgschema's fixed dump order. No fixed order among the three
  is correct in general: a domain over a domain, a composite type with a
  domain-typed field, and a domain default calling `nextval` each invert a
  different pair. The sort is also what views will need within their own
  category (§14). (§5.1)
- **[0.4] `--out` is a directory pgpushy owns** — one `<schema>.sql` per
  managed schema, with bytes outside `[A-Za-z0-9_-]` percent-encoded so a
  legal-but-hostile schema name cannot escape the directory. pgpushy creates
  it, refuses it if it holds a file pgpushy did not write, and prunes its own
  stale documents so a schema dropped from `managed_schemas` leaves nothing
  behind that reads as current. (§8.7)
- **[0.4] `sslmode` is honored in full** — all five libpq modes, interpreted
  by pgpushy rather than by its Postgres driver, which models only three and
  rejects the two verifying ones. Delegating would mean refusing a connection
  string libpq accepts, or connecting in plaintext under a mode chosen for
  verification while pgschema — which implements all five — connects encrypted
  to the same database. That is precisely the divergence §6.3 exists to
  prevent. (§6.4)
- **[0.4] Privileges are suppressed, not managed** — pgpushy writes a
  `.pgschemaignore` covering `[privileges]` and `[default_privileges]` into a
  working directory it owns, because pgschema reconciles privileges by default
  and reads a desired state mentioning no grants as a request to have none
  (verified: every grant on the target revoked). Unconditional in 0.1, since
  §4.3 admits no way to express a grant and an opt-out would have nothing to
  select between; it becomes an explicit opt-out when privileges become a
  managed kind. Owning the working directory is part of the decision: pgschema
  auto-loads that file from wherever it runs, so the operator's shell
  directory would otherwise be ambient input. (§8.3, §8.4, §12.7)
- **[0.5] Seeds are a file class, not desired-state DML** — §4.3's DML
  rejection stands untouched, because its reasons are pipeline reasons: a
  passed-through statement executes inside pgschema's comparison model, and a
  seed file never enters the pipeline — never synthesized, never shown to
  pgschema. Seeds are executed by pgpushy itself, against the target, after
  apply. The motivating shape: a library that provisions its own table
  publishes idempotent DDL and seed DML (`snowdrop-id-postgres` and its 1024
  machine-ID rows), and provisioning from the application at boot needs
  DDL-adjacent rights a production application role should not hold — so the
  rows belong to the deploy step, which is pgpushy's step. (§4.6, §8.8)
- **[0.5] Seed idempotence is enforced twice** — statically at `validate`
  (every statement `INSERT … ON CONFLICT`; `DO UPDATE` without a `WHERE` guard
  rejected, since whenever it seeds a row at all the probe pass re-updates
  every one, so it cannot converge to zero affected rows) and dynamically at
  `apply`, where §8.8 runs each file twice in one transaction and commits only
  if the second pass touched nothing. The probe's placement is the decision: a
  passing probe changed nothing, so the commit commits exactly the first pass,
  and a failing probe rolls back together with it — the failure path and the
  undo path are the same path, so the check is free on success and cannot
  itself do damage. "Converged" means zero affected rows, stricter than
  semantic idempotence for `DO UPDATE` and chosen deliberately: the guard that
  satisfies it is the same guard that stops every apply rewriting every
  seeded row. (§4.6, §8.8, §11.1)
- **[0.5] Seeds never delete rows** — §8.4's rule applied to data. Row-level
  reconciliation is declined, not deferred: it would make pgpushy a
  data-diffing engine, which G3's reasoning rejects one level down. (§12.10)
- **[0.5] A seed's target must be a modeled table, qualified, with an explicit
  column list** — seed files execute verbatim under an empty `search_path`, so
  qualification is load-bearing rather than stylistic; the model is what makes
  the offline column and conflict-target checks possible; and an implicit
  column list breaks silently the day the table gains a column. Seeding a
  table the source tree does not describe is rejected: writing rows into shape
  pgpushy cannot see is the row-level version of the misattribution §5.4
  exists to prevent. The statement's source may not read the database, and
  its expressions may call only built-in functions — a data-modifying CTE is
  a `DELETE` wearing an `INSERT`'s statement kind, a `SELECT` over a table
  makes the rows a function of target state, and a user-defined function can
  do arbitrary work; each would quietly break §12.10 or §11.3. (§4.6)
- **[0.5] Generated sources are vendored, and vendoring is the only mode** —
  `generate` is upstream of discovery; `validate`, `plan` and `apply` execute
  no configured command and read only files. Unvendored generator output is
  ambient input: plan and apply can disagree across tool versions, review
  never sees the schema a dependency bump changed, and a persisted plan (§14)
  stops being reproducible from the tree — the same family as the rejected
  working-directory `.pgschemaignore` and `PG*` overrides, with the same
  answer. Version authority is delegated to the repository's own lockfile by
  pointing `command` at a lockfile-governed tool (a workspace `xtask` printing
  a dependency's published SQL); `generate --check` then fails CI the moment a
  bump changes the emission, forcing the change into a reviewed diff. (§4.7)
- **[0.5] Two markers, opposite polarity** — the generated-document marker
  (§4.1, §8.7) marks pgpushy's output, which discovery must skip; the
  generated-source marker (§4.7) marks input, which discovery must read and
  which `generate` uses to know what it may overwrite. One marker could not
  carry both behaviors, and the overwrite refusal is what guarantees
  `generate` never clobbers SQL a human wrote. (§4.1, §4.7)
- **[0.5] pgpushy gains a write path to the target, scoped to seed DML** — the
  first statements pgpushy issues to the target itself, bounded on every axis:
  DML only, from seed files only, after apply only, one transaction per file,
  under the environment's lock timeout. §6's guarantee — no DDL of pgpushy's
  own to the target — is intact, and `plan` remains read-only in effect. (§8.8)
- **[0.6] Unmanaged kinds are suppressed and enforced, and partial adoption
  is a supported path** — a managed schema's views, materialized views,
  functions, procedures, aggregates and triggers are left alone: suppressed
  through the `.pgschemaignore` pgpushy writes, and enforced by refusing any
  plan step whose type falls outside pgpushy's model. Two layers because one
  cannot serve: pgschema silently accepts ignore sections it does not know
  (verified — `[not_a_real_section]` draws no error), so the file alone is a
  hope pinned to an upstream TOML key, and an upstream rename would silently
  re-arm the drops 0.1 shipped with. The step taxonomy backing the check was
  measured, not assumed (impl-plan §1). Partial adoption — manage the tables,
  keep the views wherever they came from — is a deliberate commitment, not a
  transition state. (§8.4, §12.5)
- **[0.6] Policies and row-level security are refused by name** — no ignore
  section exists for either (verified), so they cannot be left alone, only
  detected. Refusal beats what 0.1 did, which was silently plan `DROP POLICY`
  and `DISABLE ROW LEVEL SECURITY`. The shape follows §7's cycle precedent:
  fatal for `apply` before anything is touched, reported by `plan` alongside
  the plans the operator needs, read during inspection so the message names
  objects rather than plan steps. (§6.5, §8.4)
- **[0.6] The external plan database is single-use, and pgpushy checks
  rather than cleans** — closure members execute into real schemas pgschema
  never cleans, so a cross-schema project cannot re-plan against the same
  plan database. pgpushy follows pgschema's lead and issues no DDL there; it
  refuses early — a non-empty managed schema in the plan database, named,
  with the drop-and-recreate remedy — instead of surfacing pgschema's
  mid-loop error. The check is precisely calibrated to what was measured: an
  empty leftover schema is harmless (a single-schema project re-plans
  indefinitely), and refusing it would break what works. (§10.4)
- **[0.6] A schema or a constraint cannot be commented** — the same silent
  drop as types and domains, found the same way: apply and re-plan shows no
  step, no catalog row, and an empty plan. Accepted through 0.1, rejected
  now; a narrowing of shipped behavior, taken deliberately, because a comment
  that quietly does not exist is worse than one that is refused. (§4.3,
  §12.9)
- **[0.6] A drop paired with a create on the same kind and path is a
  modification — for the approval summary alone** — pgschema renders a
  widened UNIQUE constraint as "1 to modify" over exactly such a pair
  (verified), so counting the drop half as destructive misreports routine
  migrations. The §6.2 check deliberately does **not** pair: adversarial
  review proved the pair is exactly how a cross-schema-referenced
  constraint's removal presents, and the drop half still executes first —
  Postgres refuses it mid-apply (SQLSTATE 2BP01) after earlier schemas have
  already applied. Classification pairs; hazard detection never does.
  (§6.2, §8.6)
- **[0.6] A qualified reference into a managed schema that the tree does not
  define fails at validate** — the previously unreachable diagnostic, made
  reachable. Resolution deliberately leaves unknown *unqualified* names as
  written, because `text` and an extension's type look identical offline; a
  qualified miss carries a schema name, and when that schema is managed the
  plan database provably cannot resolve it, so the reference would otherwise
  detonate mid-plan-loop as pgschema's error. Tree-defined tables and indexes
  satisfy a literal, which may legitimately name them (`'s.t'::regclass`).
  (§4.5)
- **[0.7] The plan artifact** — the four decisions of 2026-08-19, now built
  as §8.9: the apply order lives in the manifest, never re-derived, so the
  deploy environment carries no checkout; the §6.2 check re-runs at apply
  against a fresh inspection, because the per-schema fingerprint cannot see a
  relationship between two schemas; the artifact records `system_identifier`
  and database name and apply refuses a different target, since the
  fingerprint covers drift, not identity; one plan file per managed schema,
  percent-encoded, with the manifest as the directory's mark. Two additions
  earned since: the manifest carries each plan's SHA-256, and it carries the
  checked **seed statements** verbatim — seeds are part of the reviewed unit
  (§8.8), and an artifact that applied the schemas but not the rows would
  deliver half of what was approved. (§8.9)
- **[0.7] The ignore file is part of a plan's identity** — measured at
  1.12.3: `pgschema apply --plan` refuses the very plan it wrote when run
  from a directory without the `.pgschemaignore` it was planned under, because
  the ignore file participates in the fingerprint. So artifact apply keeps
  running from a working directory pgpushy owns and writes (§8.4), exactly as
  every other pgschema invocation does. (§8.9)
- **[0.7] Destructive plans exit 2, and the opt-out is per-environment
  configuration** — 1 already means pgpushy refused, and a caller must route
  a broken tree and a dropped column to different people. The classification
  is §8.6's — recreated pairs are modifications — which is what keeps the
  gate from failing routine constraint widenings on day one. The opt-out is
  `allow_destructive` in the environment, not a flag, per §10.1's reasoning
  and following Atlas; per-environment because destructive tolerance is a
  property of the target. `apply` is deliberately not gated: §8.6's approval
  is its gate. (§9.1, §10.2)
- **[0.7] An artifact's integrity is validation, not hashing** — adversarial
  review proved the obvious-in-hindsight: with no trust anchor, hashes over
  the plan files cannot bind a hostile editor, who rewrites the manifest —
  and the manifest carries the seed statements, a direct write path to the
  target. So the hashes are scoped to corruption and mixups, and the defense
  for executable content is re-validation at the point of use: every §4.6
  form rule re-runs over the manifest's seeds at apply, bounded to the
  manifest's own schemas. The same review showed a refused plan happily
  minting an appliable artifact whose one unrecheckable failure — a
  cross-schema cycle — would then apply; a refused run therefore writes no
  artifact at all, which is the only place that hole can be closed. (§8.9)
