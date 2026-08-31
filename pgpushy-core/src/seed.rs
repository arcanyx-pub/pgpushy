//! Seed files (spec §4.6): parse, the seed allow-list, and the model checks.
//!
//! Seeds are not desired state. They never enter synthesis, never appear in
//! any document, and are never shown to pgschema; pgpushy itself executes
//! them against the target after apply (spec §8.8). This module is the
//! offline half: everything here must hold before a seed is worth carrying
//! to a database, and every violation is reported with a file, a line, and a
//! remedy.
//!
//! The rules exist to make two guarantees checkable:
//!
//! - **Idempotence by construction** (spec §11.1): every statement is an
//!   `INSERT … ON CONFLICT`, a `DO UPDATE` carries a `WHERE` guard, and the
//!   §8.8 probe can therefore demand a zero-affected second pass.
//! - **Seeds never delete** (spec §12.10): no `WITH` clause (a data-modifying
//!   CTE is a `DELETE` wearing an `INSERT`'s statement kind), no reading the
//!   database in a source query, and no user-defined functions, which can do
//!   arbitrary work.
//!
//! Expression scanning runs over the AST serialized to JSON, for the same
//! reason [`crate::literal`] does: `pg_query`'s own node walk does not
//! descend into every field, and a missed field here is a missed `DELETE`.

use crate::error::{Diagnostic, DiagnosticKind};
use crate::model::{Objects, Origin, QualifiedName, SchemaName, Table};
use crate::parse::{SourceFile, line_of};
use pg_query::NodeEnum;
use pg_query::protobuf::{
    ConstrType, InsertStmt, OnConflictAction, OnConflictClause, OverridingKind,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Everything the seed files contain, checked and ready to execute.
#[derive(Clone, Debug, Default)]
pub struct Seeds {
    /// One entry per discovered seed file, in discovery order.
    pub files: Vec<SeedFile>,
    /// `DO UPDATE` statements that share a table and conflict target across
    /// the tree. Each converges alone under the §8.8 per-file probe while
    /// together rewriting the same row on every apply, which the probe cannot
    /// see — so this is reported as a warning rather than enforced (spec
    /// §4.6, §12.11).
    pub do_update_collisions: Vec<DoUpdateCollision>,
}

impl Seeds {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn statement_count(&self) -> usize {
        self.files.iter().map(|file| file.statements.len()).sum()
    }
}

/// One seed file's statements, in file order.
#[derive(Clone, Debug)]
pub struct SeedFile {
    /// Path relative to the seed root, as discovery reported it.
    pub path: String,
    pub statements: Vec<SeedStatement>,
}

/// One checked statement, carried verbatim.
#[derive(Clone, Debug)]
pub struct SeedStatement {
    pub origin: Origin,
    /// The statement exactly as the author wrote it. Execution runs this
    /// text, not a re-rendering: what was reviewed is what runs (spec §8.8).
    pub sql: String,
    /// The inserted-into table, for reporting.
    pub table: QualifiedName,
}

/// Two or more `DO UPDATE` statements converging on one conflict target.
#[derive(Clone, Debug)]
pub struct DoUpdateCollision {
    pub table: QualifiedName,
    pub origins: Vec<Origin>,
}

/// Check every seed file against the allow-list and the model.
///
/// Like the rest of the pipeline, this collects *all* problems rather than
/// stopping at the first (impl-plan §12).
pub(crate) fn check(files: &[SourceFile], objects: &Objects) -> (Seeds, Vec<Diagnostic>) {
    let mut seeds = Seeds::default();
    let mut diagnostics = Vec::new();
    let mut do_updates: BTreeMap<(QualifiedName, String), Vec<Origin>> = BTreeMap::new();

    for file in files {
        let parsed = match pg_query::parse(&file.contents) {
            Ok(parsed) => parsed,
            Err(err) => {
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
                        "pgpushy parses with the real Postgres grammar; the file must be \
                         valid SQL",
                    ),
                );
                continue;
            }
        };

        let mut statements = Vec::new();
        for stmt in &parsed.protobuf.stmts {
            let origin = Origin {
                file: file.path.clone(),
                line: line_of(&file.contents, stmt.stmt_location),
            };
            let Some(node) = stmt.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
                continue;
            };

            let NodeEnum::InsertStmt(insert) = node else {
                diagnostics.push(disallowed(
                    format!(
                        "a seed statement must be an INSERT, not {}",
                        kind_name(node)
                    ),
                    &origin,
                    "a seed file holds idempotent baseline rows: INSERT … ON CONFLICT (…) \
                     DO NOTHING, or DO UPDATE with a WHERE guard (spec §4.6)",
                ));
                continue;
            };

            let before = diagnostics.len();
            let table = check_insert(insert, &origin, objects, &mut diagnostics, &mut do_updates);
            if diagnostics.len() != before {
                continue;
            }
            let Some(table) = table else { continue };

            statements.push(SeedStatement {
                origin,
                sql: slice_sql(&file.contents, stmt.stmt_location, stmt.stmt_len),
                table,
            });
        }

        seeds.files.push(SeedFile {
            path: file.path.clone(),
            statements,
        });
    }

    seeds.do_update_collisions = do_updates
        .into_iter()
        .filter(|(_, origins)| origins.len() > 1)
        .map(|((table, _), origins)| DoUpdateCollision { table, origins })
        .collect();

    (seeds, diagnostics)
}

