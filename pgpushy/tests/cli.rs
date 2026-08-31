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

/// A link to a file is desired state like any other file: a `.sql` file
/// missing from it is a file scheduled for deletion.
#[cfg(unix)]
#[test]
fn follows_symlinked_files() {
    let dir = project(
        "source_root = \"schema\"",
        &[
            ("schema/orders.sql", ORDERS),
            ("shared/customers.sql", CUSTOMERS),
        ],
    );
    std::os::unix::fs::symlink(
        dir.path().join("shared/customers.sql"),
        dir.path().join("schema/customers.sql"),
    )
    .expect("create symlink");

    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("(2 files)"))
        // Dropping the link would leave orders' foreign key pointing at nothing.
        .stdout(predicates::str::contains("2 tables, 1 foreign key"));
}

/// Passing over a link pgpushy cannot read would drop the objects it defines
/// from the desired state, and pgschema deletes what the desired state omits.
#[cfg(unix)]
#[test]
fn rejects_a_symlink_that_cannot_be_resolved() {
    let dir = project("source_root = \"schema\"", &[("schema/orders.sql", ORDERS)]);
    std::os::unix::fs::symlink(
        dir.path().join("shared/customers.sql"),
        dir.path().join("schema/customers.sql"),
    )
    .expect("create symlink");

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("1 symbolic link"))
        .stderr(predicates::str::contains("customers.sql -> "))
        .stderr(predicates::str::contains("No such file or directory"));
}

#[test]
fn writes_the_desired_state_with_out() {
    let dir = tree(&[("orders.sql", ORDERS), ("customers.sql", CUSTOMERS)]);
    let outputs = TempDir::new().expect("temp dir");
    let out = outputs.path().join("desired");

    validate(dir.path())
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    // One document per managed schema, named after it (spec §8.7).
    let contents = std::fs::read_to_string(out.join("public.sql")).expect("desired state written");
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
    let first = outputs.path().join("first");
    let second = outputs.path().join("second");
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
        std::fs::read_to_string(first.join("public.sql")).unwrap(),
        std::fs::read_to_string(second.join("public.sql")).unwrap(),
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

/// The managed backend is the default, so naming it explicitly must simply
/// work — this used to be the "not built yet" case.
#[test]
fn the_managed_backend_is_accepted() {
    let dir = project(
        "[pgschema]\nbackend = \"managed\"",
        &[("customers.sql", CUSTOMERS)],
    );

    // `validate` never resolves a pgschema binary, so this checks the setting
    // is accepted without downloading anything.
    validate(dir.path()).assert().success();
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

// ---------------------------------------------------------------------------
// init (spec §10.1)
// ---------------------------------------------------------------------------

fn init_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
    cmd.arg("init").current_dir(dir);
    cmd
}

/// Configuration is required, so this is the first command most projects run —
/// and what it writes has to be loadable without further editing beyond the
/// target.
#[test]
fn init_writes_a_config_that_validate_can_load() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::create_dir_all(dir.path().join("db/schema")).expect("mkdir");
    std::fs::write(dir.path().join("db/schema/customers.sql"), CUSTOMERS).expect("write");

    init_in(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("wrote pgpushy.toml"))
        .stdout(predicates::str::contains("source_root: db/schema"));

    // The generated file is immediately usable for the offline command.
    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("1 table"));
}

#[test]
fn init_leaves_the_source_root_alone_for_a_flat_project() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join("customers.sql"), CUSTOMERS).expect("write");

    init_in(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("source_root: this directory"));

    let written = std::fs::read_to_string(dir.path().join("pgpushy.toml")).expect("read");
    assert!(
        written.contains("# source_root = "),
        "the key should be present but commented out:\n{written}"
    );
    validate(dir.path()).assert().success();
}

/// A configuration file is the one thing in the project whose loss would be
/// both silent and expensive.
#[test]
fn init_refuses_to_overwrite() {
    let dir = project("", &[("customers.sql", CUSTOMERS)]);

    init_in(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("already exists"))
        .stderr(predicates::str::contains("will not overwrite"));
}

