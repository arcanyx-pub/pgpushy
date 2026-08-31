//! Diagnostics.
//!
//! Two shapes of failure, deliberately distinguished:
//!
//! - [`Diagnostic`] — something wrong with the *source tree*. These are
//!   user-facing, always carry at least one [`Origin`], and are reported as a
//!   complete list rather than one at a time (spec §4.3, §4.5, and the "name
//!   every instance" convention in impl-plan §12).
//! - [`CoreError`] — something wrong with pgpushy or its environment: a
//!   deparse failure, a caller passing garbage. These are bugs or unsupported
//!   inputs, not user mistakes in SQL.

use crate::model::Origin;
use std::fmt;

/// A problem with the source tree, addressed to the person who wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    /// One line, no trailing period: "unsupported statement: CREATE VIEW".
    pub message: String,
    /// Every place implicated. A duplicate object names both definitions; an
    /// unsupported statement names one.
    pub origins: Vec<Origin>,
    /// What to do about it. Present whenever there is a concrete next step.
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn new(kind: DiagnosticKind, message: impl Into<String>, origins: Vec<Origin>) -> Self {
        Self {
            kind,
            message: message.into(),
            origins,
            help: None,
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        for origin in &self.origins {
            write!(f, "\n  at {origin}")?;
        }
        if let Some(help) = &self.help {
            write!(f, "\n  help: {help}")?;
        }
        Ok(())
    }
}

/// What kind of source-tree problem a [`Diagnostic`] reports.
///
/// Callers switch on this to decide severity — notably a
/// [`CrossSchemaForeignKeyCycle`](DiagnosticKind::CrossSchemaForeignKeyCycle),
/// which is fatal for `apply` and `validate` but only reported by `plan`
/// (spec §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticKind {
    /// libpg_query could not parse the file at all (spec §4.2).
    ParseFailure,
    /// A statement outside the allow-list (spec §4.3).
    UnsupportedStatement,
    /// A `CREATE SCHEMA` in the nested-element or `AUTHORIZATION`-only form
    /// (spec §4.3).
    UnsupportedSchemaForm,
    /// An object name inside a string literal that does not name its schema
    /// (spec §4.3).
    UnqualifiedNameLiteral,
    /// The same object defined more than once (spec §4.5).
    DuplicateObject,
    /// A foreign key referencing a table the source tree never defines
    /// (spec §4.5).
    UnresolvedReference,
    /// Two unnamed foreign keys competing for one generated name (spec §12.4).
    CollidingUnnamedForeignKeys,
    /// An object assigned to a schema that `managed_schemas` omits (spec §4.4).
    SchemaNotManaged,
    /// A reference across a schema boundary that is not a foreign key
    /// (spec §12.6).
    CrossSchemaReference,
    /// Managed schemas whose foreign keys form a cycle (spec §7, §12.1).
    CrossSchemaForeignKeyCycle,
    /// A seed statement outside the seed allow-list (spec §4.6).
    SeedDisallowedStatement,
    /// A seed naming a table, column or conflict target the model does not
    /// hold (spec §4.6).
    SeedTargetMismatch,
}

impl DiagnosticKind {
    /// A short, stable slug, for machine-readable output.
    pub fn slug(self) -> &'static str {
        match self {
            Self::ParseFailure => "parse-failure",
            Self::UnsupportedStatement => "unsupported-statement",
            Self::UnsupportedSchemaForm => "unsupported-schema-form",
            Self::UnqualifiedNameLiteral => "unqualified-name-literal",
            Self::DuplicateObject => "duplicate-object",
            Self::UnresolvedReference => "unresolved-reference",
            Self::CollidingUnnamedForeignKeys => "colliding-unnamed-foreign-keys",
            Self::SchemaNotManaged => "schema-not-managed",
            Self::CrossSchemaReference => "cross-schema-reference",
            Self::CrossSchemaForeignKeyCycle => "cross-schema-foreign-key-cycle",
            Self::SeedDisallowedStatement => "seed-disallowed-statement",
            Self::SeedTargetMismatch => "seed-target-mismatch",
        }
    }
}

/// A failure inside pgpushy rather than in the user's SQL.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// libpg_query refused to render a statement pgpushy built.
    ///
    /// This means synthesis produced an AST Postgres does not consider valid,
    /// which is a pgpushy bug rather than anything the author did.
    #[error("failed to render synthesized SQL for {context}: {source}")]
    Deparse {
        context: String,
        #[source]
        source: Box<pg_query::Error>,
    },
    /// A name literal kept its schema in the document for that very schema.
    ///
    /// Spec §5.4 requires it to be de-qualified there, because the schema's
    /// objects live in a scratch schema by the time pgschema executes the
    /// document. Reaching every literal means walking every node kind an
    /// expression can hold, and this is what the walk missing one looks like:
    /// a pgpushy bug, reported here rather than emitted for pgschema to fail
    /// on with a relation name the author never wrote.
    #[error(
        "the name '{literal}' in {context} kept its schema in that schema's own document; \
         this is a pgpushy bug — please report it with the statement that caused it"
    )]
    QualifiedLiteral { context: String, literal: String },
    /// Types, domains or sequences that depend on each other in a circle.
    ///
    /// Postgres will not create one, so this means pgpushy derived a
    /// dependency that is not real. Emitting them in some arbitrary order
    /// would produce a document that cannot execute.
    #[error(
        "these types depend on each other in a circle: {}; \
         this is a pgpushy bug — please report it with the statements involved",
        names.join(", ")
    )]
    TypeCycle { names: Vec<String> },
}