/// The verbatim text of one statement, trimmed of surrounding trivia.
fn slice_sql(contents: &str, location: i32, len: i32) -> String {
    let start = usize::try_from(location).unwrap_or(0).min(contents.len());
    let end = if len > 0 {
        (start + usize::try_from(len).unwrap_or(0)).min(contents.len())
    } else {
        contents.len()
    };
    contents[start..end]
        .trim()
        .trim_end_matches(';')
        .trim_end()
        .to_owned()
}

/// Every rule spec §4.6 states about one `INSERT`.
///
/// Returns the inserted-into table when it resolved, so the caller can carry
/// it; pushes a diagnostic for every violation found.
fn check_insert(
    insert: &InsertStmt,
    origin: &Origin,
    objects: &Objects,
    diagnostics: &mut Vec<Diagnostic>,
    do_updates: &mut BTreeMap<(QualifiedName, String), Vec<Origin>>,
) -> Option<QualifiedName> {
    if insert.with_clause.is_some() {
        diagnostics.push(disallowed(
            "WITH is not allowed in a seed statement",
            origin,
            "a data-modifying CTE is a DELETE wearing an INSERT's statement kind, and \
             even a read-only one makes the seeded rows depend on target state \
             (spec §4.6)",
        ));
    }
    if !insert.returning_list.is_empty() {
        diagnostics.push(disallowed(
            "RETURNING is not allowed in a seed statement",
            origin,
            "nothing consumes a seed's result rows; remove the RETURNING clause \
             (spec §4.6)",
        ));
    }
    if insert.r#override == OverridingKind::OverridingUserValue as i32
        || insert.r#override == OverridingKind::OverridingSystemValue as i32
    {
        diagnostics.push(disallowed(
            "OVERRIDING is not allowed in a seed statement",
            origin,
            "a seed may not name a GENERATED ALWAYS column, so there is nothing for \
             OVERRIDING to override (spec §4.6)",
        ));
    }

    // The table, qualified and modeled (spec §4.6).
    let relation = insert.relation.as_ref()?;
    if relation.schemaname.is_empty() {
        diagnostics.push(disallowed(
            format!(
                "the seeded table {} is not schema-qualified",
                relation.relname
            ),
            origin,
            format!(
                "seed statements execute under an empty search_path (spec §8.8); write \
                 the table schema-qualified, e.g. myschema.{}",
                relation.relname
            ),
        ));
        return None;
    }
    let name = QualifiedName::new(
        SchemaName::new(relation.schemaname.clone()),
        relation.relname.clone(),
    );
    let Some(table) = objects.tables.iter().find(|t| t.name == name) else {
        diagnostics.push(
            Diagnostic::new(
                DiagnosticKind::SeedTargetMismatch,
                format!("seed inserts into {name}, which the source tree does not describe"),
                vec![origin.clone()],
            )
            .with_help(
                "a seed may only insert into a table the desired state defines, in a \
             managed schema — writing rows into shape pgpushy cannot see is what \
             spec §4.6 rejects",
            ),
        );
        return None;
    };
    let shape = Shape::of(table, objects);

    // The explicit column list (spec §4.6).
    if insert.cols.is_empty() {
        diagnostics.push(disallowed(
            format!("the INSERT into {name} has no column list"),
            origin,
            "an INSERT without a column list binds values to positions and breaks \
             silently when the table gains a column; name the columns (spec §4.6)",
        ));
    }
    for col in &insert.cols {
        let Some(NodeEnum::ResTarget(target)) = col.node.as_ref() else {
            continue;
        };
        match shape.columns.get(target.name.as_str()) {
            None => diagnostics.push(Diagnostic::new(
                DiagnosticKind::SeedTargetMismatch,
                format!("{name} has no column {}", target.name),
                vec![origin.clone()],
            )),
            Some(Generated::Always) => diagnostics.push(
                Diagnostic::new(
                    DiagnosticKind::SeedTargetMismatch,
                    format!("column {} of {name} is GENERATED ALWAYS", target.name),
                    vec![origin.clone()],
                )
                .with_help(
                    "a generated column cannot be seeded; leave it out of the column list \
                 and let the table produce it (spec §4.6)",
                ),
            ),
            Some(Generated::No) => {}
        }
    }

    // ON CONFLICT (spec §4.6).
    match insert.on_conflict_clause.as_deref() {
        None => diagnostics.push(disallowed(
            format!("the INSERT into {name} has no ON CONFLICT clause"),
            origin,
            "a bare INSERT is a duplicate row or an error on the second apply; add \
             ON CONFLICT (…) DO NOTHING, or DO UPDATE with a WHERE guard (spec §4.6)",
        )),
        Some(clause) => {
            check_on_conflict(clause, &name, &shape, origin, diagnostics);
            if clause.action == OnConflictAction::OnconflictUpdate as i32 {
                let signature = conflict_signature(clause);
                do_updates
                    .entry((name.clone(), signature))
                    .or_default()
                    .push(origin.clone());
            }
        }
    }

    // What the statement's expressions may reach (spec §4.6): no table or
    // view anywhere, only built-in functions and operators, and no bare name
    // for a type the tree defines.
    if let Ok(value) = serde_json::to_value(insert) {
        scan_expressions(&value, &name, objects, origin, diagnostics);
    }

    Some(name)
}

