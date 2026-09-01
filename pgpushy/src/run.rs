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
use crate::plan_file::{Plan, Step};
use crate::provider::{self, PgschemaBin};
use crate::report;
use crate::{artifact, discovery, hazard, outdir, pgschema, seeds};
use anyhow::{Context, Result, bail};
use pgpushy_core::{Analysis, AnalysisError, Options, SchemaName, analyze};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// What a command decided the process should exit with.
///
/// Distinguished from an `Err` because a source tree pgpushy correctly
/// rejected is not a pgpushy failure: the diagnostics have already been
/// printed, and the caller only needs the status.
pub enum Outcome {
    Ok,
    Failed,
    /// The plans are valid and would apply, but they contain destructive
    /// changes the environment does not allow (spec §9.1). Distinct from
    /// `Failed`, because a dropped column and a broken tree route to
    /// different people.
    Destructive,
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::Failed => 1,
            Self::Destructive => 2,
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
    plan_out: Option<&Path>,
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

    // Spec §6.5: reported like a cycle — the plans are still computed and
    // shown, because the operator needs them, and the run fails at the end.
    let refused = &session.inspection.policies;
    if !refused.is_empty() {
        report::policies_refused(refused, false);
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

    // Spec §8.4: a step outside pgpushy's model means pgschema would touch
    // what the source tree cannot describe.
    let violations = unmanaged_violations(&plans);
    if !violations.is_empty() {
        report::unmanaged_steps(&violations, false);
    }

    if !session.analysis.seeds.is_empty() {
        report::seeds_planned(&session.analysis.seeds);
    }

    // The §9.1 classification, and — when asked — the §8.9 artifact.
    let summary = artifact::summarize(&plans, &session.inspection.identity);
    if let Some(dir) = plan_out {
        let plan_files: Vec<PathBuf> = (0..session.analysis.order.len())
            .map(|index| session.plan_path(index))
            .collect();
        let written = artifact::write(
            dir,
            &session.analysis.order,
            &plan_files,
            &summary,
            &pgschema_version(&session.binary),
            &session.analysis.seeds,
        )?;
        report::artifact_written(dir, written.len());
    }

    if !(cycles.is_empty() && hazards.is_empty() && refused.is_empty() && violations.is_empty()) {
        return Ok(Outcome::Failed);
    }

    // Spec §9.1: valid plans that would apply, but destroy. A distinct exit,
    // gated by the environment rather than by a flag.
    if summary.total.destructive > 0 && !session.connection.allow_destructive {
        report::destructive_gate(&summary);
        return Ok(Outcome::Destructive);
    }

    Ok(Outcome::Ok)
}

/// `pgpushy apply` — plan, review, approve, then apply the reviewed plans.
// Eight arguments: every one is a distinct CLI surface, and bundling them
// into a struct would only move the list.
#[allow(clippy::too_many_arguments)]
pub fn apply(
    target: &TargetArgs,
    pgschema_args: &PgschemaArgs,
    loaded: &Loaded,
    output: Output,
    out: Option<&Path>,
    plan_artifact: Option<&Path>,
    auto_approve: bool,
    lock_timeout: Option<&str>,
) -> Result<Outcome> {
    // Spec §8.9: applying an artifact reads no source tree at all.
    if let Some(dir) = plan_artifact {
        return apply_from_artifact(
            dir,
            target,
            pgschema_args,
            loaded,
            output,
            auto_approve,
            lock_timeout,
        );
    }

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

    // Spec §6.5: fatal here, before anything is touched.
    let refused = &session.inspection.policies;
    if !refused.is_empty() {
        report::policies_refused(refused, true);
        return Ok(Outcome::Failed);
    }

    // Spec §8.6 step 1: the full plan pass runs before anything is touched, so
    // failing to even compute a plan aborts with the target untouched.
    let Some(plans) = session.plan_pass()? else {
        return Ok(Outcome::Failed);
    };

    // Spec §8.4: fatal before approval — a step outside the model is a
    // change to something the source tree cannot describe.
    let violations = unmanaged_violations(&plans);
    if !violations.is_empty() {
        report::unmanaged_steps(&violations, true);
        return Ok(Outcome::Failed);
    }

    let hazards = session.hazards(&plans);
    if !hazards.is_empty() {
        report::hazards(&hazards, true);
        return Ok(Outcome::Failed);
    }

    match approve::confirm(
        &session.analysis.seeds,
        &session.analysis.empty_schemas,
        &plans,
        auto_approve,
    )? {
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
    let outcome = session.apply_pass(&plans, lock_timeout.as_deref())?;

    // Seeds run only once every schema has applied (spec §8.8): their tables
    // are guaranteed to exist, with the modeled shape, only then.
    let seeds = &session.analysis.seeds;
    if seeds.is_empty() {
        return Ok(outcome);
    }
    match outcome {
        Outcome::Ok => seeds::execute(&session.connection, seeds, lock_timeout.as_deref()),
        // apply_pass never yields Destructive, but a failed pass of any kind
        // means the seeds' tables cannot be trusted to exist (spec §8.8).
        other => {
            report::seeds_not_attempted(seeds);
            Ok(other)
        }
    }
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
    /// The synthesized documents, one per managed schema (spec §5.4). Held
    /// for the whole session; each per-schema run is handed its own.
    document_dir: TempDir,
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

        // Spec §10.4: an external plan database accumulates each run's
        // closure members, and the next run fails on them midway through the
        // loop. Refuse early and by name, before delegating anything.
        if let Some(plan_db) = &connection.plan_db {
            let leftovers = inspect::plan_db_leftovers(plan_db, &analysis.managed_schemas)?;
            if !leftovers.is_empty() {
                report::plan_db_leftovers(&leftovers, &plan_db.db);
                return Ok(Opened::Stop(Outcome::Failed));
            }
        }

        let document_dir = tempfile::Builder::new()
            .prefix("pgpushy-desired-")
            .tempdir()
            .context("creating a temporary directory for the synthesized documents")?;
        write_documents(document_dir.path(), &analysis)?;
        if output.verbose {
            report::desired_state_at(document_dir.path());
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
            document_dir,
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

    /// The document for the schema at `index` in apply order, indexed for the
    /// same reason as the plans above.
    fn document_path(&self, index: usize) -> PathBuf {
        self.document_dir
            .path()
            .join(format!("desired-{index}.sql"))
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
                &self.document_path(index),
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
    let seed_root = match &settings.seed_root {
        Some(configured) => Some(canonical_seed_root(configured, &root)?),
        None => None,
    };
    let discovered = discovery::discover(
        &root,
        &settings.exclude,
        seed_root.as_deref().filter(|seed| seed.starts_with(&root)),
    )?;
    let (seed_files, seed_generated) = match &seed_root {
        Some(seed) => {
            let found = discovery::discover(seed, &[], None)?;
            (found.files, found.generated)
        }
        None => (Vec::new(), 0),
    };

    let options = Options {
        default_schema: SchemaName::new(&settings.default_schema),
        managed_schemas: settings
            .managed_schemas
            .as_ref()
            .map(|schemas| schemas.iter().map(SchemaName::new).collect()),
    };

    report::configuration(loaded);
    report::discovery(&root, &discovered);
    if let Some(seed) = &seed_root {
        report::seed_discovery(seed, seed_files.len(), seed_generated);
    }
    if output.verbose {
        // The first thing to check when an exclusion is doing more or less
        // than expected.
        report::discovered_files(&discovered);
    }

    match analyze(&discovered.files, &seed_files, &options) {
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

/// Resolve and sanity-check the seed root against the source root (spec §4.6).
fn canonical_seed_root(configured: &Path, source_root: &Path) -> Result<PathBuf> {
    let canonical = configured
        .canonicalize()
        .with_context(|| format!("seed root {} does not exist", configured.display()))?;
    if !canonical.is_dir() {
        bail!(
            "seed root {} is not a directory\n\
             \n\
             seed_root names the directory pgpushy walks for seed files, not a \
             single file.",
            canonical.display(),
        );
    }
    if canonical == source_root {
        bail!(
            "seed_root and source_root are the same directory\n\
             \n\
             A seed file is not desired state (spec §4.6); give the seeds a \
             directory of their own, beside or inside the source root.",
        );
    }
    if source_root.starts_with(&canonical) {
        bail!(
            "source_root {} is inside seed_root {}\n\
             \n\
             Every schema file would then also be read as a seed and rejected by \
             the seed allow-list (spec §4.6). Keep the seed root beside or inside \
             the source root, never around it.",
            source_root.display(),
            canonical.display(),
        );
    }
    Ok(canonical)
}

/// `apply --plan <dir>` (spec §8.9): the artifact is the whole input.
///
/// No discovery, no synthesis — the apply order, the plans and the seeds all
/// come from the manifest, and the checks that must be fresh (§6.2, §6.5,
/// §8.4, identity) run against a new inspection before anything is touched.
fn apply_from_artifact(
    dir: &Path,
    target: &TargetArgs,
    pgschema_args: &PgschemaArgs,
    loaded: &Loaded,
    output: Output,
    auto_approve: bool,
    lock_timeout: Option<&str>,
) -> Result<Outcome> {
    let connection = Resolved::from(&loaded.environment(&target.env)?)?;
    let artifact = artifact::read(dir)?;
    report::artifact_read(dir, &artifact);

    let managed: Vec<SchemaName> = artifact
        .plans
        .iter()
        .map(|(schema, _)| schema.clone())
        .collect();

    let binary = provider::select(
        config::backend(&loaded.file)?,
        loaded.pgschema_path(pgschema_args.pgschema_path.as_deref()),
        loaded.file.pgschema.version.clone(),
        None,
    )?;
    report::pgschema(&binary);
    let current = pgschema_version(&binary);
    if current != artifact.manifest.pgschema_version {
        report::artifact_version_mismatch(&artifact.manifest.pgschema_version, &current);
    }

    report::password_from_file(&connection, loaded);
    let inspection = inspect::inspect(&connection, &managed)?;
    report::target(&connection, &inspection.identity);

    // Spec §8.9 step 2: identity, not drift — the fingerprint is silent
    // exactly when two databases are kept identical.
    if inspection.identity.database != artifact.manifest.target.database
        || inspection.identity.system_identifier != artifact.manifest.target.system_identifier
    {
        report::artifact_wrong_target(&artifact.manifest.target, &inspection.identity);
        return Ok(Outcome::Failed);
    }

    if !inspection.missing_schemas.is_empty() {
        report::missing_schemas(&inspection.missing_schemas);
        return Ok(Outcome::Failed);
    }

    // Spec §8.9 step 3: the fresh checks.
    if !inspection.policies.is_empty() {
        report::policies_refused(&inspection.policies, true);
        return Ok(Outcome::Failed);
    }
    let violations = unmanaged_violations(&artifact.plans);
    if !violations.is_empty() {
        report::unmanaged_steps(&violations, true);
        return Ok(Outcome::Failed);
    }
    let hazards = hazard::check(
        &inspection.cross_schema_foreign_keys,
        &artifact.plans,
        &managed,
    );
    if !hazards.is_empty() {
        report::hazards(&hazards, true);
        return Ok(Outcome::Failed);
    }

    let seeds = artifact.seeds();
    match approve::confirm(&seeds, &[], &artifact.plans, auto_approve)? {
        Decision::Declined => {
            report::declined();
            return Ok(Outcome::Ok);
        }
        Decision::Approved => {}
    }

    // Spec §8.9 step 5: the ignore file is part of a plan's identity, so
    // pgschema applies from a directory carrying the same one it planned
    // under.
    let workdir = tempfile::Builder::new()
        .prefix("pgpushy-apply-")
        .tempdir()
        .context("creating a working directory for pgschema")?;
    pgschema::write_ignore_file(workdir.path())?;

    let lock_timeout = lock_timeout
        .map(ToOwned::to_owned)
        .or_else(|| connection.lock_timeout.clone());

    let mut applied = Vec::new();
    for (index, (schema, plan)) in artifact.plans.iter().enumerate() {
        if plan.is_empty() {
            report::skipped_unchanged(schema);
            continue;
        }
        report::schema_heading(schema);
        let ok = pgschema::apply_plan(
            &binary,
            &connection,
            schema,
            &artifact.paths[index],
            lock_timeout.as_deref(),
            workdir.path(),
            output,
        )?;
        if !ok {
            let unattempted: Vec<_> = artifact.plans[index + 1..]
                .iter()
                .map(|(schema, _)| schema.clone())
                .collect();
            report::partial_apply(&applied, schema, &unattempted);
            if !seeds.is_empty() {
                report::seeds_not_attempted(&seeds);
            }
            return Ok(Outcome::Failed);
        }
        applied.push(schema.clone());
    }
    report::applied(&applied);

    if seeds.is_empty() {
        return Ok(Outcome::Ok);
    }
    seeds::execute(&connection, &seeds, lock_timeout.as_deref())
}

/// The version string recorded in and checked against an artifact (§8.9).
fn pgschema_version(binary: &PgschemaBin) -> String {
    binary
        .version
        .as_ref()
        .map(|version| version.to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Plan steps naming kinds outside pgpushy's model (spec §8.4).
fn unmanaged_violations(plans: &[(SchemaName, Plan)]) -> Vec<(SchemaName, Step)> {
    plans
        .iter()
        .flat_map(|(schema, plan)| {
            plan.unmanaged_steps()
                .into_iter()
                .map(|step| (schema.clone(), step.clone()))
        })
        .collect()
}

/// Write each schema's document where its pgschema run will read it.
///
/// Named by position in apply order rather than by schema, since a schema name
/// is a Postgres identifier and may contain path separators.
fn write_documents(dir: &Path, analysis: &Analysis) -> Result<()> {
    for (index, schema) in analysis.order.iter().enumerate() {
        let document = analysis
            .documents
            .get(schema)
            .expect("every schema in the apply order has a document");
        let path = dir.join(format!("desired-{index}.sql"));
        std::fs::write(&path, document).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Write the documents where the operator asked to see them (spec §8.7).
fn write_desired_state(path: &Path, analysis: &Analysis) -> Result<()> {
    let written = outdir::write(path, &analysis.documents)?;
    report::wrote(path, &written);
    Ok(())
}