/// Guessing wrong would silently narrow the desired state, so two candidate
/// roots means pgpushy declines to guess at all.
#[test]
fn init_does_not_guess_between_two_candidate_roots() {
    let dir = TempDir::new().expect("temp dir");
    for path in ["db/customers.sql", "other/orders.sql"] {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).expect("mkdir");
        std::fs::write(&full, CUSTOMERS).expect("write");
    }

    init_in(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("source_root: this directory"));
}

// ---------------------------------------------------------------------------
// Empty trees and bad source roots
// ---------------------------------------------------------------------------

/// Nothing to reconcile is not a failure, but a silent success looks exactly
/// like a successful reconciliation — so it has to say which it was.
#[test]
fn an_empty_tree_says_there_is_nothing_to_reconcile() {
    let dir = tree(&[]);

    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("No managed schemas"))
        .stdout(predicates::str::contains("will not touch anything"));
}

/// Easy to reach by pointing `source_root` at the one file a small project has.
#[test]
fn a_source_root_that_is_a_file_says_so() {
    let dir = project(
        "source_root = \"customers.sql\"",
        &[("customers.sql", CUSTOMERS)],
    );

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("is not a directory"))
        .stderr(predicates::str::contains("names the directory"));
}

// ---------------------------------------------------------------------------
// Verbose (spec §10.3)
// ---------------------------------------------------------------------------

#[test]
fn verbose_lists_the_files_discovery_kept() {
    let dir = project(
        r#"exclude = ["seeds/**"]"#,
        &[
            ("a/customers.sql", CUSTOMERS),
            ("b/orders.sql", ORDERS),
            ("seeds/data.sql", "INSERT INTO customers VALUES (1, 'x');"),
        ],
    );

    validate(dir.path())
        .arg("--verbose")
        .assert()
        .success()
        .stdout(predicates::str::contains("a/customers.sql"))
        .stdout(predicates::str::contains("b/orders.sql"))
        // Excluded files were never read, so they are not "kept".
        .stdout(predicates::str::contains("seeds/data.sql").not());
}

// ---------------------------------------------------------------------------
// Plan database and lock timeout (spec §10.4, §10.5)
// ---------------------------------------------------------------------------

/// Same rule as the target itself: pointing scratch work at the wrong server
/// is not something to guess at.
#[test]
fn a_plan_database_must_say_which_database_and_who() {
    let dir = project(
        "[env.local]\ndb = \"a\"\nuser = \"u\"\n[env.local.plan_db]\nhost = \"plan.example\"",
        &[("customers.sql", CUSTOMERS)],
    );

    plan(dir.path(), "local")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "[env.local.plan_db] is missing db and user",
        ))
        // The reason it matters: pgschema writes there.
        .stderr(predicates::str::contains("scratch space"));
}

#[test]
fn an_unknown_key_in_a_plan_database_is_rejected() {
    let dir = project(
        "[env.local]\ndb = \"a\"\nuser = \"u\"\n[env.local.plan_db]\ndatabase = \"x\"",
        &[("customers.sql", CUSTOMERS)],
    );

    plan(dir.path(), "local")
        .assert()
        .failure()
        .stderr(predicates::str::contains("unknown field `database`"));
}

/// `plan` has no `--lock-timeout` because pgschema's `plan` does not accept one
/// (verified) — it is an apply-time concern.
#[test]
fn only_apply_takes_a_lock_timeout() {
    let dir = project(
        "[env.local]\ndb = \"a\"\nuser = \"u\"",
        &[("c.sql", CUSTOMERS)],
    );

    plan(dir.path(), "local")
        .args(["--lock-timeout", "30s"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("unexpected argument"));
}

// ---------------------------------------------------------------------------
// The pgschema backend (spec §8.5)
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_backend_names_the_valid_ones() {
    let dir = project(
        "[pgschema]\nbackend = \"magic\"",
        &[("customers.sql", CUSTOMERS)],
    );

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("unknown pgschema backend"))
        .stderr(predicates::str::contains("managed"))
        .stderr(predicates::str::contains("byo"));
}