fn check_on_conflict(
    clause: &OnConflictClause,
    table: &QualifiedName,
    shape: &Shape,
    origin: &Origin,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let update = clause.action == OnConflictAction::OnconflictUpdate as i32;
    if !update && clause.action != OnConflictAction::OnconflictNothing as i32 {
        diagnostics.push(disallowed(
            format!("the INSERT into {table} has no usable ON CONFLICT action"),
            origin,
            "write ON CONFLICT (…) DO NOTHING, or DO UPDATE with a WHERE guard \
             (spec §4.6)",
        ));
        return;
    }

    // The guard (spec §4.6): whenever the statement seeds a row, an unguarded
    // DO UPDATE re-updates every one of them on the probe pass, so it cannot
    // converge to zero affected rows.
    if update && clause.where_clause.is_none() {
        diagnostics.push(disallowed(
            format!("the DO UPDATE on {table} has no WHERE guard"),
            origin,
            "without a guard every apply rewrites every seeded row and the §8.8 probe \
             must refuse it; add WHERE <table>.<col> IS DISTINCT FROM excluded.<col> \
             (spec §4.6)",
        ));
    }

    let Some(infer) = clause.infer.as_deref() else {
        // Postgres itself requires a conflict target for DO UPDATE; a
        // targetless DO NOTHING arbitrates on any conflict, which converges
        // trivially (spec §4.6).
        if update {
            diagnostics.push(disallowed(
                format!("the DO UPDATE on {table} names no conflict target"),
                origin,
                "DO UPDATE needs ON CONFLICT (…) or ON CONFLICT ON CONSTRAINT … \
                 (spec §4.6)",
            ));
        }
        return;
    };

    if !infer.conname.is_empty() {
        if !shape.named_unique.contains(infer.conname.as_str()) {
            diagnostics.push(Diagnostic::new(
                DiagnosticKind::SeedTargetMismatch,
                format!(
                    "ON CONSTRAINT {} names no PRIMARY KEY or UNIQUE constraint on {table}",
                    infer.conname
                ),
                vec![origin.clone()],
            ));
        }
        return;
    }

    if infer.where_clause.is_some() {
        diagnostics.push(disallowed(
            format!("the conflict target on {table} carries a partial-index WHERE"),
            origin,
            "validate cannot check a partial arbiter against the model; name the \
             constraint with ON CONFLICT ON CONSTRAINT … instead (spec §4.6)",
        ));
        return;
    }

    let mut target = BTreeSet::new();
    for elem in &infer.index_elems {
        let Some(NodeEnum::IndexElem(elem)) = elem.node.as_ref() else {
            continue;
        };
        if elem.name.is_empty() || elem.expr.is_some() {
            diagnostics.push(disallowed(
                format!("the conflict target on {table} is an expression"),
                origin,
                "validate cannot check an expression arbiter against the model; name \
                 the constraint with ON CONFLICT ON CONSTRAINT … instead (spec §4.6)",
            ));
            return;
        }
        target.insert(elem.name.clone());
    }

    // Order carries no meaning: the target must equal a unique column set as
    // a set (spec §4.6).
    if !shape.unique_sets.contains(&target) {
        let cols: Vec<_> = target.iter().cloned().collect();
        diagnostics.push(Diagnostic::new(
            DiagnosticKind::SeedTargetMismatch,
            format!(
                "the conflict target ({}) matches no primary key, unique constraint \
                 or unique index on {table}",
                cols.join(", ")
            ),
            vec![origin.clone()],
        ));
    }
}

