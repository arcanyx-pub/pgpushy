//! Order-free desired-state synthesis for [pgpushy].
//!
//! This crate is the pure half of pgpushy: it takes the *contents* of a source
//! tree — path and text, already read from disk by the caller — and produces
//! the single desired-state document that pgschema consumes, plus the order in
//! which schemas must be reconciled. It performs no IO, opens no connections,
//! and is deterministic: the same input always yields byte-identical output.
//!
//! [`analyze`] runs the whole offline pipeline, which is spec §3 stages 1–6
//! minus discovery:
//!
//! ```text
//! parse + allow-list  →  resolve schemas  →  check  →  order  →  synthesize
//! ```
//!
//! Every stage collects *all* of its problems before stopping, so a source
//! tree with five mistakes reports five. Stages do not run past a failure in
//! an earlier one, because the diagnostics would be noise: unresolved
//! references are meaningless when half the files failed to parse.
//!
//! The normative description of what this crate must do lives in
//! `docs/spec.md`; section references throughout point at it.
//!
//! [pgpushy]: https://github.com/arcanyx-pub/pgpushy

#![forbid(unsafe_code)]

pub mod error;
pub mod graph;
pub mod model;
pub mod parse;
pub mod resolve;
pub mod synth;
pub mod validate;

pub use error::{CoreError, Diagnostic, DiagnosticKind};
pub use graph::{Cycle, CycleEdge};
pub use model::{Origin, QualifiedName, SchemaName};
pub use parse::SourceFile;

/// How to interpret a source tree.
#[derive(Clone, Debug)]
pub struct Options {
    /// The schema unqualified objects belong to. `public` unless configured.
    ///
    /// This governs *assignment* only. It does not make the schema managed:
    /// a default schema with no objects in it is not reconciled (spec §4.4).
    pub default_schema: SchemaName,
    /// An authoritative managed-schema declaration, if configuration supplied
    /// one. `None` derives the set from the tree instead.
    pub managed_schemas: Option<Vec<SchemaName>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            default_schema: SchemaName::new("public"),
            managed_schemas: None,
        }
    }
}

/// What the offline pipeline produced.
#[derive(Clone, Debug)]
pub struct Analysis {
    /// The schemas pgpushy will reconcile, sorted.
    pub managed_schemas: Vec<SchemaName>,
    /// The order to reconcile them in: dependencies before dependents.
    pub order: Vec<SchemaName>,
    /// Cross-schema foreign-key cycles, if any.
    ///
    /// Returned as data rather than raised as an error because the right
    /// response differs by command: `apply` and `validate` refuse, while
    /// `plan` shows the plans anyway, since those plans are what the operator
    /// needs in order to break the cycle (spec §7).
    pub cycles: Vec<Cycle>,
    /// The synthesized desired state (spec §5).
    pub desired_state: String,
    /// What the tree contained, for reporting.
    pub counts: Counts,
}

impl Analysis {
    /// The cycles rendered as diagnostics, for a caller that treats them as
    /// fatal.
    pub fn cycle_diagnostics(&self) -> Vec<Diagnostic> {
        self.cycles.iter().map(Cycle::to_diagnostic).collect()
    }
}

/// A tally of what the source tree describes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub files: usize,
    pub tables: usize,
    pub indexes: usize,
    pub constraints: usize,
    pub foreign_keys: usize,
    pub comments: usize,
}

/// Why analysis stopped.
#[derive(Debug)]
pub enum AnalysisError {
    /// The source tree is wrong, in one or more ways the author can fix.
    Source(Vec<Diagnostic>),
    /// pgpushy failed. Not the author's doing.
    Internal(CoreError),
}

impl AnalysisError {
    /// The diagnostics, if this was a source-tree problem.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            Self::Source(diagnostics) => diagnostics,
            Self::Internal(_) => &[],
        }
    }
}

impl std::fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(diagnostics) => {
                write!(f, "{} problem(s) in the source tree", diagnostics.len())
            }
            Self::Internal(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for AnalysisError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(_) => None,
            Self::Internal(err) => Some(err),
        }
    }
}

impl From<CoreError> for AnalysisError {
    fn from(err: CoreError) -> Self {
        Self::Internal(err)
    }
}

/// Run the offline pipeline over a source tree.
///
/// Cross-schema cycles do **not** fail this call; see [`Analysis::cycles`].
pub fn analyze(files: &[SourceFile], options: &Options) -> Result<Analysis, AnalysisError> {
    let (objects, diagnostics) = parse::parse_files(files, &options.default_schema);
    if !diagnostics.is_empty() {
        return Err(AnalysisError::Source(diagnostics));
    }

    let managed = resolve::managed_schemas(&objects, options.managed_schemas.as_deref())
        .map_err(AnalysisError::Source)?;

    let diagnostics = validate::check(&objects, &managed);
    if !diagnostics.is_empty() {
        return Err(AnalysisError::Source(diagnostics));
    }

    let order = graph::order_schemas(&objects, &managed);
    let desired_state = synth::synthesize(&objects, &managed)?;

    Ok(Analysis {
        managed_schemas: managed,
        order: order.order,
        cycles: order.cycles,
        desired_state,
        counts: Counts {
            files: files.len(),
            tables: objects.tables.len(),
            indexes: objects.indexes.len(),
            constraints: objects.constraints.len(),
            foreign_keys: objects.foreign_keys.len(),
            comments: objects.comments.len(),
        },
    })
}
