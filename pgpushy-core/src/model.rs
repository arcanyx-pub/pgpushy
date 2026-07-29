//! The object model: what pgpushy understands a source tree to contain.
//!
//! Scope is spec §4.3 — tables, indexes, table constraints, foreign keys and
//! comments — and nothing else. There is deliberately no catch-all variant:
//! a statement outside the allow-list becomes a diagnostic during parsing, not
//! an unmodelled node carried through synthesis (see [`crate::parse`]).

use pg_query::protobuf::{CommentStmt, Constraint, CreateStmt, IndexStmt};
use std::fmt;

/// Where a statement came from, for diagnostics.
///
/// Every statement carries one. Spec §4.3 and §4.5 require errors to name the
/// file and line, so an object that cannot say where it came from cannot
/// produce a compliant message.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Origin {
    /// Path relative to the source-tree root, as discovery reported it.
    pub file: String,
    /// 1-indexed line within that file.
    pub line: u32,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// A schema name, always resolved — never empty, never inferred later.
///
/// Unqualified objects are assigned the default schema during resolution
/// (spec §4.4), so by the time anything holds a `SchemaName` the question of
/// which schema an object belongs to has already been answered.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaName(String);

impl SchemaName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SchemaName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A schema-qualified object name.
///
/// Ordering is `(schema, name)` by byte comparison, which is what gives
/// synthesis its deterministic intra-category order (spec §11.3). Note that
/// identifier case folding has already happened in the parser: an unquoted
/// `Orders` arrives as `orders`, a quoted `"Orders"` as `Orders`, so plain
/// string equality is the correct comparison.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QualifiedName {
    pub schema: SchemaName,
    pub name: String,
}

impl QualifiedName {
    pub fn new(schema: SchemaName, name: impl Into<String>) -> Self {
        Self {
            schema,
            name: name.into(),
        }
    }
}

impl fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.schema, self.name)
    }
}

/// `CREATE SCHEMA` as written by the author.
///
/// pgpushy emits its own `CREATE SCHEMA IF NOT EXISTS` for every managed
/// schema (spec §5.2), so an authored one contributes nothing but its name to
/// the managed-schema set — it is honored, not echoed.
#[derive(Clone, Debug)]
pub struct SchemaDecl {
    pub name: SchemaName,
    pub origin: Origin,
}

/// A table, with its foreign keys already lifted out (spec §5.3).
///
/// `ast` is the original `CreateStmt` minus every foreign-key constraint, and
/// with its relation schema-qualified. It is re-emitted by deparsing.
#[derive(Clone, Debug)]
pub struct Table {
    pub name: QualifiedName,
    pub origin: Origin,
    pub ast: CreateStmt,
}

/// A foreign key, wherever the author wrote it.
///
/// Inline column constraints, table-level constraints and standalone
/// `ALTER TABLE … ADD CONSTRAINT` all land here identically; by this point
/// nothing records which shape it arrived in, because nothing downstream cares.
#[derive(Clone, Debug)]
pub struct ForeignKey {
    /// The referencing table.
    pub table: QualifiedName,
    /// The referenced table, schema resolved.
    pub referenced: QualifiedName,
    /// The author's constraint name, or `None`.
    ///
    /// `None` stays `None` all the way to the output: spec §5.3 forbids
    /// synthesizing a name, because Postgres must generate the same one in
    /// pgschema's plan database that it already generated on the target.
    pub name: Option<String>,
    /// The referencing columns, made explicit even when the author's
    /// column-level `REFERENCES` left them implied.
    pub columns: Vec<String>,
    pub origin: Origin,
    pub ast: Constraint,
}

/// An index. Depends on its table existing, hence category 3 (spec §5.1).
#[derive(Clone, Debug)]
pub struct Index {
    /// The index's own name, qualified by the schema of the table it indexes.
    pub name: QualifiedName,
    /// The table it indexes.
    pub table: QualifiedName,
    pub origin: Origin,
    pub ast: IndexStmt,
}

/// A standalone non-foreign-key `ALTER TABLE … ADD CONSTRAINT`.
///
/// `CHECK`, `UNIQUE`, `PRIMARY KEY` and `EXCLUDE` constraints written this way
/// rather than inline. Like indexes, they depend on their table (category 3).
#[derive(Clone, Debug)]
pub struct TableConstraint {
    pub table: QualifiedName,
    /// Constraint name if the author gave one. Used only for duplicate
    /// detection; the constraint is re-emitted from `ast` either way.
    pub name: Option<String>,
    pub origin: Origin,
    pub ast: Constraint,
}

/// A `COMMENT ON`, emitted last so it may reference anything (spec §5.1).
#[derive(Clone, Debug)]
pub struct Comment {
    /// The schema of the object being commented on, for per-schema attribution.
    pub schema: SchemaName,
    /// Rendered target, e.g. `TABLE public.orders`, for deterministic ordering
    /// and for diagnostics.
    pub target: String,
    pub origin: Origin,
    pub ast: CommentStmt,
}

/// Everything the allow-list admits, after parsing and schema resolution.
#[derive(Clone, Debug, Default)]
pub struct Objects {
    pub schemas: Vec<SchemaDecl>,
    pub tables: Vec<Table>,
    pub indexes: Vec<Index>,
    pub constraints: Vec<TableConstraint>,
    pub foreign_keys: Vec<ForeignKey>,
    pub comments: Vec<Comment>,
}

impl Objects {
    /// Every schema any object was assigned to, plus every declared schema.
    ///
    /// This is the raw material for the managed-schema set (spec §4.4); it is
    /// deliberately *not* the set itself, since a declaration in configuration
    /// may override it.
    pub fn mentioned_schemas(&self) -> Vec<SchemaName> {
        let mut out: Vec<SchemaName> = self
            .schemas
            .iter()
            .map(|s| s.name.clone())
            .chain(self.tables.iter().map(|t| t.name.schema.clone()))
            .chain(self.indexes.iter().map(|i| i.table.schema.clone()))
            .chain(self.constraints.iter().map(|c| c.table.schema.clone()))
            .chain(self.foreign_keys.iter().map(|f| f.table.schema.clone()))
            .chain(self.comments.iter().map(|c| c.schema.clone()))
            .collect();
        out.sort();
        out.dedup();
        out
    }
}
