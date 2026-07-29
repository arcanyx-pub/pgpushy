//! Command implementations.

use crate::cli::{PgschemaArgs, SourceArgs};
use crate::conn::{ConnectionArgs, Resolved};
use crate::inspect;
use crate::pgschema::{self, Subcommand};
use crate::provider::{PgschemaProvider, byo::Byo};
use crate::report;
use crate::{discovery, tempfile_for};
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
    let Some(analysis) = analyze_source(source)? else {
        return Ok(Outcome::Failed);
    };

    report::summary(source, &analysis);

    // A cross-schema cycle is fatal here. `plan` treats it differently — it
    // shows the plans anyway, because those plans are what the operator needs
    // in order to break the cycle — but `validate` answers "can this be
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

/// `pgpushy plan` — the offline pipeline, then one pgschema plan per schema.
pub fn plan(
    source: &SourceArgs,
    connection: &ConnectionArgs,
    pgschema_args: &PgschemaArgs,
    out: Option<&Path>,
) -> Result<Outcome> {
    let Some(analysis) = analyze_source(source)? else {
        return Ok(Outcome::Failed);
    };
    report::summary(source, &analysis);

    // Spec §7: a cross-schema cycle means no apply order exists, but it does
    // not stop pgschema computing per-schema plans — and those plans are
    // exactly what the operator needs to break the cycle. Report it, keep
    // going, and fail at the end.
    let cycles = analysis.cycle_diagnostics();
    if !cycles.is_empty() {
        report::diagnostics(&cycles);
        report::plans_are_diagnostic_only();
    }

    let binary = Byo {
        explicit: pgschema_args.pgschema_path.clone(),
    }
    .resolve()?;
    report::pgschema(&binary);

    let connection = Resolved::from(connection)?;
    let inspection = inspect::inspect(&connection, &analysis.managed_schemas)?;
    report::target(&inspection.identity);

    if !inspection.missing_schemas.is_empty() {
        report::missing_schemas(&inspection.missing_schemas);
        return Ok(Outcome::Failed);
    }

    // pgschema reads this; keep it alive for the whole loop, since every
    // per-schema run is handed the same document (spec §5.4).
    let document = tempfile_for(&analysis.desired_state)?;
    if let Some(path) = out {
        write_desired_state(path, &analysis)?;
    }

    let mut failed = Vec::new();
    for schema in &analysis.order {
        report::schema_heading(schema);
        let ok = pgschema::run(
            &binary,
            Subcommand::Plan,
            &connection,
            schema,
            document.path(),
        )?;
        if !ok {
            failed.push(schema.clone());
        }
    }

    if !failed.is_empty() {
        report::pgschema_failed(&failed);
        return Ok(Outcome::Failed);
    }

    // The plans were computed and shown; the cycle still makes them
    // unappliable, so the command has not succeeded.
    Ok(if cycles.is_empty() {
        Outcome::Ok
    } else {
        Outcome::Failed
    })
}

/// Discover and analyze, printing diagnostics and returning `None` if the
/// source tree was rejected.
fn analyze_source(source: &SourceArgs) -> Result<Option<Analysis>> {
    let root = canonical_root(&source.source_root)?;
    let discovered = discovery::discover(&root, &source.exclude)?;

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
        // Not the author's doing: surface it as a real error, with a context
        // chain through anyhow rather than as a source-tree diagnostic.
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
