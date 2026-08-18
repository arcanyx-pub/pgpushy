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
use crate::model::{Objects, Origin, SchemaName};
use std::collections::BTreeMap;

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
