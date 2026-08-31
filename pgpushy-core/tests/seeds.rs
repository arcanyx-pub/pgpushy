//! Seed files (spec §4.6): the allow-list and the model checks, from string
//! literals. The dynamic half — §8.8's probe — lives in the binary's
//! integration tests, since it needs a database.

use pgpushy_core::error::Diagnostic;
use pgpushy_core::{Analysis, AnalysisError, Options, SourceFile, analyze};

fn files(list: &[(&str, &str)]) -> Vec<SourceFile> {
    list.iter()
        .map(|(path, contents)| SourceFile {
            path: (*path).to_owned(),
            contents: (*contents).to_owned(),
        })
        .collect()
}

fn ok(tree: &[(&str, &str)], seeds: &[(&str, &str)]) -> Analysis {
    match analyze(&files(tree), &files(seeds), &Options::default()) {
        Ok(analysis) => analysis,
        Err(AnalysisError::Source(diagnostics)) => panic!("rejected: {diagnostics:#?}"),
        Err(err) => panic!("{err}"),
    }
}

fn rejected(tree: &[(&str, &str)], seeds: &[(&str, &str)]) -> String {
    match analyze(&files(tree), &files(seeds), &Options::default()) {
        Err(AnalysisError::Source(diagnostics)) => render(&diagnostics),
        Ok(_) => panic!("expected the seeds to be rejected"),
        Err(err) => panic!("{err}"),
    }
}

fn render(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|d| format!("{d}"))
        .collect::<Vec<_>>()
        .join("\n---\n")
}

const MACHINE: &str = "CREATE TABLE public.machine (\n\
     machine_id smallint PRIMARY KEY,\n\
     val text,\n\
     CONSTRAINT machine_val_key UNIQUE (val)\n\
 );";

/// The motivating case, verbatim from `snowdrop-id-postgres::seeding_sql()`'s
/// shape: a set-returning built-in as the source.
#[test]
fn the_snowdrop_seed_passes_verbatim() {
    let analysis = ok(
        &[("machine.sql", MACHINE)],
        &[(
            "leases.sql",
            "INSERT INTO public.machine (machine_id) SELECT generate_series(0, 1023) \
             ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
    assert_eq!(analysis.counts.seed_files, 1);
    assert_eq!(analysis.counts.seed_statements, 1);
    let stmt = &analysis.seeds.files[0].statements[0];
    assert!(stmt.sql.contains("generate_series"), "{}", stmt.sql);
    assert_eq!(stmt.table.to_string(), "public.machine");
    assert_eq!(stmt.origin.file, "leases.sql");
}

#[test]
fn a_values_seed_passes() {
    let analysis = ok(
        &[("machine.sql", MACHINE)],
        &[(
            "rows.sql",
            "INSERT INTO public.machine (machine_id, val) VALUES (1, 'a'), (2, 'b') \
             ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
    assert_eq!(analysis.counts.seed_statements, 1);
}

#[test]
fn a_statement_other_than_insert_is_rejected() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[("bad.sql", "DELETE FROM public.machine;")],
    );
    assert!(
        message.contains("must be an INSERT, not DELETE"),
        "{message}"
    );
    assert!(message.contains("bad.sql:1"), "{message}");
}

#[test]
fn a_bare_insert_names_the_on_conflict_remedy() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id) VALUES (1);",
        )],
    );
    assert!(message.contains("no ON CONFLICT clause"), "{message}");
    assert!(message.contains("ON CONFLICT"), "{message}");
    assert!(message.contains("DO NOTHING"), "{message}");
}

/// The hole the adversarial review found: a data-modifying CTE is a DELETE
/// wearing an INSERT's statement kind (spec §4.6, §12.10).
#[test]
fn a_data_modifying_cte_is_rejected() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "WITH gone AS (DELETE FROM public.machine RETURNING machine_id) \
             INSERT INTO public.machine (machine_id) SELECT machine_id FROM gone \
             ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
    assert!(message.contains("WITH is not allowed"), "{message}");
}

#[test]
fn a_source_reading_a_table_is_rejected() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id) \
             SELECT machine_id + 1 FROM public.machine \
             ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
    assert!(message.contains("reads machine"), "{message}");
    assert!(message.contains("generate_series"), "{message}");
}

#[test]
fn a_subquery_reading_a_table_is_rejected() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id) \
             VALUES ((SELECT max(machine_id) FROM public.machine)) \
             ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
    assert!(message.contains("reads machine"), "{message}");
}

#[test]
fn returning_is_rejected() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id) VALUES (1) \
             ON CONFLICT (machine_id) DO NOTHING RETURNING machine_id;",
        )],
    );
    assert!(message.contains("RETURNING is not allowed"), "{message}");
}

#[test]
fn an_unguarded_do_update_shows_the_guard() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id, val) VALUES (1, 'a') \
             ON CONFLICT (machine_id) DO UPDATE SET val = excluded.val;",
        )],
    );
    assert!(message.contains("no WHERE guard"), "{message}");
    assert!(message.contains("IS DISTINCT FROM"), "{message}");
}

