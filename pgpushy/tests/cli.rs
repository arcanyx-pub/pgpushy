//! `pgpushy validate` and the configuration file, end to end.
//!
//! These run the real binary against real directories, which is what makes
//! them worth having on top of the core crate's string-literal tests: the
//! filesystem behavior — hidden files, symlinks, exclusions, ordering,
//! configuration discovery — only exists here, and none of it needs a database.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use std::path::Path;
use tempfile::TempDir;

const ORDERS: &str =
    "CREATE TABLE orders (id int PRIMARY KEY, customer_id int REFERENCES customers(id));";
const CUSTOMERS: &str = "CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);";

/// A project: a `pgpushy.toml` plus the files around it.
///
/// Every test needs one, because configuration is required (spec §10.1) —
/// which is itself the point, so the tests exercise the only real user path.
fn project(config: &str, files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join("pgpushy.toml"), config).expect("write config");
    for (path, contents) in files {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create dirs");
        }
        std::fs::write(&full, contents).expect("write file");
    }
    dir
}

/// A project with nothing configured: `source_root` then defaults to the
/// directory holding the file.
fn tree(files: &[(&str, &str)]) -> TempDir {
    project("", files)
}

/// `pgpushy validate` pointed at a project, from anywhere.
fn validate(root: &Path) -> Command {
    let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
    cmd.arg("validate")
        .arg("--config")
        .arg(root.join("pgpushy.toml"));
    cmd
}

/// `pgpushy plan` pointed at a project. Used only for the tests that never get
/// as far as connecting.
fn plan(root: &Path, env: &str) -> Command {
    let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
    cmd.args(["plan", "--env", env])
        .arg("--config")
        .arg(root.join("pgpushy.toml"));
    cmd
}

// ---------------------------------------------------------------------------
// The source tree
// ---------------------------------------------------------------------------

#[test]
fn accepts_a_valid_tree() {
    let dir = tree(&[("b/orders.sql", ORDERS), ("a/customers.sql", CUSTOMERS)]);

    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("2 tables, 1 foreign key"))
        .stdout(predicates::str::contains("managed schemas: public"))
        .stdout(predicates::str::contains("ok  no duplicate objects"));
}

#[test]
fn rejects_a_statement_outside_the_allow_list_naming_file_and_line() {
    let dir = tree(&[
        ("schema/customers.sql", CUSTOMERS),
        (
            "seeds/data.sql",
            "-- seed data\nINSERT INTO customers VALUES (1, 'joe');",
        ),
    ]);

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("unsupported statement: INSERT"))
        .stderr(predicates::str::contains("seeds/data.sql:2"));
}

#[test]
fn exclusions_keep_non_schema_sql_out_of_the_tree() {
    let dir = project(
        r#"exclude = ["seeds/**", "**/*.test.sql"]"#,
        &[
            ("schema/customers.sql", CUSTOMERS),
            ("seeds/data.sql", "INSERT INTO customers VALUES (1, 'joe');"),
            (
                "schema/customers.test.sql",
                "INSERT INTO customers VALUES (2, 'ann');",
            ),
        ],
    );

    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("2 excluded"))
        .stdout(predicates::str::contains(r#"excluded by "seeds/**": 1"#));
}

/// Hidden directories are skipped, so an editor's `.backup/` or a stray
/// `.git/` full of SQL cannot join the desired state.
#[test]
fn ignores_hidden_files_and_directories() {
    let dir = tree(&[
        ("customers.sql", CUSTOMERS),
        (".backup/customers.sql", "CREATE TABLE customers (id int);"),
        (".customers.sql.swp", "garbage not sql"),
    ]);

    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("(1 file)"));
}

/// A symlinked directory can point back up the tree; following it either loops
/// or silently pulls in files from outside the source root.
#[cfg(unix)]
#[test]
fn does_not_follow_symlinked_directories() {
    let dir = project(
        "source_root = \"schema\"",
        &[("schema/customers.sql", CUSTOMERS)],
    );
    std::os::unix::fs::symlink(dir.path().join("schema"), dir.path().join("schema/loop"))
        .expect("create symlink");

    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("(1 file)"))
        // Following the link would find customers.sql twice.
        .stdout(predicates::str::contains("1 table"));
}

#[test]
fn writes_the_desired_state_with_out() {
    let dir = tree(&[("orders.sql", ORDERS), ("customers.sql", CUSTOMERS)]);
    let outputs = TempDir::new().expect("temp dir");
    let out = outputs.path().join("desired.sql");

    validate(dir.path())
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    let contents = std::fs::read_to_string(&out).expect("desired state written");
    assert!(
        contents.contains("CREATE TABLE public.customers"),
        "{contents}"
    );
    assert!(
        contents.contains(
            "ALTER TABLE public.orders ADD FOREIGN KEY (customer_id) REFERENCES public.customers (id);"
        ),
        "foreign key should be lifted and unnamed:\n{contents}"
    );
    assert!(
        !contents.contains("REFERENCES customers(id),"),
        "no inline foreign key should survive:\n{contents}"
    );
}

