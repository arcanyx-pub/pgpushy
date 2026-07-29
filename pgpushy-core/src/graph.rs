//! Cross-schema ordering (spec §7).
//!
//! pgschema applies one schema per invocation, in its own transaction, and
//! cannot defer a foreign key across schemas. So when a table in `billing`
//! references a table in `public`, `public` has to be applied first — and
//! pgpushy has to work that out.
//!
//! Same-schema foreign keys create no edge: pgschema handles those itself by
//! deferring one constraint of a cycle, which is exactly why FK-lift can
//! express same-schema cycles that no topological sort could (spec §5.3).
//!
//! A cycle *between* schemas has no valid order at all. This module reports it
//! rather than deciding what to do about it: `apply` and `validate` treat it as
//! fatal, while `plan` shows the plans anyway, because those plans are what the
//! operator needs in order to break the cycle (spec §7). Returning the cycle as
//! data keeps that decision with the caller.

use crate::error::{Diagnostic, DiagnosticKind};
use crate::model::{ForeignKey, Objects, Origin, SchemaName};
use std::collections::{BTreeMap, BTreeSet};

/// The order to reconcile schemas in, and any cycles found.
#[derive(Clone, Debug)]
pub struct SchemaOrder {
    /// Every managed schema, dependencies before dependents.
    ///
    /// When cycles exist there is no such order; the schemas of each cycle are
    /// kept adjacent and sorted by name, so the sequence stays deterministic
    /// and usable for `plan`.
    pub order: Vec<SchemaName>,
    /// One entry per cycle. Empty in the normal case.
    pub cycles: Vec<Cycle>,
}

/// A set of managed schemas whose foreign keys reference each other.
#[derive(Clone, Debug)]
pub struct Cycle {
    /// The schemas involved, sorted.
    pub schemas: Vec<SchemaName>,
    /// The foreign keys that form it, for the diagnostic.
    pub edges: Vec<CycleEdge>,
}

#[derive(Clone, Debug)]
pub struct CycleEdge {
    pub from: SchemaName,
    pub to: SchemaName,
    /// How the constraint is described in the diagnostic.
    pub constraint: String,
    pub origin: Origin,
}

impl Cycle {
    /// Render as a user-facing diagnostic naming the schemas and the
    /// constraints that tie them together.
    pub fn to_diagnostic(&self) -> Diagnostic {
        let schemas = self
            .schemas
            .iter()
            .map(SchemaName::as_str)
            .collect::<Vec<_>>()
            .join(" \u{2194} ");
        let detail = self
            .edges
            .iter()
            .map(|e| format!("{} \u{2192} {}: {}", e.from, e.to, e.constraint))
            .collect::<Vec<_>>()
            .join("; ");

        Diagnostic::new(
            DiagnosticKind::CrossSchemaForeignKeyCycle,
            format!("cross-schema foreign key cycle: {schemas} ({detail})"),
            self.edges.iter().map(|e| e.origin.clone()).collect(),
        )
        .with_help(
            "no schema order can apply this: pgschema applies each schema in its own \
             transaction and cannot defer a foreign key across schemas; break the cycle by \
             moving one of these tables into the other's schema, or by removing one of the \
             foreign keys",
        )
    }
}

/// Order the managed schemas by their cross-schema foreign keys.
///
/// `managed` supplies the node set, so a schema with no cross-schema foreign
/// keys at all — the common case — still appears in the result.
pub fn order_schemas(objects: &Objects, managed: &[SchemaName]) -> SchemaOrder {
    let nodes: BTreeSet<SchemaName> = managed.iter().cloned().collect();

    // `deps[a]` holds the schemas `a` references. Foreign keys touching a
    // schema outside the managed set are skipped: validate.rs has already
    // rejected an unresolved referent, and a resolved one is by definition in
    // a managed schema.
    let mut deps: BTreeMap<SchemaName, BTreeSet<SchemaName>> = BTreeMap::new();
    let mut edge_keys: BTreeMap<(SchemaName, SchemaName), Vec<&ForeignKey>> = BTreeMap::new();

    for fk in &objects.foreign_keys {
        let (from, to) = (&fk.table.schema, &fk.referenced.schema);
        if from == to || !nodes.contains(from) || !nodes.contains(to) {
            continue;
        }
        deps.entry(from.clone()).or_default().insert(to.clone());
        edge_keys
            .entry((from.clone(), to.clone()))
            .or_default()
            .push(fk);
    }

    let components = strongly_connected_components(&nodes, &deps);

    let mut order = Vec::with_capacity(nodes.len());
    let mut cycles = Vec::new();

    for mut component in components {
        component.sort();
        if component.len() > 1 {
            cycles.push(build_cycle(&component, &edge_keys));
        }
        order.extend(component);
    }

    SchemaOrder { order, cycles }
}

