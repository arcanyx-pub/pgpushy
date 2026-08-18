//! Desired-state synthesis (spec §5).
//!
//! Produces one SQL document per managed schema. Three properties matter, and
//! none of them is "readable output" — the reader is a machine:
//!
//! - **Executable in order.** pgschema *executes* a document to build its
//!   comparison model, so emission order is execution order. The six
//!   categories of spec §5.1 exist for exactly that reason: a table before the
//!   index on it, every table before any foreign key, comments last.
//! - **Correct for the schema it targets.** pgschema strips a schema qualifier
//!   from an identifier but cannot reach inside a string literal, so a
//!   sequence reference has to be spelled differently depending on which
//!   schema's run reads it. That is why there is a document per schema rather
//!   than one shared between them (spec §5.4).
//! - **Byte-identical across runs and platforms** (spec §11.3). Every
//!   intra-category order is a total order over the source content, never over
//!   filesystem enumeration or hash iteration.

use crate::error::CoreError;
use crate::literal;
use crate::model::{Comment, ForeignKey, Index, Objects, QualifiedName, SchemaName, Table};
use pg_query::NodeEnum;
use pg_query::protobuf::{
    AlterTableCmd, AlterTableStmt, AlterTableType, CreateSchemaStmt, DropBehavior, Node,
    ObjectType, ParseResult, RawStmt,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

/// The synthesized desired state: one document per managed schema.
pub type Documents = BTreeMap<SchemaName, String>;

/// Synthesize one document for every managed schema (spec §5.4).
pub fn synthesize(objects: &Objects, managed: &[SchemaName]) -> Result<Documents, CoreError> {
    managed
        .iter()
        .map(|target| Ok((target.clone(), for_schema(objects, target)?)))
        .collect()
}

/// The tables from other schemas that `target`'s document must also contain.
///
/// pgschema resolves every reference from the document alone and never from
/// the target database, so a foreign key into another schema needs the table
/// it points at to be present (spec §5.4). Verified: it fails with `relation …
/// does not exist` otherwise, even when the table exists on the target.
///
/// The walk is a fixpoint over creation-time references, which today closes
/// after one step. A foreign key is not a creation-time dependency — that is
/// what FK-lift bought — and a closure member contributes no foreign keys of
/// its own, so `S → X.t → Y.u` stops at `X.t`.
fn closure_for(objects: &Objects, target: &SchemaName) -> BTreeSet<QualifiedName> {
    let mut members = BTreeSet::new();
    let mut pending: Vec<QualifiedName> = objects
        .foreign_keys
        .iter()
        .filter(|fk| fk.table.schema == *target && fk.referenced.schema != *target)
        .map(|fk| fk.referenced.clone())
        .collect();

    while let Some(name) = pending.pop() {
        if !members.insert(name) {
            continue;
        }
        // Onward creation-time references of the member just added would be
        // pushed here. A table in 0.1's object scope has none: §4.5 admits no
        // cross-schema reference other than a foreign key, so everything a
        // member needs is already in its own schema's document.
    }

    members
}

fn for_schema(objects: &Objects, target: &SchemaName) -> Result<String, CoreError> {
    let closure = closure_for(objects, target);

    let mut out = String::new();
    out.push_str(GENERATED_MARKER);
    out.push_str(&format!(
        "\n\
         --\n\
         -- The desired state of schema {target}, as pgschema diffs it against the\n\
         -- target. Objects from other schemas appear only so that references\n\
         -- into them resolve; pgschema does not compare them.\n",
    ));

    let mut schemas: BTreeSet<&SchemaName> = closure.iter().map(|name| &name.schema).collect();
    schemas.insert(target);

    emit(&mut out, "schemas", schema_statements(&schemas)?);
    emit(
        &mut out,
        "tables",
        table_statements(&objects.tables, target, &closure)?,
    );
    emit(
        &mut out,
        "indexes",
        index_statements(&objects.indexes, target, &closure)?,
    );
    emit(
        &mut out,
        "foreign keys",
        foreign_key_statements(&objects.foreign_keys, target)?,
    );
    emit(
        &mut out,
        "comments",
        comment_statements(&objects.comments, target)?,
    );

    Ok(out)
}

/// Render a statement, de-qualifying its name literals when it belongs to the
/// schema this document targets (spec §5.4).
///
/// The check afterwards is the safety net for the rewrite in
/// [`crate::literal::dequalify`], whose walk enumerates the node kinds an
/// expression can hold. A literal it failed to reach would otherwise be
/// emitted still qualified, and pgschema would fail resolving it against a
/// schema whose objects are no longer there — an error naming a relation the
/// author never wrote. Failing here instead names the statement.
fn render(node: &NodeEnum, owned_by_target: bool, context: &str) -> Result<String, CoreError> {
    if !owned_by_target {
        return deparse(node, context);
    }
    let mut node = node.clone();
    literal::dequalify(&mut node);
    if let Some(missed) = literal::find(&node)
        .into_iter()
        .find(|found| literal::name_parts(&found.raw).is_some_and(|parts| parts.len() > 1))
    {
        return Err(CoreError::QualifiedLiteral {
            context: context.to_owned(),
            literal: missed.raw,
        });
    }
    deparse(&node, context)
}

/// First line of every synthesized document.
///
/// Discovery skips any source file beginning with this, so that pgpushy never
/// reads back a document it wrote. Writing the desired state into the source
/// root is a natural thing to do, and without this the next run would report
/// every object in it as a duplicate of itself — a failure that persists until
/// someone works out where the extra file came from.
pub const GENERATED_MARKER: &str = "-- Generated by pgpushy. Do not edit.";

/// A statement plus the key it sorts by within its category.
type Sorted = (Vec<String>, String);

fn emit(out: &mut String, label: &str, mut statements: Vec<Sorted>) {
    if statements.is_empty() {
        return;
    }
    statements.sort();
    out.push_str(&format!("\n-- {label}\n"));
    for (_, sql) in statements {
        out.push_str(&sql);
        out.push_str(";\n");
    }
}

// ---------------------------------------------------------------------------
// Category 1: schemas
// ---------------------------------------------------------------------------

/// `CREATE SCHEMA IF NOT EXISTS` for every managed schema.
///
/// These run only in pgschema's plan database, to make qualified references
/// resolvable while it builds its model. They never reach the target, which
/// must already hold every managed schema (spec §6.1). Authored `CREATE SCHEMA`
/// statements contribute their name to the managed set and are otherwise not
/// echoed — this is the canonical form for all of them.
fn schema_statements(schemas: &BTreeSet<&SchemaName>) -> Result<Vec<Sorted>, CoreError> {
    schemas
        .iter()
        .map(|schema| {
            let node = NodeEnum::CreateSchemaStmt(CreateSchemaStmt {
                schemaname: schema.as_str().to_owned(),
                authrole: None,
                schema_elts: Vec::new(),
                if_not_exists: true,
            });
            Ok((
                vec![schema.as_str().to_owned()],
                deparse(&node, "CREATE SCHEMA")?,
            ))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Category 3: tables
// ---------------------------------------------------------------------------

/// Tables, with foreign keys already removed by [`crate::parse`].
///
/// This category is internally order-free — that is the whole point of
/// FK-lift. Two tables have no creation-time dependency on one another once
/// their foreign keys are gone, so `(schema, name)` order is as valid as any
/// other, and it is deterministic. §4.3's rejection of `INHERITS`,
/// `PARTITION OF`, `OF <type>` and `LIKE` is what keeps that true.
///
/// The target schema's own tables and its closure members are emitted the
/// same way but spelled differently: only the target's have their name
/// literals de-qualified (spec §5.4).
fn table_statements(
    tables: &[Table],
    target: &SchemaName,
    closure: &BTreeSet<QualifiedName>,
) -> Result<Vec<Sorted>, CoreError> {
    tables
        .iter()
        .filter(|table| table.name.schema == *target || closure.contains(&table.name))
        .map(|table| {
            let node = NodeEnum::CreateStmt(table.ast.clone());
            let owned = table.name.schema == *target;
            Ok((
                name_key(&table.name, &[]),
                render(&node, owned, &table.name.to_string())?,
            ))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Category 4: indexes
// ---------------------------------------------------------------------------

/// These depend on their table existing, which is why they are not category 2.
///
/// Nothing here depends on anything else in the category, so any internal
/// order is valid.
fn index_statements(
    indexes: &[Index],
    target: &SchemaName,
    closure: &BTreeSet<QualifiedName>,
) -> Result<Vec<Sorted>, CoreError> {
    indexes
        .iter()
        .filter(|index| index.table.schema == *target || closure.contains(&index.table))
        .map(|index| {
            let node = NodeEnum::IndexStmt(Box::new(index.ast.clone()));
            let owned = index.table.schema == *target;
            Ok((
                name_key(&index.table, std::slice::from_ref(&index.name.name)),
                render(&node, owned, &index.name.to_string())?,
            ))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Category 5: foreign keys
// ---------------------------------------------------------------------------

/// Every foreign key, as a trailing `ALTER TABLE … ADD CONSTRAINT`.
///
/// With every table already created, no ordering here can dangle, and
/// mutually-referencing tables — which have no valid inline ordering at all —
/// come out correct. pgschema defers one constraint of a same-schema cycle
/// itself.
///
/// A constraint the author did not name is emitted *without* a name, so
/// Postgres generates it in the plan database by the same algorithm that
/// generated it on the target (spec §5.3). The sort key falls back to the
/// rendered SQL for exactly that case, since there is no name to sort by.
fn foreign_key_statements(
    foreign_keys: &[ForeignKey],
    target: &SchemaName,
) -> Result<Vec<Sorted>, CoreError> {
    foreign_keys
        .iter()
        .filter(|fk| fk.table.schema == *target)
        .map(|fk| {
            let node = add_constraint(&fk.table, &fk.ast);
            let sql = render(&node, true, &fk.table.to_string())?;
            Ok((
                name_key(
                    &fk.table,
                    &[
                        fk.columns.join(","),
                        fk.referenced.to_string(),
                        fk.name.clone().unwrap_or_default(),
                        sql.clone(),
                    ],
                ),
                sql,
            ))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Category 6: comments
// ---------------------------------------------------------------------------

/// Comments last, so they may reference any object in the document.
fn comment_statements(comments: &[Comment], target: &SchemaName) -> Result<Vec<Sorted>, CoreError> {
    comments
        .iter()
        .filter(|comment| comment.schema == *target)
        .map(|comment| {
            let node = NodeEnum::CommentStmt(Box::new(comment.ast.clone()));
            let sql = render(&node, true, &comment.target)?;
            Ok((sort_key(&comment.target, std::slice::from_ref(&sql)), sql))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// AST construction and rendering
// ---------------------------------------------------------------------------

/// Wrap a constraint as `ALTER TABLE <table> ADD CONSTRAINT …`.
///
/// Every enum field is set explicitly. libpg_query's deparser maps protobuf
/// enum values back to C enums through a `switch` that `Assert(false)`s on
/// anything it does not recognize, and 0 — what `Default` supplies — is
/// `Undefined` for all of them. A `..Default::default()` here aborts the
/// process rather than returning an error.
fn add_constraint(table: &QualifiedName, constraint: &pg_query::protobuf::Constraint) -> NodeEnum {
    NodeEnum::AlterTableStmt(AlterTableStmt {
        relation: Some(relation_of(table)),
        cmds: vec![Node {
            node: Some(NodeEnum::AlterTableCmd(Box::new(AlterTableCmd {
                subtype: AlterTableType::AtAddConstraint as i32,
                name: String::new(),
                num: 0,
                newowner: None,
                def: Some(Box::new(Node {
                    node: Some(NodeEnum::Constraint(Box::new(constraint.clone()))),
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

/// Build a `RangeVar` from a resolved name.
///
/// Built from the schema and name as separate fields rather than from a
/// rendered `schema.name` string: an identifier may legitimately contain a
/// dot (`"my.table"`), and splitting the rendered form on `.` would tear such
/// a name in half. The deparser adds whatever quoting each part needs.
fn relation_of(name: &QualifiedName) -> pg_query::protobuf::RangeVar {
    pg_query::protobuf::RangeVar {
        catalogname: String::new(),
        schemaname: name.schema.as_str().to_owned(),
        relname: name.name.clone(),
        inh: true,
        relpersistence: "p".to_owned(),
        alias: None,
        location: -1,
    }
}

/// Render one statement as SQL.
fn deparse(node: &NodeEnum, context: &str) -> Result<String, CoreError> {
    let wrapper = ParseResult {
        version: protobuf_version(),
        stmts: vec![RawStmt {
            stmt: Some(Box::new(Node {
                node: Some(node.clone()),
            })),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    pg_query::deparse(&wrapper).map_err(|source| CoreError::Deparse {
        context: context.to_owned(),
        source: Box::new(source),
    })
}

/// The parse-tree version libpg_query stamps on its output.
///
/// Read once from a trivial parse rather than hardcoded, so it tracks whatever
/// grammar the linked `pg_query` was built against.
fn protobuf_version() -> i32 {
    static VERSION: OnceLock<i32> = OnceLock::new();
    *VERSION.get_or_init(|| {
        pg_query::parse("SELECT 1")
            .map(|r| r.protobuf.version)
            .unwrap_or_default()
    })
}

/// Build a sort key: the owning object first, then discriminators.
///
/// Schema and name are separate elements rather than a rendered `schema.name`,
/// so that an identifier containing a dot cannot collide with a differently
/// split pair.
fn name_key(owner: &QualifiedName, rest: &[String]) -> Vec<String> {
    let mut key = Vec::with_capacity(rest.len() + 2);
    key.push(owner.schema.as_str().to_owned());
    key.push(owner.name.clone());
    key.extend_from_slice(rest);
    key
}

/// A sort key for something identified by a rendered string rather than a
/// [`QualifiedName`] — currently just comments, whose target may be a column
/// or a constraint.
fn sort_key(primary: &str, rest: &[String]) -> Vec<String> {
    let mut key = Vec::with_capacity(rest.len() + 1);
    key.push(primary.to_owned());
    key.extend_from_slice(rest);
    key
}
