//! `pgpushy plan` and `apply` against a real pgschema and a real Postgres.
//!
//! These need two things, and **skip rather than fail** when either is absent,
//! mirroring how `snowdrop-id-rs` gates its Postgres tests:
//!
//! - `PGPUSHY_TEST_PG_URL` — a target database, e.g.
//!   `postgres://postgres:pw@localhost:5432/pgpushy`
//! - `PGPUSHY_TEST_PGSCHEMA` — a path to a pgschema binary
//!
//! Each test works in its own uniquely-named schema and drops it afterwards,
//! so they neither collide with each other nor touch `public`.
//!
//! The version-floor and password tests need neither, because they fail before
//! anything connects — so they use a stub pgschema and always run.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Target {
    url: String,
    pgschema: PathBuf,
}

/// The environment for a test, or `None` if it should skip.
fn target() -> Option<Target> {
    let url = std::env::var("PGPUSHY_TEST_PG_URL")
        .ok()
        .filter(|v| !v.is_empty())?;
    let pgschema = std::env::var("PGPUSHY_TEST_PGSCHEMA")
        .ok()
        .filter(|v| !v.is_empty())?;
    let pgschema = PathBuf::from(pgschema);
    if !pgschema.exists() {
        eprintln!(
            "skipping: PGPUSHY_TEST_PGSCHEMA does not exist at {}",
            pgschema.display()
        );
        return None;
    }
    Some(Target { url, pgschema })
}

/// Announce a skip so a silently-passing test is not mistaken for a real one.
macro_rules! require_target {
    () => {
        match target() {
            Some(target) => target,
            None => {
                eprintln!(
                    "skipping: set PGPUSHY_TEST_PG_URL and PGPUSHY_TEST_PGSCHEMA to run this test"
                );
                return;
            }
        }
    };
}

/// The pieces of a `postgres://user:password@host:port/dbname` URL.
struct Parts {
    host: String,
    port: String,
    dbname: String,
    user: String,
    password: Option<String>,
}

fn parse(url: &str) -> Parts {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let (credentials, location) = match rest.split_once('@') {
        Some((credentials, location)) => (Some(credentials), location),
        None => (None, rest),
    };
    let (authority, dbname) = location.split_once('/').unwrap_or((location, "postgres"));
    let (host, port) = authority.split_once(':').unwrap_or((authority, "5432"));
    let (user, password) = match credentials {
        Some(credentials) => match credentials.split_once(':') {
            Some((user, password)) => (user.to_owned(), Some(password.to_owned())),
            None => (credentials.to_owned(), None),
        },
        None => ("postgres".to_owned(), None),
    };

    Parts {
        host: host.to_owned(),
        port: port.to_owned(),
        dbname: dbname.split('?').next().unwrap_or(dbname).to_owned(),
        user,
        password,
    }
}

impl Target {
    fn client(&self) -> postgres::Client {
        let parts = parse(&self.url);
        let mut conninfo = format!(
            "host={} port={} dbname={} user={} sslmode=disable",
            parts.host, parts.port, parts.dbname, parts.user,
        );
        if let Some(password) = &parts.password {
            conninfo.push_str(&format!(" password={password}"));
        }
        postgres::Client::connect(&conninfo, postgres::NoTls).expect("connect to the test target")
    }

    /// A project wired to this target, with `[env.test]` pointing at it.
    ///
    /// `extra` is prepended to the generated configuration, for tests that need
    /// `default_schema`, `exclude`, and so on.
    fn project(&self, extra: &str, files: &[(&str, String)]) -> Project {
        let parts = parse(&self.url);
        let dir = TempDir::new().expect("temp dir");

        let config = format!(
            "{extra}\n\
             [pgschema]\n\
             path = \"{}\"\n\
             \n\
             [env.test]\n\
             host = \"{}\"\n\
             port = {}\n\
             db = \"{}\"\n\
             user = \"{}\"\n\
             sslmode = \"disable\"\n",
            self.pgschema.display(),
            parts.host,
            parts.port,
            parts.dbname,
            parts.user,
        );
        std::fs::write(dir.path().join("pgpushy.toml"), config).expect("write config");

        for (path, contents) in files {
            write(&dir, path, contents);
        }

        Project {
            dir,
            password: parts.password,
        }
    }

    /// Apply the desired state with pgschema directly.
    ///
    /// Used only where a test needs the target in a known state without going
    /// through the code under test.
    fn apply_directly(&self, document: &Path, schemas: &[&str]) {
        let parts = parse(&self.url);
        for schema in schemas {
            let mut cmd = std::process::Command::new(&self.pgschema);
            cmd.arg("apply")
                .args(["--schema", schema])
                .arg("--file")
                .arg(document)
                .args(["--host", &parts.host])
                .args(["--port", &parts.port])
                .args(["--db", &parts.dbname])
                .args(["--user", &parts.user])
                .args(["--sslmode", "disable"])
                .arg("--auto-approve");
            if let Some(password) = &parts.password {
                cmd.env("PGPASSWORD", password);
            }
            let status = cmd.status().expect("run pgschema apply");
            assert!(status.success(), "pgschema apply failed for {schema}");
        }
    }
}

/// A configured project directory.
struct Project {
    dir: TempDir,
    password: Option<String>,
}

impl Project {
    fn command(&self, subcommand: &str) -> Command {
        let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
        cmd.arg(subcommand)
            .arg("--config")
            .arg(self.dir.path().join("pgpushy.toml"));
        if subcommand != "validate" {
            cmd.args(["--env", "test"]);
        }
        // The secret comes from the environment, so the generated config has
        // none — and no test trips the file-password warning by accident.
        if let Some(password) = &self.password {
            cmd.env("PGPASSWORD", password);
        }
        cmd
    }

