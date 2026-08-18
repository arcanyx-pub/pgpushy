//! Getting hold of a usable `pgschema` binary (spec §8.5).
//!
//! Two backends. **Managed** downloads a pinned version and verifies it
//! against a hash pgpushy ships; it is the default, because "install pgschema
//! first" is a step pgpushy can remove. **BYO** takes a binary the operator
//! supplies and checks its version; it is the only option on Windows and in
//! air-gapped environments, so it is permanent rather than a stepping stone.
//!
//! [`select`] is the seam. Everything above it asks for "a pgschema" and gets
//! one; which backend answered is a configuration detail.

pub mod byo;
pub mod managed;

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

/// Which backend to use, per configuration (spec §8.5, §10.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Backend {
    /// Download and cache a pinned version. The default.
    #[default]
    Managed,
    /// Use a binary the operator supplies.
    Byo,
}

impl Backend {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "managed" => Ok(Self::Managed),
            "byo" => Ok(Self::Byo),
            other => anyhow::bail!(
                "unknown pgschema backend {other:?}\n\
                 \n\
                 Valid backends are \"managed\" (download and verify a pinned version, \
                 the default) and \"byo\" (a binary you supply)."
            ),
        }
    }
}

/// Pick a backend and resolve it.
///
/// An explicit path always means BYO, whatever the configured backend says:
/// naming a binary and then downloading a different one would be absurd, and
/// it saves anyone passing `--pgschema-path` from also setting the backend.
pub fn select(
    backend: Backend,
    explicit_path: Option<PathBuf>,
    version: Option<String>,
    cache_root: Option<PathBuf>,
) -> Result<PgschemaBin> {
    if explicit_path.is_some() || backend == Backend::Byo {
        return byo::Byo {
            explicit: explicit_path,
        }
        .resolve();
    }
    managed::Managed {
        version: version.unwrap_or_else(|| managed::PINNED_PGSCHEMA.to_owned()),
        cache_root,
    }
    .resolve()
}
