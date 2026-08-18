//! The `pgpushy` binary: the IO shell around [`pgpushy_core`].
//!
//! Everything that touches the filesystem, the network, a subprocess, or a
//! database lives here; everything deterministic lives in the core crate.

#![forbid(unsafe_code)]

mod approve;
mod cli;
mod config;
mod conn;
mod discovery;
mod hazard;
mod init;
mod inspect;
mod outdir;
mod output;
mod pgschema;
mod plan_file;
mod provider;
mod report;
mod run;
mod tls;

use clap::Parser;
use cli::{Cli, Command};

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    let output = output::Output::resolve(cli.verbose, cli.no_color);

    // `init` writes the configuration, so it must run before anything tries to
    // load one — otherwise the command that fixes a missing file would itself
    // fail on the missing file.
    if let Command::Init { out } = &cli.command {
        return match init::init(out.as_deref()) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(err) => {
                eprintln!(
                    "
error: {err:#}"
                );
                std::process::ExitCode::FAILURE
            }
        };
    }

    let loaded = match config::load(cli.config.as_deref()) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!(
                "
error: {err:#}"
            );
            return std::process::ExitCode::FAILURE;
        }
    };

    let result = match &cli.command {
        // Handled above, before the configuration file was required.
        Command::Init { .. } => unreachable!("init runs before config is loaded"),
        Command::Validate { out } => run::validate(&loaded, output, out.as_deref()),
        Command::Plan {
            target,
            pgschema,
            out,
        } => run::plan(target, pgschema, &loaded, output, out.as_deref()),
        Command::Apply {
            target,
            pgschema,
            out,
            auto_approve,
            lock_timeout,
        } => run::apply(
            target,
            pgschema,
            &loaded,
            output,
            out.as_deref(),
            *auto_approve,
            lock_timeout.as_deref(),
        ),
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