    fn write(&self, path: &str, contents: &str) {
        write(&self.dir, path, contents);
    }
}

fn write(dir: &TempDir, path: &str, contents: &str) {
    let full = dir.path().join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("create dirs");
    }
    std::fs::write(&full, contents).expect("write file");
}

/// A schema name unique to this test, so tests never collide.
fn unique_schema(label: &str) -> String {
    format!("pgpushy_t_{label}_{}", std::process::id())
}

struct Schemas<'a> {
    target: &'a Target,
    names: Vec<String>,
}

impl<'a> Schemas<'a> {
    fn create(target: &'a Target, names: &[String]) -> Self {
        let mut client = target.client();
        for name in names {
            client
                .batch_execute(&format!(r#"DROP SCHEMA IF EXISTS "{name}" CASCADE"#))
                .expect("drop stale schema");
            client
                .batch_execute(&format!(r#"CREATE SCHEMA "{name}""#))
                .expect("create test schema");
        }
        Self {
            target,
            names: names.to_vec(),
        }
    }
}

impl Drop for Schemas<'_> {
    fn drop(&mut self) {
        let mut client = self.target.client();
        for name in &self.names {
            let _ = client.batch_execute(&format!(r#"DROP SCHEMA IF EXISTS "{name}" CASCADE"#));
        }
    }
}

const ORDERS_UNORDERED: &str =
    "CREATE TABLE orders (id int PRIMARY KEY, customer_id int REFERENCES customers(id));";
const CUSTOMERS: &str = "CREATE TABLE customers (id int PRIMARY KEY);";

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// The thesis, end to end: a tree with nothing ordered and nothing qualified
/// produces a correct plan against a real target.
#[test]
fn plans_an_unordered_unqualified_tree() {
    let target = require_target!();
    let schema = unique_schema("plan");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[
            // The child is defined before the parent, and in a file that sorts
            // first — raw pgschema cannot build desired state from this.
            ("a_orders.sql", ORDERS_UNORDERED.to_owned()),
            ("z_customers.sql", CUSTOMERS.to_owned()),
        ],
    );

    project
        .command("plan")
        .assert()
        .success()
        .stdout(predicates::str::contains("2 to add"))
        .stdout(predicates::str::contains("customers"))
        .stdout(predicates::str::contains("orders"))
        // The environment is named in the output, so a mismatch between "which
        // env" and "which database" is visible before anything is approved.
        .stdout(predicates::str::contains("env test:"));
}

/// Spec §11.1, and the real proof of the §5.3 naming decision: after applying,
/// the plan must be empty. A synthesized constraint name would show a
/// drop-and-recreate here, on this run and every run after it.
#[test]
fn an_applied_tree_replans_empty() {
    let target = require_target!();
    let schema = unique_schema("idem");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[(
            "schema.sql",
            "CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);
             CREATE TABLE orders (
                 id int PRIMARY KEY,
                 customer_id int NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
                 total numeric CHECK (total >= 0)
             );
             CREATE INDEX orders_customer_idx ON orders (customer_id);
             COMMENT ON TABLE orders IS 'one row per purchase';"
                .to_owned(),
        )],
    );

    let outputs = TempDir::new().expect("temp dir");
    let out = outputs.path().join("desired");
    project
        .command("validate")
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    // `--out` writes one document per managed schema (spec §8.7); this tree
    // has exactly one, and it is the one to apply.
    target.apply_directly(&out.join(format!("{schema}.sql")), &[&schema]);

    project
        .command("plan")
        .assert()
        .success()
        .stdout(predicates::str::contains("No changes detected"));

    // The constraint the author left unnamed must carry the name Postgres
    // generates, or the empty plan above was luck.
    let name: String = target
        .client()
        .query_one(
            "SELECT con.conname FROM pg_constraint con
             JOIN pg_class c ON c.oid = con.conrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND con.contype = 'f'",
            &[&schema],
        )
        .expect("the foreign key exists")
        .get(0);
    assert_eq!(name, "orders_customer_id_fkey");
}

/// Cross-schema foreign keys resolve, and the referenced schema is planned
/// first (spec §7).
#[test]
fn orders_schemas_by_their_cross_schema_foreign_keys() {
    let target = require_target!();
    let upstream = unique_schema("up");
    let downstream = unique_schema("down");
    let _schemas = Schemas::create(&target, &[upstream.clone(), downstream.clone()]);

    let project = target.project(
        &format!("default_schema = \"{upstream}\""),
        &[(
            "schema.sql",
            format!(
                "CREATE TABLE {upstream}.customers (id int PRIMARY KEY);
                 CREATE TABLE {downstream}.invoices (
                     id int PRIMARY KEY,
                     customer_id int REFERENCES {upstream}.customers(id)
                 );"
            ),
        )],
    );

    let output = project
        .command("plan")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output);

    let up_at = stdout
        .find(&format!("── {upstream} ──"))
        .expect("upstream planned");
    let down_at = stdout
        .find(&format!("── {downstream} ──"))
        .expect("downstream planned");
    assert!(
        up_at < down_at,
        "the referenced schema must be planned first:\n{stdout}"
    );
}

/// Spec §6.1: pgpushy creates no schemas, so a missing one is a clean failure
/// before any delegation — and every missing schema is named at once.
#[test]
fn a_missing_schema_fails_before_delegating() {
    let target = require_target!();
    let absent = unique_schema("absent");

    let project = target.project(
        "",
        &[(
            "schema.sql",
            format!("CREATE TABLE {absent}.customers (id int PRIMARY KEY);"),
        )],
    );

    project
        .command("plan")
        .assert()
        .failure()
        .stderr(predicates::str::contains("missing from the target"))
        .stderr(predicates::str::contains(&absent))
        .stderr(predicates::str::contains(format!(
            "CREATE SCHEMA {absent};"
        )));
}

// ---------------------------------------------------------------------------
// Apply (spec §6.2, §8.6, §9)
// ---------------------------------------------------------------------------

/// The end of the thesis: an unordered, unqualified tree reconciles a real
/// database, and running it again does nothing.
#[test]
fn applies_and_then_converges() {
    let target = require_target!();
    let schema = unique_schema("apply");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[
            ("b_orders.sql", ORDERS_UNORDERED.to_owned()),
            ("a_customers.sql", CUSTOMERS.to_owned()),
        ],
    );

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success()
        .stdout(predicates::str::contains("2 changes"))
        .stdout(predicates::str::contains("Applied 1 schema"));

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success()
        .stdout(predicates::str::contains("Nothing to apply"));
}

/// Spec §8.6: approval is required, and a run that cannot ask must not assume
/// the answer is yes. Test processes never have a terminal, so this is exactly
/// the CI case.
#[test]
fn refuses_to_apply_unattended_without_auto_approve() {
    let target = require_target!();
    let schema = unique_schema("noapprove");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[("t.sql", "CREATE TABLE t (id int PRIMARY KEY);".to_owned())],
    );

