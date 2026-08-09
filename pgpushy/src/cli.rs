//! Command-line interface.
//!
//! Note what is *not* here. The source root, default schema, managed-schema
//! declaration and exclusions are all in `pgpushy.toml` and nowhere else
//! (spec §10.1): each describes the project and each is a way to change what
//! gets reconciled, so a flag that silently narrowed the desired state would be
//! the same hazard as guessing at a missing configuration file. Connection
//! settings are likewise absent — the target comes from a named environment
//! (§10.2), so that it is never assembled from ambient state.
//!
//! What remains a flag describes *this run* or *this machine*: which project
//! (`--config`), which target (`--env`), which pgschema binary, where to write
//! the synthesized document, and whether to prompt.

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
    /// Path to the configuration file. [default: ./pgpushy.toml]
    ///
    /// Looked for in the working directory only; it is not searched for in
    /// parent directories. Paths inside it resolve against its own directory,
    /// so this selects a whole project, not just a file.
    #[arg(long, short = 'c', value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Show what pgpushy is doing: the pgschema command line, and where the
    /// synthesized desired state was written.
    ///
    /// The first thing worth seeing when a run does something unexpected, and
    /// the reason pgpushy has no log levels — this is the only extra detail
    /// there is to want.
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    /// Never emit colored output, and tell pgschema not to either.
    ///
    /// Colour is suppressed automatically when stdout is not a terminal, so
    /// this is for the cases that heuristic gets wrong. `NO_COLOR` in the
    /// environment does the same.
    #[arg(long, global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Write a starter pgpushy.toml.
    ///
    /// Configuration is required, so this is the first command most projects
    /// run. It guesses the source root from any `*.sql` it can find nearby and
    /// scaffolds an environment to fill in.
    Init {
        /// Where to write it. Defaults to ./pgpushy.toml.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },

    /// Check the source tree. Connects to nothing.
    ///
    /// Runs the whole offline pipeline — discovery, parsing, the statement
    /// allow-list, schema resolution, the validity checks and cross-schema
    /// ordering — and reports what it found. Suitable for a pre-commit hook or
    /// a CI job with no database available. Takes no `--env`, because it has
    /// no target.
    Validate {
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
        target: TargetArgs,

        #[command(flatten)]
        pgschema: PgschemaArgs,

        /// Write the synthesized desired state to a file.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },

    /// Reconcile the database: plan every managed schema, then apply.
    ///
    /// Approval is asked once, for the whole database, after every plan has
    /// been computed and shown — so declining leaves the target untouched.
    /// Apply is not atomic across schemas: a failure partway leaves earlier
    /// schemas applied.
    Apply {
        #[command(flatten)]
        target: TargetArgs,

        #[command(flatten)]
        pgschema: PgschemaArgs,

        /// Write the synthesized desired state to a file.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,

        /// Apply without prompting. Required when stdin is not a terminal.
        #[arg(long)]
        auto_approve: bool,

        /// How long Postgres waits for a lock before giving up, e.g. `30s`.
        ///
        /// Overrides the environment's `lock_timeout`. Unlike the project
        /// settings this is a flag, because it cannot change what gets
        /// reconciled — only whether the apply gives up waiting.
        #[arg(long, value_name = "DURATION")]
        lock_timeout: Option<String>,
    },
}

#[derive(Args, Debug)]
pub struct TargetArgs {
    /// Which `[env.<name>]` block to reconcile against.
    ///
    /// Required even when only one environment is defined: selecting the sole
    /// one automatically would make adding a second silently change what an
    /// existing command reconciles.
    #[arg(long, short = 'e', value_name = "NAME")]
    pub env: String,
}

#[derive(Args, Debug)]
pub struct PgschemaArgs {
    /// Path to the pgschema binary. Defaults to `[pgschema] path`, then `PATH`.
    ///
    /// Stays a flag, unlike the project settings, because it differs per
    /// machine and cannot change what gets reconciled.
    #[arg(long, value_name = "PATH")]
    pub pgschema_path: Option<PathBuf>,
}