/// A stable signature for one `DO UPDATE`'s conflict target, for collision
/// grouping across files.
fn conflict_signature(clause: &OnConflictClause) -> String {
    let Some(infer) = clause.infer.as_deref() else {
        return String::new();
    };
    if !infer.conname.is_empty() {
        return format!("constraint:{}", infer.conname);
    }
    let mut cols = BTreeSet::new();
    for elem in &infer.index_elems {
        if let Some(NodeEnum::IndexElem(elem)) = elem.node.as_ref() {
            cols.insert(elem.name.clone());
        }
    }
    cols.into_iter().collect::<Vec<_>>().join(",")
}

/// Walk the statement's JSON for what its expressions reach (spec §4.6).
///
/// The typed `relation` field — the insert target — serializes as a bare
/// struct rather than under a `RangeVar` key, so the walk finding a
/// `RangeVar` key means a table reference in the *source*, exactly what the
/// rule forbids.
fn scan_expressions(
    value: &Value,
    table: &QualifiedName,
    objects: &Objects,
    origin: &Origin,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match key.as_str() {
                    "RangeVar" => {
                        let named = child
                            .get("relname")
                            .and_then(Value::as_str)
                            .unwrap_or("a table");
                        diagnostics.push(disallowed(
                            format!("the seed for {table} reads {named}"),
                            origin,
                            "a seed's source may not read the database: use VALUES, or a \
                             SELECT over set-returning built-ins like generate_series \
                             (spec §4.6)",
                        ));
                    }
                    "FuncCall" => {
                        if let Some(schema) = qualified_outside_pg_catalog(child.get("funcname")) {
                            diagnostics.push(disallowed(
                                format!("the seed for {table} calls a function in schema {schema}"),
                                origin,
                                "a seed expression may call only built-in functions — a \
                                 user-defined one can do arbitrary work, including deletes \
                                 (spec §4.6)",
                            ));
                        }
                    }
                    "AExpr" => {
                        if let Some(schema) = qualified_outside_pg_catalog(child.get("name")) {
                            diagnostics.push(disallowed(
                                format!("the seed for {table} uses an operator in schema {schema}"),
                                origin,
                                "a seed expression may use only built-in operators — a \
                                 user-defined one calls a user-defined function \
                                 (spec §4.6)",
                            ));
                        }
                    }
                    "TypeCast" => {
                        if let Some(bare) = bare_tree_type(child, objects) {
                            diagnostics.push(disallowed(
                                format!(
                                    "the cast to {bare} in the seed for {table} is not \
                                         schema-qualified"
                                ),
                                origin,
                                format!(
                                    "an unqualified type cannot resolve under §8.8's empty \
                                     search_path; write the schema, as in ::{}",
                                    bare_qualified(&bare, objects)
                                ),
                            ));
                        }
                    }
                    "with_clause" if !child.is_null() => {
                        diagnostics.push(disallowed(
                            format!("WITH is not allowed in the seed for {table}"),
                            origin,
                            "a data-modifying CTE is a DELETE wearing an INSERT's statement \
                             kind (spec §4.6)",
                        ));
                    }
                    _ => {}
                }
                scan_expressions(child, table, objects, origin, diagnostics);
            }
        }
        Value::Array(items) => {
            for item in items {
                scan_expressions(item, table, objects, origin, diagnostics);
            }
        }
        _ => {}
    }
}

