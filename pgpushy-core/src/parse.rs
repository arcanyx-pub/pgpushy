//! Parsing and classification: SQL text in, [`Objects`] out.
//!
//! This module does three things at once, because they need the same
//! information:
//!
//! 1. Parse each file with libpg_query (spec §4.2).
//! 2. Enforce the statement allow-list (spec §4.3), rejecting anything
//!    outside it with a diagnostic naming file, line, and statement kind.
//! 3. Assign every object and every foreign-key referent to a schema —
//!    the qualifier the author wrote, or the default schema (spec §4.4).
//!
//! Foreign keys are also lifted out of table definitions here rather than in
//! [`crate::synth`]. Lifting is a *structural* fact about where a constraint
//! lives, not a rendering choice, and the checks in [`crate::validate`] and
//! the graph in [`crate::graph`] both need foreign keys as first-class
//! objects. By the time anything sees a [`Table`], its foreign keys are
//! already separate.

use crate::error::{Diagnostic, DiagnosticKind};
use crate::literal;
use crate::model::{
    Comment, ForeignKey, Index, Objects, Origin, QualifiedName, SchemaDecl, SchemaName, Table,
    TypeKind, TypeLike,
};
use pg_query::NodeEnum;
use pg_query::protobuf::{
    AlterTableType, CommentStmt, ConstrType, Constraint, CreateStmt, IndexStmt, Node, ObjectType,
    RangeVar,
};

/// One file's worth of source, as read by the caller.
pub struct SourceFile {
    /// Path relative to the source-tree root, used verbatim in diagnostics.
    pub path: String,
    pub contents: String,
}

/// Parse and classify every file, collecting objects and diagnostics.
///
/// Never stops at the first problem: a source tree with five unsupported
/// statements reports all five (impl-plan §12). Objects from files that did
/// parse are still returned, so later stages can report *their* problems too
/// in the same run.
pub fn parse_files(
    files: &[SourceFile],
    default_schema: &SchemaName,
) -> (Objects, Vec<Diagnostic>) {
    let mut objects = Objects::default();
    let mut diagnostics = Vec::new();

    for file in files {
        parse_file(file, default_schema, &mut objects, &mut diagnostics);
    }

    (objects, diagnostics)
}

fn parse_file(
    file: &SourceFile,
    default_schema: &SchemaName,
    objects: &mut Objects,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let parsed = match pg_query::parse(&file.contents) {
        Ok(parsed) => parsed,
        Err(err) => {
            // libpg_query reports a cursor position, not a line; without a
            // usable offset the file itself is the best location available.
            diagnostics.push(
                Diagnostic::new(
                    DiagnosticKind::ParseFailure,
                    format!("could not parse SQL: {err}"),
                    vec![Origin {
                        file: file.path.clone(),
                        line: 1,
                    }],
                )
                .with_help(
                    "pgpushy parses with the real Postgres grammar; the file must be valid SQL",
                ),
            );
            return;
        }
    };

    for stmt in &parsed.protobuf.stmts {
        let origin = Origin {
            file: file.path.clone(),
            line: line_of(&file.contents, stmt.stmt_location),
        };
        let Some(node) = stmt.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
            continue;
        };
        classify(node, &origin, default_schema, objects, diagnostics);
    }
}

/// Convert a statement's byte offset into a 1-indexed line number.
///
/// libpg_query reports `stmt_location` as the position just past the previous
/// statement's semicolon, so it points at whatever separates the two — the
/// newline ending the previous line, blank lines, and any comment written
/// above the statement. Counting lines from there points the diagnostic at the
/// separator rather than the statement, so skip past it first.
pub(crate) fn line_of(contents: &str, offset: i32) -> u32 {
    let offset = usize::try_from(offset).unwrap_or(0).min(contents.len());
    let start = offset + skip_trivia(&contents[offset..]);
    u32::try_from(contents[..start].bytes().filter(|b| *b == b'\n').count() + 1).unwrap_or(u32::MAX)
}

