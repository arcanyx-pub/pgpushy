//! Reading the plans pgschema produces (`--output-json`).
//!
//! pgpushy reads these for two things it cannot get any other way: the
//! approval summary (§8.6 — how many changes, and which are destructive) and
//! the cross-schema removal check (§6.2 — which columns and constraints a plan
//! will drop).
//!
//! This is not diffing, and G3 still holds. pgschema decided every one of
//! these steps; pgpushy is reading its conclusion, not recomputing it. Nothing
//! here inspects SQL or compares schema state — only `operation` and `path`,
//! which pgschema states outright.
//!
//! The struct is deliberately partial. pgschema's plan format carries more
//! than this (fingerprints, transaction grouping, the generated SQL), and
//! `serde` ignores what is not named, so a future field cannot break pgpushy.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// One schema's plan, as pgschema wrote it.
#[derive(Debug, Deserialize)]
pub struct Plan {
    /// `null` when there is nothing to do, which is how pgschema renders an
    /// empty plan rather than an empty array.
    #[serde(default)]
    groups: Option<Vec<Group>>,
}

#[derive(Debug, Deserialize)]
struct Group {
    #[serde(default)]
    steps: Vec<Step>,
}

/// A single change pgschema intends to make.
#[derive(Debug, Deserialize, Clone)]
pub struct Step {
    /// What kind of object: `table`, `table.column`, `table.constraint`,
    /// `table.index`, `table.comment`, and so on.
    #[serde(rename = "type")]
    pub kind: String,
    /// `create`, `drop`, or `alter`.
    pub operation: String,
    /// The object's dotted path, e.g. `public.orders.customer_id`.
    pub path: String,
}

impl Step {
    /// Whether this step removes something.
    ///
    /// The whole basis of "destructive" in the approval summary: pgschema says
    /// so, pgpushy does not infer it from the SQL.
    pub fn is_drop(&self) -> bool {
        self.operation == "drop"
    }
}

/// Every step type pgpushy's model can produce, measured against pgschema
/// 1.12.3 (impl-plan §1). A step outside this list is a change to something
/// the source tree cannot describe, which §8.4 forbids pgpushy to touch —
/// the enforcement behind the `.pgschemaignore` suppression, and the net
/// that catches an upstream ignore-section rename loudly instead of letting
/// it re-arm the drops.
const MODEL_KINDS: &[&str] = &[
    "table",
    "table.column",
    "table.constraint",
    "table.index",
    "table.comment",
    "table.column.comment",
    "table.index.comment",
    "sequence",
    "type",
    "domain",
];

impl Plan {
    /// Read a plan pgschema wrote.
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the plan pgschema wrote to {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parsing the plan pgschema wrote to {}", path.display()))
    }

    /// Every step, across every transaction group.
    ///
    /// Group boundaries matter to pgschema's execution and not to anything
    /// pgpushy decides, so they are flattened away here.
    pub fn steps(&self) -> impl Iterator<Item = &Step> {
        self.groups
            .iter()
            .flatten()
            .flat_map(|group| group.steps.iter())
    }

    pub fn is_empty(&self) -> bool {
        self.steps().next().is_none()
    }

    pub fn step_count(&self) -> usize {
        self.steps().count()
    }

    pub fn drops(&self) -> impl Iterator<Item = &Step> {
        self.steps().filter(|step| step.is_drop())
    }

    /// Steps naming a kind outside pgpushy's model (spec §8.4).
    pub fn unmanaged_steps(&self) -> Vec<&Step> {
        self.steps()
            .filter(|step| !MODEL_KINDS.contains(&step.kind.as_str()))
            .collect()
    }

    /// `(kind, path)` pairs that both drop and create — a modification worn
    /// as two steps. pgschema renders a widened UNIQUE constraint exactly
    /// this way and calls it "1 to modify" (spec §8.6), so counting the drop
    /// half as destructive misreports routine migrations.
    fn recreated(&self) -> std::collections::BTreeSet<(&str, &str)> {
        let mut dropped = std::collections::BTreeSet::new();
        let mut created = std::collections::BTreeSet::new();
        for step in self.steps() {
            let key = (step.kind.as_str(), step.path.as_str());
            match step.operation.as_str() {
                "drop" => {
                    dropped.insert(key);
                }
                "create" => {
                    created.insert(key);
                }
                _ => {}
            }
        }
        dropped.intersection(&created).copied().collect()
    }