    project
        .command("apply")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "standard input is not a terminal",
        ))
        .stderr(predicates::str::contains("--auto-approve"));

    let tables: i64 = target
        .client()
        .query_one(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND c.relkind = 'r'",
            &[&schema],
        )
        .expect("count tables")
        .get(0);
    assert_eq!(
        tables, 0,
        "the target must be untouched when approval was refused"
    );
}

/// The approval summary must name destructive changes individually — a count
/// alone is not a review (spec §8.6).
#[test]
fn names_each_destructive_change_before_asking() {
    let target = require_target!();
    let schema = unique_schema("destr");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[(
            "t.sql",
            "CREATE TABLE t (id int PRIMARY KEY, doomed text);".to_owned(),
        )],
    );
    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success();

    project.write("t.sql", "CREATE TABLE t (id int PRIMARY KEY);");

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success()
        .stdout(predicates::str::contains("1 destructive"))
        .stdout(predicates::str::contains("Destructive changes:"))
        .stdout(predicates::str::contains(format!("{schema}.t.doomed")));
}

/// Spec §6.2, verified end to end: removing a cross-schema foreign key and the
/// column it points at in one change is refused, and nothing is applied.
///
/// The schema names matter. Apply order falls back to name order once the
/// desired state no longer has the foreign key linking them, and `aa_` sorts
/// before `zz_` — putting the referenced schema first, which is the order that
/// cannot work.
#[test]
fn refuses_a_cross_schema_removal_the_order_cannot_satisfy() {
    let target = require_target!();
    let id = std::process::id();
    let referenced = format!("pgpushy_t_aa_up_{id}");
    let referencing = format!("pgpushy_t_zz_down_{id}");
    let _schemas = Schemas::create(&target, &[referenced.clone(), referencing.clone()]);

    let project = target.project(
        "",
        &[
            (
                "a.sql",
                format!(
                    "CREATE TABLE {referenced}.parent \
                     (id int PRIMARY KEY, alt int, CONSTRAINT parent_alt_key UNIQUE (alt));"
                ),
            ),
            (
                "z.sql",
                format!(
                    "CREATE TABLE {referencing}.child (id int PRIMARY KEY, alt_ref int);
                     ALTER TABLE {referencing}.child ADD CONSTRAINT child_parent_fk
                         FOREIGN KEY (alt_ref) REFERENCES {referenced}.parent (alt);"
                ),
            ),
        ],
    );
    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success();

    // Now remove both the foreign key and the column it depends on.
    project.write(
        "a.sql",
        &format!("CREATE TABLE {referenced}.parent (id int PRIMARY KEY);"),
    );
    project.write(
        "z.sql",
        &format!("CREATE TABLE {referencing}.child (id int PRIMARY KEY, alt_ref int);"),
    );

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be applied in one pass"))
        .stderr(predicates::str::contains("child_parent_fk"))
        .stderr(predicates::str::contains("apply this in two steps"))
        .stderr(predicates::str::contains("no schemas were applied"));

    // Refused means refused. Scoped to this test's own schema — constraint
    // names are per-table, so an unscoped count would also see the
    // identically-named constraint another test created.
    let remaining: i64 = target
        .client()
        .query_one(
            "SELECT count(*) FROM pg_constraint con
             JOIN pg_class c ON c.oid = con.conrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND con.conname = 'child_parent_fk'",
            &[&referencing],
        )
        .expect("count")
        .get(0);
    assert_eq!(remaining, 1, "nothing should have been applied");
}

