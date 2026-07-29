//! Running pgschema (spec §8.1, §8.3).
//!
//! Deliberately thin. pgpushy builds the argv, hands over the synthesized
//! document, and lets pgschema's own output through untouched — it does not
//! parse plans or reformat them (G3). What pgpushy *does* own is the three
//! arguments it must not let the user set: `--schema`, because it loops over
//! the managed set; `--file`, because it synthesizes the desired state; and
//! `--auto-approve`, because approval happens once at the pgpushy level.

use crate::conn::Resolved;
use crate::provider::PgschemaBin;
use anyhow::{Context, Result};
use pgpushy_core::SchemaName;
use std::path::Path;
use std::process::Command;

/// Which pgschema subcommand to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subcommand {
    Plan,
    Apply,
}

impl Subcommand {
    fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Apply => "apply",
        }
    }
}

/// Run pgschema against one schema, streaming its output through.
///
/// Returns whether pgschema succeeded. A non-zero exit is not an error in the
/// `anyhow` sense — pgschema has already explained itself on stderr, and the
/// caller decides what a failure means for the run as a whole.
pub fn run(
    binary: &PgschemaBin,
    subcommand: Subcommand,
    connection: &Resolved,
    schema: &SchemaName,
    file: &Path,
) -> Result<bool> {
    let mut command = Command::new(&binary.path);
    command.arg(subcommand.as_str());
    command.args(["--schema", schema.as_str()]);
    command.arg("--file").arg(file);
    command.args(connection.pgschema_flags());

    if subcommand == Subcommand::Apply {
        // Approval happened once, for the database, before anything was
        // touched (spec §8.6). pgschema must not ask again.
        command.arg("--auto-approve");
    }

    // Resolve nothing for itself: the password arrives here, and every other
    // PG* variable is stripped (spec §6.3).
    connection.command_env(&mut command);

    let status = command
        .status()
        .with_context(|| format!("running {} {}", binary.path.display(), subcommand.as_str()))?;

    Ok(status.success())
}