/// Bytes of whitespace and comments at the start of `text`.
///
/// Postgres block comments nest, so the depth has to be tracked rather than
/// scanning for the first `*/`.
fn skip_trivia(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut i = 0;

    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes[i..].starts_with(b"--") {
            i += bytes[i..]
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len() - i, |n| n + 1);
        } else if bytes[i..].starts_with(b"/*") {
            let mut depth = 1;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else {
            return i;
        }
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

fn classify(
    node: &NodeEnum,
    origin: &Origin,
    default_schema: &SchemaName,
    objects: &mut Objects,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Spec §4.3: an object name inside a string literal must name its schema.
    // Checked here, over the statement as written, so the diagnostic can quote
    // the literal the author typed. Only for statements the allow-list admits
    // — an unsupported statement already has its own diagnostic, and a second
    // one about its interior would be noise.
    if is_allowed_kind(node) {
        check_name_literals(node, origin, diagnostics);
    }

    match node {
        NodeEnum::CreateSchemaStmt(stmt) => match classify_create_schema(stmt, origin) {
            Ok(decl) => objects.schemas.push(decl),
            Err(diag) => diagnostics.push(diag),
        },
        NodeEnum::CreateStmt(stmt) => {
            classify_create_table(stmt, origin, default_schema, objects, diagnostics);
        }
        NodeEnum::IndexStmt(stmt) => match classify_index(stmt, origin, default_schema) {
            Ok(index) => objects.indexes.push(index),
            Err(diag) => diagnostics.push(diag),
        },
        NodeEnum::AlterTableStmt(stmt) => {
            classify_alter_table(stmt, origin, default_schema, objects, diagnostics);
        }
        NodeEnum::CommentStmt(stmt) => match classify_comment(stmt, origin, default_schema) {
            Ok(comment) => objects.comments.push(comment),
            Err(diag) => diagnostics.push(diag),
        },
        NodeEnum::CreateEnumStmt(stmt) => {
            let names = stmt.type_name.clone();
            match classify_named_type(
                &names,
                NodeEnum::CreateEnumStmt(requalify_enum(stmt, origin, default_schema)),
                origin,
                default_schema,
                Vec::new(),
            ) {
                Ok(kind) => objects.types.push(kind),
                Err(diag) => diagnostics.push(diag),
            }
        }
        NodeEnum::CompositeTypeStmt(stmt) => {
            match classify_composite_type(stmt, origin, default_schema) {
                Ok(kind) => objects.types.push(kind),
                Err(diag) => diagnostics.push(diag),
            }
        }
        NodeEnum::CreateDomainStmt(stmt) => match classify_domain(stmt, origin, default_schema) {
            Ok(kind) => objects.types.push(kind),
            Err(diag) => diagnostics.push(diag),
        },
        NodeEnum::CreateSeqStmt(stmt) => match classify_sequence(stmt, origin, default_schema) {
            Ok(kind) => objects.types.push(kind),
            Err(diag) => diagnostics.push(diag),
        },
        other => diagnostics.push(unsupported(other, origin)),
    }
}

fn is_allowed_kind(node: &NodeEnum) -> bool {
    matches!(
        node,
        NodeEnum::CreateSchemaStmt(_)
            | NodeEnum::CreateStmt(_)
            | NodeEnum::IndexStmt(_)
            | NodeEnum::AlterTableStmt(_)
            | NodeEnum::CommentStmt(_)
            | NodeEnum::CreateEnumStmt(_)
            | NodeEnum::CompositeTypeStmt(_)
            | NodeEnum::CreateDomainStmt(_)
            | NodeEnum::CreateSeqStmt(_)
    )
}

/// Reject an object name inside a string literal that does not name a schema.
///
/// pgpushy will not infer one. §4.4's rule for identifiers is deliberately not
/// reused: the default schema and the owning object's schema are both
/// defensible readings, they disagree, and neither disagreement is visible as
/// an error — so choosing now would quietly fix the answer for good. Demanding
/// the schema keeps it open, and costs an imported tree nothing, since pg_dump
/// qualifies inside literals already.
fn check_name_literals(node: &NodeEnum, origin: &Origin, diagnostics: &mut Vec<Diagnostic>) {
    for found in literal::find(node) {
        let raw = &found.raw;
        let what = found.what;

        // Spec §4.3: pgschema models any default calling `nextval` as SERIAL.
        // Verified against pgschema 1.12.0: applying `CREATE SEQUENCE s` plus
        // a column defaulting to it creates an owned `<table>_<column>_seq`
        // instead, never creates `s`, reports success, and leaves every later
        // plan showing the same drop and add. A domain default calling it
        // fails outright, since pgschema applies domains before sequences.
        // Neither is something pgpushy can order around — the apply order is
        // pgschema's.
        if literal::names_a_sequence_call(&found) {
            diagnostics.push(
                Diagnostic::new(
                    DiagnosticKind::UnsupportedStatement,
                    format!("a default calling nextval on '{raw}'"),
                    vec![origin.clone()],
                )
                .with_help(
                    "pgschema applies this as SERIAL: it creates a sequence owned by the \
                     column instead of the one named here, and the plan never converges. \
                     Write the column as `serial` or `GENERATED BY DEFAULT AS IDENTITY`; a \
                     sequence nothing defaults to is managed normally (spec §4.3, §12.8)",
                ),
            );
            continue;
        }
        let problem = match literal::name_parts(raw) {
            Some(parts) if parts.len() == 2 => continue,
            Some(parts) if parts.len() == 1 => {
                format!("the {what} name '{raw}' does not say which schema it is in")
            }
            Some(_) => format!("the {what} name '{raw}' names more than a schema and an object"),
            None => format!("'{raw}' is not a name pgpushy can read"),
        };
        diagnostics.push(
            Diagnostic::new(
                DiagnosticKind::UnqualifiedNameLiteral,
                problem,
                vec![origin.clone()],
            )
            .with_help(
                "pgschema strips a schema qualifier from an identifier but cannot reach \
                 inside a string literal, so pgpushy must know which schema this names; \
                 write it as 'schema.name' (spec §4.3)",
            ),
        );
    }
}

fn classify_create_schema(
    stmt: &pg_query::protobuf::CreateSchemaStmt,
    origin: &Origin,
) -> Result<SchemaDecl, Diagnostic> {
    // Spec §4.3: only the bare form. A schema whose name comes from a role
    // cannot be resolved offline, and nested elements would need the schema
    // assignment rules of §4.4 to describe a shape they do not cover.
    if stmt.schemaname.is_empty() {
        return Err(Diagnostic::new(
            DiagnosticKind::UnsupportedSchemaForm,
            "CREATE SCHEMA without an explicit name",
            vec![origin.clone()],
        )
        .with_help(
            "pgpushy cannot resolve a schema name from a role offline; write \
             CREATE SCHEMA <name> AUTHORIZATION <role> as CREATE SCHEMA <name>",
        ));
    }
    if !stmt.schema_elts.is_empty() {
        return Err(Diagnostic::new(
            DiagnosticKind::UnsupportedSchemaForm,
            format!("CREATE SCHEMA {} with nested elements", stmt.schemaname),
            vec![origin.clone()],
        )
        .with_help(
            "split the nested objects into their own statements, qualified with the schema",
        ));
    }
    if stmt.authrole.is_some() {
        return Err(Diagnostic::new(
            DiagnosticKind::UnsupportedSchemaForm,
            format!("CREATE SCHEMA {} with AUTHORIZATION", stmt.schemaname),
            vec![origin.clone()],
        )
        .with_help(
            "pgpushy does not manage schema ownership; write CREATE SCHEMA <name> \
             and grant ownership separately",
        ));
    }

    Ok(SchemaDecl {
        name: SchemaName::new(&stmt.schemaname),
        origin: origin.clone(),
    })
}

fn classify_create_table(
    stmt: &CreateStmt,
    origin: &Origin,
    default_schema: &SchemaName,
    objects: &mut Objects,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Inheritance and partitioning create table-to-table *creation*
    // dependencies, which FK-lift does not resolve — precisely the class of
    // ordering problem spec §12.5 keeps out of 0.x. Reject them here, where a
    // good diagnostic is possible, rather than letting pgschema fail on an
    // ordering pgpushy chose.
    if !stmt.inh_relations.is_empty() {
        diagnostics.push(
            Diagnostic::new(
                DiagnosticKind::UnsupportedStatement,
                "CREATE TABLE with INHERITS or PARTITION OF",
                vec![origin.clone()],
            )
            .with_help(
                "pgpushy 0.x resolves foreign-key ordering only; a table that depends on \
                 another table's definition is not supported (spec §12.5)",
            ),
        );
        return;
    }
    if stmt.partspec.is_some() {
        diagnostics.push(
            Diagnostic::new(
                DiagnosticKind::UnsupportedStatement,
                "CREATE TABLE with PARTITION BY",
                vec![origin.clone()],
            )
            .with_help("pgpushy 0.x does not manage partitioned tables (spec §12.5)"),
        );
        return;
    }
    if stmt.of_typename.is_some() {
        diagnostics.push(
            Diagnostic::new(
                DiagnosticKind::UnsupportedStatement,
                "CREATE TABLE OF <type>",
                vec![origin.clone()],
            )
            .with_help(
                "a table whose shape comes from a type cannot be created before that type; \
                 write the columns out (spec §12.5)",
            ),
        );
        return;
    }
    // `LIKE` arrives inside the column list rather than in a clause of its
    // own, which is what makes it easy to miss beside the three above. It is
    // the same hazard: the copied-from table must exist first, and category 3
    // is emitted in name order, so a `LIKE` naming a table that sorts after it
    // produces a document that cannot execute.
    if stmt
        .table_elts
        .iter()
        .any(|elt| matches!(elt.node.as_ref(), Some(NodeEnum::TableLikeClause(_))))
    {
        diagnostics.push(
            Diagnostic::new(
                DiagnosticKind::UnsupportedStatement,
                "CREATE TABLE with LIKE",
                vec![origin.clone()],
            )
            .with_help(
                "a table copied from another cannot be created before it; write the columns \
                 out (spec §12.5)",
            ),
        );
        return;
    }

    let Some(relation) = stmt.relation.as_ref() else {
        diagnostics.push(unsupported_named(
            "CREATE TABLE without a table name",
            origin,
        ));
        return;
    };
    let name = qualify(relation, default_schema);

    let mut ast = stmt.clone();
    let lifted = lift_foreign_keys(&mut ast);
    qualify_relation(ast.relation.as_mut(), default_schema);

    for mut fk in lifted {
        let Some(pktable) = fk.constraint.pktable.as_ref() else {
            diagnostics.push(unsupported_named(
                "FOREIGN KEY without a referenced table",
                origin,
            ));
            continue;
        };
        let referenced = qualify(pktable, default_schema);
        qualify_relation(fk.constraint.pktable.as_mut(), default_schema);
        objects.foreign_keys.push(ForeignKey {
            table: name.clone(),
            referenced,
            name: non_empty(&fk.constraint.conname),
            columns: std::mem::take(&mut fk.columns),
            origin: origin.clone(),
            ast: fk.constraint,
        });
    }

    objects.tables.push(Table {
        name,
        depends_on: Vec::new(),
        origin: origin.clone(),
        ast,
    });
}

fn classify_index(
    stmt: &IndexStmt,
    origin: &Origin,
    default_schema: &SchemaName,
) -> Result<Index, Diagnostic> {
    // CONCURRENTLY cannot run inside a transaction block, and the desired
    // state is executed as one when pgschema builds its model. It also says
    // nothing about the schema's shape — it is a strategy for reaching it.
    if stmt.concurrent {
        return Err(Diagnostic::new(
            DiagnosticKind::UnsupportedStatement,
            "CREATE INDEX CONCURRENTLY",
            vec![origin.clone()],
        )
        .with_help(
            "the desired state describes the schema, not how to reach it; \
             drop CONCURRENTLY and let pgschema choose how to apply the index",
        ));
    }

    let Some(relation) = stmt.relation.as_ref() else {
        return Err(unsupported_named("CREATE INDEX without a table", origin));
    };
    let table = qualify(relation, default_schema);

    if stmt.idxname.is_empty() {
        // An unnamed index is named by Postgres from its table and columns.
        // Unlike a foreign key (spec §5.3) this cannot simply be passed
        // through, because duplicate detection and comments both need a name
        // to refer to, and the generated one depends on expression rendering
        // pgpushy does not reproduce.
        return Err(Diagnostic::new(
            DiagnosticKind::UnsupportedStatement,
            "CREATE INDEX without an index name",
            vec![origin.clone()],
        )
        .with_help("give the index an explicit name so plans stay stable"));
    }

    let mut ast = stmt.clone();
    qualify_relation(ast.relation.as_mut(), default_schema);

    Ok(Index {
        name: QualifiedName::new(table.schema.clone(), &stmt.idxname),
        table,
        origin: origin.clone(),
        ast,
    })
}

fn classify_alter_table(
    stmt: &pg_query::protobuf::AlterTableStmt,
    origin: &Origin,
    default_schema: &SchemaName,
    objects: &mut Objects,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if stmt.objtype != ObjectType::ObjectTable as i32 {
        diagnostics.push(unsupported_named(
            "ALTER on something other than a table",
            origin,
        ));
        return;
    }
    let Some(relation) = stmt.relation.as_ref() else {
        diagnostics.push(unsupported_named(
            "ALTER TABLE without a table name",
            origin,
        ));
        return;
    };
    let table = qualify(relation, default_schema);

    for cmd in &stmt.cmds {
        let Some(NodeEnum::AlterTableCmd(cmd)) = cmd.node.as_ref() else {
            diagnostics.push(unsupported_named(
                "ALTER TABLE with an unrecognized command",
                origin,
            ));
            continue;
        };
        // Spec §4.3: a source file says what exists, not the steps that reach
        // it, so `ALTER` is rejected. `ADD CONSTRAINT` for a foreign key is
        // the single exception, because it is pgpushy's own output shape —
        // §5.3 lifts every foreign key into exactly that form — and it is what
        // pg_dump emits, so a tree derived from one needs no rewriting there.
        if cmd.subtype != AlterTableType::AtAddConstraint as i32 {
            diagnostics.push(
                Diagnostic::new(
                    DiagnosticKind::UnsupportedStatement,
                    "ALTER TABLE subcommand other than ADD CONSTRAINT",
                    vec![origin.clone()],
                )
                .with_help(
                    "the source tree describes the schema you want, not the steps to get \
                     there; express this in the table's CREATE TABLE instead",
                ),
            );
            continue;
        }

        let Some(NodeEnum::Constraint(constraint)) = cmd.def.as_ref().and_then(|d| d.node.as_ref())
        else {
            diagnostics.push(unsupported_named(
                "ADD CONSTRAINT without a constraint",
                origin,
            ));
            continue;
        };
        let mut constraint = (**constraint).clone();

        if constraint.contype == ConstrType::ConstrForeign as i32 {
            let Some(pktable) = constraint.pktable.as_ref() else {
                diagnostics.push(unsupported_named(
                    "FOREIGN KEY without a referenced table",
                    origin,
                ));
                continue;
            };
            let referenced = qualify(pktable, default_schema);
            let columns = string_list(&constraint.fk_attrs);
            qualify_relation(constraint.pktable.as_mut(), default_schema);
            objects.foreign_keys.push(ForeignKey {
                table: table.clone(),
                referenced,
                name: non_empty(&constraint.conname),
                columns,
                origin: origin.clone(),
                ast: constraint,
            });
        } else {
            // A CHECK, UNIQUE, PRIMARY KEY or EXCLUDE constraint written
            // standalone. Rejecting it is what keeps the index category
            // internally order-free (spec §5.1): `ADD CONSTRAINT … UNIQUE
            // USING INDEX` needs its index to exist first, and no other form
            // in that category depends on anything but its own table.
            //
            // Verified against Postgres 18 that an inline table constraint can
            // carry an explicit name, so the inline form loses nothing.
            diagnostics.push(
                Diagnostic::new(
                    DiagnosticKind::UnsupportedStatement,
                    format!(
                        "ALTER TABLE … ADD CONSTRAINT {}",
                        constraint_kind(&constraint)
                    ),
                    vec![origin.clone()],
                )
                .with_help(format!(
                    "only a FOREIGN KEY may be added this way; write it inline in \
                     CREATE TABLE {table} instead, as `CONSTRAINT {} …` if you want to keep \
                     the name (spec §4.3)",
                    non_empty(&constraint.conname).unwrap_or_else(|| "<name>".to_owned()),
                )),
            );
        }
    }
}

fn classify_comment(
    stmt: &CommentStmt,
    origin: &Origin,
    default_schema: &SchemaName,
) -> Result<Comment, Diagnostic> {
    let objtype = stmt.objtype;
    let kind = if objtype == ObjectType::ObjectTable as i32 {
        CommentTarget::Table
    } else if objtype == ObjectType::ObjectColumn as i32 {
        CommentTarget::Column
    } else if objtype == ObjectType::ObjectIndex as i32 {
        CommentTarget::Index
    } else if objtype == ObjectType::ObjectTabconstraint as i32 {
        CommentTarget::Constraint
    } else if objtype == ObjectType::ObjectSchema as i32 {
        CommentTarget::Schema
    } else if objtype == ObjectType::ObjectType as i32 || objtype == ObjectType::ObjectDomain as i32
    {
        // Verified against pgschema 1.12.3 by applying and re-planning:
        // pgschema generates no DDL for either, applies the rest, and then
        // reports no changes — so the comment never reaches the database and
        // nothing ever says so. A comment that silently does not exist is
        // worse than one that is refused (spec §4.3, §12.9).
        return Err(Diagnostic::new(
            DiagnosticKind::UnsupportedStatement,
            "COMMENT ON a type or a domain",
            vec![origin.clone()],
        )
        .with_help(
            "pgschema drops these without applying them and without reporting it, so the \
             comment would never appear on the target; describe the type in the source \
             file instead (spec §12.9)",
        ));
    } else if objtype == ObjectType::ObjectSequence as i32 {
        CommentTarget::Sequence
    } else {
        return Err(Diagnostic::new(
            DiagnosticKind::UnsupportedStatement,
            "COMMENT ON an object kind pgpushy does not manage",
            vec![origin.clone()],
        )
        .with_help(
            "pgpushy 0.1 manages comments on schemas, tables, columns, indexes, table \
             constraints and sequences",
        ));
    };

    let mut ast = stmt.clone();
    let parts = comment_object_parts(&ast).ok_or_else(|| {
        unsupported_named("COMMENT ON with an object name pgpushy cannot read", origin)
    })?;

    // A comment's object is a dotted list whose length tells us whether a
    // schema qualifier is present: TABLE takes (table) or (schema, table),
    // COLUMN takes (table, column) or (schema, table, column), and so on.
    let (schema, target) = match (kind, parts.len()) {
        (CommentTarget::Schema, 1) => (SchemaName::new(&parts[0]), parts[0].clone()),
        (CommentTarget::Table | CommentTarget::Index | CommentTarget::Sequence, 1) => (
            default_schema.clone(),
            format!("{default_schema}.{}", parts[0]),
        ),
        (CommentTarget::Table | CommentTarget::Index | CommentTarget::Sequence, 2) => {
            (SchemaName::new(&parts[0]), parts.join("."))
        }
        (CommentTarget::Column | CommentTarget::Constraint, 2) => (
            default_schema.clone(),
            format!("{default_schema}.{}", parts.join(".")),
        ),
        (CommentTarget::Column | CommentTarget::Constraint, 3) => {
            (SchemaName::new(&parts[0]), parts.join("."))
        }
        _ => {
            return Err(unsupported_named(
                "COMMENT ON with an object name pgpushy cannot read",
                origin,
            ));
        }
    };

    if kind != CommentTarget::Schema {
        qualify_comment_object(&mut ast, kind, &schema);
    }

    Ok(Comment {
        schema,
        target: format!("{} {}", kind.keyword(), target),
        origin: origin.clone(),
        ast,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommentTarget {
    Schema,
    Table,
    Column,
    Index,
    Constraint,
    Sequence,
}

impl CommentTarget {
    fn keyword(self) -> &'static str {
        match self {
            Self::Schema => "SCHEMA",
            Self::Table => "TABLE",
            Self::Column => "COLUMN",
            Self::Index => "INDEX",
            Self::Constraint => "CONSTRAINT",
            Self::Sequence => "SEQUENCE",
        }
    }

    /// How many trailing parts are the object itself rather than its schema.
    fn unqualified_len(self) -> usize {
        match self {
            Self::Schema | Self::Table | Self::Index | Self::Sequence => 1,
            Self::Column | Self::Constraint => 2,
        }
    }
}

// ---------------------------------------------------------------------------
// Foreign-key lifting
// ---------------------------------------------------------------------------

struct LiftedForeignKey {
    constraint: Constraint,
    columns: Vec<String>,
}

/// Remove every foreign key from a `CREATE TABLE`, returning them separately.
///
/// Foreign keys arrive in two shapes. A **table-level** constraint sits
/// directly in `table_elts` and carries its own `fk_attrs`. A **column-level**
/// `REFERENCES` sits inside a `ColumnDef` with `fk_attrs` *empty*, because the
/// column it hangs off is implied — and lifting it to a standalone
/// `ALTER TABLE` destroys that context. The column name is written in here, on
/// the way out; without it the constraint would silently cover no columns.
fn lift_foreign_keys(create: &mut CreateStmt) -> Vec<LiftedForeignKey> {
    let mut lifted = Vec::new();
    let mut kept = Vec::with_capacity(create.table_elts.len());

    for elt in std::mem::take(&mut create.table_elts) {
        match elt.node {
            Some(NodeEnum::Constraint(ref c)) if is_foreign_key(c) => {
                let columns = string_list(&c.fk_attrs);
                lifted.push(LiftedForeignKey {
                    constraint: (**c).clone(),
                    columns,
                });
            }
            Some(NodeEnum::ColumnDef(mut col)) => {
                let mut col_kept = Vec::with_capacity(col.constraints.len());
                for con in std::mem::take(&mut col.constraints) {
                    match con.node {
                        Some(NodeEnum::Constraint(ref c)) if is_foreign_key(c) => {
                            let mut constraint = (**c).clone();
                            if constraint.fk_attrs.is_empty() {
                                constraint.fk_attrs = vec![string_node(&col.colname)];
                            }
                            let columns = string_list(&constraint.fk_attrs);
                            lifted.push(LiftedForeignKey {
                                constraint,
                                columns,
                            });
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

fn is_foreign_key(c: &Constraint) -> bool {
    c.contype == ConstrType::ConstrForeign as i32
}

/// Name a constraint's kind for a diagnostic, so the message says which
/// statement the author is looking at rather than making them work it out.
fn constraint_kind(c: &Constraint) -> &'static str {
    match c.contype {
        t if t == ConstrType::ConstrCheck as i32 => "CHECK",
        t if t == ConstrType::ConstrUnique as i32 => "UNIQUE",
        t if t == ConstrType::ConstrPrimary as i32 => "PRIMARY KEY",
        t if t == ConstrType::ConstrExclusion as i32 => "EXCLUDE",
        _ => "constraint",
    }
}

// ---------------------------------------------------------------------------
// Qualification helpers (spec §5.4)
// ---------------------------------------------------------------------------

fn qualify(relation: &RangeVar, default_schema: &SchemaName) -> QualifiedName {
    let schema = if relation.schemaname.is_empty() {
        default_schema.clone()
    } else {
        SchemaName::new(&relation.schemaname)
    };
    QualifiedName::new(schema, &relation.relname)
}

fn qualify_relation(relation: Option<&mut RangeVar>, default_schema: &SchemaName) {
    if let Some(relation) = relation
        && relation.schemaname.is_empty()
    {
        relation.schemaname = default_schema.as_str().to_owned();
    }
}

/// Read a comment's target as a list of identifier parts.
fn comment_object_parts(stmt: &CommentStmt) -> Option<Vec<String>> {
    match stmt.object.as_ref()?.node.as_ref()? {
        NodeEnum::List(list) => {
            let parts = string_list(&list.items);
            (parts.len() == list.items.len()).then_some(parts)
        }
        // `COMMENT ON TYPE` and `COMMENT ON DOMAIN` name their object with a
        // `TypeName` rather than a plain list, since either could name an
        // array or a parameterised type.
        NodeEnum::TypeName(type_name) => {
            let parts = string_list(&type_name.names);
            (parts.len() == type_name.names.len()).then_some(parts)
        }
        NodeEnum::String(s) => Some(vec![s.sval.clone()]),
        _ => None,
    }
}

/// Prepend the resolved schema to a comment's target if it lacks one.
fn qualify_comment_object(stmt: &mut CommentStmt, kind: CommentTarget, schema: &SchemaName) {
    let items = match stmt.object.as_mut().and_then(|o| o.node.as_mut()) {
        Some(NodeEnum::List(list)) => &mut list.items,
        Some(NodeEnum::TypeName(type_name)) => &mut type_name.names,
        _ => return,
    };
    if items.len() == kind.unqualified_len() {
        items.insert(0, string_node(schema.as_str()));
    }
}

fn string_list(nodes: &[Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.node.as_ref() {
            Some(NodeEnum::String(s)) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}

fn string_node(s: &str) -> Node {
    Node {
        node: Some(NodeEnum::String(pg_query::protobuf::String {
            sval: s.to_owned(),
        })),
    }
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
}

// ---------------------------------------------------------------------------
// Diagnostics for rejected statements
// ---------------------------------------------------------------------------

fn unsupported(node: &NodeEnum, origin: &Origin) -> Diagnostic {
    Diagnostic::new(
        DiagnosticKind::UnsupportedStatement,
        format!("unsupported statement: {}", statement_kind(node)),
        vec![origin.clone()],
    )
    .with_help(
        "pgpushy 0.1 manages tables, indexes, foreign keys, types, domains, sequences and \
         comments; see spec §4.3 for the full list and §14 for what may come later",
    )
}

fn unsupported_named(what: &str, origin: &Origin) -> Diagnostic {
    Diagnostic::new(
        DiagnosticKind::UnsupportedStatement,
        format!("unsupported statement: {what}"),
        vec![origin.clone()],
    )
}

/// A human-readable name for a statement kind, for diagnostics.
///
/// The common rejections are spelled the way an author wrote them; anything
/// else falls back to the node's own name, which is still better than nothing.
fn statement_kind(node: &NodeEnum) -> String {
    match node {
        NodeEnum::ViewStmt(_) => "CREATE VIEW".into(),
        NodeEnum::CreateFunctionStmt(_) => "CREATE FUNCTION".into(),
        NodeEnum::CreateTrigStmt(_) => "CREATE TRIGGER".into(),
        NodeEnum::CreateEnumStmt(_) | NodeEnum::CompositeTypeStmt(_) => "CREATE TYPE".into(),
        NodeEnum::CreateRangeStmt(_) => "CREATE TYPE ... AS RANGE".into(),
        NodeEnum::CreateDomainStmt(_) => "CREATE DOMAIN".into(),
        NodeEnum::CreatePolicyStmt(_) => "CREATE POLICY".into(),
        NodeEnum::CreateExtensionStmt(_) => "CREATE EXTENSION".into(),
        NodeEnum::CreateSeqStmt(_) => "CREATE SEQUENCE".into(),
        NodeEnum::GrantStmt(_) => "GRANT or REVOKE".into(),
        NodeEnum::GrantRoleStmt(_) => "GRANT or REVOKE ROLE".into(),
        NodeEnum::DropStmt(_) => "DROP".into(),
        NodeEnum::TruncateStmt(_) => "TRUNCATE".into(),
        NodeEnum::InsertStmt(_) => "INSERT".into(),
        NodeEnum::UpdateStmt(_) => "UPDATE".into(),
        NodeEnum::DeleteStmt(_) => "DELETE".into(),
        NodeEnum::SelectStmt(_) => "SELECT".into(),
        NodeEnum::VariableSetStmt(_) => "SET".into(),
        NodeEnum::TransactionStmt(_) => "a transaction control statement".into(),
        NodeEnum::RenameStmt(_) => "ALTER ... RENAME".into(),
        NodeEnum::AlterOwnerStmt(_) => "ALTER ... OWNER TO".into(),
        NodeEnum::AlterObjectSchemaStmt(_) => "ALTER ... SET SCHEMA".into(),
        NodeEnum::AlterSeqStmt(_) => "ALTER SEQUENCE".into(),
        NodeEnum::AlterDatabaseStmt(_) | NodeEnum::AlterDatabaseSetStmt(_) => {
            "ALTER DATABASE".into()
        }
        NodeEnum::DefineStmt(_) => "CREATE AGGREGATE, OPERATOR, or similar".into(),
        NodeEnum::RuleStmt(_) => "CREATE RULE".into(),
        NodeEnum::CreateTableAsStmt(_) => "CREATE TABLE AS or CREATE MATERIALIZED VIEW".into(),
        NodeEnum::RefreshMatViewStmt(_) => "REFRESH MATERIALIZED VIEW".into(),
        NodeEnum::DoStmt(_) => "DO".into(),
        NodeEnum::CallStmt(_) => "CALL".into(),
        other => {
            // Node names come through as `CamelCaseStmt`; the caller sees
            // something like "AlterFdwStmt", which is at least identifiable.
            let full = format!("{other:?}");
            full.split(['(', ' '])
                .next()
                .unwrap_or("an unsupported statement")
                .to_owned()
        }
    }
}

// ---------------------------------------------------------------------------
// Category 2: types, domains and standalone sequences (spec §4.3, §5.1)
// ---------------------------------------------------------------------------

/// `CREATE TYPE … AS ENUM` and `CREATE TYPE … AS RANGE`, whose name is a
/// dotted list rather than a `RangeVar`.
fn classify_named_type(
    names: &[Node],
    ast: NodeEnum,
    origin: &Origin,
    default_schema: &SchemaName,
    depends_on: Vec<QualifiedName>,
) -> Result<TypeLike, Diagnostic> {
    let name = qualify_name_list(names, default_schema)
        .ok_or_else(|| unsupported_named("CREATE TYPE without a usable name", origin))?;
    Ok(TypeLike {
        name,
        kind: TypeKind::Type,
        origin: origin.clone(),
        depends_on,
        ast,
    })
}

fn classify_domain(
    stmt: &pg_query::protobuf::CreateDomainStmt,
    origin: &Origin,
    default_schema: &SchemaName,
) -> Result<TypeLike, Diagnostic> {
    let mut ast = stmt.clone();
    let name = qualify_name_list(&ast.domainname, default_schema)
        .ok_or_else(|| unsupported_named("CREATE DOMAIN without a usable name", origin))?;
    ast.domainname = name_list_nodes(&name);

    Ok(TypeLike {
        name,
        kind: TypeKind::Domain,
        origin: origin.clone(),
        depends_on: Vec::new(),
        ast: NodeEnum::CreateDomainStmt(Box::new(ast)),
    })
}

fn classify_composite_type(
    stmt: &pg_query::protobuf::CompositeTypeStmt,
    origin: &Origin,
    default_schema: &SchemaName,
) -> Result<TypeLike, Diagnostic> {
    let mut ast = stmt.clone();
    let relation = ast
        .typevar
        .as_ref()
        .ok_or_else(|| unsupported_named("CREATE TYPE without a name", origin))?;
    let name = qualify(relation, default_schema);
    qualify_relation(ast.typevar.as_mut(), default_schema);

    Ok(TypeLike {
        name,
        kind: TypeKind::Type,
        origin: origin.clone(),
        depends_on: Vec::new(),
        ast: NodeEnum::CompositeTypeStmt(ast),
    })
}

fn classify_sequence(
    stmt: &pg_query::protobuf::CreateSeqStmt,
    origin: &Origin,
    default_schema: &SchemaName,
) -> Result<TypeLike, Diagnostic> {
    // Spec §4.3: `OWNED BY` makes a sequence depend on a table, inverting
    // category 2 and category 3 — and pgschema models a column-owned sequence
    // as `SERIAL` rather than an object of its own, so the shape does not
    // survive a dump and reapply.
    if stmt.options.iter().any(is_owned_by) {
        return Err(Diagnostic::new(
            DiagnosticKind::UnsupportedStatement,
            "CREATE SEQUENCE with OWNED BY",
            vec![origin.clone()],
        )
        .with_help(
            "a sequence owned by a column is part of that column: write the column as \
             `serial` or `GENERATED … AS IDENTITY` instead (spec §4.3)",
        ));
    }

    let mut ast = stmt.clone();
    let relation = ast
        .sequence
        .as_ref()
        .ok_or_else(|| unsupported_named("CREATE SEQUENCE without a name", origin))?;
    let name = qualify(relation, default_schema);
    qualify_relation(ast.sequence.as_mut(), default_schema);

    Ok(TypeLike {
        name,
        kind: TypeKind::Sequence,
        origin: origin.clone(),
        depends_on: Vec::new(),
        ast: NodeEnum::CreateSeqStmt(ast),
    })
}

fn is_owned_by(option: &Node) -> bool {
    matches!(option.node.as_ref(), Some(NodeEnum::DefElem(elem)) if elem.defname == "owned_by")
}

/// Resolve a dotted name list to a qualified name, and nothing longer.
fn qualify_name_list(names: &[Node], default_schema: &SchemaName) -> Option<QualifiedName> {
    let parts = string_list(names);
    if parts.len() != names.len() {
        return None;
    }
    match parts.as_slice() {
        [name] => Some(QualifiedName::new(default_schema.clone(), name)),
        [schema, name] => Some(QualifiedName::new(SchemaName::new(schema), name)),
        _ => None,
    }
}

fn name_list_nodes(name: &QualifiedName) -> Vec<Node> {
    vec![string_node(name.schema.as_str()), string_node(&name.name)]
}

/// Qualify an enum's name in place and hand back the statement.
fn requalify_enum(
    stmt: &pg_query::protobuf::CreateEnumStmt,
    origin: &Origin,
    default_schema: &SchemaName,
) -> pg_query::protobuf::CreateEnumStmt {
    let _ = origin;
    let mut ast = stmt.clone();
    if let Some(name) = qualify_name_list(&ast.type_name, default_schema) {
        ast.type_name = name_list_nodes(&name);
    }
    ast
}