/// And the two-step remedy pgpushy prints actually works.
#[test]
fn the_two_step_remedy_converges() {
    let target = require_target!();
    let id = std::process::id();
    let referenced = format!("pgpushy_t_aa_up2_{id}");
    let referencing = format!("pgpushy_t_zz_down2_{id}");
    let _schemas = Schemas::create(&target, &[referenced.clone(), referencing.clone()]);

    let project = target.project(
        "",
        &[
            (
                "a.sql",
                format!(
                    "CREATE TABLE {referenced}.parent \
                     (id int PRIMARY KEY, alt int, CONSTRAINT parent_alt_key UNIQUE (alt));"
                ),
            ),
            (
                "z.sql",
                format!(
                    "CREATE TABLE {referencing}.child (id int PRIMARY KEY, alt_ref int);
                     ALTER TABLE {referencing}.child ADD CONSTRAINT child_parent_fk
                         FOREIGN KEY (alt_ref) REFERENCES {referenced}.parent (alt);"
                ),
            ),
        ],
    );
    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success();

    // Step 1: remove only the foreign key, keeping the column.
    project.write(
        "z.sql",
        &format!("CREATE TABLE {referencing}.child (id int PRIMARY KEY, alt_ref int);"),
    );
    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success();

    // Step 2: now the column can go.
    project.write(
        "a.sql",
        &format!("CREATE TABLE {referenced}.parent (id int PRIMARY KEY);"),
    );
    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success();

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success()
        .stdout(predicates::str::contains("Nothing to apply"));
}

/// Spec §7: a cycle has no valid apply order, so `apply` refuses outright —
/// unlike `plan`, which shows the plans because they are how you break it.
#[test]
fn a_cycle_blocks_apply_entirely() {
    let target = require_target!();
    let id = std::process::id();
    let one = format!("pgpushy_t_cyc1_{id}");
    let two = format!("pgpushy_t_cyc2_{id}");
    let _schemas = Schemas::create(&target, &[one.clone(), two.clone()]);

    let project = target.project(
        "",
        &[(
            "cycle.sql",
            format!(
                "CREATE TABLE {one}.a (id int PRIMARY KEY, r int REFERENCES {two}.b(id));
                 CREATE TABLE {two}.b (id int PRIMARY KEY, r int REFERENCES {one}.a(id));"
            ),
        )],
    );

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cross-schema foreign key cycle"))
        .stderr(predicates::str::contains("no schemas were applied"));
}

/// Spec §9: when a schema fails partway, say exactly what landed, what broke,
/// and what never ran — and that the applied ones are not rolled back.
#[test]
fn a_partial_failure_reports_what_landed() {
    let target = require_target!();
    let id = std::process::id();
    // Name-ordered so the schema that succeeds is applied first.
    let good = format!("pgpushy_t_p1good_{id}");
    let bad = format!("pgpushy_t_p2bad_{id}");
    let _schemas = Schemas::create(&target, &[good.clone(), bad.clone()]);

    let project = target.project(
        "",
        &[
            (
                "good.sql",
                format!("CREATE TABLE {good}.t (id int PRIMARY KEY);"),
            ),
            (
                "bad.sql",
                format!("CREATE TABLE {bad}.t (id int PRIMARY KEY, amount int);"),
            ),
        ],
    );
    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success();

    // Data that the new constraint will reject, so the second schema's apply
    // fails for a reason pgpushy cannot foresee — which is the point.
    target
        .client()
        .batch_execute(&format!("INSERT INTO {bad}.t (id, amount) VALUES (1, -5)"))
        .expect("insert violating row");

    project.write(
        "good.sql",
        &format!("CREATE TABLE {good}.t (id int PRIMARY KEY, added text);"),
    );
    project.write(
        "bad.sql",
        &format!(
            "CREATE TABLE {bad}.t (id int PRIMARY KEY, amount int,
                 CONSTRAINT amount_positive CHECK (amount > 0));"
        ),
    );

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .failure()
        .stderr(predicates::str::contains(format!(
            "apply failed at schema {bad}"
        )))
        .stderr(predicates::str::contains(format!("applied:      {good}")))
        .stderr(predicates::str::contains("NOT rolled back"));

    // The successful schema really did land, which is what "not rolled back"
    // means and why it has to be said.
    let added: i64 = target
        .client()
        .query_one(
            "SELECT count(*) FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = 't' AND column_name = 'added'",
            &[&good],
        )
        .expect("count")
        .get(0);
    assert_eq!(added, 1);
}

// ---------------------------------------------------------------------------
// The pgschema version floor and the password warning
// ---------------------------------------------------------------------------
//
// No database and no real pgschema needed: both are decided before pgpushy
// connects, so a stub that prints a version line is enough.

#[cfg(unix)]
fn stub_project(config_extra: &str, help: &str) -> TempDir {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("temp dir");
    let stub = dir.path().join("pgschema-stub");
    std::fs::write(&stub, format!("#!/bin/sh\ncat <<'EOF'\n{help}\nEOF\n")).expect("write stub");
    let mut permissions = std::fs::metadata(&stub).expect("stat stub").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&stub, permissions).expect("chmod stub");

    std::fs::write(
        dir.path().join("pgpushy.toml"),
        format!(
            "{config_extra}\n\
             [pgschema]\n\
             path = \"{}\"\n\
             \n\
             [env.test]\n\
             host = \"127.0.0.1\"\n\
             port = 1\n\
             db = \"nope\"\n\
             user = \"nope\"\n",
            stub.display(),
        ),
    )
    .expect("write config");
    write(&dir, "t.sql", "CREATE TABLE t (id int PRIMARY KEY);");
    dir
}

#[cfg(unix)]
fn stub_plan(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
    cmd.args(["plan", "--env", "test"])
        .arg("--config")
        .arg(dir.path().join("pgpushy.toml"));
    cmd
}

#[cfg(unix)]
#[test]
fn a_below_floor_pgschema_is_a_hard_error() {
    let dir = stub_project("", "Version: 1.4.2@abc linux/amd64 2025-11-14 00:00:00");

    stub_plan(&dir)
        .assert()
        .failure()
        .stderr(predicates::str::contains("1.4.2"))
        .stderr(predicates::str::contains("below the minimum 1.12.0"));
}

