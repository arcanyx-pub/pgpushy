//! Getting hold of a usable `pgschema` binary (spec §8.5).
//!
//! Two backends are planned. **BYO** takes a binary the operator supplies and
//! checks its version; it is the only option on Windows and in air-gapped
//! environments, so it is permanent rather than a stepping stone. **Managed**
//! downloads a pinned version and verifies it against a hash pgpushy ships,
//! and is intended to become the default. Only BYO exists so far.
//!
//! The seam is this module: adding the managed backend changes which
//! implementation `resolve` returns and nothing else.

pub mod byo;

use anyhow::Result;
use semver::Version;
use std::path::PathBuf;

/// The minimum pgschema pgpushy supports.
///
/// Spec §13: this is the version pgpushy is *tested against*, not the oldest
/// that would work — the relied-upon foreign-key deferral landed in v1.4.2.
/// Raising this constant and raising the CI matrix are the same action, and
/// there is deliberately no override.
pub const MIN_PGSCHEMA: &str = "1.12.0";

/// A resolved pgschema binary.
pub struct PgschemaBin {
    pub path: PathBuf,
    /// The parsed version, or `None` if the `Version:` line could not be read.
    ///
    /// Unparseable is a warning rather than a failure: that line is a
    /// human-readable string, not a stability contract, and refusing to run
    /// because its formatting changed would be pgpushy breaking itself.
    pub version: Option<Version>,
}

/// Something that can produce a runnable pgschema.
pub trait PgschemaProvider {
    fn resolve(&self) -> Result<PgschemaBin>;
}
