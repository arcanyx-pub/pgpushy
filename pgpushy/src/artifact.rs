//! The plan artifact (spec §8.9): a plan pass persisted, reviewed, and
//! applied exactly.
//!
//! `plan --plan-out` writes it; `apply --plan` consumes it with no source
//! tree in sight — the apply order lives in the manifest, never re-derived,
//! and the seed statements ride along verbatim so the seeds that run are the
//! seeds that were reviewed. Every plan file is hash-pinned by its manifest
//! entry, and the manifest records which target the plans were computed
//! against, because pgschema's fingerprint covers target *drift*, not target
//! *identity*.

use crate::inspect::Identity;
use crate::plan_file::Plan;
use anyhow::{Context, Result, bail};
use pgpushy_core::seed::{SeedFile, SeedStatement};
use pgpushy_core::{Origin, QualifiedName, SchemaName, Seeds};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Bumped when the manifest's shape changes incompatibly. A reader MUST
/// refuse a version it does not know: guessing at a future format is how an
/// approved artifact applies as something nobody reviewed.
pub const FORMAT_VERSION: u32 = 1;

const MANIFEST: &str = "manifest.json";
const SUMMARY: &str = "summary.json";

/// `manifest.json` — the artifact's spine (spec §8.9).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    /// Which database the plans were computed against (spec §6.3).
    pub target: TargetIdentity,
    /// The pgschema version that planned; a differing one at apply is
    /// warned about (spec §8.9).
    pub pgschema_version: String,
    /// The managed schemas in apply order, each pinning its plan file.
    pub schemas: Vec<SchemaEntry>,
    /// The checked seed statements (spec §4.6, §8.8), verbatim.
    pub seeds: Vec<SeedFileEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetIdentity {
    pub database: String,
    pub system_identifier: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaEntry {
    pub schema: String,
    /// The plan's file name within the artifact directory.
    pub plan: String,
    /// SHA-256 of the plan file, hex — a plan file and its manifest entry
    /// cannot disagree silently.
    pub sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedFileEntry {
    pub path: String,
    pub statements: Vec<SeedStatementEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedStatementEntry {
    pub line: u32,
    pub table: String,
    pub sql: String,
}

/// `summary.json` — the §9.1 machine-readable classification.
#[derive(Debug, Serialize, Deserialize)]
pub struct Summary {
    pub version: u32,
    pub target: TargetIdentity,
    pub schemas: Vec<SchemaSummary>,
    pub destructive: Vec<DestructiveStep>,
    pub total: Totals,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SchemaSummary {
    pub schema: String,
    pub steps: usize,
    pub destructive: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DestructiveStep {
    pub schema: String,
    /// `drop.` plus pgschema's own step type: `drop.table.column`.
    pub kind: String,
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Totals {
    pub steps: usize,
    pub destructive: usize,
}

/// Build the §9.1 summary from a plan pass.
pub fn summarize(plans: &[(SchemaName, Plan)], identity: &Identity) -> Summary {
    let mut schemas = Vec::with_capacity(plans.len());
    let mut destructive = Vec::new();
    for (schema, plan) in plans {
        let drops = plan.destructive_drops();
        schemas.push(SchemaSummary {
            schema: schema.as_str().to_owned(),
            steps: plan.step_count(),
            destructive: drops.len(),
        });
        for step in drops {
            destructive.push(DestructiveStep {
                schema: schema.as_str().to_owned(),
                kind: format!("drop.{}", step.kind),
                path: step.path.clone(),
            });
        }
    }
    Summary {
        version: FORMAT_VERSION,
        target: TargetIdentity {
            database: identity.database.clone(),
            system_identifier: identity.system_identifier.clone(),
        },
        total: Totals {
            steps: schemas.iter().map(|s| s.steps).sum(),
            destructive: destructive.len(),
        },
        schemas,
        destructive,
    }
}

/// Write the artifact (spec §8.9). `plan_files[i]` holds the raw plan
/// pgschema wrote for `order[i]`.
pub fn write(
    dir: &Path,
    order: &[SchemaName],
    plan_files: &[PathBuf],
    summary: &Summary,
    pgschema_version: &str,
    seeds: &Seeds,
) -> Result<Vec<PathBuf>> {
    prepare_directory(dir)?;

    let mut written = Vec::new();
    let mut schemas = Vec::with_capacity(order.len());
    for (schema, source) in order.iter().zip(plan_files) {
        let bytes = std::fs::read(source)
            .with_context(|| format!("reading the plan pgschema wrote to {}", source.display()))?;
        let name = plan_file_name(schema);
        let path = dir.join(&name);
        std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
        schemas.push(SchemaEntry {
            schema: schema.as_str().to_owned(),
            plan: name,
            sha256: hex_sha256(&bytes),
        });
        written.push(path);
    }

    let manifest = Manifest {
        version: FORMAT_VERSION,
        target: TargetIdentity {
            database: summary.target.database.clone(),
            system_identifier: summary.target.system_identifier.clone(),
        },
        pgschema_version: pgschema_version.to_owned(),
        schemas,
        seeds: seeds
            .files
            .iter()
            .map(|file| SeedFileEntry {
                path: file.path.clone(),
                statements: file
                    .statements
                    .iter()
                    .map(|stmt| SeedStatementEntry {
                        line: stmt.origin.line,
                        table: stmt.table.to_string(),
                        sql: stmt.sql.clone(),
                    })
                    .collect(),
            })
            .collect(),
    };

    for (name, json) in [
        (MANIFEST, serde_json::to_string_pretty(&manifest)?),
        (SUMMARY, serde_json::to_string_pretty(summary)?),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, json + "\n")
            .with_context(|| format!("writing {}", path.display()))?;
        written.push(path);
    }

    prune(dir, &written)?;
    Ok(written)
}

/// What `apply --plan` reads back: the manifest, with every plan file
/// verified against its hash (spec §8.9).
#[derive(Debug)]
pub struct ReadArtifact {
    pub manifest: Manifest,
    /// `(schema, parsed plan)`, in apply order.
    pub plans: Vec<(SchemaName, Plan)>,
    /// The raw plan files, parallel to `plans` — what pgschema is handed.
    pub paths: Vec<PathBuf>,
}

impl ReadArtifact {
    /// The manifest's seeds, rebuilt for §8.8 execution.
    pub fn seeds(&self) -> Seeds {
        Seeds {
            files: self
                .manifest
                .seeds
                .iter()
                .map(|file| SeedFile {
                    path: file.path.clone(),
                    statements: file
                        .statements
                        .iter()
                        .map(|stmt| SeedStatement {
                            origin: Origin {
                                file: file.path.clone(),
                                line: stmt.line,
                            },
                            sql: stmt.sql.clone(),
                            table: parse_qualified(&stmt.table),
                        })
                        .collect(),
                })
                .collect(),
            // Collision analysis is validate's business; the artifact
            // carries what was already checked.
            do_update_collisions: Vec::new(),
        }
    }
}

/// Read and verify an artifact (spec §8.9).
pub fn read(dir: &Path) -> Result<ReadArtifact> {
    let manifest_path = dir.join(MANIFEST);
    let text = std::fs::read_to_string(&manifest_path).with_context(|| {
        format!(
            "reading {} — is {} a plan artifact written by `pgpushy plan --plan-out`?",
            manifest_path.display(),
            dir.display(),
        )
    })?;
    let manifest: Manifest = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    if manifest.version != FORMAT_VERSION {
        bail!(
            "{} is a version-{} artifact, and this pgpushy reads version {}\n\
             \n\
             Guessing at another format could apply something nobody reviewed. \
             Re-plan with this pgpushy, or apply with the one that wrote it.",
            dir.display(),
            manifest.version,
            FORMAT_VERSION,
        );
    }

    let mut plans = Vec::with_capacity(manifest.schemas.len());
    let mut paths = Vec::with_capacity(manifest.schemas.len());
    for entry in &manifest.schemas {
        let path = dir.join(&entry.plan);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let actual = hex_sha256(&bytes);
        if actual != entry.sha256 {
            bail!(
                "{} does not match its manifest hash\n\
                 \n\
                 The plan file changed after the artifact was written. What would \
                 apply is not what was reviewed, so nothing is applied; re-run \
                 `pgpushy plan --plan-out` to produce a fresh artifact.",
                path.display(),
            );
        }
        let plan: Plan = serde_json::from_str(&String::from_utf8_lossy(&bytes))
            .with_context(|| format!("parsing {}", path.display()))?;
        plans.push((SchemaName::new(&entry.schema), plan));
        paths.push(path);
    }

    Ok(ReadArtifact {
        manifest,
        plans,
        paths,
    })
}

/// The artifact directory must be pgpushy's alone, by §8.7's rule; the
/// manifest is the mark, since JSON cannot carry §4.1's marker.
fn prepare_directory(dir: &Path) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        return Ok(());
    }
    let has_manifest = dir.join(MANIFEST).exists();
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .peekable();
    if entries.peek().is_some() && !has_manifest {
        bail!(
            "{} exists and is not a plan artifact directory\n\
             \n\
             pgpushy owns the directory --plan-out names: it prunes stale plans \
             from it, so it refuses one holding files it cannot prove it wrote. \
             Point --plan-out at a new or empty directory.",
            dir.display(),
        );
    }
    Ok(())
}

/// Remove files a previous artifact left that this write did not produce, so
/// a schema dropped from the managed set leaves no plan behind that reads as
/// current (§8.7's rule).
fn prune(dir: &Path, written: &[PathBuf]) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_file() && !written.contains(&path) {
            std::fs::remove_file(&path)
                .with_context(|| format!("pruning stale {}", path.display()))?;
        }
    }
    Ok(())
}

/// `<schema>.json`, percent-encoded by §8.7's rule.
fn plan_file_name(schema: &SchemaName) -> String {
    let mut name = String::new();
    for byte in schema.as_str().bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            name.push(char::from(byte));
        } else {
            name.push_str(&format!("%{byte:02X}"));
        }
    }
    name.push_str(".json");
    name
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// A manifest table name is `schema.name` as pgpushy printed it. Splitting on
/// the first dot is correct because pgpushy wrote it from a `QualifiedName`.
fn parse_qualified(name: &str) -> QualifiedName {
    match name.split_once('.') {
        Some((schema, object)) => QualifiedName::new(SchemaName::new(schema), object),
        None => QualifiedName::new(SchemaName::new("public"), name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_artifact_reads_back_verified() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan_src = dir.path().join("src-plan.json");
        std::fs::write(
            &plan_src,
            r#"{"groups":[{"steps":[{"type":"table","operation":"create","path":"a.t"}]}]}"#,
        )
        .unwrap();

        let out = dir.path().join("artifact");
        let order = [SchemaName::new("a")];
        let plans = vec![(
            SchemaName::new("a"),
            serde_json::from_str::<Plan>(
                r#"{"groups":[{"steps":[{"type":"table","operation":"create","path":"a.t"}]}]}"#,
            )
            .unwrap(),
        )];
        let identity = crate::inspect::Identity {
            database: "shop".into(),
            server: "x:1".into(),
            system_identifier: "123".into(),
        };
        let summary = summarize(&plans, &identity);
        write(
            &out,
            &order,
            &[plan_src],
            &summary,
            "1.12.3",
            &Seeds::default(),
        )
        .unwrap();

        let artifact = read(&out).unwrap();
        assert_eq!(artifact.manifest.version, FORMAT_VERSION);
        assert_eq!(artifact.manifest.target.database, "shop");
        assert_eq!(artifact.manifest.pgschema_version, "1.12.3");
        assert_eq!(artifact.plans.len(), 1);
        assert_eq!(artifact.plans[0].0.as_str(), "a");
        assert_eq!(artifact.plans[0].1.step_count(), 1);
    }

    #[test]
    fn a_doctored_plan_file_is_refused_by_hash() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan_src = dir.path().join("src-plan.json");
        std::fs::write(&plan_src, r#"{"groups":null}"#).unwrap();
        let out = dir.path().join("artifact");
        let order = [SchemaName::new("a")];
        let plans = vec![(
            SchemaName::new("a"),
            serde_json::from_str::<Plan>(r#"{"groups":null}"#).unwrap(),
        )];
        let identity = crate::inspect::Identity {
            database: "shop".into(),
            server: "x:1".into(),
            system_identifier: "123".into(),
        };
        let summary = summarize(&plans, &identity);
        write(
            &out,
            &order,
            &[plan_src],
            &summary,
            "1.12.3",
            &Seeds::default(),
        )
        .unwrap();

        std::fs::write(
            out.join("a.json"),
            r#"{"groups":[{"steps":[{"type":"table","operation":"drop","path":"a.t"}]}]}"#,
        )
        .unwrap();
        let err = read(&out).unwrap_err().to_string();
        assert!(err.contains("does not match its manifest hash"), "{err}");
    }

    #[test]
    fn an_unknown_format_version_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("manifest.json"),
            r#"{"version":99,"target":{"database":"d","system_identifier":"1"},
               "pgschema_version":"1.12.3","schemas":[],"seeds":[]}"#,
        )
        .unwrap();
        let err = read(dir.path()).unwrap_err().to_string();
        assert!(err.contains("version-99 artifact"), "{err}");
    }

    #[test]
    fn a_foreign_directory_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("precious.sql"), "hands off").unwrap();
        let identity = crate::inspect::Identity {
            database: "d".into(),
            server: "x:1".into(),
            system_identifier: "1".into(),
        };
        let summary = summarize(&[], &identity);
        let err = write(dir.path(), &[], &[], &summary, "1.12.3", &Seeds::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a plan artifact directory"), "{err}");
        assert!(dir.path().join("precious.sql").exists());
    }

    /// The destructive classification is §8.6's: a recreated pair is not
    /// destructive, and kinds are `drop.` + pgschema's type.
    #[test]
    fn the_summary_classifies_like_the_approval_gate() {
        let plan: Plan = serde_json::from_str(
            r#"{"groups":[{"steps":[
                {"type":"table.constraint","operation":"drop","path":"a.t.k"},
                {"type":"table.constraint","operation":"create","path":"a.t.k"},
                {"type":"table.column","operation":"drop","path":"a.t.old"}
            ]}]}"#,
        )
        .unwrap();
        let identity = crate::inspect::Identity {
            database: "d".into(),
            server: "x:1".into(),
            system_identifier: "1".into(),
        };
        let summary = summarize(&[(SchemaName::new("a"), plan)], &identity);
        assert_eq!(summary.total.steps, 3);
        assert_eq!(summary.total.destructive, 1);
        assert_eq!(summary.destructive[0].kind, "drop.table.column");
        assert_eq!(summary.destructive[0].path, "a.t.old");
    }
}