/// Spec §8.5: the `Version:` line is a human-readable string, not a stability
/// contract, so an unreadable one warns rather than refusing to run.
#[cfg(unix)]
#[test]
fn an_unreadable_version_warns_and_proceeds() {
    let dir = stub_project("", "pgschema, the schema tool\n(no version line here)");

    // It gets past the version check and fails later, at the connection —
    // which is the point: the version did not stop it.
    stub_plan(&dir)
        .assert()
        .failure()
        .stderr(predicates::str::contains("could not read a version"));
}

#[test]
fn a_missing_pgschema_says_how_to_get_one() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(
        dir.path().join("pgpushy.toml"),
        "[env.test]\ndb = \"nope\"\nuser = \"nope\"\n",
    )
    .expect("write config");
    write(&dir, "t.sql", "CREATE TABLE t (id int PRIMARY KEY);");

    Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .args([
            "plan",
            "--env",
            "test",
            "--pgschema-path",
            "/nonexistent/pgschema",
        ])
        .arg("--config")
        .arg(dir.path().join("pgpushy.toml"))
        .assert()
        .failure()
        .stderr(predicates::str::contains("no pgschema binary at"));
}

/// A password read from a file that is easily committed is worth interrupting
/// someone about (spec §10.2).
#[cfg(unix)]
#[test]
fn warns_when_the_password_comes_from_the_config_file() {
    let dir = stub_project("", "Version: 1.12.0@abc linux/amd64 2026-07-06 00:00:00");
    // Append a password to the generated [env.test] block.
    let config = dir.path().join("pgpushy.toml");
    let mut text = std::fs::read_to_string(&config).expect("read config");
    text.push_str("password = \"hunter2\"\n");
    std::fs::write(&config, text).expect("write config");

    stub_plan(&dir)
        .env_remove("PGPASSWORD")
        .assert()
        .failure()
        .stderr(predicates::str::contains("PASSWORD READ FROM"))
        .stderr(predicates::str::contains("pgpushy.toml"))
        // The warning must not become a new way to leak the password.
        .stderr(predicates::str::contains("hunter2").not());
}

/// Spec §10.2 is explicit that the warning fires on *use*, not on presence: a
/// file password `PGPASSWORD` overrode is not worth interrupting for.
#[cfg(unix)]
#[test]
fn does_not_warn_when_the_file_password_is_overridden() {
    let dir = stub_project("", "Version: 1.12.0@abc linux/amd64 2026-07-06 00:00:00");
    let config = dir.path().join("pgpushy.toml");
    let mut text = std::fs::read_to_string(&config).expect("read config");
    text.push_str("password = \"hunter2\"\n");
    std::fs::write(&config, text).expect("write config");

    stub_plan(&dir)
        .env("PGPASSWORD", "from-the-environment")
        .assert()
        .failure()
        .stderr(predicates::str::contains("PASSWORD READ FROM").not());
}

/// Spec §10.2: an ambient `PGHOST` must not redirect a named environment —
/// the point of naming the target is that it is unambiguous.
#[cfg(unix)]
#[test]
fn an_ambient_pghost_does_not_redirect_a_named_environment() {
    let dir = stub_project("", "Version: 1.12.0@abc linux/amd64 2026-07-06 00:00:00");

    stub_plan(&dir)
        .env("PGHOST", "somewhere.else.example")
        .env("PGDATABASE", "some_other_database")
        .assert()
        .failure()
        // The failure is the connection to what the environment block named,
        // not to what the ambient variables said.
        .stderr(predicates::str::contains("127.0.0.1"))
        .stderr(predicates::str::contains("somewhere.else.example").not());
}

// ---------------------------------------------------------------------------
// Plan database, lock timeout, colour (spec §10.4, §10.5, §8.3)
// ---------------------------------------------------------------------------

/// The plan database is where pgschema executes the desired state to build its
/// comparison model. Pointing it at a real server must work — and must leave
/// the *target's* result unchanged, since it is only scratch space.
#[test]
fn plans_through_an_external_plan_database() {
    let target = require_target!();
    let schema = unique_schema("plandb");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    // A separate database on the same server. It gets written to, so it must
    // not be one that matters — which is exactly why the spec says so.
    let plan_db = format!("pgpushy_plan_{}", std::process::id());
    let mut client = target.client();
    client
        .batch_execute(&format!("DROP DATABASE IF EXISTS {plan_db}"))
        .ok();
    client
        .batch_execute(&format!("CREATE DATABASE {plan_db}"))
        .expect("create plan database");

    let parts = parse(&target.url);
    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[(
            "t.sql",
            "CREATE TABLE t (id int PRIMARY KEY, name text);".to_owned(),
        )],
    );
    // Append the plan database to the generated [env.test] block.
    let config = project.dir.path().join("pgpushy.toml");
    let mut text = std::fs::read_to_string(&config).expect("read config");
    text.push_str(&format!(
        "\n[env.test.plan_db]\nhost = \"{}\"\nport = {}\ndb = \"{plan_db}\"\n\
         user = \"{}\"\nsslmode = \"disable\"\n",
        parts.host, parts.port, parts.user,
    ));
    std::fs::write(&config, text).expect("write config");

    let mut cmd = project.command("plan");
    if let Some(password) = &parts.password {
        cmd.env("PGPUSHY_PLAN_PASSWORD", password);
    }
    cmd.assert()
        .success()
        .stdout(predicates::str::contains("1 to add"))
        .stdout(predicates::str::contains(format!(
            "plan database: {}",
            parts.user
        )));

    let mut client = target.client();
    let _ = client.batch_execute(&format!("DROP DATABASE IF EXISTS {plan_db}"));
}

