//! The `pgpushy` binary: the IO shell around [`pgpushy_core`].
//!
//! Everything that touches the filesystem, the network, a subprocess, or a
//! database lives here; everything deterministic lives in the core crate.

#![forbid(unsafe_code)]

mod cli;
mod discovery;
mod report;
mod run;

use clap::Parser;
use cli::{Cli, Command};

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    let result = match &cli.command {
        Command::Validate { source, out } => run::validate(source, out.as_deref()),
    };

    match result {
        Ok(outcome) => std::process::ExitCode::from(u8::try_from(outcome.exit_code()).unwrap_or(1)),
        Err(err) => {
            // A pgpushy failure rather than a source-tree problem: print the
            // whole context chain, since the useful detail is usually in the
            // cause rather than the top-level message.
            eprintln!("\nerror: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
