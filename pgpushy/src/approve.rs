//! The approval gate (spec §8.6).
//!
//! `apply` reconciles several schemas in sequence and is not atomic across
//! them, so approval is sought **once, for the database, before any schema is
//! touched** — not per schema as each apply begins. That is the only model in
//! which declining is guaranteed to leave the target untouched.
//!
//! The summary below is read from the plans pgschema produced. Every "N
//! destructive" here is a step pgschema itself labelled `drop`; pgpushy does
//! not inspect SQL or compare state to work that out (G3).

use crate::plan_file::Plan;
use anyhow::{Result, bail};
use pgpushy_core::{Analysis, SchemaName};
use std::io::{IsTerminal, Write};

/// What the operator decided.
pub enum Decision {
    Approved,
    Declined,
}

/// Present every plan as one unit and ask once.
pub fn confirm(
    analysis: &Analysis,
    plans: &[(SchemaName, Plan)],
    auto_approve: bool,
) -> Result<Decision> {
    summarize(analysis, plans);

    if auto_approve {
        println!("\n  --auto-approve given; applying without prompting.");
        return Ok(Decision::Approved);
    }

    // Proceeding unapproved because nobody could answer is exactly the failure
    // this gate exists to prevent, so a non-interactive run must say
    // --auto-approve rather than have it inferred.
    if !std::io::stdin().is_terminal() {
        bail!(
            "apply needs approval, but standard input is not a terminal\n\
             \n\
             Re-run with --auto-approve to apply without prompting."
        );
    }

    print!("\nApply? [y/N] ");
    std::io::stdout().flush().ok();

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;

    let answer = answer.trim().to_ascii_lowercase();
    if answer == "y" || answer == "yes" {
        Ok(Decision::Approved)
    } else {
        Ok(Decision::Declined)
    }
}

/// The reviewable unit: what changes, where, and what is destructive.
fn summarize(analysis: &Analysis, plans: &[(SchemaName, Plan)]) {
    let changing: Vec<_> = plans.iter().filter(|(_, plan)| !plan.is_empty()).collect();
    let steps: usize = plans.iter().map(|(_, plan)| plan.step_count()).sum();
    let drops: usize = plans.iter().map(|(_, plan)| plan.drop_count()).sum();

    let width = plans
        .iter()
        .map(|(schema, _)| schema.as_str().len())
        .max()
        .unwrap_or(0)
        .max(8);

    println!(
        "\n  Plan for {} managed schema{}, in apply order:\n",
        plans.len(),
        plural(plans.len()),
    );
    for (schema, plan) in plans {
        if plan.is_empty() {
            println!("    {schema:<width$}  no changes");
        } else {
            let destructive = match plan.drop_count() {
                0 => String::new(),
                n => format!("  ({n} destructive)"),
            };
            println!(
                "    {schema:<width$}  {} change{}{destructive}",
                plan.step_count(),
                plural(plan.step_count()),
            );
        }
    }

    // Seeds are inside the approved unit (spec §8.6, §8.8): the writes they
    // make are writes, whatever the schema plans say.
    let seeds = &analysis.seeds;
    if !seeds.is_empty() {
        println!(
            "\n  {} seed file{}, applied after the schemas, each in its own transaction:",
            seeds.files.len(),
            plural(seeds.files.len()),
        );
        for file in &seeds.files {
            println!(
                "    {}  {} statement{}",
                file.path,
                file.statements.len(),
                plural(file.statements.len()),
            );
        }
    }

    if changing.is_empty() && seeds.is_empty() {
        println!("\n  Nothing to apply.");
        return;
    }

    if !changing.is_empty() {
        println!(
            "\n  {steps} change{} across {} schema{}, {drops} destructive.",
            plural(steps),
            changing.len(),
            plural(changing.len()),
        );
    }

    // Destructive changes are listed individually. A count alone is not a
    // review: "1 destructive" reads very differently from
    // "drop table.column public.customers.email".
    if drops > 0 {
        println!("\n  Destructive changes:");
        for (_, plan) in plans {
            for step in plan.drops() {
                println!("    drop {:<18} {}", step.kind, step.path);
            }
        }
    }

    // A managed schema with no source is reconciled to empty, which plans a
    // drop of everything the target holds there. Said again here because this
    // is the last moment before it happens.
    let empty: Vec<_> = analysis
        .empty_schemas
        .iter()
        .map(|schema| schema.as_str())
        .collect();
    if !empty.is_empty() {
        println!(
            "\n  WARNING: no source file describes {}; applying reconciles \
             {} to empty.",
            empty.join(", "),
            if empty.len() == 1 { "it" } else { "them" },
        );
    }

    println!(
        "\n  apply is not atomic across schemas: a failure partway leaves\n  \
         earlier schemas applied and the rest unapplied."
    );
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
