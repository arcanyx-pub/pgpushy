//! The bring-your-own backend: an operator-supplied pgschema (spec §8.5).

use super::{MIN_PGSCHEMA, PgschemaBin, PgschemaProvider};
use anyhow::{Context, Result, bail};
use semver::Version;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolves pgschema from an explicit path, or from `PATH`.
pub struct Byo {
    /// A path the operator named, if any.
    pub explicit: Option<PathBuf>,
}

impl PgschemaProvider for Byo {
    fn resolve(&self) -> Result<PgschemaBin> {
        let path = match &self.explicit {
            Some(path) => {
                if !path.exists() {
                    bail!("no pgschema binary at {}", path.display());
                }
                path.clone()
            }
            None => which::which("pgschema").context(
                "pgschema was not found on PATH; install it, or pass --pgschema-path \
                 (see https://www.pgschema.com/installation)",
            )?,
        };

        let version = read_version(&path)?;
        enforce_floor(&path, version.as_ref())?;
        Ok(PgschemaBin { path, version })
    }
}

/// Read the version pgschema prints in its help output.
///
/// pgschema has no `--version` flag and no `version` subcommand, so the only
/// source is the `Version: X.Y.Z@hash os/arch timestamp` line in `--help`.
fn read_version(path: &Path) -> Result<Option<Version>> {
    let output = Command::new(path)
        .arg("--help")
        .output()
        .with_context(|| format!("running {} --help", path.display()))?;

    // Some tools print help to stderr; take whichever stream has the line.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    Ok(parse_version(&text))
}

/// Pull a semver out of the `Version:` line, if it is there and readable.
fn parse_version(help: &str) -> Option<Version> {
    let line = help
        .lines()
        .find_map(|line| line.trim().strip_prefix("Version:"))?;
    // The line continues past the version with `@hash os/arch timestamp`, so
    // take the leading token and drop any build suffix from it.
    let token = line.split_whitespace().next()?;
    let number = token.split('@').next()?;
    Version::parse(number).ok()
}

fn enforce_floor(path: &Path, version: Option<&Version>) -> Result<()> {
    let minimum = Version::parse(MIN_PGSCHEMA).expect("MIN_PGSCHEMA is valid semver");

    let Some(version) = version else {
        // Spec §8.5: unparseable is a warning, not a failure.
        eprintln!(
            "warning: could not read a version from `{} --help`; \
             pgpushy requires pgschema >= {minimum} and cannot confirm this binary meets it",
            path.display(),
        );
        return Ok(());
    };

    if *version < minimum {
        // Spec §13: no override. The floor is the version pgpushy is tested
        // against, and the remedy is a supported binary rather than a flag.
        bail!(
            "pgschema {version} at {} is below the minimum {minimum}\n\
             \n\
             pgpushy requires pgschema >= {minimum} — the version it is tested against.\n\
             Upgrade pgschema, or point --pgschema-path at a supported binary.",
            path.display(),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn reads_the_version_pgschema_actually_prints() {
        // Verbatim from `pgschema --help` at 1.12.0.
        let help = "Declarative schema migration for Postgres\n\
                    \n\
                    Version: 1.12.0@62d09975 linux/amd64 2026-07-06 13:32:02\n\
                    \n\
                    Commands:\n";
        assert_eq!(parse_version(help).unwrap().to_string(), "1.12.0");
    }

    #[test]
    fn tolerates_a_plain_version_with_no_build_suffix() {
        assert_eq!(
            parse_version("Version: 2.0.1\n").unwrap().to_string(),
            "2.0.1"
        );
    }

    #[test]
    fn returns_none_when_the_line_is_missing_or_unreadable() {
        assert!(parse_version("some other help text\n").is_none());
        assert!(parse_version("Version: not-a-version\n").is_none());
        assert!(parse_version("Version:\n").is_none());
    }
}