/// The schema of a dotted name list, when it is qualified and not pg_catalog.
fn qualified_outside_pg_catalog(names: Option<&Value>) -> Option<String> {
    let names = names?.as_array()?;
    if names.len() < 2 {
        return None;
    }
    let first = names
        .first()?
        .get("node")?
        .get("String")?
        .get("sval")?
        .as_str()?;
    (first != "pg_catalog").then(|| first.to_owned())
}

/// A bare (single-part) cast type name that the tree itself defines.
///
/// pgpushy cannot tell every user-defined type from a built-in without a
/// catalog, but a type the source tree defines is known — and a bare
/// reference to one cannot resolve at apply, so it is caught here rather
/// than in the seed's transaction.
fn bare_tree_type(cast: &Value, objects: &Objects) -> Option<String> {
    let names = cast.get("type_name")?.get("names")?.as_array()?;
    if names.len() != 1 {
        return None;
    }
    let name = names
        .first()?
        .get("node")?
        .get("String")?
        .get("sval")?
        .as_str()?;
    objects
        .types
        .iter()
        .any(|t| t.name.name == name)
        .then(|| name.to_owned())
}

/// The qualified spelling to suggest for a bare tree-defined type.
fn bare_qualified(name: &str, objects: &Objects) -> String {
    objects
        .types
        .iter()
        .find(|t| t.name.name == name)
        .map(|t| t.name.to_string())
        .unwrap_or_else(|| name.to_owned())
}

/// Whether a column is generated `ALWAYS`, which a seed may not name.
#[derive(Clone, Copy, PartialEq)]
enum Generated {
    No,
    Always,
}

/// What the model knows about one table, distilled for the seed checks.
struct Shape {
    columns: BTreeMap<String, Generated>,
    /// Every column set a conflict target may name: the primary key, each
    /// UNIQUE constraint, and each unique non-partial plain-column index.
    unique_sets: Vec<BTreeSet<String>>,
    /// Names ON CONFLICT ON CONSTRAINT may use.
    named_unique: BTreeSet<String>,
}