fn build_cycle(
    schemas: &[SchemaName],
    edge_keys: &BTreeMap<(SchemaName, SchemaName), Vec<&ForeignKey>>,
) -> Cycle {
    let members: BTreeSet<&SchemaName> = schemas.iter().collect();
    let mut edges = Vec::new();

    for ((from, to), fks) in edge_keys {
        if !members.contains(from) || !members.contains(to) {
            continue;
        }
        // One constraint per schema pair is enough to explain the cycle;
        // listing every foreign key between two busy schemas would obscure it.
        if let Some(fk) = fks.first() {
            edges.push(CycleEdge {
                from: from.clone(),
                to: to.clone(),
                constraint: fk
                    .name
                    .as_deref()
                    .map(|n| format!("{n} on {}", fk.table))
                    .unwrap_or_else(|| format!("{}({})", fk.table, fk.columns.join(", "))),
                origin: fk.origin.clone(),
            });
        }
    }

    Cycle {
        schemas: schemas.to_vec(),
        edges,
    }
}

/// Tarjan's strongly-connected-components algorithm.
///
/// Chosen over a plain topological sort for two reasons: it identifies cycles
/// as complete groups rather than reporting "there is a cycle somewhere", and
/// it emits components already in the order we want. Tarjan closes a component
/// only after everything it can reach has been closed, so with edges meaning
/// "depends on", components come out dependencies-first — exactly spec §7's
/// reverse-dependency order, with no second pass.
///
/// Iteration over both the node set and each adjacency set is sorted, so the
/// output is deterministic (spec §11.3).
fn strongly_connected_components(
    nodes: &BTreeSet<SchemaName>,
    deps: &BTreeMap<SchemaName, BTreeSet<SchemaName>>,
) -> Vec<Vec<SchemaName>> {
    struct State<'a> {
        deps: &'a BTreeMap<SchemaName, BTreeSet<SchemaName>>,
        index: BTreeMap<SchemaName, usize>,
        low: BTreeMap<SchemaName, usize>,
        on_stack: BTreeSet<SchemaName>,
        stack: Vec<SchemaName>,
        next: usize,
        out: Vec<Vec<SchemaName>>,
    }

    fn visit(state: &mut State<'_>, node: &SchemaName) {
        state.index.insert(node.clone(), state.next);
        state.low.insert(node.clone(), state.next);
        state.next += 1;
        state.stack.push(node.clone());
        state.on_stack.insert(node.clone());

        if let Some(neighbors) = state.deps.get(node) {
            for neighbor in neighbors.clone() {
                if !state.index.contains_key(&neighbor) {
                    visit(state, &neighbor);
                    let low = state.low[&neighbor];
                    let entry = state.low.get_mut(node).expect("node was indexed");
                    *entry = (*entry).min(low);
                } else if state.on_stack.contains(&neighbor) {
                    let idx = state.index[&neighbor];
                    let entry = state.low.get_mut(node).expect("node was indexed");
                    *entry = (*entry).min(idx);
                }
            }
        }

        if state.low[node] == state.index[node] {
            let mut component = Vec::new();
            while let Some(member) = state.stack.pop() {
                state.on_stack.remove(&member);
                let done = member == *node;
                component.push(member);
                if done {
                    break;
                }
            }
            state.out.push(component);
        }
    }

    let mut state = State {
        deps,
        index: BTreeMap::new(),
        low: BTreeMap::new(),
        on_stack: BTreeSet::new(),
        stack: Vec::new(),
        next: 0,
        out: Vec::new(),
    };

    for node in nodes {
        if !state.index.contains_key(node) {
            visit(&mut state, node);
        }
    }

    state.out
}
