# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **A persistable plan artifact** (spec §8.9). `plan --plan-out <dir>` writes
  the plan pass as a reviewable, applicable artifact: one plan file per
  managed schema, hash-pinned by a manifest that also records the apply
  order, the target's identity, the pgschema version, and the checked seed
  statements verbatim. `apply --plan <dir>` applies exactly it — no source
  tree is read at the deploy end — after verifying the hashes, refusing a
  different database (pgschema's fingerprint covers drift, not identity),
  and re-running the cross-schema, policy and unmanaged-kind checks against
  a fresh inspection. The seeds that run are the seeds that were reviewed.
  One measured fact worth knowing: the `.pgschemaignore` participates in
  pgschema's fingerprint, so applying always runs from a working directory
  pgpushy writes.
- **A destructive gate with a distinct exit code** (spec §9.1). A valid plan
  containing destructive changes exits **2** — never 1, which means a refused
  run — so a pipeline can route a dropped column and a broken tree to
  different people. The classification pairs a drop with a create on the same
  kind and path as a modification, so a widened constraint does not trip the
  gate. The opt-out is `allow_destructive = true` in the environment, not a
  flag. With `--plan-out`, the classification is also written as
  `summary.json`: per-schema counts and every destructive step as `drop.` +
  pgschema's own type, with its path.

## [0.2.0] - 2026-08-31

### Added

- **Seed files** (spec §4.6, §8.8). A `seed_root` in `pgpushy.toml` names a
  directory of idempotent baseline rows — reference data the application
  needs, provisioned by the deploy role instead of by every webserver at
  boot. Seeds are not desired state: they are never shown to pgschema, and
  `apply` executes them itself after every schema has applied, one
  transaction per file. Each statement must be `INSERT … ON CONFLICT` with a
  database-free source, an explicit column list, a schema-qualified, modeled
  table, and — for `DO UPDATE` — a `WHERE … IS DISTINCT FROM` guard; the
  offline checks verify columns and conflict targets against the model.
  Inside each transaction the file runs twice, and the second pass must
  affect zero rows: a seed that does not converge is rolled back whole, so a
  volatile expression lands nothing. Rows are never deleted.

- **`pgpushy generate`** (spec §4.7). `[[generate]]` entries in
  `pgpushy.toml` name an argv command — never a shell — whose output is
  vendored into the tree under a generated-source marker, for SQL owned by a
  dependency rather than an author (a workspace `xtask` printing
  `snowdrop-id-postgres`'s published DDL, say). Generation is upstream of
  discovery: `validate`, `plan` and `apply` execute no configured command
  and read only files. `generate --check` fails when any output is stale, so
  a dependency bump that changes the emitted SQL must land as a reviewed
  diff. `generate` never overwrites a file it cannot prove it wrote.

### Fixed

- **A managed schema's unmanageable objects are no longer dropped.** 0.1
  planned `DROP VIEW`, `DROP FUNCTION`, `DROP TRIGGER` and the rest for any
  object it could not describe, violating the spec's own rule that what the
  source tree does not describe, pgpushy does not touch. Those kinds are now
  suppressed in the `.pgschemaignore` pgpushy writes — and enforced: any plan
  step naming a kind outside pgpushy's model is refused, so an upstream
  ignore-section rename fails loudly instead of re-arming the drops. Partial
  adoption is a supported path. Policies and row-level security have no
  suppression lever and are refused by name instead of silently removed
  (spec §6.5, §8.4).
- **A stale external plan database is refused by name.** A cross-schema
  project's closure members accumulate in an external plan database, and the
  second run used to fail midway through the loop with pgschema's
  `relation … already exists`. pgpushy now checks before delegating and
  refuses when a managed schema there is non-empty, naming the schemas and
  the drop-and-recreate remedy. A single-schema project keeps re-planning
  against the same plan database, as it always could (spec §10.4).
- **A qualified type or literal reference into a managed schema that the
  tree does not define is rejected at `validate`**, instead of failing
  mid-plan-loop as pgschema's error (spec §4.5).
- **A recreated object no longer reads as destructive.** pgschema renders a
  widened UNIQUE constraint as a drop plus a create on one path and calls it
  a modify; the approval summary and the cross-schema removal check now pair
  the steps before counting (spec §8.6).

### Changed

- `apply` gains a write path to the target, scoped to seed DML: pgpushy
  still issues no DDL of its own, and `plan` still executes nothing.
- `pgpushy-core` drops the unused `error::Analysis<T>` alias, which shadowed
  the exported `Analysis` struct.
- **`COMMENT ON SCHEMA` and `COMMENT ON CONSTRAINT` are rejected.** Both were
  accepted through 0.1 and silently dropped by pgschema — no plan step,
  nothing in the catalog — the same class as type and domain comments, found
  the same way. A comment that quietly does not exist is worse than one that
  is refused (spec §12.9).

## [0.1.1] - 2026-08-18

Nothing yet.

## [0.1.0] - 2026-08-18

The first release. pgpushy reconciles a whole Postgres database — every schema
it is told to manage — from a directory tree of SQL files, delegating the
diffing and applying to [pgschema](https://github.com/pgplex/pgschema).
[`docs/spec.md`](docs/spec.md) is the specification, and the record of why each
decision below was made.

### Added

- **Four commands.** `pgpushy init` writes a starter `pgpushy.toml`, guessing
  the source root from where the `*.sql` files are; it declines to guess when
  the answer is ambiguous and never overwrites an existing file. `pgpushy
  validate` runs the entire offline pipeline — discovery, parsing, the
  statement allow-list, schema resolution, the validity checks, cross-schema
  ordering and synthesis — needing neither a database nor a pgschema binary, so
  it belongs in a pre-commit hook or in CI with no Postgres service. `pgpushy
  plan` shows one pgschema plan per managed schema. `pgpushy apply` reconciles
  the database.

- **Order-free authoring.** Files may sit anywhere under the source root, in
  any layout, under any names: pgpushy discovers `*.sql` recursively and works
  out the order itself. It does so by lifting every foreign key out of its
  `CREATE TABLE` into a trailing `ALTER TABLE … ADD CONSTRAINT`, the way
  `pg_dump` does, so no table has to precede another and two tables in one
  schema may reference each other. There is no include list to maintain and no
  naming convention to obey.

- **Whole-database management.** One `pgpushy` invocation runs pgschema once
  per managed schema, in an order derived from the cross-schema foreign keys: a
  schema is processed after every schema it references, with ties broken by
  name so the order is reproducible. A pgschema invocation targets exactly one
  schema; supplying that orchestration is what makes the database, rather than
  the schema, the unit of management.

- **The managed-schema set is derived from the source tree** — every schema a
  discovered object is assigned to, plus every schema named in a `CREATE
  SCHEMA` — or declared in configuration, in which case the declaration is
  authoritative and a schema the tree uses but the list omits is an error. A
  schema the source tree never mentions is never reconciled and therefore never
  emptied. A declared schema with no source *is* managed and reconciles to
  empty, which is destructive, so pgpushy says so in both `validate` and the
  approval summary.

- **Object scope: tables, indexes, inline table constraints, foreign keys,
  user-defined types, domains, standalone sequences, and comments on schemas,
  tables, columns, indexes, table constraints and sequences.** Everything else is rejected, naming the file, the line and the
  statement kind: views, materialized views, functions, procedures, triggers,
  policies, `GRANT`/`REVOKE`, `CREATE EXTENSION`, every `DROP`, and all DML.
  Rejection rather than pass-through is the point. pgpushy schema-qualifies
  every identifier it emits, because an unqualified one is misattributed to
  whichever schema's run reads it, and it cannot qualify the interior of a
  statement it does not model — a table reference inside a view body, say. A
  statement passed through would emit exactly the construct qualification
  exists to prevent, and a `DROP` or an `INSERT` riding along would execute
  inside pgschema's comparison model and distort the state everything is
  diffed against.

- **`ALTER` is rejected in source, with one exception.** A source file says
  what exists, not the steps that reach it. `ALTER TABLE … ADD CONSTRAINT` for
  a **foreign key** is accepted, because it is pgpushy's own output form and
  what `pg_dump` emits, so an imported tree needs no rewriting. `CHECK`,
  `UNIQUE`, `PRIMARY KEY` and `EXCLUDE` are written inline in `CREATE TABLE`,
  where they can still carry an explicit name — nothing is lost but the
  spelling.

- **Further rejections inside the allowed statements**, each for a failure that
  would otherwise be silent or unfixable:
  - `CREATE TABLE … (LIKE t)`, `… INHERITS`, `… PARTITION OF`,
    `… PARTITION BY` and `CREATE TABLE … OF <type>`. Each makes a table's
    *creation* depend on another table; FK-lift resolves foreign-key ordering
    only. `LIKE` is the easiest to miss, because it hides inside the column
    list rather than in a clause of its own.
  - `CREATE SEQUENCE … OWNED BY`, and any **default calling `nextval`** on a
    column or a domain. pgschema models a column-owned sequence as `SERIAL`
    rather than as an object of its own, so none of these round-trips: on a
    column it silently creates a different, column-owned sequence, reports
    success, and shows the same drop and add on every later plan; on a domain
    it fails to apply at all. `serial` or `GENERATED … AS IDENTITY` is the
    spelling that works, and a sequence nothing defaults to is managed
    normally.
  - `COMMENT ON` a **type** or a **domain**, though a comment on a sequence is
    fine. pgschema generates no DDL for either: it applies everything around
    them, omits the comment, and then reports no changes — so the comment never
    reaches the target and nothing says so. Refusing beats vanishing.
  - `CREATE INDEX CONCURRENTLY`, which cannot run inside a transaction block
    and describes a strategy rather than a state — how an index is built is
    pgschema's decision.
  - `CREATE INDEX` without an explicit name. Postgres would generate one from
    the table and the indexed expressions; pgpushy needs a stable name to
    detect duplicates against and to attach comments to.
  - A bare object name inside a **string literal** — `'x'::regclass`. pgpushy
    does not infer a schema inside a literal, because that inference and the
    one for identifiers would disagree silently the moment cross-schema
    references widen. Name the schema: `'public.x'::regclass`. `pg_dump`
    already qualifies inside literals.
  - Two **unnamed foreign keys** on one table over the identical column set.
    They compete for a single generated name whose numeric suffix follows
    creation order, which pgpushy's emission order need not match, so the names
    can attach to opposite constraints and pgschema plans two renames on every
    run. Naming either one resolves it.
  - `CREATE SCHEMA` in any form but the bare one. The nested form's elements
    and `AUTHORIZATION` both say things pgpushy does not manage, and silently
    discarding a clause would make the synthesized state differ from what the
    author wrote.

- **Source-tree checks that run before anything is synthesized**: duplicate
  objects, naming both source locations; foreign keys whose referent is nowhere
  in the tree; cross-schema references other than foreign keys; and
  cross-schema foreign-key cycles, for which no apply order exists. Every
  diagnostic names every instance rather than the first, with file and line —
  a tree with five unsupported statements prints five.

- **Cross-schema foreign keys are supported, and are the only reference
  permitted to cross a schema boundary.** A column typed by another schema's
  domain or user-defined type is rejected, naming the referring object, the
  referenced object and both schemas. A foreign key
  is not a creation-time dependency — FK-lift is what bought that — so it is
  the one reference kind that can cross a schema without dragging a transitive
  closure of another schema's objects behind it.

- **A required `pgpushy.toml`**, read from the working directory and never
  searched for in parent directories; `-c`/`--config <path>` names one
  anywhere. It holds everything that decides *what* gets reconciled:
  `source_root` (defaulting to the file's own directory, so the source tree is
  anchored to the project rather than to the working directory),
  `default_schema`, `exclude` globs, `managed_schemas`, the pgschema provider,
  and the named environments. None of those is settable by flag, because a flag
  that silently narrowed the desired state would be the same hazard as guessing
  at a missing file: everything outside the narrowed set is then
  desired-state-absent, which is to say scheduled for deletion. Unknown keys
  are rejected — a mistyped key is invisible from behavior, so silence is the
  one response that cannot be recovered from. When the file is missing, pgpushy
  shows the smallest one that works rather than falling back to defaults.

- **Named environments, and `--env <name>` required on `plan` and `apply`** —
  even when only one environment is defined, since selecting the sole one
  automatically would make adding a second silently change what an existing
  command reconciles. `validate` does not accept it, having no target. `PG*`
  deliberately does not override a named environment's target: naming the
  target unambiguously is the whole purpose of `--env prod`. `PGPASSWORD` is
  the exception, because a secret should not live in a version-controlled file,
  and pgpushy warns prominently whenever the password it actually used came
  from the file — on use, so an overridden one is silent, and never echoing it.

- **A managed pgschema provider, and it is the default.** pgpushy downloads
  pgschema 1.12.3 over HTTPS, verifies it against a SHA-256 it ships — pgschema
  publishes no checksums of its own — and caches it per version and platform
  under `$XDG_CACHE_HOME/pgpushy`. There is no install step. A cached binary is
  re-verified on every run rather than trusted for existing; a mismatch is
  reported and re-fetched rather than executed. `backend = "byo"` or naming a
  binary opts out, which is required on Windows, where pgschema publishes no
  binary, and in air-gapped environments. A bring-your-own binary must be at
  least 1.12.0 — the oldest version pgpushy's CI tests — enforced from the
  `Version:` line of `pgschema --help`, with no override; an unreadable version
  line warns rather than failing, since that line is a human-readable string
  and not a stability contract.

- **All five libpq `sslmode` values — `disable`, `prefer`, `require`,
  `verify-ca`, `verify-full` — interpreted by pgpushy itself** rather than
  delegated to its Postgres driver, which models only the first three and
  rejects the verifying two outright. Delegating would mean either refusing a
  connection string libpq accepts or, worse, connecting in plaintext under a
  mode chosen for verification while pgschema, which implements all five,
  connects encrypted to the same database. The verifying modes check against
  the platform trust store, which is where pgschema's Go TLS stack looks too;
  `SSL_CERT_FILE` and `SSL_CERT_DIR` are resolved to absolute paths before
  pgschema sees them, so a relative one cannot name different files for the two
  connections. An unrecognized mode is a hard error listing the five.

- **One connection resolution, not two.** pgpushy resolves every parameter
  itself and passes all of them to pgschema explicitly, so pgschema resolves
  nothing and the two cannot reach different databases. `PG*` and the
  `PGSCHEMA_PLAN_*` family are stripped from the subprocess environment for the
  same reason; `PGSERVICE` and `PGSERVICEFILE` are refused rather than silently
  dropped, since pgpushy cannot interpret them and a dropped one would mean
  connecting somewhere nobody named. `plan` and `apply` report the database,
  server and cluster system identifier they reached, so the target is visible
  in the record of any change.

- **One synthesized document per managed schema, and `--out <dir>` to keep
  them.** Each carries that schema's objects plus the closure of what they
  reference elsewhere, and a name inside a string literal is spelled
  differently depending on which schema's run reads it, so no single document
  is correct for all of them. `--out` therefore names a directory and writes
  one `<schema>.sql` into it, percent-encoding anything outside `[A-Za-z0-9_-]`
  so a legal but hostile schema name yields a legal filename. pgpushy owns that
  directory: it refuses outright if it finds a file it cannot prove it wrote,
  and removes only its own stale documents. Synthesis is deterministic —
  byte-identical across runs and platforms, from source content and never
  filesystem order — so plans stay stable and reviewable.

- **Privileges are left exactly as they were found.** pgschema reconciles them
  by default and reads a desired state mentioning no grants as a statement that
  there should be none, revoking every grant on the target along with the
  schema's default privileges. pgpushy has no way to express a grant, so their
  absence from the desired state carries no intent, and pgpushy writes a
  `.pgschemaignore` suppressing `[privileges]` and `[default_privileges]` for
  every run. One rule covers this and unmanaged schemas alike: what the source
  tree does not describe, pgpushy does not touch.

- **Managed schemas must already exist on the target, and pgpushy issues no DDL
  of its own.** Every query pgpushy makes directly is a `SELECT`, so `plan`
  cannot mutate the target even incidentally, and every change flows through
  pgschema. A missing schema fails before pgschema is invoked, naming all of
  them at once with the `CREATE SCHEMA` to run.

- **Approval once, for the database.** `apply` plans every managed schema
  first, presents the plans together with a per-schema change count, names each
  destructive change individually rather than counting them, states that apply
  is not atomic across schemas, and asks once — so declining leaves the target
  untouched. On approval it applies **the plans it just showed** rather than
  recomputing them, which makes the change that is applied the same one that
  was reviewed; pgschema fingerprints the state a plan was computed against and
  refuses a plan whose target has since drifted. `--auto-approve` is for
  non-interactive use, and without a terminal and without that flag `apply`
  fails rather than assuming yes.

- **Detection of a cross-schema foreign-key removal the apply order cannot
  satisfy.** Creation order and removal order are reverses of each other, so a
  plan that drops the column or unique constraint a cross-schema foreign key
  points at, while the referencing schema is applied later, cannot work.
  pgpushy finds this in the plans it already has, names the constraint, both
  schemas and the object being dropped, and spells out the two-step remedy —
  before anything is applied. Dropping the referenced *table* is unaffected:
  pgschema drops tables with `CASCADE`, which takes the dependent foreign key
  with it.

- **Failure reporting for a partial apply.** `apply` stops at the first schema
  that fails, then reports what landed, what broke, what was never attempted,
  and that the applied schemas are not rolled back.

- **`--verbose`**, printing the pgschema command line as it will actually run —
  the password never appears there, traveling through the environment instead
  — the synthesized document's path, and every file discovery kept. Color is
  suppressed when output is not a terminal, and by `--no-color` or `NO_COLOR`,
  including pgschema's own.

- **An optional external plan database per environment**
  (`[env.<name>.plan_db]`) and a `lock_timeout`, both forwarded to pgschema.
  pgschema builds its comparison model in an ephemeral embedded Postgres by
  default; an external one exists for environments where spawning a process is
  not possible, and it is scratch space that pgschema *writes* to rather than a
  reference. `--lock-timeout` on `apply` overrides the environment — the one
  setting that works both ways, because it cannot change what is reconciled,
  only whether the apply gives up waiting. Neither password is ever passed on a
  command line: the target's travels through `PGPASSWORD`, the plan database's
  through `PGPUSHY_PLAN_PASSWORD`.

- **`pgpushy-core`**, the pure half of the workspace: parsing, FK-lift,
  qualification, the validity checks, cross-schema ordering and synthesis, with
  no IO of any kind. `pgpushy` is the IO shell around it.
