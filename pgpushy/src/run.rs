//! Command implementations.
//!
//! `plan` and `apply` are the same pipeline up to the point of decision, which
//! is why they share [`Session`] rather than each building their own. `apply`
//! is `plan` plus a gate: the same plans, reviewed, then handed back to
//! pgschema.

use crate::approve::{self, Decision};
use crate::cli::{PgschemaArgs, TargetArgs};
use crate::config::{self, Loaded, Settings};
use crate::conn::Resolved;
use crate::inspect::{self, Inspection};
use crate::output::Output;
use crate::plan_file::Plan;
use crate::provider::{self, PgschemaBin};
use crate::report;
use crate::{discovery, hazard, pgschema, tempfile_for};
use anyhow::{Context, Result, bail};
use pgpushy_core::{Analysis, AnalysisError, Options, SchemaName, analyze};
use std::path::{Path, PathBuf};
use tempfile::{NamedTempFile, TempDir};

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
pub fn validate(loaded: &Loaded, output: Output, out: Option<&Path>) -> Result<Outcome> {
    let settings = loaded.settings();
    let Some(analysis) = analyze_source(&settings, loaded, output)? else {
        return Ok(Outcome::Failed);
    };

    report::summary(&settings, &analysis);

    // Nothing named, nothing to do — and the reason is worth stating, since a
    // silent success here looks identical to a successful reconciliation.
    if analysis.managed_schemas.is_empty() {
        report::nothing_managed();
        return Ok(Outcome::Ok);
    }

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
    target: &TargetArgs,
    pgschema_args: &PgschemaArgs,
    loaded: &Loaded,
    output: Output,
    out: Option<&Path>,
) -> Result<Outcome> {
    let session = match Session::open(target, pgschema_args, loaded, output, out)? {
        Opened::Session(session) => session,
        Opened::Stop(outcome) => return Ok(outcome),
    };

    // Spec §7: a cycle means no apply order exists, but it does not stop
    // pgschema computing per-schema plans — and those plans are exactly what
    // the operator needs to break it. Report, keep going, fail at the end.
    let cycles = session.analysis.cycle_diagnostics();
    if !cycles.is_empty() {
        report::diagnostics(&cycles);
        report::plans_are_diagnostic_only();
    }

    let Some(plans) = session.plan_pass()? else {
        return Ok(Outcome::Failed);
    };

    // Reported rather than fatal: `plan` changes nothing, so the useful thing
    // is to show what would go wrong before the operator tries it.
    let hazards = session.hazards(&plans);
    if !hazards.is_empty() {
        report::hazards(&hazards, false);
    }

    Ok(if cycles.is_empty() && hazards.is_empty() {
        Outcome::Ok
    } else {
        Outcome::Failed
    })
}

/// `pgpushy apply` — plan, review, approve, then apply the reviewed plans.
pub fn apply(
    target: &TargetArgs,
    pgschema_args: &PgschemaArgs,
    loaded: &Loaded,
    output: Output,
    out: Option<&Path>,
    auto_approve: bool,
    lock_timeout: Option<&str>,
) -> Result<Outcome> {
    let session = match Session::open(target, pgschema_args, loaded, output, out)? {
        Opened::Session(session) => session,
        Opened::Stop(outcome) => return Ok(outcome),
    };

    // Fatal here, unlike `plan`: no schema order can apply a cycle, so there
    // is nothing to approve (spec §7, §12.1).
    let cycles = session.analysis.cycle_diagnostics();
    if !cycles.is_empty() {
        report::diagnostics(&cycles);
        report::cycle_blocks_apply();
        return Ok(Outcome::Failed);
    }

    // Spec §8.6 step 1: the full plan pass runs before anything is touched, so
    // failing to even compute a plan aborts with the target untouched.
    let Some(plans) = session.plan_pass()? else {
        return Ok(Outcome::Failed);
    };

    let hazards = session.hazards(&plans);
    if !hazards.is_empty() {
        report::hazards(&hazards, true);
        return Ok(Outcome::Failed);
    }

    match approve::confirm(&session.analysis, &plans, auto_approve)? {
        Decision::Declined => {
            report::declined();
            return Ok(Outcome::Ok);
        }
        Decision::Approved => {}
    }

    // The flag wins over the environment's setting. Safe precedence, unlike
    // project structure: a lock timeout cannot change what is reconciled, only
    // whether the apply gives up waiting (spec §10.5).
    let lock_timeout = lock_timeout
        .map(ToOwned::to_owned)
        .or_else(|| session.connection.lock_timeout.clone());
    session.apply_pass(&plans, lock_timeout.as_deref())
}

