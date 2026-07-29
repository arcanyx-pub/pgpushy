//! Command-line interface.
//!
//! `apply` arrives with the approval gate and the cross-schema removal check.
//! Flags are grouped by what they describe — the source tree, the connection,
//! the pgschema binary — so the groups can be shared across subcommands
//! without repeating them.

use crate::conn::ConnectionArgs;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "pgpushy",
    version,
    about = "Declarative Postgres schema management at the database level, powered by pgschema",
    long_about = None,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Check the source tree. Connects to nothing.
    ///
    /// Runs the whole offline pipeline — discovery, parsing, the statement
    /// allow-list, schema resolution, the validity checks and cross-schema
    /// ordering — and reports what it found. Suitable for a pre-commit hook or
    /// a CI job with no database available.
    Validate {
        #[command(flatten)]
        source: SourceArgs,

        /// Write the synthesized desired state to a file.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },

    /// Show what would change, one plan per managed schema.
    ///
    /// Read-only: pgschema reads the target and builds its comparison model in
    /// a separate plan database, and pgpushy's own inspection is a single
    /// read-only query. Nothing here modifies the target.
    Plan {
        #[command(flatten)]
        source: SourceArgs,

        #[command(flatten)]
        connection: ConnectionArgs,

        #[command(flatten)]
        pgschema: PgschemaArgs,

        /// Write the synthesized desired state to a file.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

#[derive(Args, Debug)]
pub struct SourceArgs {
    /// Root of the source tree to read `*.sql` from.
    #[arg(long, value_name = "PATH", default_value = ".")]
    pub source_root: PathBuf,

    /// Schema that unqualified objects belong to.
    ///
    /// This assigns objects; it does not make the schema managed. A default
    /// schema with no objects in it is not reconciled.
    #[arg(long, value_name = "SCHEMA", default_value = "public")]
    pub default_schema: String,

    /// Restrict the schemas pgpushy may reconcile. Repeatable.
    ///
    /// When given, this is authoritative: a schema the source tree uses but
    /// this list omits is an error, and a schema listed here that the tree
    /// never mentions is managed with an empty desired state — which will plan
    /// to drop whatever the target holds in it.
    #[arg(long = "managed-schema", value_name = "SCHEMA")]
    pub managed_schemas: Vec<String>,

    /// Glob of paths to exclude, relative to the source root. Repeatable.
    ///
    /// Excluded files are never read or parsed, so this is how a tree holds
    /// seed data or fixtures alongside desired state.
    #[arg(long = "exclude", value_name = "GLOB")]
    pub exclude: Vec<String>,
}

#[derive(Args, Debug)]
pub struct PgschemaArgs {
    /// Path to the pgschema binary. Defaults to looking on `PATH`.
    #[arg(long, value_name = "PATH")]
    pub pgschema_path: Option<PathBuf>,
}