/// Rejected while loading the configuration, before a source tree is parsed —
/// a typo in the backend name is a problem with the command, not the SQL.
#[test]
fn a_bad_backend_is_rejected_before_parsing() {
    let dir = project(
        "[pgschema]\nbackend = \"magic\"",
        &[("broken.sql", "CREATE TABLE (((;")],
    );

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("unknown pgschema backend"))
        .stderr(predicates::str::contains("could not parse SQL").not());
}

/// An explicit path means BYO whatever the backend says: naming a binary and
/// then downloading a different one would be absurd.
#[test]
fn an_explicit_path_wins_over_the_managed_backend() {
    let dir = project(
        "[pgschema]\nbackend = \"managed\"\npath = \"/nonexistent/pgschema\"\n\
         [env.local]\ndb = \"a\"\nuser = \"u\"",
        &[("customers.sql", CUSTOMERS)],
    );

    // Reaching the BYO "no binary here" error proves nothing was downloaded.
    plan(dir.path(), "local")
        .assert()
        .failure()
        .stderr(predicates::str::contains("no pgschema binary at"));
}

// ---------------------------------------------------------------------------
// Seeds (spec §4.6) — the offline half; execution needs a database and lives
// in integration.rs
// ---------------------------------------------------------------------------

/// `pgpushy generate` pointed at a project.
fn generate(root: &Path, check: bool) -> Command {
    let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
    cmd.arg("generate")
        .arg("--config")
        .arg(root.join("pgpushy.toml"));
    if check {
        cmd.arg("--check");
    }
    cmd
}

const SEED: &str = "INSERT INTO public.customers (id, name) VALUES (1, 'acme') \
                    ON CONFLICT (id) DO NOTHING;";

#[test]
fn validate_accepts_a_seeded_project() {
    let dir = project(
        "source_root = \"schema\"\nseed_root = \"seeds\"\n",
        &[
            ("schema/customers.sql", CUSTOMERS),
            ("seeds/rows.sql", SEED),
        ],
    );

    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "1 seed statement across 1 seed file",
        ))
        .stdout(predicates::str::contains("idempotent by construction"));
}

#[test]
fn validate_rejects_a_bad_seed_naming_file_and_line() {
    let dir = project(
        "source_root = \"schema\"\nseed_root = \"seeds\"\n",
        &[
            ("schema/customers.sql", CUSTOMERS),
            ("seeds/bad.sql", "DELETE FROM public.customers;"),
        ],
    );

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("must be an INSERT, not DELETE"))
        .stderr(predicates::str::contains("bad.sql:1"));
}

#[test]
fn the_seed_root_may_not_be_the_source_root() {
    let dir = project("seed_root = \".\"\n", &[("customers.sql", CUSTOMERS)]);

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("same directory"));
}

#[test]
fn the_source_root_may_not_sit_inside_the_seed_root() {
    let dir = project(
        "source_root = \"seeds/schema\"\nseed_root = \"seeds\"\n",
        &[("seeds/schema/customers.sql", CUSTOMERS)],
    );

    validate(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("is inside seed_root"));
}

/// A seed root nested in the source root is not desired state (spec §4.6):
/// its INSERTs must not reach the §4.3 allow-list.
#[test]
fn a_nested_seed_root_is_excluded_from_desired_state() {
    let dir = project(
        "seed_root = \"seeds\"\n",
        &[("customers.sql", CUSTOMERS), ("seeds/rows.sql", SEED)],
    );

    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "excluded 1 seed file under the seed root",
        ))
        .stdout(predicates::str::contains("1 seed statement"));
}

// ---------------------------------------------------------------------------
// Generated sources (spec §4.7)
// ---------------------------------------------------------------------------

