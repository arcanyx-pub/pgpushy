//! **R1 spike** — does `pg_query` survive parse → mutate → deparse?
//!
//! The whole synthesis design (spec §5) assumes pgpushy can parse real DDL
//! into libpg_query's AST, perform two targeted edits on it — lift foreign
//! keys out of `CREATE TABLE` into trailing `ALTER TABLE … ADD CONSTRAINT`
//! (spec §5.3), and schema-qualify every relation (spec §5.4) — and deparse
//! the result back into SQL that means the same thing. If deparse mangles
//! anything, `synth.rs` needs the text-slicing fallback in impl-plan §5
//! instead, so this is answered before any of it is built.
//!
//! The checks here are deliberately structural rather than string-comparing
//! against hand-written expected SQL: deparse output is consumed by pgschema,
//! a machine, so its exact formatting is irrelevant (impl-plan §5). What must
//! hold is that it re-parses to the same thing.
//!
//! This file is a spike, not the implementation. The transforms below are the
//! smallest thing that answers the question; `synth.rs` will reimplement them
//! properly with diagnostics, ordering, and the full statement model.

use pg_query::NodeEnum;
use pg_query::protobuf::{
    AlterTableCmd, AlterTableStmt, AlterTableType, ConstrType, Constraint, CreateStmt,
    DropBehavior, Node, ObjectType, ParseResult, RawStmt,
};

// ---------------------------------------------------------------------------
// Minimal AST plumbing
// ---------------------------------------------------------------------------

/// Parse SQL known to hold exactly one statement, returning its root node.
fn parse_one(sql: &str) -> NodeEnum {
    let parsed = pg_query::parse(sql).expect("parses");
    let stmts = &parsed.protobuf.stmts;
    assert_eq!(stmts.len(), 1, "expected exactly one statement in {sql:?}");
    stmts[0]
        .stmt
        .clone()
        .expect("statement present")
        .node
        .expect("node present")
}

