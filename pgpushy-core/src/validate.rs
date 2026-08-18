//! Source-tree validity checks (spec §4.5, §12.4).
//!
//! Each check here turns something that would otherwise fail late and
//! obscurely — a `relation already exists` from a subprocess, a plan that
//! churns forever — into a diagnostic naming the source lines responsible.
//! That is spec §G5: pgpushy must not let a condition it can detect surface as
//! an opaque error from something else.
//!
//! All checks run; the caller gets every problem at once.

use crate::error::{Diagnostic, DiagnosticKind};
use crate::model::{Objects, Origin, QualifiedName, SchemaName};
use std::collections::BTreeMap;

/// Run every source-tree check, collecting all diagnostics.
pub fn check(objects: &Objects, managed: &[SchemaName]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    duplicate_objects(objects, &mut diagnostics);
    unresolved_references(objects, managed, &mut diagnostics);
    colliding_unnamed_foreign_keys(objects, &mut diagnostics);
    diagnostics
}

/// The same object defined twice (spec §4.5).
///
/// Easy to reach by accident, precisely because directory layout is free: a
/// copy-paste, a half-finished refactor, an editor backup saved as `.sql`.
/// Both definitions are named, since knowing only one is barely useful.
fn duplicate_objects(objects: &Objects, diagnostics: &mut Vec<Diagnostic>) {
    let mut seen: BTreeMap<String, Vec<Origin>> = BTreeMap::new();

    for table in &objects.tables {
        seen.entry(format!("table {}", table.name))
            .or_default()
            .push(table.origin.clone());
    }
    for index in &objects.indexes {
        seen.entry(format!("index {}", index.name))
            .or_default()
            .push(index.origin.clone());
    }
    for schema in &objects.schemas {
        seen.entry(format!("schema {}", schema.name))
            .or_default()
            .push(schema.origin.clone());
    }
    // Constraints share a namespace per table, so an explicitly named
    // constraint collides with another of the same name on the same table
    // whatever its kind. Unnamed ones cannot collide by name — spec §12.4
    // handles the one way they can still conflict.
    for fk in &objects.foreign_keys {
        if let Some(name) = &fk.name {
            seen.entry(format!("constraint {name} on {}", fk.table))
                .or_default()
                .push(fk.origin.clone());
        }
    }

    for (what, mut origins) in seen {
        if origins.len() < 2 {
            continue;
        }
        origins.sort();
        diagnostics.push(
            Diagnostic::new(
                DiagnosticKind::DuplicateObject,
                format!("{what} is defined {} times", origins.len()),
                origins,
            )
            .with_help(
                "every object must be defined exactly once in the source tree; \
                 remove the duplicate, or exclude the file if it is not desired state",
            ),
        );
    }
}

/// Foreign keys pointing at tables the source tree never defines (spec §4.5).
///
/// pgschema builds desired state from the synthesized document alone and never
/// from the target, so a referent missing from the tree fails when the document
/// executes — even if the table exists on the target. The most common cause is
/// a foreign key into a schema pgpushy does not manage, which is future work
/// (spec §14) rather than a supported configuration, so the diagnostic says so.
fn unresolved_references(
    objects: &Objects,
    managed: &[SchemaName],
    diagnostics: &mut Vec<Diagnostic>,
) {
    let defined: std::collections::BTreeSet<&QualifiedName> =
        objects.tables.iter().map(|t| &t.name).collect();

    for fk in &objects.foreign_keys {
        if defined.contains(&fk.referenced) {
            continue;
        }

        let constraint = fk
            .name
            .as_deref()
            .map(|n| format!("foreign key {n}"))
            .unwrap_or_else(|| format!("foreign key on {}({})", fk.table, fk.columns.join(", ")));

        let help = if managed.contains(&fk.referenced.schema) {
            format!(
                "{} is in a managed schema but no source file defines it",
                fk.referenced
            )
        } else {
            format!(
                "{} is in schema {}, which the source tree does not describe; pgpushy 0.x \
                 cannot reference tables outside the schemas it manages (spec §14)",
                fk.referenced, fk.referenced.schema
            )
        };

        diagnostics.push(
            Diagnostic::new(
                DiagnosticKind::UnresolvedReference,
                format!(
                    "{constraint} references {}, which is not defined",
                    fk.referenced
                ),
                vec![fk.origin.clone()],
            )
            .with_help(help),
        );
    }
}

/// Two unnamed foreign keys competing for one generated name (spec §12.4).
///
/// Verified against Postgres 18: when two constraints would take the same
/// generated name, the numeric suffix is assigned in *creation* order. Since
/// pgpushy's emission order comes from source content and need not match the
/// order the target's constraints were created in, the two names can attach to
/// opposite constraints — which pgschema reads as two renames, on every run,
/// forever.
///
/// The check is exact, because the failure is: it fires only when both
/// constraints are unnamed *and* cover the identical column list on the same
/// table. Naming either one resolves it completely, which is what the help
/// says.
fn colliding_unnamed_foreign_keys(objects: &Objects, diagnostics: &mut Vec<Diagnostic>) {
    let mut by_shape: BTreeMap<(QualifiedName, Vec<String>), Vec<Origin>> = BTreeMap::new();

    for fk in &objects.foreign_keys {
        if fk.name.is_some() {
            continue;
        }
        by_shape
            .entry((fk.table.clone(), fk.columns.clone()))
            .or_default()
            .push(fk.origin.clone());
    }

    for ((table, columns), mut origins) in by_shape {
        if origins.len() < 2 {
            continue;
        }
        origins.sort();
        diagnostics.push(
            Diagnostic::new(
                DiagnosticKind::CollidingUnnamedForeignKeys,
                format!(
                    "{} unnamed foreign keys on {table}({}) would compete for one generated name",
                    origins.len(),
                    columns.join(", "),
                ),
                origins,
            )
            .with_help(
                "Postgres assigns the numeric suffix in creation order, so these names could \
                 attach to opposite constraints and churn the plan on every run; give each \
                 constraint an explicit name",
            ),
        );
    }
}