/// Spec §10.5: forwarded from the environment, and overridable by the flag —
/// safe precedence, because a lock timeout cannot change what is reconciled.
#[test]
fn the_lock_timeout_is_forwarded_and_the_flag_wins() {
    let target = require_target!();
    let schema = unique_schema("lock");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[("t.sql", "CREATE TABLE t (id int PRIMARY KEY);".to_owned())],
    );
    let config = project.dir.path().join("pgpushy.toml");
    let mut text = std::fs::read_to_string(&config).expect("read config");
    text.push_str("lock_timeout = \"7s\"\n");
    std::fs::write(&config, text).expect("write config");

    project
        .command("apply")
        .args(["--auto-approve", "--verbose"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--lock-timeout 7s"));

    // Re-plan is empty, so apply again from scratch to see the override.
    let mut client = target.client();
    client
        .batch_execute(&format!(
            r#"DROP SCHEMA "{schema}" CASCADE; CREATE SCHEMA "{schema}""#
        ))
        .expect("reset schema");

    project
        .command("apply")
        .args(["--auto-approve", "--verbose", "--lock-timeout", "99s"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--lock-timeout 99s"));
}

/// pgschema colours unconditionally, so without `--no-color` a captured plan is
/// full of escape sequences. Test output is never a terminal, which is exactly
/// the case that must stay clean.
#[test]
fn captured_output_carries_no_escape_sequences() {
    let target = require_target!();
    let schema = unique_schema("color");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[("t.sql", "CREATE TABLE t (id int PRIMARY KEY);".to_owned())],
    );

    let assert = project.command("plan").assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    assert!(stdout.contains("to add"), "expected a plan:\n{stdout}");
    assert!(
        !stdout.contains('\u{1b}'),
        "pgschema's colour leaked into captured output:\n{stdout}"
    );
}

/// The plan database's password travels through `PGSCHEMA_PLAN_PASSWORD`, not
/// `--plan-password`: a secret in the argv is visible in the process list, and
/// in anything `--verbose` prints into a bug report.
///
/// Needs a real target because pgpushy inspects it before ever invoking
/// pgschema — but the *plan* database can be anywhere, since pgpushy never
/// connects there itself. So it gets a deliberately distinctive password,
/// which a real target's could not be: CI's happens to equal the username,
/// and asserting on a value that appears legitimately all over the output
/// cannot work.
#[test]
fn the_plan_database_password_never_reaches_the_command_line() {
    let target = require_target!();
    const DISTINCTIVE: &str = "zzz-plan-secret-zzz";

    let schema = unique_schema("planpw");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[("t.sql", "CREATE TABLE t (id int PRIMARY KEY);".to_owned())],
    );
    let config = project.dir.path().join("pgpushy.toml");
    let mut text = std::fs::read_to_string(&config).expect("read config");
    text.push_str(
        "\n[env.test.plan_db]\nhost = \"127.0.0.1\"\nport = 1\n\
         db = \"plan\"\nuser = \"planner\"\nsslmode = \"disable\"\n",
    );
    std::fs::write(&config, text).expect("write config");

    // pgschema cannot reach that plan database, and does not need to: the
    // command line is printed before it runs, which is all this asserts on.
    let assert = project
        .command("plan")
        .arg("--verbose")
        .env("PGPUSHY_PLAN_PASSWORD", DISTINCTIVE)
        .assert();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("--plan-db"),
        "the plan database should be forwarded:\n{stdout}"
    );
    assert!(
        !stdout.contains("--plan-password"),
        "the password must not be a flag:\n{stdout}"
    );
    assert!(
        !stdout.contains(DISTINCTIVE),
        "the password leaked into output:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// The managed backend (spec §8.5)
// ---------------------------------------------------------------------------
//
// Downloading ~19 MB from GitHub is not something an ordinary `cargo test`
// should do, so these are gated on `PGPUSHY_TEST_DOWNLOAD=1` — separately from
// the database variables, because the resource they need is different.

fn downloads_enabled() -> bool {
    std::env::var("PGPUSHY_TEST_DOWNLOAD").is_ok_and(|value| value == "1")
}

macro_rules! require_downloads {
    () => {
        if !downloads_enabled() {
            eprintln!("skipping: set PGPUSHY_TEST_DOWNLOAD=1 to run this test");
            return;
        }
    };
}

/// The whole point of the managed backend: pgschema arrives without anyone
/// installing it, and what arrives is what pgpushy expects byte for byte.
#[test]
fn downloads_verifies_and_caches_pgschema() {
    let target = require_target!();
    require_downloads!();

    let schema = unique_schema("managed");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    // A project with no [pgschema] path, so the managed backend is used.
    let parts = parse(&target.url);
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(
        dir.path().join("pgpushy.toml"),
        format!(
            "default_schema = \"{schema}\"\n\
             \n[env.test]\nhost = \"{}\"\nport = {}\ndb = \"{}\"\n\
             user = \"{}\"\nsslmode = \"disable\"\n",
            parts.host, parts.port, parts.dbname, parts.user,
        ),
    )
    .expect("write config");
    write(&dir, "t.sql", "CREATE TABLE t (id int PRIMARY KEY);");

    let cache = TempDir::new().expect("temp dir");
    let run = || {
        let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
        cmd.args(["plan", "--env", "test"])
            .arg("--config")
            .arg(dir.path().join("pgpushy.toml"))
            .env("XDG_CACHE_HOME", cache.path());
        if let Some(password) = &parts.password {
            cmd.env("PGPASSWORD", password);
        }
        cmd
    };

    // Cold cache: downloads, then plans with what it fetched.
    run()
        .assert()
        .success()
        .stdout(predicates::str::contains("downloading pgschema"))
        .stdout(predicates::str::contains("1 to add"));

    // Warm cache: no second download.
    run()
        .assert()
        .success()
        .stdout(predicates::str::contains("downloading pgschema").not())
        .stdout(predicates::str::contains("1 to add"));
}

/// The cache is re-verified on every hit rather than trusted for existing:
/// atomic writes cover pgpushy's own partial writes and nothing else.
#[test]
fn a_tampered_cache_is_noticed_and_replaced() {
    let target = require_target!();
    require_downloads!();

    let schema = unique_schema("tamper");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let parts = parse(&target.url);
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(
        dir.path().join("pgpushy.toml"),
        format!(
            "default_schema = \"{schema}\"\n\
             \n[env.test]\nhost = \"{}\"\nport = {}\ndb = \"{}\"\n\
             user = \"{}\"\nsslmode = \"disable\"\n",
            parts.host, parts.port, parts.dbname, parts.user,
        ),
    )
    .expect("write config");
    write(&dir, "t.sql", "CREATE TABLE t (id int PRIMARY KEY);");

    let cache = TempDir::new().expect("temp dir");
    let run = || {
        let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
        cmd.args(["plan", "--env", "test"])
            .arg("--config")
            .arg(dir.path().join("pgpushy.toml"))
            .env("XDG_CACHE_HOME", cache.path());
        if let Some(password) = &parts.password {
            cmd.env("PGPASSWORD", password);
        }
        cmd
    };

    run().assert().success();

    // Find the cached binary and corrupt it.
    let cached = walk_for_file(cache.path(), "pgschema").expect("a cached binary");
    let original = std::fs::read(&cached).expect("read cached");
    let mut tampered = original.clone();
    tampered.extend_from_slice(b"not the real binary");
    std::fs::write(&cached, &tampered).expect("tamper");

    run()
        .assert()
        .success()
        .stderr(predicates::str::contains("does not match its"))
        .stdout(predicates::str::contains("downloading pgschema"));

    assert_eq!(
        std::fs::read(&cached).expect("read cached"),
        original,
        "the tampered binary should have been replaced",
    );
}

/// Find a file by name anywhere under `root`.
fn walk_for_file(root: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = walk_for_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|f| f == name) {
            return Some(path);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Privileges pgpushy does not manage
// ---------------------------------------------------------------------------

/// pgpushy 0.x does not manage privileges, and a desired state that mentions
/// none is not a statement that there should be none. Without the
/// `.pgschemaignore` pgpushy writes, pgschema reads it that way and plans a
/// `REVOKE` for every grant on the target — silently stripping permissions
/// nobody asked it to touch.
#[test]
fn does_not_revoke_grants_it_does_not_manage() {
    let target = require_target!();
    let schema = unique_schema("grants");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let role = format!("pgpushy_role_{}", std::process::id());
    let mut client = target.client();
    let _ = client.batch_execute(&format!("DROP ROLE IF EXISTS {role}"));
    client
        .batch_execute(&format!("CREATE ROLE {role}"))
        .expect("create role");

    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[("t.sql", "CREATE TABLE t (id int PRIMARY KEY);".to_owned())],
    );
    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success();

    // Grant outside pgpushy, the way a permissions tool or a DBA would.
    let mut client = target.client();
    client
        .batch_execute(&format!(
            "GRANT SELECT, INSERT ON {schema}.t TO {role};
             ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} GRANT SELECT ON TABLES TO {role}"
        ))
        .expect("grant");

    project
        .command("plan")
        .assert()
        .success()
        .stdout(predicates::str::contains("REVOKE").not())
        .stdout(predicates::str::contains("No changes detected"));

    let remaining: i64 = target
        .client()
        .query_one(
            "SELECT count(*) FROM information_schema.role_table_grants
             WHERE table_schema = $1 AND grantee = $2",
            &[&schema, &role],
        )
        .expect("count grants")
        .get(0);
    assert_eq!(
        remaining, 2,
        "pgpushy must leave privileges it does not manage alone"
    );

    let mut client = target.client();
    let _ = client.batch_execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"));
}

/// pgschema auto-loads `.pgschemaignore` from wherever it runs, which is
/// ambient state of exactly the kind §6.3 refuses for connections. pgpushy runs
/// it in a directory pgpushy owns, so a file in the operator's shell directory
/// cannot silently change what gets reconciled.
#[test]
fn a_stray_pgschemaignore_cannot_change_what_is_reconciled() {
    let target = require_target!();
    let schema = unique_schema("stray");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[("t.sql", "CREATE TABLE t (id int PRIMARY KEY);".to_owned())],
    );
    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success();

    // Make the source differ, so ignoring tables would be visible as a
    // *missing* change rather than an ambiguous "no changes".
    project.write("t.sql", "CREATE TABLE t (id int PRIMARY KEY, added text);");
    std::fs::write(
        project.dir.path().join(".pgschemaignore"),
        "[tables]\npatterns = [\"*\"]\n",
    )
    .expect("write stray ignore file");

    project
        .command("plan")
        .current_dir(project.dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("to modify"));
}

