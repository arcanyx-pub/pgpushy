//! Running pgschema (spec §8.1, §8.3, §8.6).
//!
//! Deliberately thin. pgpushy builds the argv, hands over the synthesized
//! document, and lets pgschema's own output through untouched — it does not
//! reformat plans (G3). What pgpushy *does* own is the arguments it must not
//! let the user set: `--schema`, because it loops over the managed set;
//! `--file` and `--plan`, because it supplies both; and `--auto-approve`,
//! because approval happens once at the pgpushy level.

use crate::conn::Resolved;
use crate::provider::PgschemaBin;
use anyhow::{Context, Result};
use pgpushy_core::SchemaName;
use std::path::Path;
use std::process::Command;

/// Plan one schema, streaming the human-readable output through and writing a
/// machine-readable copy for pgpushy to read (spec §8.6).
///
/// Both forms come out of the same invocation, so the plan the operator reads
/// and the plan pgpushy summarizes are the same computation — not two runs
/// that could disagree.
pub fn plan(
    binary: &PgschemaBin,
    connection: &Resolved,
    schema: &SchemaName,
    file: &Path,
    json_out: &Path,
) -> Result<bool> {
    let mut command = base(binary, connection, "plan", schema);
    command.arg("--file").arg(file);
    command.arg("--output-human").arg("stdout");
    command.arg("--output-json").arg(json_out);
    run(command, binary, "plan")
}

/// Apply a plan pgschema computed earlier (spec §8.6).
///
/// Applying the reviewed plan rather than recomputing from the desired state
/// is what makes the change that lands the change that was approved. pgschema
/// fingerprints the state each plan was computed against and refuses one whose
/// target has since moved, so drift between plan and apply fails loudly
/// instead of quietly applying something nobody saw.
pub fn apply_plan(
    binary: &PgschemaBin,
    connection: &Resolved,
    schema: &SchemaName,
    plan_file: &Path,
) -> Result<bool> {
    let mut command = base(binary, connection, "apply", schema);
    command.arg("--plan").arg(plan_file);
    // Approval already happened, once, for the whole database (spec §8.6).
    command.arg("--auto-approve");
    run(command, binary, "apply")
}

fn base(
    binary: &PgschemaBin,
    connection: &Resolved,
    subcommand: &str,
    schema: &SchemaName,
) -> Command {
    let mut command = Command::new(&binary.path);
    command.arg(subcommand);
    command.args(["--schema", schema.as_str()]);
    command.args(connection.pgschema_flags());
    // Resolve nothing for itself: the password arrives through the
    // environment, and every other PG* variable is stripped (spec §6.3).
    connection.command_env(&mut command);
    command
}

/// Run pgschema, streaming its output.
///
/// A non-zero exit is not an error in the `anyhow` sense — pgschema has already
/// explained itself on stderr, and the caller decides what a failure means for
/// the run as a whole.
fn run(mut command: Command, binary: &PgschemaBin, subcommand: &str) -> Result<bool> {
    let status = command
        .status()
        .with_context(|| format!("running {} {subcommand}", binary.path.display()))?;
    Ok(status.success())
}
