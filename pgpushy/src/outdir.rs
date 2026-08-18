//! Writing the synthesized documents where someone can read them (spec §8.7).
//!
//! `--out` names a directory rather than a file, because there is one document
//! per managed schema. The differences between them — which closure members
//! each carries, and how each spells a name inside a string literal — are the
//! whole reason anyone reaches for `--out`.
//!
//! pgpushy owns the directory it is given. It will create one, and it will
//! delete documents it wrote on an earlier run, but it refuses outright if it
//! finds a file it cannot prove it wrote. `--out db/schema` pointed at a source
//! tree is an easy mistake to make, and it must not be the one that empties it.

use anyhow::{Context, Result, bail};
use pgpushy_core::{Documents, GENERATED_MARKER, SchemaName};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Write one `<schema>.sql` per managed schema into `dir`.
///
/// Returns the paths written, in the order the schemas were given.
pub fn write(dir: &Path, documents: &Documents) -> Result<Vec<PathBuf>> {
    let existing = claim(dir)?;

    let mut written = Vec::with_capacity(documents.len());
    let mut keep = BTreeSet::new();
    for (schema, document) in documents {
        let path = dir.join(file_name(schema));
        std::fs::write(&path, document).with_context(|| format!("writing {}", path.display()))?;
        keep.insert(path.clone());
        written.push(path);
    }

    // A schema dropped from `managed_schemas` leaves a document behind that
    // still reads as current. Only files pgpushy wrote are removed, which is
    // what `claim` has already established about every one of these.
    for stale in existing.difference(&keep) {
        std::fs::remove_file(stale)
            .with_context(|| format!("removing the stale document {}", stale.display()))?;
    }

    Ok(written)
}

/// Establish that `dir` is pgpushy's to write into, creating it if absent.
///
/// Returns the generated documents already there, which are the only files
/// [`write()`] may remove.
fn claim(dir: &Path) -> Result<BTreeSet<PathBuf>> {
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        return Ok(BTreeSet::new());
    }
    if !dir.is_dir() {
        bail!(
            "--out names {}, which is a file\n\
             \n\
             pgpushy synthesizes one document per managed schema, so --out takes a \
             directory to write them into.",
            dir.display()
        );
    }

    let mut generated = BTreeSet::new();
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        if !is_generated(&path)? {
            bail!(
                "--out names {}, which holds {} — a file pgpushy did not write\n\
                 \n\
                 pgpushy writes into a directory it owns, and removes its own stale \
                 documents from it. Name an empty directory, a new one, or one that \
                 holds only documents from a previous run.",
                dir.display(),
                entry.file_name().to_string_lossy(),
            );
        }
        generated.insert(path);
    }
    Ok(generated)
}

/// Whether `path` is a document pgpushy wrote.
///
/// A directory is never one, and neither is a file whose first bytes are
/// something else — including a file too short to hold the marker at all.
fn is_generated(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        // Unreadable is not provably ours, which is the answer that refuses.
        Err(_) => return Ok(false),
    };
    Ok(contents.starts_with(GENERATED_MARKER.as_bytes()))
}

/// The file name for a schema's document.
///
/// A schema name is a Postgres identifier: it may hold a path separator, a
/// leading dot, or anything else a filesystem gives meaning to. Encoding
/// everything outside a conservative set keeps the name readable in every real
/// case — `billing` stays `billing.sql` — while making it impossible for a
/// legal name to address anything outside the directory.
fn file_name(schema: &SchemaName) -> String {
    let mut name = String::new();
    for byte in schema.as_str().bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            name.push(char::from(byte));
        } else {
            name.push_str(&format!("%{byte:02X}"));
        }
    }
    name.push_str(".sql");
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn documents(pairs: &[(&str, &str)]) -> Documents {
        pairs
            .iter()
            .map(|(schema, body)| {
                (
                    SchemaName::new(*schema),
                    format!("{GENERATED_MARKER}\n{body}\n"),
                )
            })
            .collect()
    }

    #[test]
    fn names_ordinary_schemas_plainly() {
        assert_eq!(file_name(&SchemaName::new("billing")), "billing.sql");
        assert_eq!(file_name(&SchemaName::new("my_schema")), "my_schema.sql");
        assert_eq!(file_name(&SchemaName::new("a-b")), "a-b.sql");
    }

    #[test]
    fn a_name_cannot_escape_the_directory() {
        assert_eq!(file_name(&SchemaName::new("we/rd")), "we%2Frd.sql");
        assert_eq!(file_name(&SchemaName::new("..")), "%2E%2E.sql");
        assert_eq!(file_name(&SchemaName::new("a b")), "a%20b.sql");
        // Encoding `%` itself is what keeps the mapping injective.
        assert_eq!(file_name(&SchemaName::new("%2F")), "%252F.sql");
    }

    #[test]
    fn creates_the_directory_and_writes_one_file_per_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested/out");
        let written = write(&dir, &documents(&[("public", "-- p"), ("billing", "-- b")])).unwrap();

        assert_eq!(written.len(), 2);
        assert!(dir.join("public.sql").is_file());
        assert!(dir.join("billing.sql").is_file());
        assert!(
            std::fs::read_to_string(dir.join("public.sql"))
                .unwrap()
                .contains("-- p")
        );
    }

    #[test]
    fn removes_a_document_for_a_schema_no_longer_managed() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("out"),
            &documents(&[("public", "-- p"), ("gone", "-- g")]),
        )
        .unwrap();
        let dir = tmp.path().join("out");
        assert!(dir.join("gone.sql").is_file());

        write(&dir, &documents(&[("public", "-- p")])).unwrap();
        assert!(dir.join("public.sql").is_file());
        assert!(!dir.join("gone.sql").exists());
    }

    #[test]
    fn refuses_a_directory_holding_a_file_pgpushy_did_not_write() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("customers.sql"),
            "CREATE TABLE customers (id int);",
        )
        .unwrap();

        let err = write(dir, &documents(&[("public", "-- p")])).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("customers.sql"), "got: {message}");
        assert!(message.contains("did not write"), "got: {message}");
        // And it refused before touching anything.
        assert!(!dir.join("public.sql").exists());
    }

    #[test]
    fn refuses_a_path_that_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.sql");
        std::fs::write(&path, "").unwrap();

        let err = write(&path, &documents(&[("public", "-- p")])).unwrap_err();
        assert!(format!("{err:#}").contains("which is a file"));
    }

    #[test]
    fn refuses_a_directory_holding_a_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let err = write(tmp.path(), &documents(&[("public", "-- p")])).unwrap_err();
        assert!(format!("{err:#}").contains("did not write"));
    }
}