#[test]
fn a_guarded_do_update_passes() {
    ok(
        &[("machine.sql", MACHINE)],
        &[(
            "rows.sql",
            "INSERT INTO public.machine AS m (machine_id, val) VALUES (1, 'a') \
             ON CONFLICT (machine_id) DO UPDATE SET val = excluded.val \
             WHERE m.val IS DISTINCT FROM excluded.val;",
        )],
    );
}

#[test]
fn an_implicit_column_list_is_rejected() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine VALUES (1, 'a') \
             ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
    assert!(message.contains("no column list"), "{message}");
}

#[test]
fn an_unknown_column_is_rejected() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id, nope) VALUES (1, 'a') \
             ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
    assert!(message.contains("no column nope"), "{message}");
}

#[test]
fn a_generated_always_identity_column_is_rejected() {
    let message = rejected(
        &[(
            "t.sql",
            "CREATE TABLE public.t (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v text);",
        )],
        &[(
            "bad.sql",
            "INSERT INTO public.t (id, v) VALUES (1, 'a') ON CONFLICT (id) DO NOTHING;",
        )],
    );
    assert!(message.contains("GENERATED ALWAYS"), "{message}");
}

#[test]
fn a_by_default_identity_column_may_be_seeded() {
    ok(
        &[(
            "t.sql",
            "CREATE TABLE public.t (id int GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, v text);",
        )],
        &[(
            "rows.sql",
            "INSERT INTO public.t (id, v) VALUES (1, 'a') ON CONFLICT (id) DO NOTHING;",
        )],
    );
}

#[test]
fn a_generated_expression_column_is_rejected() {
    let message = rejected(
        &[(
            "t.sql",
            "CREATE TABLE public.t (id int PRIMARY KEY, v int, dbl int GENERATED ALWAYS AS (v * 2) STORED);",
        )],
        &[(
            "bad.sql",
            "INSERT INTO public.t (id, dbl) VALUES (1, 2) ON CONFLICT (id) DO NOTHING;",
        )],
    );
    assert!(message.contains("GENERATED ALWAYS"), "{message}");
}

#[test]
fn an_unqualified_table_is_rejected() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO machine (machine_id) VALUES (1) ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
    assert!(message.contains("not schema-qualified"), "{message}");
    assert!(message.contains("empty search_path"), "{message}");
}

#[test]
fn a_table_the_tree_does_not_describe_is_rejected() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.other (id) VALUES (1) ON CONFLICT (id) DO NOTHING;",
        )],
    );
    assert!(
        message.contains("which the source tree does not describe"),
        "{message}"
    );
}

#[test]
fn the_conflict_target_must_match_a_unique_column_set() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id, val) VALUES (1, 'a') \
             ON CONFLICT (machine_id, val) DO NOTHING;",
        )],
    );
    assert!(
        message.contains("matches no primary key, unique constraint or unique index"),
        "{message}"
    );
}

#[test]
fn a_unique_index_arbitrates() {
    ok(
        &[
            (
                "t.sql",
                "CREATE TABLE public.t (a int PRIMARY KEY, b text);",
            ),
            ("i.sql", "CREATE UNIQUE INDEX t_b ON public.t (b);"),
        ],
        &[(
            "rows.sql",
            "INSERT INTO public.t (a, b) VALUES (1, 'x') ON CONFLICT (b) DO NOTHING;",
        )],
    );
}

#[test]
fn on_constraint_must_name_a_modeled_constraint() {
    ok(
        &[("machine.sql", MACHINE)],
        &[(
            "rows.sql",
            "INSERT INTO public.machine (machine_id, val) VALUES (1, 'a') \
             ON CONFLICT ON CONSTRAINT machine_val_key DO NOTHING;",
        )],
    );
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id) VALUES (1) \
             ON CONFLICT ON CONSTRAINT no_such_key DO NOTHING;",
        )],
    );
    assert!(
        message.contains("names no PRIMARY KEY or UNIQUE constraint"),
        "{message}"
    );
}

#[test]
fn an_expression_conflict_target_is_rejected_with_the_on_constraint_remedy() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id, val) VALUES (1, 'a') \
             ON CONFLICT ((lower(val))) DO NOTHING;",
        )],
    );
    assert!(message.contains("is an expression"), "{message}");
    assert!(message.contains("ON CONFLICT ON CONSTRAINT"), "{message}");
}

#[test]
fn a_partial_index_arbiter_is_rejected() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id, val) VALUES (1, 'a') \
             ON CONFLICT (val) WHERE val IS NOT NULL DO NOTHING;",
        )],
    );
    assert!(message.contains("partial-index WHERE"), "{message}");
}

#[test]
fn a_targetless_do_nothing_passes() {
    ok(
        &[("machine.sql", MACHINE)],
        &[(
            "rows.sql",
            "INSERT INTO public.machine (machine_id) VALUES (1) ON CONFLICT DO NOTHING;",
        )],
    );
}

