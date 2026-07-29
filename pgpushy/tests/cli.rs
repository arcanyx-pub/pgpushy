//! `pgpushy validate` end to end.
//!
//! These run the real binary against real directories, which is what makes
//! them worth having on top of the core crate's string-literal tests: the
//! filesystem behavior — hidden files, symlinks, exclusions, ordering — only
//! exists here, and none of it needs a database.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use std::path::Path;
use tempfile::TempDir;

/// Build a source tree from `(relative path, contents)` pairs.
fn tree(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    for (path, contents) in files {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create dirs");
        }
        std::fs::write(&full, contents).expect("write file");
    }
    dir
}

fn validate(root: &Path) -> Command {
    let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
    cmd.args(["validate", "--source-root"]).arg(root);
    cmd
}

const ORDERS: &str =
    "CREATE TABLE orders (id int PRIMARY KEY, customer_id int REFERENCES customers(id));";
const CUSTOMERS: &str = "CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);";

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
    let dir = tree(&[
        ("schema/customers.sql", CUSTOMERS),
        ("seeds/data.sql", "INSERT INTO customers VALUES (1, 'joe');"),
        (
            "schema/customers.test.sql",
            "INSERT INTO customers VALUES (2, 'ann');",
        ),
    ]);

    validate(dir.path())
        .args(["--exclude", "seeds/**", "--exclude", "**/*.test.sql"])
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
    let dir = tree(&[("schema/customers.sql", CUSTOMERS)]);
    std::os::unix::fs::symlink(dir.path().join("schema"), dir.path().join("loop"))
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
    let out = dir.path().join("desired.sql");

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
/// and pgpushy's own output must not come back as input on the next run — every
/// object in it would be reported as a duplicate of itself.
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

/// A declared schema the tree never mentions reconciles to empty, which is
/// destructive. It must be said out loud rather than left to the reader.
#[test]
fn warns_when_a_managed_schema_has_no_source() {
    let dir = tree(&[("customers.sql", CUSTOMERS)]);

    validate(dir.path())
        .args(["--managed-schema", "public", "--managed-schema", "legacy"])
        .assert()
        .success()
        .stdout(predicates::str::contains("WARNING"))
        .stdout(predicates::str::contains("legacy"));
}

#[test]
fn rejects_an_object_in_an_undeclared_schema() {
    let dir = tree(&[
        ("customers.sql", CUSTOMERS),
        (
            "events.sql",
            "CREATE TABLE analytics.events (id int PRIMARY KEY);",
        ),
    ]);

    validate(dir.path())
        .args(["--managed-schema", "public"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("analytics"))
        .stderr(predicates::str::contains("events.sql"));
}

#[test]
fn an_empty_tree_is_not_an_error() {
    let dir = TempDir::new().expect("temp dir");
    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("(0 files)"));
}

#[test]
fn a_missing_source_root_is_an_error() {
    let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
    cmd.args(["validate", "--source-root", "/nonexistent/path/for/a/test"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("does not exist"));
}

// ---------------------------------------------------------------------------
// pgpushy.toml (spec §10)
// ---------------------------------------------------------------------------

/// A project laid out the way the configuration file expects.
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

/// `pgpushy validate` run from inside the project, with no flags at all.
fn bare(root: &Path) -> Command {
    let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
    cmd.arg("validate").current_dir(root);
    cmd
}

#[test]
fn a_config_file_supplies_everything_with_no_flags() {
    let dir = project(
        r#"
        source_root = "db/schema"
        default_schema = "app"
        exclude = ["seeds/**"]
        "#,
        &[
            ("db/schema/customers.sql", CUSTOMERS),
            ("db/schema/orders.sql", ORDERS),
            (
                "db/schema/seeds/data.sql",
                "INSERT INTO customers VALUES (1, 'joe');",
            ),
        ],
    );

    bare(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("config: pgpushy.toml"))
        .stdout(predicates::str::contains("2 files, 1 excluded"))
        .stdout(predicates::str::contains("managed schemas: app"));
}

#[test]
fn no_config_file_is_not_an_error() {
    let dir = tree(&[("customers.sql", CUSTOMERS)]);

    bare(dir.path())
        .assert()
        .success()
        // Nothing to report when there was no file to read.
        .stdout(predicates::str::contains("config:").not())
        .stdout(predicates::str::contains("managed schemas: public"));
}

#[test]
fn a_flag_beats_the_config_file() {
    let dir = project(
        "default_schema = \"from_file\"",
        &[("customers.sql", CUSTOMERS)],
    );

    bare(dir.path())
        .args(["--default-schema", "from_flag"])
        .assert()
        .success()
        .stdout(predicates::str::contains("managed schemas: from_flag"));
}

/// Spec §10 precedence is CLI over file, and for lists that means *replace*:
/// otherwise a project's exclusions could never be narrowed for one run.
#[test]
fn an_exclude_flag_replaces_the_files_excludes() {
    let files = &[
        ("customers.sql", CUSTOMERS),
        ("seeds/data.sql", "INSERT INTO customers VALUES (1, 'joe');"),
    ];
    let dir = project(r#"exclude = ["seeds/**"]"#, files);

    // With the file's list, the seed file is excluded and all is well.
    bare(dir.path()).assert().success();

    // Replaced by a pattern that matches nothing, the seed file comes back —
    // and the allow-list rejects it.
    bare(dir.path())
        .args(["--exclude", "no-such-dir/**"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("unsupported statement: INSERT"));
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

    bare(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("analytics"))
        .stderr(predicates::str::contains("managed_schemas"));
}

/// Paths in the file are relative to the file, not to the working directory,
/// so `--config` pointing elsewhere means what it looks like.
#[test]
fn file_paths_resolve_against_the_config_files_directory() {
    let dir = project(
        "source_root = \"db/schema\"",
        &[("db/schema/customers.sql", CUSTOMERS)],
    );
    let elsewhere = TempDir::new().expect("temp dir");

    let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
    cmd.arg("validate")
        .current_dir(elsewhere.path())
        .arg("--config")
        .arg(dir.path().join("pgpushy.toml"))
        .assert()
        .success()
        .stdout(predicates::str::contains("1 table"));
}

/// A mistyped key would otherwise be invisible: pgpushy would simply behave as
/// though the setting were absent.
#[test]
fn a_mistyped_key_is_rejected_with_the_valid_ones_listed() {
    let dir = project(
        r#"exclude_patterns = ["x"]"#,
        &[("customers.sql", CUSTOMERS)],
    );

    bare(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "unknown field `exclude_patterns`",
        ))
        .stderr(predicates::str::contains("source_root"));
}

#[test]
fn an_explicit_config_that_does_not_exist_is_an_error() {
    let dir = tree(&[("customers.sql", CUSTOMERS)]);

    bare(dir.path())
        .args(["--config", "/nonexistent/pgpushy.toml"])
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

    bare(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("not available yet"))
        .stderr(predicates::str::contains("fast-follow"));
}

/// Spec §10: the file is read from the working directory and *not* searched
/// for in parent directories. Running from a subdirectory therefore picks up
/// nothing — which is only safe because the absent `config:` line says so.
#[test]
fn the_config_file_is_not_searched_for_in_parent_directories() {
    let dir = project(
        "default_schema = \"app\"",
        &[("db/customers.sql", CUSTOMERS)],
    );

    bare(&dir.path().join("db"))
        .assert()
        .success()
        .stdout(predicates::str::contains("config:").not())
        .stdout(predicates::str::contains("managed schemas: public"));
}