const GENERATED_TABLE: &str = "CREATE TABLE public.leases (id int PRIMARY KEY);";

fn generator_config() -> String {
    format!(
        "source_root = \"schema\"\n\
         \n\
         [[generate]]\n\
         output = \"schema/leases.sql\"\n\
         command = [\"echo\", \"{GENERATED_TABLE}\"]\n"
    )
}

#[test]
fn generate_writes_the_marker_and_discovery_reads_the_file() {
    let dir = project(&generator_config(), &[("schema/customers.sql", CUSTOMERS)]);

    generate(dir.path(), false).assert().success();

    let written =
        std::fs::read_to_string(dir.path().join("schema/leases.sql")).expect("file written");
    assert!(written.starts_with("-- Generated source."), "{written}");
    assert!(written.contains("-- Command: echo"), "{written}");
    assert!(written.contains(GENERATED_TABLE), "{written}");

    // A generated *source* is discovered — opposite polarity to a generated
    // document (spec §4.1) — so validate now sees two tables.
    validate(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("2 tables"));
}

#[test]
fn generate_refuses_to_overwrite_an_unmarked_file() {
    let dir = project(
        &generator_config(),
        &[
            ("schema/customers.sql", CUSTOMERS),
            (
                "schema/leases.sql",
                "CREATE TABLE public.leases (id int PRIMARY KEY);",
            ),
        ],
    );

    generate(dir.path(), false)
        .assert()
        .failure()
        .stderr(predicates::str::contains("generated-source marker"));
}

#[test]
fn generate_check_fails_on_a_stale_output_and_passes_after_regeneration() {
    let dir = project(&generator_config(), &[("schema/customers.sql", CUSTOMERS)]);

    // Before the first generation, the output is missing — that is stale too.
    generate(dir.path(), true)
        .assert()
        .failure()
        .stderr(predicates::str::contains("does not exist"));

    generate(dir.path(), false).assert().success();
    generate(dir.path(), true).assert().success();

    let path = dir.path().join("schema/leases.sql");
    let mut drifted = std::fs::read_to_string(&path).expect("read");
    drifted.push_str("-- drifted\n");
    std::fs::write(&path, drifted).expect("write");

    generate(dir.path(), true)
        .assert()
        .failure()
        .stderr(predicates::str::contains("out of date"))
        .stderr(predicates::str::contains("leases.sql"));

    generate(dir.path(), false).assert().success();
    generate(dir.path(), true).assert().success();
}

#[test]
fn a_generate_output_must_land_under_a_root() {
    let dir = project(
        "source_root = \"schema\"\n\
         \n\
         [[generate]]\n\
         output = \"elsewhere/leases.sql\"\n\
         command = [\"echo\", \"x\"]\n",
        &[("schema/customers.sql", CUSTOMERS)],
    );

    generate(dir.path(), false)
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "neither the source root nor the seed root",
        ));
}

#[test]
fn a_generate_output_may_not_climb_out_of_the_project() {
    let dir = project(
        "[[generate]]\noutput = \"../leases.sql\"\ncommand = [\"echo\", \"x\"]\n",
        &[("customers.sql", CUSTOMERS)],
    );

    generate(dir.path(), false)
        .assert()
        .failure()
        .stderr(predicates::str::contains("leaves the directory"));
}

#[test]
fn an_empty_generator_emission_is_refused() {
    let dir = project(
        "[[generate]]\noutput = \"leases.sql\"\ncommand = [\"echo\"]\n",
        &[("customers.sql", CUSTOMERS)],
    );

    generate(dir.path(), false)
        .assert()
        .failure()
        .stderr(predicates::str::contains("produced no output"));
}

#[test]
fn generate_with_no_entries_is_a_no_op() {
    let dir = project("", &[("customers.sql", CUSTOMERS)]);

    generate(dir.path(), false)
        .assert()
        .success()
        .stdout(predicates::str::contains("no [[generate]] entries"));
}