// ---------------------------------------------------------------------------
// The shared pipeline
// ---------------------------------------------------------------------------

/// What opening a session produced.
enum Opened {
    /// Ready to delegate.
    Session(Box<Session>),
    /// Already reported; the command should stop with this outcome. Carries
    /// the outcome because the reasons differ: a rejected source tree is a
    /// failure, an empty one is simply nothing to do.
    Stop(Outcome),
}

/// Everything `plan` and `apply` both need, established once.
struct Session {
    analysis: Analysis,
    binary: PgschemaBin,
    connection: Resolved,
    inspection: Inspection,
    output: Output,
    /// The synthesized desired state. Held for the whole session because every
    /// per-schema run is handed the same document (spec §5.4).
    document: NamedTempFile,
    /// Where pgschema writes the JSON plans.
    plan_dir: TempDir,
}

impl Session {
    /// Run the offline pipeline and everything that must hold before
    /// delegating. `None` means the problem has already been reported and the
    /// command should fail.
    fn open(
        target: &TargetArgs,
        pgschema_args: &PgschemaArgs,
        loaded: &Loaded,
        output: Output,
        out: Option<&Path>,
    ) -> Result<Opened> {
        // The target is resolved before anything else runs: naming a database
        // that does not exist in the configuration should fail immediately,
        // not after a source tree has been parsed and synthesized.
        let connection = Resolved::from(&loaded.environment(&target.env)?)?;

        let settings = loaded.settings();
        let Some(analysis) = analyze_source(&settings, loaded, output)? else {
            return Ok(Opened::Stop(Outcome::Failed));
        };
        report::summary(&settings, &analysis);

        let binary = provider::select(
            config::backend(&loaded.file)?,
            loaded.pgschema_path(pgschema_args.pgschema_path.as_deref()),
            loaded.file.pgschema.version.clone(),
            None,
        )?;
        report::pgschema(&binary);

        // Spec §10.2: the warning fires on use, not on presence, and this is
        // the moment the password is about to be used.
        report::password_from_file(&connection, loaded);
        let inspection = inspect::inspect(&connection, &analysis.managed_schemas)?;
        report::target(&connection, &inspection.identity);
        report::plan_database(&connection);

        // Nothing to reconcile is not a failure — pgpushy correctly determined
        // there is no work — so this stops successfully.
        if analysis.managed_schemas.is_empty() {
            report::nothing_managed();
            return Ok(Opened::Stop(Outcome::Ok));
        }

        if !inspection.missing_schemas.is_empty() {
            report::missing_schemas(&inspection.missing_schemas);
            return Ok(Opened::Stop(Outcome::Failed));
        }

        let document = tempfile_for(&analysis.desired_state)?;
        if output.verbose {
            report::desired_state_at(document.path());
        }
        if let Some(path) = out {
            write_desired_state(path, &analysis)?;
        }

        let plan_dir = tempfile::Builder::new()
            .prefix("pgpushy-plans-")
            .tempdir()
            .context("creating a temporary directory for pgschema's plans")?;
        // pgschema runs here, so this is also where its `.pgschemaignore`
        // comes from — pgpushy's, not whatever is in the operator's shell
        // directory.
        pgschema::write_ignore_file(plan_dir.path())?;

        Ok(Opened::Session(Box::new(Self {
            analysis,
            binary,
            connection,
            inspection,
            output,
            document,
            plan_dir,
        })))
    }

    /// Where pgschema writes the plan for the schema at `index` in apply order.
    ///
    /// Indexed rather than named after the schema, because a schema name is a
    /// Postgres identifier and may contain path separators.
    fn plan_path(&self, index: usize) -> PathBuf {
        self.plan_dir.path().join(format!("plan-{index}.json"))
    }