/// Writing the desired state into the source root is a natural thing to do,
/// and pgpushy's own output must not come back as input — every object in it
/// would be reported as a duplicate of itself. The generated document is
/// recognized by its first line, so this holds on later runs too, whether or
/// not they pass `--out`.
#[test]
fn does_not_read_back_its_own_output() {
    let dir = tree(&[("orders.sql", ORDERS), ("customers.sql", CUSTOMERS)]);
    let out = dir.path().join("desired.sql");

    validate(dir.path())
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    validate(dir.path())
        .arg("--out")
        .arg(&out)
        .assert()
        .success()
        .stdout(predicates::str::contains("2 tables"));

    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("2 tables"))
        .stdout(predicates::str::contains(
            "1 previously generated by pgpushy",
        ));
}

/// Spec §11.3, from the outside: the same tree must produce the same bytes.
#[test]
fn output_is_byte_identical_across_runs() {
    let dir = tree(&[
        (
            "z.sql",
            "CREATE TABLE zebra (id int PRIMARY KEY, a int REFERENCES aardvark(id));",
        ),
        ("a.sql", "CREATE TABLE aardvark (id int PRIMARY KEY);"),
        (
            "nested/deep/m.sql",
            "CREATE TABLE mongoose (id int PRIMARY KEY);",
        ),
    ]);

    let outputs = TempDir::new().expect("temp dir");
    let first = outputs.path().join("first.sql");
    let second = outputs.path().join("second.sql");
    validate(dir.path())
        .arg("--out")
        .arg(&first)
        .assert()
        .success();
    validate(dir.path())
        .arg("--out")
        .arg(&second)
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(&first).unwrap(),
        std::fs::read_to_string(&second).unwrap(),
    );
}

#[test]
fn rejects_a_cross_schema_cycle() {
    let dir = tree(&[(
        "cycle.sql",
        "CREATE SCHEMA billing;
         CREATE TABLE customers (id int PRIMARY KEY, a int REFERENCES billing.accounts(id));
         CREATE TABLE billing.accounts (id int PRIMARY KEY, o int REFERENCES customers(id));",
    )]);

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("cross-schema foreign key cycle"))
        .stderr(predicates::str::contains("billing"))
        .stderr(predicates::str::contains("public"));
}

#[test]
fn reports_both_definitions_of_a_duplicate() {
    let dir = tree(&[
        ("a/orders.sql", ORDERS),
        ("b/orders.sql", ORDERS),
        ("c.sql", CUSTOMERS),
    ]);

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "table public.orders is defined 2 times",
        ))
        .stderr(predicates::str::contains("a/orders.sql"))
        .stderr(predicates::str::contains("b/orders.sql"));
}

#[test]
fn an_empty_tree_is_not_an_error() {
    let dir = tree(&[]);
    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("(0 files)"));
}

#[test]
fn a_missing_source_root_is_an_error() {
    let dir = project(
        "source_root = \"no-such-directory\"",
        &[("customers.sql", CUSTOMERS)],
    );

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("does not exist"));
}

// ---------------------------------------------------------------------------
// Configuration (spec §10.1)
// ---------------------------------------------------------------------------

/// The reason configuration is required: without it, a run from the wrong
/// directory would treat a fragment of the tree as the whole desired state.
#[test]
fn refuses_to_run_without_a_configuration_file() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join("customers.sql"), CUSTOMERS).expect("write");

    Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .arg("validate")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("no pgpushy.toml"))
        // The message must explain itself and show a working starting point.
        .stderr(predicates::str::contains("plan to drop everything else"))
        .stderr(predicates::str::contains("[env.local]"));
}

#[test]
fn finds_the_config_in_the_working_directory() {
    let dir = project("default_schema = \"app\"", &[("customers.sql", CUSTOMERS)]);

    Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .arg("validate")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("config: pgpushy.toml"))
        .stdout(predicates::str::contains("managed schemas: app"));
}

/// Spec §10.1: not searched for in parent directories. Running from a
/// subdirectory therefore fails outright rather than silently reconciling a
/// fragment of the tree — which is the whole reason the file is required.
#[test]
fn the_config_file_is_not_searched_for_in_parent_directories() {
    let dir = project(
        "default_schema = \"app\"",
        &[("db/customers.sql", CUSTOMERS)],
    );

    Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .arg("validate")
        .current_dir(dir.path().join("db"))
        .assert()
        .failure()
        .stderr(predicates::str::contains("no pgpushy.toml"));
}

