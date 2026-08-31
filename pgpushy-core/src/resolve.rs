//! The managed-schema set (spec §4.4).
//!
//! Which schemas pgpushy reconciles is the most consequential thing it
//! decides without being told, because reconciling a schema means being
//! willing to drop what the source tree does not describe. Two rules follow
//! from that, and they point in opposite directions:
//!
//! - A schema the tree never mentions is **never** managed. Its desired state
//!   would be empty, and an empty desired state plans a drop of everything.
//! - A schema the tree mentions **is** managed by default — which is
//!   convenient, and is also how one stray file quietly enlists a whole
//!   schema. `managed_schemas` in configuration exists to close that.

use crate::error::{Diagnostic, DiagnosticKind};
use crate::model::{Objects, Origin, QualifiedName, SchemaName};
use pg_query::NodeEnum;
use pg_query::protobuf::Node;
use std::collections::{BTreeMap, BTreeSet};

/// Determine the managed-schema set.
///
/// With no declaration, the set is every schema the tree mentions. With a
/// declaration, the declaration is authoritative: a mentioned schema it omits
/// is an error naming what enlisted it, and a schema it lists that the tree
/// never mentions is managed with an empty desired state — the only way to
/// say "reconcile this schema to empty", and destructive on purpose.
pub fn managed_schemas(
    objects: &Objects,
    declared: Option<&[SchemaName]>,
) -> Result<Vec<SchemaName>, Vec<Diagnostic>> {
    let mentioned = objects.mentioned_schemas();

    let Some(declared) = declared else {
        return Ok(mentioned);
    };

    let enlisted_by = enlisting_origins(objects);
    let mut diagnostics = Vec::new();

    for schema in &mentioned {
        if declared.contains(schema) {
            continue;
        }
        let origins = enlisted_by.get(schema).cloned().unwrap_or_default();
        diagnostics.push(
            Diagnostic::new(
                DiagnosticKind::SchemaNotManaged,
                format!("schema {schema} is used by the source tree but not in managed_schemas"),
                origins,
            )
            .with_help(format!(
                "add {schema} to managed_schemas, or move these objects into a managed schema; \
                 managed_schemas currently lists: {}",
                render_list(declared),
            )),
        );
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let mut managed = declared.to_vec();
    managed.sort();
    managed.dedup();
    Ok(managed)
}

/// The first place each schema was mentioned, for the "what enlisted this"
/// half of the diagnostic.
///
/// Only the earliest mention per schema is kept: listing forty files that all
/// use `analytics` would bury the point. Ordering is by file then line, and
/// discovery is already deterministic (spec §4.1), so "first" is stable.
fn enlisting_origins(objects: &Objects) -> BTreeMap<SchemaName, Vec<Origin>> {
    let mut all: Vec<(SchemaName, Origin)> = Vec::new();

    all.extend(
        objects
            .schemas
            .iter()
            .map(|s| (s.name.clone(), s.origin.clone())),
    );
    all.extend(
        objects
            .tables
            .iter()
            .map(|t| (t.name.schema.clone(), t.origin.clone())),
    );
    all.extend(
        objects
            .indexes
            .iter()
            .map(|i| (i.table.schema.clone(), i.origin.clone())),
    );
    all.extend(
        objects
            .foreign_keys
            .iter()
            .map(|f| (f.table.schema.clone(), f.origin.clone())),
    );
    all.extend(
        objects
            .comments
            .iter()
            .map(|c| (c.schema.clone(), c.origin.clone())),
    );

    let mut first: BTreeMap<SchemaName, Origin> = BTreeMap::new();
    for (schema, origin) in all {
        first
            .entry(schema)
            .and_modify(|existing| {
                if origin < *existing {
                    *existing = origin.clone();
                }
            })
            .or_insert(origin);
    }

    first
        .into_iter()
        .map(|(schema, origin)| (schema, vec![origin]))
        .collect()
}

fn render_list(schemas: &[SchemaName]) -> String {
    if schemas.is_empty() {
        return "(nothing)".to_owned();
    }
    schemas
        .iter()
        .map(SchemaName::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve which type names actually refer to something the source tree
/// defines, qualify those, and record them as dependencies.
///
/// This cannot happen while parsing, for two reasons. A type may be defined in
/// a file that has not been read yet, and — more sharply — libpg_query only
/// writes `pg_catalog` in front of the built-ins that have an alias spelling.
/// `int` arrives as `pg_catalog.int4`, but `text` arrives as plain `text`, so
/// "not `pg_catalog`" does not mean "user-defined". Treating it that way
/// rewrites `text` to `public.text` in the emitted SQL, which is a type that
/// does not exist.
///
/// Matching against what the tree defines is the only test that cannot make
/// that mistake. A name that matches nothing is left exactly as written: it
/// belongs to an extension or to the target, and spec §14 covers it.
pub fn type_references(objects: &mut Objects, default_schema: &SchemaName) {
    let defined: BTreeSet<QualifiedName> =
        objects.types.iter().map(|kind| kind.name.clone()).collect();
    // A literal can name a table or an index too — `'s.t'::regclass` — so
    // the is-it-defined question for literals ranges over every namespace
    // the tree populates.
    let relations: BTreeSet<QualifiedName> = objects
        .tables
        .iter()
        .map(|table| table.name.clone())
        .chain(objects.indexes.iter().map(|index| index.name.clone()))
        .collect();

    for index in 0..objects.tables.len() {
        let schema = objects.tables[index].name.schema.clone();
        let mut found = Vec::new();
        let mut unresolved = Vec::new();
        resolve_columns(
            &mut objects.tables[index].ast.table_elts,
            &schema,
            default_schema,
            &defined,
            &mut found,
            &mut unresolved,
        );
        resolve_literals(
            &NodeEnum::CreateStmt(objects.tables[index].ast.clone()),
            &defined,
            &relations,
            &mut found,
            &mut unresolved,
        );
        objects.tables[index].depends_on = found;
        objects.tables[index].unresolved = unresolved;
    }

    for index in 0..objects.types.len() {
        let schema = objects.types[index].name.schema.clone();
        let mut found = Vec::new();
        let mut unresolved = Vec::new();
        match &mut objects.types[index].ast {
            NodeEnum::CompositeTypeStmt(composite) => resolve_columns(
                &mut composite.coldeflist,
                &schema,
                default_schema,
                &defined,
                &mut found,
                &mut unresolved,
            ),
            NodeEnum::CreateDomainStmt(domain) => {
                if let Some(type_name) = domain.type_name.as_mut() {
                    resolve_type_name(
                        type_name,
                        &schema,
                        default_schema,
                        &defined,
                        &mut found,
                        &mut unresolved,
                    );
                }
            }
            _ => {}
        }
        resolve_literals(
            &objects.types[index].ast.clone(),
            &defined,
            &relations,
            &mut found,
            &mut unresolved,
        );
        objects.types[index].depends_on = found;
        objects.types[index].unresolved = unresolved;
    }
}

/// Record the objects a statement names inside a string literal.
///
/// `DEFAULT nextval('public.ticket_no')` is a creation-time dependency exactly
/// as a column's type is: the sequence has to exist before the table can be
/// created. It reaches the closure and the cross-schema check the same way,
/// which is why it is collected here rather than handled separately.
///
/// The literal is already qualified — spec §4.3 rejects a bare one — so there
/// is nothing to resolve against a search path, only to look up.
fn resolve_literals(
    node: &NodeEnum,
    defined: &BTreeSet<QualifiedName>,
    relations: &BTreeSet<QualifiedName>,
    found: &mut Vec<QualifiedName>,
    unresolved: &mut Vec<QualifiedName>,
) {
    for name in crate::literal::find(node) {
        let Some(parts) = crate::literal::name_parts(&name.raw) else {
            continue;
        };
        let [schema, object] = parts.as_slice() else {
            continue;
        };
        let referenced = QualifiedName::new(SchemaName::new(schema), object);
        if defined.contains(&referenced) {
            found.push(referenced);
        } else if !relations.contains(&referenced) {
            // A literal is always qualified (spec §4.3), so a name that is
            // neither a tree-defined type nor a tree-defined relation is
            // worth recording: validate reports it when the schema is
            // managed. A relation hit is simply not a category-2 dependency.
            unresolved.push(referenced);
        }
    }
}

fn resolve_columns(
    columns: &mut [Node],
    owner: &SchemaName,
    default_schema: &SchemaName,
    defined: &BTreeSet<QualifiedName>,
    found: &mut Vec<QualifiedName>,
    unresolved: &mut Vec<QualifiedName>,
) {
    for column in columns {
        if let Some(NodeEnum::ColumnDef(column)) = column.node.as_mut()
            && let Some(type_name) = column.type_name.as_mut()
        {
            resolve_type_name(type_name, owner, default_schema, defined, found, unresolved);
        }
    }
}

/// Qualify one type reference, if it names something the tree defines.
///
/// An unqualified name is tried against the owning object's schema first and
/// the default schema second, which is how Postgres itself would resolve it
/// under an ordinary `search_path`. A qualified one is taken as written —
/// including when it points at another schema, which [`crate::validate`]
/// rejects rather than silently accepting (spec §12.6).
fn resolve_type_name(
    type_name: &mut pg_query::protobuf::TypeName,
    owner: &SchemaName,
    default_schema: &SchemaName,
    defined: &BTreeSet<QualifiedName>,
    found: &mut Vec<QualifiedName>,
    unresolved: &mut Vec<QualifiedName>,
) {
    let parts: Vec<String> = type_name
        .names
        .iter()
        .filter_map(|node| match node.node.as_ref() {
            Some(NodeEnum::String(s)) => Some(s.sval.clone()),
            _ => None,
        })
        .collect();
    if parts.len() != type_name.names.len() {
        return;
    }

    let candidates = match parts.as_slice() {
        [name] => vec![
            QualifiedName::new(owner.clone(), name),
            QualifiedName::new(default_schema.clone(), name),
        ],
        [schema, name] if schema != "pg_catalog" => {
            vec![QualifiedName::new(SchemaName::new(schema), name)]
        }
        _ => return,
    };

    let qualified = parts.len() == 2;
    let Some(resolved) = candidates
        .clone()
        .into_iter()
        .find(|name| defined.contains(name))
    else {
        // A qualified miss is recorded rather than dropped: an unqualified
        // name that matches nothing may be `text` or an extension's type
        // (see the module note), but a qualified one names a schema — and
        // when that schema is managed, the reference cannot exist in the
        // plan database, which validate reports (spec §4.5).
        if qualified && let Some(missed) = candidates.into_iter().next() {
            unresolved.push(missed);
        }
        return;
    };
    type_name.names = vec![
        string_node(resolved.schema.as_str()),
        string_node(&resolved.name),
    ];
    found.push(resolved);
}

fn string_node(s: &str) -> Node {
    Node {
        node: Some(NodeEnum::String(pg_query::protobuf::String {
            sval: s.to_owned(),
        })),
    }
}
