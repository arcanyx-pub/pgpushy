//! `pgpushy plan` against a real pgschema and a real Postgres.
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
//! The version-floor tests need neither, because the provider resolves before
//! anything connects — so they use a stub script and always run.

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

    /// A `pgpushy validate` invocation. Takes no connection flags — the whole
    /// point of the command is that it connects to nothing.
    fn validate(&self, root: &Path) -> Command {
        let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
        cmd.arg("validate").arg("--source-root").arg(root);
        cmd
    }

    /// A `pgpushy` invocation wired to this target.
    fn pgpushy(&self, subcommand: &str, root: &Path) -> Command {
        let parts = parse(&self.url);
        let mut cmd = Command::cargo_bin("pgpushy").expect("binary builds");
        cmd.arg(subcommand)
            .arg("--source-root")
            .arg(root)
            .arg("--pgschema-path")
            .arg(&self.pgschema)
            .args(["--host", &parts.host])
            .args(["--port", &parts.port])
            .args(["--db", &parts.dbname])
            .args(["--user", &parts.user])
            .args(["--sslmode", "disable"]);
        if let Some(password) = &parts.password {
            cmd.env("PGPASSWORD", password);
        }
        cmd
    }

    /// Apply the desired state with pgschema directly.
    ///
    /// `pgpushy apply` does not exist yet, but idempotence cannot be tested
    /// without getting the target into the reconciled state somehow.
    fn apply(&self, document: &Path, schemas: &[&str]) {
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

/// A schema name unique to this test, so tests never collide.
fn unique_schema(label: &str) -> String {
    // Test names are unique within the binary, and the process id separates
    // concurrent runs against a shared database.
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

fn tree(files: &[(&str, String)]) -> TempDir {
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

    let dir = tree(&[
        // The child is defined before the parent, and in a file that sorts
        // first — raw pgschema cannot build desired state from this.
        (
            "a_orders.sql",
            "CREATE TABLE orders (id int PRIMARY KEY, customer_id int REFERENCES customers(id));"
                .to_owned(),
        ),
        (
            "z_customers.sql",
            "CREATE TABLE customers (id int PRIMARY KEY);".to_owned(),
        ),
    ]);

    target
        .pgpushy("plan", dir.path())
        .args(["--default-schema", &schema])
        .assert()
        .success()
        .stdout(predicates::str::contains("2 to add"))
        .stdout(predicates::str::contains("customers"))
        .stdout(predicates::str::contains("orders"));
}

/// Spec §11.1, and the real proof of the §5.3 naming decision: after applying,
/// the plan must be empty. A synthesized constraint name would show a
/// drop-and-recreate here, on this run and every run after it.
#[test]
fn an_applied_tree_replans_empty() {
    let target = require_target!();
    let schema = unique_schema("idem");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let dir = tree(&[(
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
    )]);

    let document = dir.path().join("desired.sql");
    target
        .validate(dir.path())
        .args(["--default-schema", &schema])
        .arg("--out")
        .arg(&document)
        .assert()
        .success();

    target.apply(&document, &[&schema]);

    target
        .pgpushy("plan", dir.path())
        .args(["--default-schema", &schema])
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

    let dir = tree(&[(
        "schema.sql",
        format!(
            "CREATE TABLE {upstream}.customers (id int PRIMARY KEY);
             CREATE TABLE {downstream}.invoices (
                 id int PRIMARY KEY,
                 customer_id int REFERENCES {upstream}.customers(id)
             );"
        ),
    )]);

    let output = target
        .pgpushy("plan", dir.path())
        .args(["--default-schema", &upstream])
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

    let dir = tree(&[(
        "schema.sql",
        format!("CREATE TABLE {absent}.customers (id int PRIMARY KEY);"),
    )]);

    target
        .pgpushy("plan", dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("missing from the target"))
        .stderr(predicates::str::contains(&absent))
        .stderr(predicates::str::contains(format!(
            "CREATE SCHEMA {absent};"
        )));
}

// ---------------------------------------------------------------------------
// The pgschema version floor (spec §8.5, §13)
// ---------------------------------------------------------------------------
//
// No database and no real pgschema needed: the provider resolves before
// anything connects, so a stub that prints a version line is enough.

#[cfg(unix)]
fn stub_pgschema(dir: &TempDir, help: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.path().join("pgschema-stub");
    std::fs::write(&path, format!("#!/bin/sh\ncat <<'EOF'\n{help}\nEOF\n")).expect("write stub");
    let mut permissions = std::fs::metadata(&path).expect("stat stub").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("chmod stub");
    path
}

#[cfg(unix)]
#[test]
fn a_below_floor_pgschema_is_a_hard_error() {
    let dir = tree(&[(
        "schema.sql",
        "CREATE TABLE t (id int PRIMARY KEY);".to_owned(),
    )]);
    let stub = stub_pgschema(&dir, "Version: 1.4.2@abc linux/amd64 2025-11-14 00:00:00");

    Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .arg("plan")
        .arg("--source-root")
        .arg(dir.path())
        .arg("--pgschema-path")
        .arg(&stub)
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
    let dir = tree(&[(
        "schema.sql",
        "CREATE TABLE t (id int PRIMARY KEY);".to_owned(),
    )]);
    let stub = stub_pgschema(&dir, "pgschema, the schema tool\n(no version line here)");

    // It gets past the version check and fails later, at the connection —
    // which is the point: the version did not stop it.
    let assert = Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .arg("plan")
        .arg("--source-root")
        .arg(dir.path())
        .arg("--pgschema-path")
        .arg(&stub)
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            "1",
            "--db",
            "nope",
            "--user",
            "nope",
        ])
        .assert()
        .failure();

    assert.stderr(predicates::str::contains("could not read a version"));
}