/// The source tree is anchored to the project, not to the caller — so where
/// pgpushy runs from cannot change what it reconciles.
#[test]
fn the_source_root_is_relative_to_the_config_file() {
    let dir = project(
        "source_root = \"db/schema\"",
        &[("db/schema/customers.sql", CUSTOMERS)],
    );
    let elsewhere = TempDir::new().expect("temp dir");

    Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .arg("validate")
        .current_dir(elsewhere.path())
        .arg("--config")
        .arg(dir.path().join("pgpushy.toml"))
        .assert()
        .success()
        .stdout(predicates::str::contains("1 table"));
}

#[test]
fn managed_schemas_can_be_declared_in_the_file() {
    let dir = project(
        r#"managed_schemas = ["public"]"#,
        &[
            ("customers.sql", CUSTOMERS),
            (
                "events.sql",
                "CREATE TABLE analytics.events (id int PRIMARY KEY);",
            ),
        ],
    );

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("analytics"))
        .stderr(predicates::str::contains("managed_schemas"));
}

/// A declared schema the tree never mentions reconciles to empty, which is
/// destructive. It must be said out loud rather than left to the reader.
#[test]
fn warns_when_a_managed_schema_has_no_source() {
    let dir = project(
        r#"managed_schemas = ["public", "legacy"]"#,
        &[("c.sql", CUSTOMERS)],
    );

    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("WARNING"))
        .stdout(predicates::str::contains("legacy"));
}

/// A mistyped key would otherwise be invisible: pgpushy would simply behave as
/// though the setting were absent.
#[test]
fn a_mistyped_key_is_rejected_with_the_valid_ones_listed() {
    let dir = project(
        r#"exclude_patterns = ["x"]"#,
        &[("customers.sql", CUSTOMERS)],
    );

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "unknown field `exclude_patterns`",
        ))
        .stderr(predicates::str::contains("source_root"));
}

#[test]
fn an_explicit_config_that_does_not_exist_is_an_error() {
    Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .args(["validate", "--config", "/nonexistent/pgpushy.toml"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no configuration file at"));
}

/// The managed backend is in the spec but not built yet, so someone reading
/// the spec will try it. "Unknown field" would be the wrong answer.
#[test]
fn settings_that_are_not_built_yet_say_so_plainly() {
    let dir = project(
        "[pgschema]\nbackend = \"managed\"",
        &[("customers.sql", CUSTOMERS)],
    );

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("not available yet"))
        .stderr(predicates::str::contains("fast-follow"));
}

// ---------------------------------------------------------------------------
// Environments (spec §10.2)
// ---------------------------------------------------------------------------

/// `plan` and `apply` name their target; `validate` has none, and must not
/// pretend otherwise.
#[test]
fn validate_takes_no_env() {
    let dir = tree(&[("customers.sql", CUSTOMERS)]);

    validate(dir.path())
        .args(["--env", "local"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("unexpected argument"));
}

#[test]
fn plan_requires_an_env() {
    let dir = tree(&[("customers.sql", CUSTOMERS)]);

    Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .arg("plan")
        .arg("--config")
        .arg(dir.path().join("pgpushy.toml"))
        .assert()
        .failure()
        .stderr(predicates::str::contains("--env"));
}

#[test]
fn an_unknown_env_lists_the_defined_ones() {
    let dir = project(
        "[env.local]\ndb = \"a\"\nuser = \"u\"\n[env.prod]\ndb = \"b\"\nuser = \"u\"",
        &[("customers.sql", CUSTOMERS)],
    );

    plan(dir.path(), "staging")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "no environment named \"staging\"",
        ))
        .stderr(predicates::str::contains("local, prod"));
}

#[test]
fn a_file_with_no_environments_says_how_to_add_one() {
    let dir = tree(&[("customers.sql", CUSTOMERS)]);

    plan(dir.path(), "local")
        .assert()
        .failure()
        .stderr(predicates::str::contains("no environments are defined"))
        .stderr(predicates::str::contains("[env.local]"));
}

/// Reconciling the wrong database, or as the wrong role, are the mistakes
/// naming an environment exists to prevent — so neither has a default.
#[test]
fn an_environment_must_say_which_database_and_who() {
    let dir = project(
        "[env.local]\nhost = \"db\"",
        &[("customers.sql", CUSTOMERS)],
    );

    plan(dir.path(), "local")
        .assert()
        .failure()
        .stderr(predicates::str::contains("missing db and user"));
}

/// A bad target fails before the source tree is read: the problem is with the
/// command, not with the SQL, and saying so first is less confusing.
#[test]
fn the_target_is_resolved_before_anything_is_parsed() {
    let dir = project(
        "[env.local]\ndb = \"a\"\nuser = \"u\"",
        &[("broken.sql", "CREATE TABLE (((;")],
    );

    plan(dir.path(), "nope")
        .assert()
        .failure()
        .stderr(predicates::str::contains("no environment named"))
        .stderr(predicates::str::contains("could not parse SQL").not());
}
