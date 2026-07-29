//! User-facing output.
//!
//! Kept in one module so that the shape of pgpushy's output is visible in a
//! single place rather than scattered through the commands. Two conventions,
//! both from spec §G5 and impl-plan §12:
//!
//! - Diagnostics name **every** instance, not the first. A tree with five
//!   unsupported statements prints five.
//! - Anything sourced from the tree names its file and line, and says what to
//!   do about it.
//!
//! Progress and results go to stdout; diagnostics go to stderr, so that
//! `--out`-style piping and error reporting do not interleave.

use crate::cli::SourceArgs;
use crate::discovery::Discovered;
use pgpushy_core::{Analysis, Diagnostic};
use std::path::Path;

/// What discovery found, before anything is parsed.
///
/// Printed early and unconditionally: when an exclude pattern is too broad, or
/// a source root points somewhere unexpected, the file count is the thing that
/// gives it away.
pub fn discovery(root: &Path, discovered: &Discovered) {
    let total: usize = discovered.excluded.iter().map(|(_, n)| n).sum();
    let plural = if discovered.files.len() == 1 { "" } else { "s" };

    if total == 0 {
        println!(
            "  {} ({} file{plural})",
            root.display(),
            discovered.files.len()
        );
    } else {
        println!(
            "  {} ({} file{plural}, {total} excluded)",
            root.display(),
            discovered.files.len(),
        );
        for (pattern, count) in &discovered.excluded {
            if *count > 0 {
                println!("    excluded by {pattern:?}: {count}");
            }
        }
    }
}

/// The managed-schema set and what the tree contains.
pub fn summary(source: &SourceArgs, analysis: &Analysis) {
    let counts = &analysis.counts;
    let mut parts = vec![format!("{} table{}", counts.tables, s(counts.tables))];
    if counts.foreign_keys > 0 {
        parts.push(format!(
            "{} foreign key{}",
            counts.foreign_keys,
            s(counts.foreign_keys)
        ));
    }
    if counts.indexes > 0 {
        parts.push(format!("{} index{}", counts.indexes, es(counts.indexes)));
    }
    if counts.constraints > 0 {
        parts.push(format!(
            "{} constraint{}",
            counts.constraints,
            s(counts.constraints)
        ));
    }
    if counts.comments > 0 {
        parts.push(format!("{} comment{}", counts.comments, s(counts.comments)));
    }
    println!("  {}", parts.join(", "));

    let declared = if source.managed_schemas.is_empty() {
        ""
    } else {
        " (declared)"
    };
    println!(
        "\n  managed schemas{declared}: {}",
        analysis
            .managed_schemas
            .iter()
            .map(|s| s.as_str().to_owned())
            .collect::<Vec<_>>()
            .join(", "),
    );

    // A managed schema with nothing in it reconciles the target's copy to
    // empty. That is the only way to say so, and it is destructive, so it is
    // never left implicit.
    let empty: Vec<_> = analysis
        .managed_schemas
        .iter()
        .filter(|schema| !mentions(analysis, schema))
        .map(|s| s.as_str())
        .collect();
    if !empty.is_empty() {
        println!(
            "  WARNING: no source file describes {}; applying would plan to drop \
             everything the target holds there",
            empty.join(", "),
        );
    }
}

/// The all-clear, plus the order the schemas would be applied in.
pub fn checks_passed(analysis: &Analysis) {
    println!("\n  ok  no duplicate objects");
    println!("  ok  all foreign key referents resolvable");
    println!("  ok  no cross-schema foreign key cycles");
    println!("  ok  no unsupported statements");

    if analysis.order.len() > 1 {
        println!(
            "\n  schema apply order: {}",
            analysis
                .order
                .iter()
                .map(|s| s.as_str().to_owned())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
}

pub fn wrote(path: &Path, contents: &str) {
    let lines = contents.lines().count();
    println!("\n  wrote {} ({lines} lines)", path.display());
}

/// Print every diagnostic, to stderr.
pub fn diagnostics(diagnostics: &[Diagnostic]) {
    eprintln!();
    for diagnostic in diagnostics {
        eprintln!("error: {}", diagnostic.message);
        for origin in &diagnostic.origins {
            eprintln!("  at {origin}");
        }
        if let Some(help) = &diagnostic.help {
            eprintln!("  help: {help}");
        }
        eprintln!();
    }

    let count = diagnostics.len();
    eprintln!("{count} problem{} found in the source tree", s(count));
}

/// Whether any object was assigned to this schema.
///
/// [`Analysis`] reports counts rather than objects, so this is answered from
/// the synthesized document: a schema with objects appears qualifying at least
/// one of them, while a schema with none appears only in its own
/// `CREATE SCHEMA`.
fn mentions(analysis: &Analysis, schema: &pgpushy_core::SchemaName) -> bool {
    analysis.desired_state.contains(&format!("{schema}."))
}

fn s(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn es(n: usize) -> &'static str {
    if n == 1 { "" } else { "es" }
}