#[test]
fn a_missing_pgschema_says_how_to_get_one() {
    let dir = tree(&[(
        "schema.sql",
        "CREATE TABLE t (id int PRIMARY KEY);".to_owned(),
    )]);

    Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .arg("plan")
        .arg("--source-root")
        .arg(dir.path())
        .args(["--pgschema-path", "/nonexistent/pgschema"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no pgschema binary at"));
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

    let dir = tree(&[
        ("b_orders.sql", ORDERS_UNORDERED.to_owned()),
        (
            "a_customers.sql",
            "CREATE TABLE customers (id int PRIMARY KEY);".to_owned(),
        ),
    ]);

    target
        .pgpushy("apply", dir.path())
        .args(["--default-schema", &schema, "--auto-approve"])
        .assert()
        .success()
        .stdout(predicates::str::contains("2 changes"))
        .stdout(predicates::str::contains("Applied 1 schema"));

    target
        .pgpushy("apply", dir.path())
        .args(["--default-schema", &schema, "--auto-approve"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Nothing to apply"));
}

const ORDERS_UNORDERED: &str =
    "CREATE TABLE orders (id int PRIMARY KEY, customer_id int REFERENCES customers(id));";

/// Spec §8.6: approval is required, and a run that cannot ask must not assume
/// the answer is yes. Test processes never have a terminal, so this is exactly
/// the CI case.
#[test]
fn refuses_to_apply_unattended_without_auto_approve() {
    let target = require_target!();
    let schema = unique_schema("noapprove");
    let _schemas = Schemas::create(&target, std::slice::from_ref(&schema));

    let dir = tree(&[("t.sql", "CREATE TABLE t (id int PRIMARY KEY);".to_owned())]);

    target
        .pgpushy("apply", dir.path())
        .args(["--default-schema", &schema])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "standard input is not a terminal",
        ))
        .stderr(predicates::str::contains("--auto-approve"));

    // And nothing was applied.
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

    let dir = tree(&[(
        "t.sql",
        "CREATE TABLE t (id int PRIMARY KEY, doomed text);".to_owned(),
    )]);
    target
        .pgpushy("apply", dir.path())
        .args(["--default-schema", &schema, "--auto-approve"])
        .assert()
        .success();

    std::fs::write(
        dir.path().join("t.sql"),
        "CREATE TABLE t (id int PRIMARY KEY);",
    )
    .expect("rewrite");

    target
        .pgpushy("apply", dir.path())
        .args(["--default-schema", &schema, "--auto-approve"])
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

    let dir = tree(&[
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
    ]);
    target
        .pgpushy("apply", dir.path())
        .arg("--auto-approve")
        .assert()
        .success();

    // Now remove both the foreign key and the column it depends on.
    std::fs::write(
        dir.path().join("a.sql"),
        format!("CREATE TABLE {referenced}.parent (id int PRIMARY KEY);"),
    )
    .expect("rewrite");
    std::fs::write(
        dir.path().join("z.sql"),
        format!("CREATE TABLE {referencing}.child (id int PRIMARY KEY, alt_ref int);"),
    )
    .expect("rewrite");

    target
        .pgpushy("apply", dir.path())
        .arg("--auto-approve")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be applied in one pass"))
        .stderr(predicates::str::contains("child_parent_fk"))
        .stderr(predicates::str::contains("apply this in two steps"))
        .stderr(predicates::str::contains("no schemas were applied"));

    // Refused means refused: the foreign key is still there. Scoped to this
    // test's own schema — constraint names are per-table, so an unscoped count
    // would also see the identically-named constraint another test created.
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

    let with_column = format!(
        "CREATE TABLE {referenced}.parent \
         (id int PRIMARY KEY, alt int, CONSTRAINT parent_alt_key UNIQUE (alt));"
    );
    let dir = tree(&[
        ("a.sql", with_column.clone()),
        (
            "z.sql",
            format!(
                "CREATE TABLE {referencing}.child (id int PRIMARY KEY, alt_ref int);
                 ALTER TABLE {referencing}.child ADD CONSTRAINT child_parent_fk
                     FOREIGN KEY (alt_ref) REFERENCES {referenced}.parent (alt);"
            ),
        ),
    ]);
    target
        .pgpushy("apply", dir.path())
        .arg("--auto-approve")
        .assert()
        .success();

    // Step 1: remove only the foreign key, keeping the column.
    std::fs::write(
        dir.path().join("z.sql"),
        format!("CREATE TABLE {referencing}.child (id int PRIMARY KEY, alt_ref int);"),
    )
    .expect("rewrite");
    target
        .pgpushy("apply", dir.path())
        .arg("--auto-approve")
        .assert()
        .success();

    // Step 2: now the column can go.
    std::fs::write(
        dir.path().join("a.sql"),
        format!("CREATE TABLE {referenced}.parent (id int PRIMARY KEY);"),
    )
    .expect("rewrite");
    target
        .pgpushy("apply", dir.path())
        .arg("--auto-approve")
        .assert()
        .success();

    target
        .pgpushy("apply", dir.path())
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

    let dir = tree(&[(
        "cycle.sql",
        format!(
            "CREATE TABLE {one}.a (id int PRIMARY KEY, r int REFERENCES {two}.b(id));
             CREATE TABLE {two}.b (id int PRIMARY KEY, r int REFERENCES {one}.a(id));"
        ),
    )]);

    target
        .pgpushy("apply", dir.path())
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

    let dir = tree(&[
        (
            "good.sql",
            format!("CREATE TABLE {good}.t (id int PRIMARY KEY);"),
        ),
        (
            "bad.sql",
            format!("CREATE TABLE {bad}.t (id int PRIMARY KEY, amount int);"),
        ),
    ]);
    target
        .pgpushy("apply", dir.path())
        .arg("--auto-approve")
        .assert()
        .success();

    // Data that the new constraint will reject, so the second schema's apply
    // fails for a reason pgpushy cannot foresee — which is the point.
    target
        .client()
        .batch_execute(&format!("INSERT INTO {bad}.t (id, amount) VALUES (1, -5)"))
        .expect("insert violating row");

    std::fs::write(
        dir.path().join("good.sql"),
        format!("CREATE TABLE {good}.t (id int PRIMARY KEY, added text);"),
    )
    .expect("rewrite");
    std::fs::write(
        dir.path().join("bad.sql"),
        format!(
            "CREATE TABLE {bad}.t (id int PRIMARY KEY, amount int,
                 CONSTRAINT amount_positive CHECK (amount > 0));"
        ),
    )
    .expect("rewrite");

    target
        .pgpushy("apply", dir.path())
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
// The password warning (spec §10)
// ---------------------------------------------------------------------------
//
// No database needed: the warning fires when the resolved connection is about
// to be used, which is before pgpushy connects. A stub pgschema gets past the
// provider, and the connection then fails — after the warning has been said.

#[cfg(unix)]
fn password_warning_run(dir: &TempDir, stub: &Path) -> assert_cmd::assert::Assert {
    Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .arg("plan")
        .current_dir(dir.path())
        .arg("--pgschema-path")
        .arg(stub)
        .assert()
        .failure()
}

/// A password read from a file that is easily committed is worth interrupting
/// someone about.
#[cfg(unix)]
#[test]
fn warns_when_the_password_comes_from_the_config_file() {
    let dir = tree(&[("t.sql", "CREATE TABLE t (id int PRIMARY KEY);".to_owned())]);
    let stub = stub_pgschema(&dir, "Version: 1.12.0@abc linux/amd64 2026-07-06 00:00:00");
    std::fs::write(
        dir.path().join("pgpushy.toml"),
        "[connection]\nhost = \"127.0.0.1\"\nport = 1\ndb = \"nope\"\n\
         user = \"nope\"\npassword = \"hunter2\"\n",
    )
    .expect("write config");

    password_warning_run(&dir, &stub)
        .stderr(predicates::str::contains("PASSWORD READ FROM"))
        .stderr(predicates::str::contains("pgpushy.toml"))
        // The warning must not become a new way to leak the password.
        .stderr(predicates::str::contains("hunter2").not());
}

/// Spec §10 is explicit that the warning fires on *use*, not on presence: a
/// file password something else overrode is not a risk worth interrupting for.
#[cfg(unix)]
#[test]
fn does_not_warn_when_the_file_password_is_overridden() {
    let dir = tree(&[("t.sql", "CREATE TABLE t (id int PRIMARY KEY);".to_owned())]);
    let stub = stub_pgschema(&dir, "Version: 1.12.0@abc linux/amd64 2026-07-06 00:00:00");
    std::fs::write(
        dir.path().join("pgpushy.toml"),
        "[connection]\nhost = \"127.0.0.1\"\nport = 1\ndb = \"nope\"\n\
         user = \"nope\"\npassword = \"hunter2\"\n",
    )
    .expect("write config");

    Command::cargo_bin("pgpushy")
        .expect("binary builds")
        .arg("plan")
        .current_dir(dir.path())
        .arg("--pgschema-path")
        .arg(&stub)
        .env("PGPASSWORD", "from-the-environment")
        .assert()
        .failure()
        .stderr(predicates::str::contains("PASSWORD READ FROM").not());
}
