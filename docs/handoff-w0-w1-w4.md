# Handoff: W0, W1 and W4

**Written:** 2026-08-18
**Branch:** `feat/validate-plan-apply` (PR #1, open, CI green)
**Delete this file when W4 lands.** It is a note about work in flight, not
documentation. Everything durable in it has already been folded into
[`impl-plan.md`](./impl-plan.md) §1 and the milestone list; this file only says
what to do next and why.

---

## Why the order changed

The plan was W1 → W2 → W3 → W4. The first real project needs **sequences and
grants**, so W1 and W4 come first. Spiking them turned up a prerequisite
(**W0**) that neither the spec nor the plan anticipated, and one live bug that
is already fixed on the branch.

## What is already done

`fix: stop revoking privileges pgpushy does not manage` (commit `79d81e7`).

pgpushy rejects `GRANT` in source, so its desired state mentions no privileges,
and **pgschema reads that as "there should be none"** — it planned `REVOKE` for
every grant on the target plus the schema's default privileges. Anyone managing
permissions outside pgpushy would have lost them on their first `apply`.

pgpushy now writes a `.pgschemaignore` covering `[privileges]` and
`[default_privileges]`, and runs pgschema in a directory pgpushy owns — that
file is auto-loaded from the working directory, so without owning the cwd a
stray one in the operator's shell directory could silently change what gets
reconciled.

**This is also the "ignore grants entirely" feature, currently unconditional.**
W4 turns it into an explicit opt-out rather than the only mode.

---

## W0 — Per-schema trimmed synthesis

**Do this first. W1 is not correct without it, and W3 needs it too.**

### The problem

pgschema strips schema qualifiers from *identifiers* but cannot strip them from
inside a *string literal*. So for `--schema w1`:

```sql
CREATE SEQUENCE w1.s     -- becomes CREATE SEQUENCE s, in the scratch schema
... DEFAULT nextval('w1.s')   -- still says w1.s, which is now empty → fails
```

`pg_dump` emits exactly the failing form (`nextval('s.seq'::regclass)`), so this
is not a corner case.

Worse, **no single document can satisfy every run**:

| | same-schema `nextval` | cross-schema `nextval` |
|---|---|---|
| the run that owns the schema | must be **unqualified** | must be qualified |
| every other run | must be **qualified** | must be qualified |

And a cross-schema reference *into* the target schema is unresolvable at all,
because the target's objects live in a scratch schema whose name is
unpredictable.

### The fix, verified working

Per-schema documents **trimmed to the closure** — which spec §5.4 already
permits, describing it as a large-database optimization. It is not; it is a
correctness requirement. For schema `S`, emit:

- every object in `S`, with string-literal references to objects in `S`
  **unqualified**;
- plus the transitive closure of what those objects reference in other schemas,
  fully qualified;
- and **nothing else** — in particular, no object in another schema that merely
  happens to reference `S`, which is what made the naive per-schema version
  still fail.

### Shape of the change

- `pgpushy_core::synth::synthesize(objects, managed, target)` — one document per
  target schema. `Analysis` carries a document per schema rather than one.
- `Session` writes one tempfile per schema; `plan_pass` and `apply_pass` pass the
  matching one.
- `--out` becomes a **directory**, one `<schema>.sql` per managed schema
  (decided 2026-08-16). Uniform, and the differences between documents are
  exactly what someone reaching for `--out` wants to see.
- Spec §5.4 needs rewriting: the combined document is no longer the RECOMMENDED
  default, it is wrong.

---

## W1 — Sequences, types, domains

Straightforward once W0 lands.

- **Category order is pgschema's own** (from its `dump`): TYPE → DOMAIN →
  SEQUENCE → TABLE. Slot these ahead of tables in spec §5.1's category list.
- **Owned sequences are not separate objects.** pgschema renders a sequence
  owned by a column as `SERIAL` on that column; a table created with `serial`
  and one created with an explicit sequence + `OWNED BY` dump identically. Only
  **standalone** sequences are their own object.
- `CREATE SEQUENCE`, `CREATE TYPE` (enum and composite) and `CREATE DOMAIN` all
  have structured names in the AST and qualify exactly as tables do — verified
  across two schemas, including a cross-schema type reference.

---

## W4 — Grants and default privileges

### What is settled

- **Attribution**, which was the open question: `ALTER DEFAULT PRIVILEGES`
  *requires* `IN SCHEMA`, so every such statement names its own schema; a grant
  attributes to the schema of the object granted on. Both verified to plan
  correctly per `--schema`.
- pgschema supports grants on tables, sequences, functions/procedures and
  types/domains, column-level privileges, `WITH GRANT OPTION`, and `PUBLIC` as a
  grantee.

### What still needs deciding or building

1. **Grants need their roles in the plan database.** The embedded one has none
   and fails with `role "x" does not exist`. Roles are **cluster-wide**, so an
   external plan database on the *same cluster* as the target has them — this is
   what makes W4 workable, and it makes M5's `[env.*.plan_db]` load-bearing
   rather than optional.

   **Decided:** pgpushy refuses with an explanation when a source tree grants
   and no plan database is configured, naming the file and line and showing the
   `[env.*.plan_db]` block to add. Not a warning, and not left to pgschema's own
   error, which names a role rather than the missing configuration.

2. **`ignore_grants`**, so today's behaviour stays available as an explicit
   opt-out for projects whose permissions are managed elsewhere. When set,
   pgpushy keeps writing the `.pgschemaignore` sections and rejects `GRANT` in
   source; when unset and grants are present, it manages them.

3. **`GRANT ... ON SCHEMA` appears to be silently ignored** by pgschema — it
   produced no plan step in a spike. Confirm, and if so reject it in the
   allow-list rather than accepting a statement that does nothing.

---

## Things that will bite whoever picks this up

- **Never reuse a plan database between spikes.** Ours accumulated seven
  schemas, and stale objects made a *broken* desired state look like it worked —
  which sent an earlier conclusion in the wrong direction for a while. Drop and
  recreate it for every measurement.
- The local dev setup: Postgres in `docker` as `pgpushy-dev` on port 55434
  (`postgres`/`pw`), and a pgschema 1.12.0 binary in the scratchpad. Integration
  tests want `PGPUSHY_TEST_PG_URL` and `PGPUSHY_TEST_PGSCHEMA`;
  `PGPUSHY_TEST_DOWNLOAD=1` opts into the managed-provider download tests.
- `just msrv` before pushing. A modern toolchain cannot see MSRV breakage, and
  CI caught exactly that once already.