#[test]
fn a_function_outside_pg_catalog_is_rejected() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id, val) VALUES (1, public.f()) \
             ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
    assert!(message.contains("function in schema public"), "{message}");
}

#[test]
fn built_in_functions_pass_qualified_or_bare() {
    ok(
        &[("machine.sql", MACHINE)],
        &[(
            "rows.sql",
            "INSERT INTO public.machine (machine_id, val) \
             VALUES (1, upper('a')), (2, pg_catalog.lower('B')) \
             ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
}

#[test]
fn a_bare_cast_to_a_tree_defined_type_is_rejected() {
    let tree = &[
        ("machine.sql", MACHINE),
        ("d.sql", "CREATE DOMAIN public.label AS text;"),
    ];
    let message = rejected(
        tree,
        &[(
            "bad.sql",
            "INSERT INTO public.machine (machine_id, val) VALUES (1, 'a'::label) \
             ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
    assert!(message.contains("cast to label"), "{message}");
    assert!(message.contains("public.label"), "{message}");

    ok(
        tree,
        &[(
            "rows.sql",
            "INSERT INTO public.machine (machine_id, val) VALUES (1, 'a'::public.label) \
             ON CONFLICT (machine_id) DO NOTHING;",
        )],
    );
}

#[test]
fn two_do_updates_sharing_a_conflict_target_warn() {
    let analysis = ok(
        &[("machine.sql", MACHINE)],
        &[
            (
                "a.sql",
                "INSERT INTO public.machine AS m (machine_id, val) VALUES (1, 'a') \
                 ON CONFLICT (machine_id) DO UPDATE SET val = excluded.val \
                 WHERE m.val IS DISTINCT FROM excluded.val;",
            ),
            (
                "b.sql",
                "INSERT INTO public.machine AS m (machine_id, val) VALUES (1, 'b') \
                 ON CONFLICT (machine_id) DO UPDATE SET val = excluded.val \
                 WHERE m.val IS DISTINCT FROM excluded.val;",
            ),
        ],
    );
    assert_eq!(analysis.seeds.do_update_collisions.len(), 1);
    let collision = &analysis.seeds.do_update_collisions[0];
    assert_eq!(collision.table.to_string(), "public.machine");
    assert_eq!(collision.origins.len(), 2);
}

#[test]
fn overriding_is_rejected() {
    let message = rejected(
        &[(
            "t.sql",
            "CREATE TABLE public.t (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY);",
        )],
        &[(
            "bad.sql",
            "INSERT INTO public.t (id) OVERRIDING SYSTEM VALUE VALUES (1) \
             ON CONFLICT (id) DO NOTHING;",
        )],
    );
    assert!(message.contains("OVERRIDING is not allowed"), "{message}");
}

#[test]
fn a_seed_that_does_not_parse_names_the_file() {
    let message = rejected(&[("machine.sql", MACHINE)], &[("bad.sql", "INSERT INTO;")]);
    assert!(message.contains("could not parse SQL"), "{message}");
    assert!(message.contains("bad.sql"), "{message}");
}

#[test]
fn every_problem_is_reported_not_just_the_first() {
    let message = rejected(
        &[("machine.sql", MACHINE)],
        &[
            ("a.sql", "DELETE FROM public.machine;"),
            (
                "b.sql",
                "INSERT INTO public.machine (machine_id) VALUES (1);",
            ),
        ],
    );
    assert!(message.contains("must be an INSERT"), "{message}");
    assert!(message.contains("no ON CONFLICT"), "{message}");
}

/// No seeds is the common case, and must change nothing.
#[test]
fn no_seed_files_is_no_seeds() {
    let analysis = ok(&[("machine.sql", MACHINE)], &[]);
    assert!(analysis.seeds.is_empty());
    assert_eq!(analysis.counts.seed_files, 0);
}

/// A named arbiter and its column-list spelling are the same conflict
/// target, so the collision warning groups them together.
#[test]
fn collisions_are_detected_across_arbiter_spellings() {
    let analysis = ok(
        &[("machine.sql", MACHINE)],
        &[
            (
                "a.sql",
                "INSERT INTO public.machine AS m (machine_id, val) VALUES (1, 'a') \
                 ON CONFLICT (val) DO UPDATE SET machine_id = excluded.machine_id \
                 WHERE m.machine_id IS DISTINCT FROM excluded.machine_id;",
            ),
            (
                "b.sql",
                "INSERT INTO public.machine AS m (machine_id, val) VALUES (2, 'a') \
                 ON CONFLICT ON CONSTRAINT machine_val_key DO UPDATE \
                 SET machine_id = excluded.machine_id \
                 WHERE m.machine_id IS DISTINCT FROM excluded.machine_id;",
            ),
        ],
    );
    assert_eq!(analysis.seeds.do_update_collisions.len(), 1);
    assert_eq!(analysis.seeds.do_update_collisions[0].origins.len(), 2);
}