/// Every category-2 kind, applied and then converged (spec §4.3, §5.1).
///
/// The order here is one no fixed rule gets right: the composite type is
/// written first and needs the domain written last. pgpushy sorts category 2
/// by what each object needs, and pgschema then applies what it is given.
///
/// The sequence is deliberately one nothing defaults to. Verified against
/// pgschema 1.12.0: a default calling `nextval` is applied as SERIAL — an
/// owned sequence is created instead of the one named, apply reports success,
/// and every later plan shows the same drop and add. §4.3 rejects that shape
/// rather than letting a database silently never converge.
#[test]
fn a_tree_with_types_domains_and_sequences_converges() {
    let target = require_target!();
    let schema = unique_schema("m8");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        &format!("default_schema = \"{schema}\""),
        &[(
            "schema.sql",
            "CREATE TYPE addr AS (city text, zip zipcode);
             CREATE DOMAIN zipcode AS text CHECK (VALUE ~ '^[0-9]{5}$');
             CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
             CREATE SEQUENCE ticket_no START 1000 INCREMENT 5;
             CREATE TABLE people (
                 id     int PRIMARY KEY,
                 how    mood,
                 home   addr,
                 ticket int
             );"
            .to_owned(),
        )],
    );

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success();

    // A composite emitted before the domain it is written in would have failed
    // the apply; anything pgschema normalises would show up here.
    project
        .command("plan")
        .assert()
        .success()
        .stdout(predicates::str::contains("No changes detected"));

    // The sequence is a standalone object, with the parameters as written.
    let increment: i64 = target
        .client()
        .query_one(
            "SELECT seqincrement FROM pg_sequence s
             JOIN pg_class c ON c.oid = s.seqrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND c.relname = 'ticket_no'",
            &[&schema],
        )
        .expect("the standalone sequence should exist")
        .get(0);
    assert_eq!(increment, 5);
}

