//! Seed execution (spec §8.8): pgpushy's one write path to the target.
//!
//! Bounded on every axis: DML only, from seed files only, after apply only,
//! one transaction per file, under the environment's lock timeout. Within
//! each file's transaction the statements run twice — the second pass is the
//! convergence probe, and it must touch nothing. A probe that passes changed
//! nothing, so the commit commits exactly the first pass; a probe that fails
//! rolls back together with it, so a non-idempotent seed lands *nothing*.
//! The failure path and the undo path are the same path.

use crate::conn::Resolved;
use crate::inspect;
use crate::report;
use crate::run::Outcome;
use anyhow::{Context, Result};
use pgpushy_core::Seeds;

/// Apply every seed file, in order, stopping at the first failure (spec §9).
pub fn execute(
    connection: &Resolved,
    seeds: &Seeds,
    lock_timeout: Option<&str>,
) -> Result<Outcome> {
    let mut client = inspect::connect(connection)?;
    let mut applied: Vec<String> = Vec::new();

    for (index, file) in seeds.files.iter().enumerate() {
        report::seed_heading(&file.path);

        let mut tx = client
            .transaction()
            .with_context(|| format!("opening a transaction for seed {}", file.path))?;

        // A qualified statement cannot be diverted, and §4.6 guarantees every
        // statement is qualified — so nothing here resolves through a path an
        // operator's session could have altered.
        tx.batch_execute("SET LOCAL search_path = ''")
            .context("pinning search_path for the seed transaction")?;
        if let Some(timeout) = lock_timeout {
            // The same bound DDL gets (spec §10.5): a seed insert contending
            // with application traffic should give up, not queue behind it.
            let escaped = timeout.replace('\'', "''");
            tx.batch_execute(&format!("SET LOCAL lock_timeout = '{escaped}'"))
                .context("setting lock_timeout for the seed transaction")?;
        }

        let mut rows: u64 = 0;
        let mut failed = false;

        for stmt in &file.statements {
            match tx.execute(stmt.sql.as_str(), &[]) {
                Ok(count) => rows += count,
                Err(err) => {
                    report::seed_statement_error(&file.path, &stmt.origin, &err);
                    failed = true;
                    break;
                }
            }
        }

        // The probe (spec §8.8): the same statements again, in the same
        // transaction. Any affected row means the file does not converge.
        if !failed {
            for stmt in &file.statements {
                match tx.execute(stmt.sql.as_str(), &[]) {
                    Ok(0) => {}
                    Ok(count) => {
                        report::seed_probe_failure(&file.path, &stmt.origin, count);
                        failed = true;
                        break;
                    }
                    Err(err) => {
                        report::seed_statement_error(&file.path, &stmt.origin, &err);
                        failed = true;
                        break;
                    }
                }
            }
        }

        if failed {
            // Dropping the transaction rolls back both passes: the failing
            // file lands nothing (spec §11.2).
            drop(tx);
            let unattempted: Vec<&str> = seeds.files[index + 1..]
                .iter()
                .map(|f| f.path.as_str())
                .collect();
            report::seeds_partial(&applied, &file.path, &unattempted);
            return Ok(Outcome::Failed);
        }

        tx.commit()
            .with_context(|| format!("committing seed {}", file.path))?;
        report::seed_applied(&file.path, rows);
        applied.push(file.path.clone());
    }

    report::seeds_done(applied.len());
    Ok(Outcome::Ok)
}