    /// Drops that actually remove something: the unpaired ones (spec §8.6).
    pub fn destructive_drops(&self) -> Vec<&Step> {
        let recreated = self.recreated();
        self.drops()
            .filter(|step| !recreated.contains(&(step.kind.as_str(), step.path.as_str())))
            .collect()
    }

    pub fn destructive_count(&self) -> usize {
        self.destructive_drops().len()
    }
}

#[cfg(test)]
mod tests {
    use super::Plan;

    /// Verbatim shape from pgschema 1.12.0.
    const WITH_CHANGES: &str = r#"{
        "version": "1.0.0",
        "pgschema_version": "1.12.0",
        "source_fingerprint": { "hash": "abc" },
        "groups": [
            { "steps": [
                { "sql": "CREATE TABLE ...", "type": "table", "operation": "create", "path": "public.shipments" },
                { "sql": "ALTER TABLE ...", "type": "table.column", "operation": "drop", "path": "public.customers.name" }
            ] }
        ]
    }"#;

    /// pgschema renders "nothing to do" as a null `groups`, not `[]`.
    const EMPTY: &str = r#"{
        "version": "1.0.0",
        "pgschema_version": "1.12.0",
        "source_fingerprint": { "hash": "abc" },
        "groups": null
    }"#;

    #[test]
    fn reads_steps_and_counts_drops() {
        let plan: Plan = serde_json::from_str(WITH_CHANGES).unwrap();
        assert_eq!(plan.step_count(), 2);
        assert_eq!(plan.destructive_count(), 1);
        assert!(!plan.is_empty());

        let drop = plan.drops().next().unwrap();
        assert_eq!(drop.kind, "table.column");
        assert_eq!(drop.path, "public.customers.name");
    }

    #[test]
    fn treats_a_null_groups_as_no_changes() {
        let plan: Plan = serde_json::from_str(EMPTY).unwrap();
        assert!(plan.is_empty());
        assert_eq!(plan.step_count(), 0);
        assert_eq!(plan.destructive_count(), 0);
    }

    /// Fields pgpushy does not name must not break it, so that a pgschema
    /// release adding to the format does not require a pgpushy release.
    #[test]
    fn ignores_fields_it_does_not_know() {
        let plan: Plan = serde_json::from_str(
            r#"{"groups":[{"steps":[{"type":"table","operation":"create","path":"a.b",
               "something_new":42}],"future_field":true}],"another":"x"}"#,
        )
        .unwrap();
        assert_eq!(plan.step_count(), 1);
    }

    /// A drop paired with a create on the same kind and path is a
    /// modification (spec §8.6): pgschema renders a widened UNIQUE as
    /// exactly this pair and calls it "1 to modify".
    #[test]
    fn a_recreated_object_is_not_destructive() {
        let plan: Plan = serde_json::from_str(
            r#"{"groups":[{"steps":[
                {"type":"table.constraint","operation":"drop","path":"public.p.p_alt"},
                {"type":"table.constraint","operation":"create","path":"public.p.p_alt"},
                {"type":"table.column","operation":"drop","path":"public.p.old"}
            ]}]}"#,
        )
        .unwrap();
        assert_eq!(plan.drops().count(), 2);
        assert_eq!(plan.destructive_count(), 1);
        assert_eq!(plan.destructive_drops()[0].path, "public.p.old");
    }

    /// Steps outside pgpushy's model are the §8.4 tripwire's business; steps
    /// inside it are not.
    #[test]
    fn steps_outside_the_model_are_reported() {
        let plan: Plan = serde_json::from_str(
            r#"{"groups":[{"steps":[
                {"type":"table","operation":"create","path":"a.t"},
                {"type":"view","operation":"drop","path":"a.v"},
                {"type":"table.rls","operation":"drop","path":"a.t"}
            ]}]}"#,
        )
        .unwrap();
        let outside: Vec<&str> = plan
            .unmanaged_steps()
            .iter()
            .map(|step| step.kind.as_str())
            .collect();
        assert_eq!(outside, ["view", "table.rls"]);
    }
}