// ---------------------------------------------------------------------------
// Seeds (spec §8.8): execution and the convergence probe, against a real
// database. The offline rules are covered in pgpushy-core/tests/seeds.rs.
// ---------------------------------------------------------------------------

#[test]
fn seeds_apply_and_converge() {
    let target = require_target!();
    let schema = unique_schema("seed");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        "seed_root = \"seeds\"\n",
        &[
            (
                "t.sql",
                format!("CREATE TABLE {schema}.t (id int PRIMARY KEY, val text);"),
            ),
            (
                "seeds/rows.sql",
                format!(
                    "INSERT INTO {schema}.t (id, val) VALUES (1, 'a'), (2, 'b'), (3, 'c') \
                     ON CONFLICT (id) DO NOTHING;"
                ),
            ),
        ],
    );

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success()
        .stdout(predicates::str::contains("3 rows affected; probe passed"));

    let mut client = target.client();
    let rows: i64 = client
        .query_one(&format!("SELECT count(*) FROM {schema}.t"), &[])
        .expect("count")
        .get(0);
    assert_eq!(rows, 3);

    // The second apply has an empty schema plan and still runs the seeds
    // (spec §8.8); the probe passes and nothing changes.
    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success()
        .stdout(predicates::str::contains("0 rows affected; probe passed"));

    let rows: i64 = client
        .query_one(&format!("SELECT count(*) FROM {schema}.t"), &[])
        .expect("count")
        .get(0);
    assert_eq!(rows, 3);
}

/// The probe's whole reason to exist: a volatile expression passes every
/// static check and never converges. The failure must land *nothing* — the
/// probe rolls back the first pass along with itself (spec §8.8, §11.1).
#[test]
fn a_volatile_seed_rolls_back_and_fails() {
    let target = require_target!();
    let schema = unique_schema("volatile");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let project = target.project(
        "seed_root = \"seeds\"\n",
        &[
            (
                "t.sql",
                format!("CREATE TABLE {schema}.t (id int PRIMARY KEY, val text);"),
            ),
            (
                "seeds/volatile.sql",
                format!(
                    "INSERT INTO {schema}.t AS t (id, val) VALUES (1, random()::text) \
                     ON CONFLICT (id) DO UPDATE SET val = excluded.val \
                     WHERE t.val IS DISTINCT FROM excluded.val;"
                ),
            ),
        ],
    );

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .failure()
        .stderr(predicates::str::contains("does not converge"))
        .stderr(predicates::str::contains("volatile.sql"));

    // Nothing from the failing file landed: the rollback covered both passes.
    let mut client = target.client();
    let rows: i64 = client
        .query_one(&format!("SELECT count(*) FROM {schema}.t"), &[])
        .expect("count")
        .get(0);
    assert_eq!(rows, 0);
}

/// A guarded DO UPDATE corrects drifted reference data, then converges.
#[test]
fn a_guarded_do_update_corrects_and_converges() {
    let target = require_target!();
    let schema = unique_schema("guarded");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let seed = |val: &str| {
        format!(
            "INSERT INTO {schema}.t AS t (id, val) VALUES (1, '{val}') \
             ON CONFLICT (id) DO UPDATE SET val = excluded.val \
             WHERE t.val IS DISTINCT FROM excluded.val;"
        )
    };

    let project = target.project(
        "seed_root = \"seeds\"\n",
        &[
            (
                "t.sql",
                format!("CREATE TABLE {schema}.t (id int PRIMARY KEY, val text);"),
            ),
            ("seeds/labels.sql", seed("first")),
        ],
    );

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success()
        .stdout(predicates::str::contains("1 row affected; probe passed"));

    // The seed changes: the listed row is authoritative, so the next apply
    // corrects it — once — and the probe still passes.
    project.write("seeds/labels.sql", &seed("second"));
    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success()
        .stdout(predicates::str::contains("1 row affected; probe passed"));

    let mut client = target.client();
    let val: String = client
        .query_one(&format!("SELECT val FROM {schema}.t WHERE id = 1"), &[])
        .expect("row")
        .get(0);
    assert_eq!(val, "second");

    project
        .command("apply")
        .arg("--auto-approve")
        .assert()
        .success()
        .stdout(predicates::str::contains("0 rows affected; probe passed"));
}