impl Shape {
    fn of(table: &Table, objects: &Objects) -> Shape {
        let mut columns = BTreeMap::new();
        let mut unique_sets = Vec::new();
        let mut named_unique = BTreeSet::new();

        for elt in &table.ast.table_elts {
            match elt.node.as_ref() {
                Some(NodeEnum::ColumnDef(col)) => {
                    let mut generated = Generated::No;
                    for c in &col.constraints {
                        let Some(NodeEnum::Constraint(c)) = c.node.as_ref() else {
                            continue;
                        };
                        let identity_always = c.contype == ConstrType::ConstrIdentity as i32
                            && c.generated_when == "a";
                        if identity_always || c.contype == ConstrType::ConstrGenerated as i32 {
                            generated = Generated::Always;
                        }
                        if c.contype == ConstrType::ConstrPrimary as i32
                            || c.contype == ConstrType::ConstrUnique as i32
                        {
                            unique_sets.push(BTreeSet::from([col.colname.clone()]));
                            if !c.conname.is_empty() {
                                named_unique.insert(c.conname.clone());
                            }
                        }
                    }
                    columns.insert(col.colname.clone(), generated);
                }
                Some(NodeEnum::Constraint(c))
                    if c.contype == ConstrType::ConstrPrimary as i32
                        || c.contype == ConstrType::ConstrUnique as i32 =>
                {
                    let keys: BTreeSet<String> = c
                        .keys
                        .iter()
                        .filter_map(|k| match k.node.as_ref() {
                            Some(NodeEnum::String(s)) => Some(s.sval.clone()),
                            _ => None,
                        })
                        .collect();
                    if !keys.is_empty() {
                        unique_sets.push(keys);
                    }
                    if !c.conname.is_empty() {
                        named_unique.insert(c.conname.clone());
                    }
                }
                _ => {}
            }
        }

        // Unique plain-column indexes arbitrate too. A partial or expression
        // index is skipped: it cannot be matched by a plain column target.
        for index in objects.indexes.iter().filter(|i| i.table == table.name) {
            if !index.ast.unique || index.ast.where_clause.is_some() {
                continue;
            }
            let mut keys = BTreeSet::new();
            let mut plain = true;
            for param in &index.ast.index_params {
                match param.node.as_ref() {
                    Some(NodeEnum::IndexElem(elem)) if !elem.name.is_empty() => {
                        keys.insert(elem.name.clone());
                    }
                    _ => plain = false,
                }
            }
            if plain && !keys.is_empty() {
                unique_sets.push(keys);
            }
        }

        Shape {
            columns,
            unique_sets,
            named_unique,
        }
    }
}

fn disallowed(message: impl Into<String>, origin: &Origin, help: impl Into<String>) -> Diagnostic {
    Diagnostic::new(
        DiagnosticKind::SeedDisallowedStatement,
        message,
        vec![origin.clone()],
    )
    .with_help(help)
}

/// A readable name for a rejected statement's kind.
fn kind_name(node: &NodeEnum) -> String {
    let variant = serde_json::to_value(node)
        .ok()
        .and_then(|v| v.as_object()?.keys().next().cloned())
        .unwrap_or_else(|| "an unknown statement".to_owned());
    match variant.as_str() {
        "DeleteStmt" => "DELETE".into(),
        "UpdateStmt" => "UPDATE".into(),
        "SelectStmt" => "SELECT".into(),
        "CopyStmt" => "COPY".into(),
        "TruncateStmt" => "TRUNCATE".into(),
        "MergeStmt" => "MERGE".into(),
        "VariableSetStmt" => "SET".into(),
        "TransactionStmt" => "a transaction command".into(),
        "CreateStmt" => "CREATE TABLE".into(),
        other => other.to_owned(),
    }
}
