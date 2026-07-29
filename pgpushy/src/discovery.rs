//! Finding the source files (spec §4.1).
//!
//! This is the only stage that touches the filesystem; everything downstream
//! works from the `(path, contents)` pairs it produces. Three rules matter
//! more than they look:
//!
//! - **Deterministic order.** Directory enumeration order varies by
//!   filesystem and platform. Sorting by relative path as bytes makes the
//!   whole pipeline reproducible (spec §11.3).
//! - **No symlink following.** A symlinked directory can point back up the
//!   tree, and a walk that follows it either loops forever or silently pulls
//!   in files from outside the source tree.
//! - **Exclusions applied here.** An excluded file is never read and never
//!   parsed, which is what lets a tree hold seed data and fixtures alongside
//!   desired state now that the allow-list makes a stray `INSERT` fatal.

use anyhow::{Context, Result};
use globset::{Glob, GlobMatcher};
use pgpushy_core::SourceFile;
use std::path::{Path, PathBuf};

/// What discovery found.
pub struct Discovered {
    pub files: Vec<SourceFile>,
    /// How many files each exclude pattern matched, in the order given.
    ///
    /// Reported so that an over-broad pattern quietly swallowing real tables
    /// is visible rather than mysterious (spec §4.1).
    pub excluded: Vec<(String, usize)>,
}

/// Walk a source tree, honoring exclusions.
///
/// `ignore` names a file that must never be read even though it lives in the
/// tree — pgpushy's own `--out` document. Writing the desired state into the
/// source root is a natural thing to do, and without this the next run would
/// discover that output as input and report every object in it as a duplicate
/// of itself.
pub fn discover(root: &Path, exclude: &[String], ignore: Option<&Path>) -> Result<Discovered> {
    let matchers = compile(exclude)?;
    let ignore = ignore.and_then(|path| path.canonicalize().ok());
    let mut counts = vec![0usize; matchers.len()];
    let mut paths = Vec::new();

    walk(root, root, &mut paths)
        .with_context(|| format!("reading source tree {}", root.display()))?;

    // Sort before reading so that the order is fixed by path alone, and
    // compare as bytes: locale-aware collation would make output depend on the
    // machine's environment.
    paths.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    let mut files = Vec::with_capacity(paths.len());
    'next: for (relative, absolute) in paths {
        if ignore
            .as_ref()
            .is_some_and(|ignored| absolute.canonicalize().ok().as_ref() == Some(ignored))
        {
            continue;
        }
        for (index, matcher) in matchers.iter().enumerate() {
            if matcher.is_match(&relative) {
                counts[index] += 1;
                continue 'next;
            }
        }
        let contents = std::fs::read_to_string(&absolute)
            .with_context(|| format!("reading {}", absolute.display()))?;
        files.push(SourceFile {
            path: relative,
            contents,
        });
    }

    Ok(Discovered {
        files,
        excluded: exclude.iter().cloned().zip(counts).collect(),
    })
}

fn compile(patterns: &[String]) -> Result<Vec<GlobMatcher>> {
    patterns
        .iter()
        .map(|pattern| {
            Glob::new(pattern)
                .map(|glob| glob.compile_matcher())
                .with_context(|| format!("invalid exclude pattern {pattern:?}"))
        })
        .collect()
}

/// Recursive walk collecting `(relative, absolute)` pairs for `.sql` files.
fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }

        // `file_type` does not traverse symlinks, so a symlinked directory
        // reports as a symlink and is skipped rather than descended into.
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk(root, &path, out)?;
        } else if file_type.is_file() && has_sql_extension(&path) {
            out.push((relative_path(root, &path), path));
        }
    }

    Ok(())
}

fn has_sql_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
}

/// The path relative to the source root, with `/` separators.
///
/// Normalized so that exclude patterns and diagnostics read the same on every
/// platform.
fn relative_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}
