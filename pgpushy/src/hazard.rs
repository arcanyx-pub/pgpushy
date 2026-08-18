//! The cross-schema foreign-key removal check (spec §6.2).
//!
//! pgpushy applies a referenced schema before the schema that references it,
//! which is right for *creating* a cross-schema foreign key and backwards for
//! removing what one points at. Verified against pgschema 1.12.0, that is a
//! narrower problem than it sounds:
//!
//! | The referenced schema's plan drops | pgschema emits | Result |
//! |---|---|---|
//! | the referenced table | `DROP TABLE … CASCADE` | fine — the CASCADE takes the foreign key with it |
//! | the referenced column | `ALTER TABLE … DROP COLUMN` | **fails** |
//! | the referenced unique/primary key | `ALTER TABLE … DROP CONSTRAINT` | **fails** |
//!
//! So the hazard is exactly: a plan that removes a column a cross-schema
//! foreign key points at, or the unique constraint it depends on, while
//! leaving the table in place — and the referencing schema applying later.
//!
//! Detection reads the plans pgschema already produced rather than diffing
//! anything, so G3 holds: pgschema decided what to drop, and pgpushy is only
//! noticing that one of those drops is load-bearing for a constraint in
//! another schema.

use crate::inspect::CrossSchemaForeignKey;
use crate::plan_file::Plan;
use pgpushy_core::SchemaName;

/// A removal the apply order cannot satisfy.
pub struct Hazard {
    pub foreign_key: CrossSchemaForeignKey,
    /// What the referenced schema's plan drops, as pgschema named it.
    pub dropped: String,
    /// The kind of object dropped, for the message: "column" or "constraint".
    pub dropped_kind: &'static str,
}

impl Hazard {
    /// The two-step resolution, spelled out.
    pub fn remedy(&self) -> String {
        let fk = &self.foreign_key;
        format!(
            "apply this in two steps:\n    \
             1. remove only the foreign key {} from {}.{}, then run pgpushy apply\n    \
             2. remove {} from {}.{}, then run pgpushy apply again",
            fk.name, fk.from_schema, fk.from_table, self.dropped, fk.to_schema, fk.to_table,
        )
    }
}

/// Find every cross-schema removal the apply order cannot satisfy.
///
/// `order` is the sequence schemas will be applied in; a foreign key is only at
/// risk when its *referencing* schema comes after the schema whose plan does
/// the dropping. When the tie-break happens to put the referencing schema
/// first, the foreign key is gone before the thing it depends on and there is
/// nothing to report.
pub fn check(
    foreign_keys: &[CrossSchemaForeignKey],
    plans: &[(SchemaName, Plan)],
    order: &[SchemaName],
) -> Vec<Hazard> {
    let position = |schema: &SchemaName| order.iter().position(|s| s == schema);
    let mut hazards = Vec::new();

    for fk in foreign_keys {
        let (Some(from_at), Some(to_at)) = (position(&fk.from_schema), position(&fk.to_schema))
        else {
            continue;
        };
        if from_at < to_at {
            // The foreign key is removed before anything it depends on is.
            continue;
        }

        let Some((_, plan)) = plans.iter().find(|(schema, _)| *schema == fk.to_schema) else {
            continue;
        };

        for step in plan.drops() {
            let Some(object) = step
                .path
                .strip_prefix(&format!("{}.{}.", fk.to_schema, fk.to_table))
            else {
                continue;
            };

            // A dropped table is safe: pgschema uses CASCADE, which removes
            // the dependent foreign key along with it.
            let kind = match step.kind.as_str() {
                "table.column" if fk.to_columns.iter().any(|column| column == object) => "column",
                "table.constraint" if fk.to_constraint.as_deref() == Some(object) => "constraint",
                _ => continue,
            };

            hazards.push(Hazard {
                foreign_key: fk.clone(),
                dropped: object.to_owned(),
                dropped_kind: kind,
            });
        }
    }

    hazards
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(name: &str) -> SchemaName {
        SchemaName::new(name)
    }

    fn foreign_key() -> CrossSchemaForeignKey {
        CrossSchemaForeignKey {
            from_schema: schema("down"),
            from_table: "child".into(),
            name: "child_parent_fk".into(),
            to_schema: schema("up"),
            to_table: "parent".into(),
            to_columns: vec!["alt".into()],
            to_constraint: Some("parent_alt_key".into()),
        }
    }

    fn plan_with(steps: &str) -> Plan {
        serde_json::from_str(&format!(r#"{{"groups":[{{"steps":[{steps}]}}]}}"#)).unwrap()
    }

    fn drop_step(kind: &str, path: &str) -> String {
        format!(r#"{{"type":"{kind}","operation":"drop","path":"{path}"}}"#)
    }

    /// The verified failure: the referenced column goes while the table stays.
    #[test]
    fn flags_a_dropped_referenced_column() {
        let plans = vec![(
            schema("up"),
            plan_with(&drop_step("table.column", "up.parent.alt")),
        )];
        let hazards = check(&[foreign_key()], &plans, &[schema("up"), schema("down")]);

        assert_eq!(hazards.len(), 1);
        assert_eq!(hazards[0].dropped, "alt");
        assert_eq!(hazards[0].dropped_kind, "column");
    }

    #[test]
    fn flags_a_dropped_referenced_unique_constraint() {
        let plans = vec![(
            schema("up"),
            plan_with(&drop_step("table.constraint", "up.parent.parent_alt_key")),
        )];
        let hazards = check(&[foreign_key()], &plans, &[schema("up"), schema("down")]);

        assert_eq!(hazards.len(), 1);
        assert_eq!(hazards[0].dropped_kind, "constraint");
    }

    /// pgschema drops tables with CASCADE, so this one is not a hazard — and
    /// flagging it would block a legitimate change.
    #[test]
    fn ignores_a_dropped_table() {
        let plans = vec![(schema("up"), plan_with(&drop_step("table", "up.parent")))];
        assert!(check(&[foreign_key()], &plans, &[schema("up"), schema("down")]).is_empty());
    }

    #[test]
    fn ignores_drops_the_foreign_key_does_not_depend_on() {
        let plans = vec![(
            schema("up"),
            plan_with(&format!(
                "{},{}",
                drop_step("table.column", "up.parent.unrelated"),
                drop_step("table.constraint", "up.parent.some_check"),
            )),
        )];
        assert!(check(&[foreign_key()], &plans, &[schema("up"), schema("down")]).is_empty());
    }

    /// When the referencing schema happens to be applied first, the foreign key
    /// is gone before the column it depends on, and there is nothing to report.
    #[test]
    fn ignores_a_drop_the_order_already_handles() {
        let plans = vec![(
            schema("up"),
            plan_with(&drop_step("table.column", "up.parent.alt")),
        )];
        assert!(check(&[foreign_key()], &plans, &[schema("down"), schema("up")]).is_empty());
    }

    /// A column of the same name on a different table must not match.
    #[test]
    fn matches_the_table_as_well_as_the_column() {
        let plans = vec![(
            schema("up"),
            plan_with(&drop_step("table.column", "up.other_table.alt")),
        )];
        assert!(check(&[foreign_key()], &plans, &[schema("up"), schema("down")]).is_empty());
    }
}