/// Deparse a single statement node back to SQL.
fn deparse_one(node: &NodeEnum) -> String {
    let wrapper = ParseResult {
        version: pg_query::parse("SELECT 1")
            .expect("parses")
            .protobuf
            .version,
        stmts: vec![RawStmt {
            stmt: Some(Box::new(Node {
                node: Some(node.clone()),
            })),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    pg_query::deparse(&wrapper).expect("deparses")
}

fn string_node(s: &str) -> Node {
    Node {
        node: Some(NodeEnum::String(pg_query::protobuf::String {
            sval: s.to_owned(),
        })),
    }
}

// ---------------------------------------------------------------------------
// The two transforms under test
// ---------------------------------------------------------------------------

/// Remove every foreign key from a `CREATE TABLE`, returning them separately.
///
/// Foreign keys arrive in two shapes and both must be handled:
///
/// - **Table-level** — a `Constraint` directly in `table_elts`, carrying its
///   own `fk_attrs`.
/// - **Column-level** — a `Constraint` inside a `ColumnDef`'s `constraints`,
///   with `fk_attrs` *empty*, because the column it hangs off is implied.
///   Lifting it to a standalone `ALTER TABLE` loses that context, so the
///   column name has to be written into `fk_attrs` on the way out. Getting
///   this wrong silently produces a constraint on the wrong columns.
fn lift_foreign_keys(create: &mut CreateStmt) -> Vec<Constraint> {
    let mut lifted = Vec::new();
    let mut kept = Vec::new();

    for elt in std::mem::take(&mut create.table_elts) {
        match elt.node {
            Some(NodeEnum::Constraint(ref c)) if c.contype == ConstrType::ConstrForeign as i32 => {
                lifted.push((**c).clone());
            }
            Some(NodeEnum::ColumnDef(mut col)) => {
                let mut col_kept = Vec::new();
                for con in std::mem::take(&mut col.constraints) {
                    match con.node {
                        Some(NodeEnum::Constraint(ref c))
                            if c.contype == ConstrType::ConstrForeign as i32 =>
                        {
                            let mut fk = (**c).clone();
                            // The implied column, now made explicit.
                            if fk.fk_attrs.is_empty() {
                                fk.fk_attrs = vec![string_node(&col.colname)];
                            }
                            lifted.push(fk);
                        }
                        _ => col_kept.push(con),
                    }
                }
                col.constraints = col_kept;
                kept.push(Node {
                    node: Some(NodeEnum::ColumnDef(col)),
                });
            }
            _ => kept.push(elt),
        }
    }

    create.table_elts = kept;
    lifted
}

/// Wrap a lifted foreign key as `ALTER TABLE <table> ADD CONSTRAINT …`.
///
/// `conname` is left exactly as the author wrote it — empty when they wrote
/// nothing, which is what spec §5.3 requires: an unnamed constraint must stay
/// unnamed so Postgres generates the same name it generated on the target.
///
/// Note `behavior`: libpg_query's deparser maps protobuf enum values back to C
/// enums through a `switch` that `Assert(false)`s on anything it does not
/// recognize, and every one of these enums reserves 0 for `Undefined`. A
/// `..Default::default()` here therefore aborts the process rather than
/// returning an error. Synthesized nodes must set every enum field explicitly.
fn fk_to_alter_table(table: &pg_query::protobuf::RangeVar, fk: Constraint) -> NodeEnum {
    NodeEnum::AlterTableStmt(AlterTableStmt {
        relation: Some(table.clone()),
        cmds: vec![Node {
            node: Some(NodeEnum::AlterTableCmd(Box::new(AlterTableCmd {
                subtype: AlterTableType::AtAddConstraint as i32,
                name: std::string::String::new(),
                num: 0,
                newowner: None,
                def: Some(Box::new(Node {
                    node: Some(NodeEnum::Constraint(Box::new(fk))),
                })),
                behavior: DropBehavior::DropRestrict as i32,
                missing_ok: false,
                recurse: false,
            }))),
        }],
        objtype: ObjectType::ObjectTable as i32,
        missing_ok: false,
    })
}

/// Set the schema on a relation that may have been written unqualified.
fn qualify(rel: &mut pg_query::protobuf::RangeVar, default_schema: &str) {
    if rel.schemaname.is_empty() {
        rel.schemaname = default_schema.to_owned();
    }
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// Deparse is *stable*: its output re-parses to something that deparses
/// identically.
///
/// This is the property that matters. Comparing the protobuf before and after
/// would fail on `location` fields alone — they are byte offsets into the
/// original text, so they necessarily move — and comparing against expected
/// SQL strings would test formatting nobody depends on. Stability says the
/// output is inside deparse's own fixed point, i.e. nothing was lost that a
/// second pass would reveal.
fn assert_deparse_roundtrips(node: &NodeEnum) -> String {
    let once = deparse_one(node);
    let twice = deparse_one(&parse_one(&once));
    assert_eq!(once, twice, "deparse is not stable for: {once}");
    once
}

/// Representative DDL: every feature the transforms have to survive.
///
/// Kept one-per-line: the value of this table is being able to read the
/// coverage at a glance, which rustfmt's wrapping destroys.
#[rustfmt::skip]
const FIXTURES: &[(&str, &str)] = &[
    ("simple inline fk", "CREATE TABLE orders (id int PRIMARY KEY, customer_id int NOT NULL REFERENCES customers(id))"),
    ("named table-level fk", "CREATE TABLE orders (id int PRIMARY KEY, customer_id int, CONSTRAINT orders_cust_fk FOREIGN KEY (customer_id) REFERENCES customers(id))"),
    ("unnamed table-level fk", "CREATE TABLE orders (id int PRIMARY KEY, customer_id int, FOREIGN KEY (customer_id) REFERENCES customers(id))"),
    ("composite fk", "CREATE TABLE line_items (order_id int, seq int, PRIMARY KEY (order_id, seq), FOREIGN KEY (order_id, seq) REFERENCES orders(id, seq))"),
    ("referential actions", "CREATE TABLE orders (id int PRIMARY KEY, customer_id int REFERENCES customers(id) ON DELETE CASCADE ON UPDATE RESTRICT)"),
    ("match full, deferrable", "CREATE TABLE orders (id int PRIMARY KEY, a int, b int, FOREIGN KEY (a, b) REFERENCES other(a, b) MATCH FULL DEFERRABLE INITIALLY DEFERRED)"),
    ("set null with columns", "CREATE TABLE orders (id int PRIMARY KEY, a int, b int, FOREIGN KEY (a, b) REFERENCES other(a, b) ON DELETE SET NULL (a, b))"),
    ("quoted mixed case", r#"CREATE TABLE "Orders" ("Id" int PRIMARY KEY, "customerId" int REFERENCES "Customers"("Id"))"#),
    ("already qualified fk", "CREATE TABLE billing.invoices (id int PRIMARY KEY, customer_id int REFERENCES public.customers(id))"),
    ("check and unique and default", "CREATE TABLE t (id int PRIMARY KEY, qty int NOT NULL DEFAULT 0 CHECK (qty >= 0), code text UNIQUE, ref_id int REFERENCES other(id))"),
    ("generated and identity", "CREATE TABLE t (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, w numeric, h numeric, area numeric GENERATED ALWAYS AS (w * h) STORED, ref_id int REFERENCES other(id))"),
    // "window" is a reserved word, so it exercises quoting on the way back out.
    ("array, interval, reserved word", r#"CREATE TABLE t (id int PRIMARY KEY, tags text[], "window" interval day to second, ref_id int REFERENCES other(id))"#),
    ("no fk at all", "CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL)"),
];

#[test]
fn deparse_roundtrips_unmodified_ddl() {
    // Baseline: before asking whether deparse survives *mutation*, confirm it
    // survives the identity transform. A failure here would be a pg_query
    // problem, not a pgpushy one.
    for (name, sql) in FIXTURES {
        let node = parse_one(sql);
        assert_deparse_roundtrips(&node);
        eprintln!("ok  identity        {name}");
    }
}

#[test]
fn fk_lift_and_qualify_roundtrip() {
    for (name, sql) in FIXTURES {
        let NodeEnum::CreateStmt(mut create) = parse_one(sql) else {
            panic!("{name}: expected CREATE TABLE");
        };

        let lifted = lift_foreign_keys(&mut create);
        qualify(create.relation.as_mut().expect("has a relation"), "public");

        // The table itself, with foreign keys gone.
        let table_sql = assert_deparse_roundtrips(&NodeEnum::CreateStmt(create.clone()));
        assert!(
            !table_sql.to_uppercase().contains("REFERENCES"),
            "{name}: foreign key survived in the table definition: {table_sql}"
        );
        assert!(
            table_sql.contains("public.") || table_sql.contains("billing."),
            "{name}: table was not schema-qualified: {table_sql}"
        );

        // Each lifted foreign key, as its own statement.
        let table = create.relation.clone().expect("has a relation");
        for mut fk in lifted {
            qualify(
                fk.pktable.as_mut().expect("fk has a referenced table"),
                "public",
            );
            let alter = fk_to_alter_table(&table, fk);
            let alter_sql = assert_deparse_roundtrips(&alter);
            assert!(
                alter_sql.to_uppercase().contains("FOREIGN KEY"),
                "{name}: lifted constraint lost its foreign key: {alter_sql}"
            );
            eprintln!("ok  lifted          {name}: {alter_sql}");
        }
        eprintln!("ok  lift+qualify    {name}: {table_sql}");
    }
}

#[test]
fn column_level_fk_gains_its_implied_column() {
    // A column-level REFERENCES has empty fk_attrs; lifting it must write the
    // column in, or the constraint silently lands on nothing.
    let NodeEnum::CreateStmt(mut create) = parse_one(
        "CREATE TABLE orders (id int PRIMARY KEY, customer_id int REFERENCES customers(id))",
    ) else {
        panic!("expected CREATE TABLE");
    };
    let lifted = lift_foreign_keys(&mut create);
    assert_eq!(lifted.len(), 1);

    let table = create.relation.clone().expect("has a relation");
    let sql = deparse_one(&fk_to_alter_table(&table, lifted[0].clone()));
    assert!(
        sql.contains("customer_id"),
        "lifted fk lost its column: {sql}"
    );
}

#[test]
fn unnamed_constraints_stay_unnamed() {
    // Spec §5.3: pgpushy must not invent a constraint name. Confirm the lifted
    // ALTER TABLE carries no name when the author wrote none, and keeps the
    // author's name when they did.
    let cases = [
        (
            "CREATE TABLE orders (id int, cid int REFERENCES customers(id))",
            None,
        ),
        (
            "CREATE TABLE orders (id int, cid int, CONSTRAINT my_fk FOREIGN KEY (cid) REFERENCES customers(id))",
            Some("my_fk"),
        ),
    ];

    for (sql, expected_name) in cases {
        let NodeEnum::CreateStmt(mut create) = parse_one(sql) else {
            panic!("expected CREATE TABLE");
        };
        let lifted = lift_foreign_keys(&mut create);
        assert_eq!(lifted.len(), 1);
        assert_eq!(lifted[0].conname.as_str(), expected_name.unwrap_or(""));

        let table = create.relation.clone().expect("has a relation");
        let out = deparse_one(&fk_to_alter_table(&table, lifted[0].clone()));
        match expected_name {
            Some(n) => assert!(out.contains(n), "author's name lost: {out}"),
            None => assert!(
                !out.to_uppercase().contains("ADD CONSTRAINT"),
                "a name was invented for an unnamed constraint: {out}"
            ),
        }
    }
}
