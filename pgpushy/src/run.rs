//! Command implementations.

use crate::cli::SourceArgs;
use crate::discovery;
use crate::report;
use anyhow::{Context, Result};
use pgpushy_core::{Analysis, AnalysisError, Options, SchemaName, analyze};
use std::path::{Path, PathBuf};

/// What a command decided the process should exit with.
///
/// Distinguished from an `Err` because a source tree pgpushy correctly
/// rejected is not a pgpushy failure: the diagnostics have already been
/// printed, and the caller only needs the status.
pub enum Outcome {
    Ok,
    Failed,
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::Failed => 1,
        }
    }
}

/// `pgpushy validate` — the offline pipeline, and nothing else.
pub fn validate(source: &SourceArgs, out: Option<&Path>) -> Result<Outcome> {
    let Some(analysis) = analyze_source(source, out)? else {
        return Ok(Outcome::Failed);
    };

    report::summary(source, &analysis);

    // A cross-schema cycle is fatal here. `plan` will treat it differently —
    // it shows the plans anyway, because those plans are what the operator
    // needs in order to break the cycle — but `validate` answers "can this be
    // applied?", and the answer is no.
    let cycles = analysis.cycle_diagnostics();
    if !cycles.is_empty() {
        report::diagnostics(&cycles);
        return Ok(Outcome::Failed);
    }

    report::checks_passed(&analysis);

    if let Some(path) = out {
        write_desired_state(path, &analysis)?;
    }

    Ok(Outcome::Ok)
}

/// Discover and analyze, printing diagnostics and returning `None` if the
/// source tree was rejected.
fn analyze_source(source: &SourceArgs, out: Option<&Path>) -> Result<Option<Analysis>> {
    let root = canonical_root(&source.source_root)?;
    let discovered = discovery::discover(&root, &source.exclude, out)?;

    let options = Options {
        default_schema: SchemaName::new(&source.default_schema),
        managed_schemas: (!source.managed_schemas.is_empty())
            .then(|| source.managed_schemas.iter().map(SchemaName::new).collect()),
    };

    report::discovery(&root, &discovered);

    match analyze(&discovered.files, &options) {
        Ok(analysis) => Ok(Some(analysis)),
        Err(AnalysisError::Source(diagnostics)) => {
            report::diagnostics(&diagnostics);
            Ok(None)
        }
        // Not the author's doing: surface it as a real error, with a backtrace
        // path through anyhow rather than as a source-tree diagnostic.
        Err(AnalysisError::Internal(err)) => Err(err).context("synthesizing the desired state"),
    }
}

fn canonical_root(root: &Path) -> Result<PathBuf> {
    root.canonicalize()
        .with_context(|| format!("source root {} does not exist", root.display()))
}

fn write_desired_state(path: &Path, analysis: &Analysis) -> Result<()> {
    std::fs::write(path, &analysis.desired_state)
        .with_context(|| format!("writing {}", path.display()))?;
    report::wrote(path, &analysis.desired_state);
    Ok(())
}