    /// Plan every managed schema, in apply order, keeping each plan.
    ///
    /// `None` means pgschema failed for at least one schema; it has already
    /// printed why.
    fn plan_pass(&self) -> Result<Option<Vec<(SchemaName, Plan)>>> {
        let mut plans = Vec::with_capacity(self.analysis.order.len());
        let mut failed = Vec::new();

        for (index, schema) in self.analysis.order.iter().enumerate() {
            report::schema_heading(schema);
            let json = self.plan_path(index);

            let ok = pgschema::plan(
                &self.binary,
                &self.connection,
                schema,
                self.document.path(),
                &json,
                self.plan_dir.path(),
                self.output,
            )?;
            if !ok {
                failed.push(schema.clone());
                continue;
            }

            plans.push((schema.clone(), Plan::read(&json)?));
        }

        if !failed.is_empty() {
            report::pgschema_failed(&failed);
            return Ok(None);
        }

        Ok(Some(plans))
    }

    /// Cross-schema removals the apply order cannot satisfy (spec §6.2).
    fn hazards(&self, plans: &[(SchemaName, Plan)]) -> Vec<hazard::Hazard> {
        hazard::check(
            &self.inspection.cross_schema_foreign_keys,
            plans,
            &self.analysis.order,
        )
    }

    /// Apply the reviewed plans, in order, stopping at the first failure.
    fn apply_pass(
        &self,
        plans: &[(SchemaName, Plan)],
        lock_timeout: Option<&str>,
    ) -> Result<Outcome> {
        let mut applied = Vec::new();

        for (index, (schema, plan)) in plans.iter().enumerate() {
            if plan.is_empty() {
                report::skipped_unchanged(schema);
                continue;
            }

            report::schema_heading(schema);
            let ok = pgschema::apply_plan(
                &self.binary,
                &self.connection,
                schema,
                &self.plan_path(index),
                lock_timeout,
                self.plan_dir.path(),
                self.output,
            )?;

            if !ok {
                // Spec §9: stop here. Later schemas may depend on this one, so
                // continuing would compound the failure rather than limit it.
                let unattempted: Vec<_> = plans[index + 1..]
                    .iter()
                    .map(|(schema, _)| schema.clone())
                    .collect();
                report::partial_apply(&applied, schema, &unattempted);
                return Ok(Outcome::Failed);
            }

            applied.push(schema.clone());
        }

        report::applied(&applied);
        Ok(Outcome::Ok)
    }
}

/// Discover and analyze, printing diagnostics and returning `None` if the
/// source tree was rejected.
fn analyze_source(
    settings: &Settings,
    loaded: &Loaded,
    output: Output,
) -> Result<Option<Analysis>> {
    let root = canonical_root(&settings.source_root)?;
    let discovered = discovery::discover(&root, &settings.exclude)?;

    let options = Options {
        default_schema: SchemaName::new(&settings.default_schema),
        managed_schemas: settings
            .managed_schemas
            .as_ref()
            .map(|schemas| schemas.iter().map(SchemaName::new).collect()),
    };

    report::configuration(loaded);
    report::discovery(&root, &discovered);
    if output.verbose {
        // The first thing to check when an exclusion is doing more or less
        // than expected.
        report::discovered_files(&discovered);
    }

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
    let canonical = root
        .canonicalize()
        .with_context(|| format!("source root {} does not exist", root.display()))?;

    // Easy to reach by pointing `source_root` at the one file a small project
    // has. Without this the walk fails with a bare "Not a directory".
    if !canonical.is_dir() {
        bail!(
            "source root {} is not a directory\n\
             \n\
             source_root names the directory pgpushy walks for *.sql files, not a \
             single file. Point it at the directory containing your schema.",
            canonical.display(),
        );
    }
    Ok(canonical)
}

fn write_desired_state(path: &Path, analysis: &Analysis) -> Result<()> {
    std::fs::write(path, &analysis.desired_state)
        .with_context(|| format!("writing {}", path.display()))?;
    report::wrote(path, &analysis.desired_state);
    Ok(())
}
