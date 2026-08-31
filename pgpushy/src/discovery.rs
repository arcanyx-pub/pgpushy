//! Finding the source files (spec §4.1).
//!
//! This is the only stage that touches the filesystem; everything downstream
//! works from the `(path, contents)` pairs it produces. Three rules matter
//! more than they look:
//!
//! - **Deterministic order.** Directory enumeration order varies by
//!   filesystem and platform. Sorting by relative path as bytes makes the
//!   whole pipeline reproducible (spec §11.3).
//! - **Links to directories are not followed; links to files are.** A
//!   symlinked directory can point back up the tree, and a walk that followed
//!   it would either loop forever or silently pull in files from outside the
//!   source tree. A link to a file carries neither risk, and a `.sql` file
//!   missing from the desired state is a file scheduled for deletion — the
//!   direction that hurts.
//! - **Exclusions applied here.** An excluded file is never read and never
//!   parsed, which is what lets a tree hold seed data and fixtures alongside
//!   desired state now that the allow-list makes a stray `INSERT` fatal.

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobMatcher};
use pgpushy_core::{GENERATED_MARKER, SourceFile};
use std::path::{Path, PathBuf};

/// What discovery found.
pub struct Discovered {
    pub files: Vec<SourceFile>,
    /// How many files each exclude pattern matched, in the order given.
    ///
    /// Reported so that an over-broad pattern quietly swallowing real tables
    /// is visible rather than mysterious (spec §4.1).
    pub excluded: Vec<(String, usize)>,
    /// How many files were skipped for being pgpushy's own output.
    pub generated: usize,
    /// How many `.sql` files sat under a nested seed root (spec §4.6).
    pub skipped_seed_root: usize,
}

/// Walk a source tree, honoring exclusions.
///
/// `skip` names a directory the walk must not descend into — a seed root
/// nested inside the source root (spec §4.6), whose files are seed files
/// rather than desired state. Its `.sql` files are counted so the skip is
/// visible in the report rather than mysterious.
pub fn discover(root: &Path, exclude: &[String], skip: Option<&Path>) -> Result<Discovered> {
    let matchers = compile(exclude)?;
    let mut counts = vec![0usize; matchers.len()];
    let mut candidates = Vec::new();
    let mut skipped_seed_root = 0usize;

    walk(root, root, skip, &mut candidates, &mut skipped_seed_root)
        .with_context(|| format!("reading source tree {}", root.display()))?;

    // Sort before reading so that the order is fixed by path alone, and
    // compare as bytes: locale-aware collation would make output depend on the
    // machine's environment.
    candidates.sort_by(|a, b| a.relative.as_bytes().cmp(b.relative.as_bytes()));

    let mut kept = Vec::with_capacity(candidates.len());
    let mut unresolved = Vec::new();
    'next: for candidate in candidates {
        for (index, matcher) in matchers.iter().enumerate() {
            if matcher.is_match(&candidate.relative) {
                counts[index] += 1;
                continue 'next;
            }
        }

        match candidate.kind {
            Kind::File(absolute) => kept.push((candidate.relative, absolute)),
            // Collected rather than raised on the spot, so that one run names
            // every unreadable link instead of sending the author back for the
            // next one.
            Kind::Unresolved { target, error } => {
                unresolved.push((candidate.relative, target, error));
            }
        }
    }

    if !unresolved.is_empty() {
        bail!("{}", unresolved_message(root, &unresolved));
    }

    let mut files = Vec::with_capacity(kept.len());
    let mut generated = 0usize;
    for (relative, absolute) in kept {
        let contents = std::fs::read_to_string(&absolute)
            .with_context(|| format!("reading {}", absolute.display()))?;

        // pgpushy never reads a document it wrote. Writing the desired state
        // into the source root with `--out` is a natural thing to do, and
        // without this every later run would report every object in it as a
        // duplicate of itself — a failure that outlives the run that caused it
        // and gives no hint where the extra file came from.
        if contents.starts_with(GENERATED_MARKER) {
            generated += 1;
            continue;
        }

        files.push(SourceFile {
            path: relative,
            contents,
        });
    }

    Ok(Discovered {
        files,
        excluded: exclude.iter().cloned().zip(counts).collect(),
        generated,
        skipped_seed_root,
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

/// A `.sql` path the walk found, keyed by its source-root-relative form.
struct Candidate {
    relative: String,
    kind: Kind,
}

enum Kind {
    /// A regular file, or a symbolic link that resolves to one.
    File(PathBuf),
    /// A symbolic link whose target could not be reached, and why. The target
    /// is what the link says rather than where it lands, so the diagnostic can
    /// show the author the string they wrote; it is absent only if the link
    /// disappeared mid-walk.
    Unresolved {
        target: Option<PathBuf>,
        error: String,
    },
}

/// Recursive walk collecting the `.sql` candidates under `dir`.
fn walk(
    root: &Path,
    dir: &Path,
    skip: Option<&Path>,
    out: &mut Vec<Candidate>,
    skipped: &mut usize,
) -> Result<()> {
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
        // reports as a symlink and is never descended into. That is what makes
        // the walk terminate and keeps it inside the source tree.
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if skip == Some(path.as_path()) {
                *skipped += count_sql(&path);
                continue;
            }
            walk(root, &path, skip, out, skipped)?;
            continue;
        }
        if !has_sql_extension(&path) {
            continue;
        }

        let relative = relative_path(root, &path);
        let kind = if file_type.is_file() {
            Kind::File(path)
        } else if file_type.is_symlink() {
            // A link's own type says nothing about what it points at, so ask
            // the filesystem, which resolves the whole chain. A link landing on
            // a directory falls under the rule above and is left alone; one
            // landing on a socket or a device holds no more desired state than
            // the same thing would unlinked.
            match std::fs::metadata(&path) {
                Ok(target) if target.is_file() => Kind::File(path),
                Ok(_) => continue,
                Err(error) => Kind::Unresolved {
                    target: std::fs::read_link(&path).ok(),
                    error: error.to_string(),
                },
            }
        } else {
            continue;
        };

        out.push(Candidate { relative, kind });
    }

    Ok(())
}

/// The diagnostic for `.sql` links the walk could not resolve.
///
/// A hard error rather than a skip: pgpushy follows links to files precisely
/// because a `.sql` file absent from the desired state is a file scheduled for
/// deletion, and passing over an unreadable one would drop its objects exactly
/// that way — silently, and only on the machine where the link happens to
/// dangle (G5).
fn unresolved_message(root: &Path, unresolved: &[(String, Option<PathBuf>, String)]) -> String {
    let mut message = if unresolved.len() == 1 {
        format!(
            "1 symbolic link under {} could not be resolved:\n",
            root.display()
        )
    } else {
        format!(
            "{} symbolic links under {} could not be resolved:\n",
            unresolved.len(),
            root.display()
        )
    };

    for (relative, target, error) in unresolved {
        match target {
            Some(target) => {
                message.push_str(&format!("\n  {relative} -> {}: {error}", target.display()));
            }
            None => message.push_str(&format!("\n  {relative}: {error}")),
        }
    }

    message.push_str(
        "\n\nEach of these is a *.sql file pgpushy would read as desired state, and \
         skipping one would schedule the objects it defines for deletion. Point each \
         link at a file that exists, delete the link, or add it to `exclude` in your \
         configuration.",
    );
    message
}

/// How many `.sql` files a skipped subtree holds, for the report.
fn count_sql(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let hidden = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_none_or(|n| n.starts_with('.'));
        if hidden {
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_dir() => count += count_sql(&path),
            Ok(_) if has_sql_extension(&path) => count += 1,
            _ => {}
        }
    }
    count
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

#[cfg(test)]
mod tests {
    use super::discover;
    use std::path::Path;
    use tempfile::TempDir;

    const CUSTOMERS: &str = "CREATE TABLE customers (id int PRIMARY KEY);";

    fn tree(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().expect("temp dir");
        for (path, contents) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().expect("has a parent")).expect("mkdir");
            std::fs::write(&full, contents).expect("write");
        }
        dir
    }

    /// The source-root-relative paths discovery yielded, in the order it
    /// yielded them.
    fn found(root: &Path) -> Vec<String> {
        discover(root, &[], None)
            .expect("discovery succeeds")
            .files
            .into_iter()
            .map(|file| file.path)
            .collect()
    }

    #[test]
    fn finds_sql_files_under_real_directories() {
        let dir = tree(&[
            ("b/orders.sql", CUSTOMERS),
            ("a/customers.sql", CUSTOMERS),
            ("a/notes.md", "not sql"),
            (".hidden/secret.sql", CUSTOMERS),
        ]);

        assert_eq!(found(dir.path()), ["a/customers.sql", "b/orders.sql"]);
    }

    /// A link to a file is desired state like any other file (spec §4.1), and
    /// its contents come from the target.
    #[cfg(unix)]
    #[test]
    fn follows_a_symlink_to_a_file() {
        let dir = tree(&[
            ("schema/orders.sql", "CREATE TABLE orders (id int);"),
            ("shared/customers.sql", CUSTOMERS),
        ]);
        std::os::unix::fs::symlink(
            dir.path().join("shared/customers.sql"),
            dir.path().join("schema/customers.sql"),
        )
        .expect("create symlink");

        let discovered =
            discover(&dir.path().join("schema"), &[], None).expect("discovery succeeds");
        let paths: Vec<_> = discovered
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(paths, ["customers.sql", "orders.sql"]);
        assert_eq!(discovered.files[0].contents, CUSTOMERS);
    }

    /// Descending into a linked directory would let the walk leave the source
    /// tree, or loop forever when the link points back up it.
    #[cfg(unix)]
    #[test]
    fn does_not_descend_into_a_symlinked_directory() {
        let dir = tree(&[
            ("schema/orders.sql", CUSTOMERS),
            ("outside/customers.sql", CUSTOMERS),
        ]);
        let root = dir.path().join("schema");
        std::os::unix::fs::symlink(dir.path().join("outside"), root.join("elsewhere"))
            .expect("create symlink");
        // A directory link is a directory link whatever it is called.
        std::os::unix::fs::symlink(dir.path().join("outside"), root.join("named.sql"))
            .expect("create symlink");
        std::os::unix::fs::symlink(&root, root.join("loop")).expect("create symlink");

        assert_eq!(found(&root), ["orders.sql"]);
    }

    /// Skipping an unreadable link would drop its objects from the desired
    /// state, which is a scheduled deletion — so the run stops, and names all
    /// of them rather than one per attempt.
    #[cfg(unix)]
    #[test]
    fn an_unresolvable_symlink_names_every_offender() {
        let dir = tree(&[("schema/orders.sql", CUSTOMERS)]);
        let root = dir.path().join("schema");
        std::fs::create_dir_all(root.join("vendor")).expect("mkdir");
        std::os::unix::fs::symlink("/nowhere/customers.sql", root.join("customers.sql"))
            .expect("create symlink");
        std::os::unix::fs::symlink("../../gone/prices.sql", root.join("vendor/prices.sql"))
            .expect("create symlink");

        let Err(err) = discover(&root, &[], None) else {
            panic!("an unresolvable link must be fatal");
        };
        let message = format!("{err:#}");
        assert!(message.contains("2 symbolic links"), "{message}");
        assert!(
            message.contains("customers.sql -> /nowhere/customers.sql"),
            "{message}"
        );
        assert!(
            message.contains("vendor/prices.sql -> ../../gone/prices.sql"),
            "{message}"
        );
        assert!(message.contains("exclude"), "{message}");
    }

    /// An exclusion says the file is not desired state, so there is nothing
    /// left to be missing from it.
    #[cfg(unix)]
    #[test]
    fn an_excluded_unresolvable_symlink_is_not_an_error() {
        let dir = tree(&[("schema/orders.sql", CUSTOMERS)]);
        let root = dir.path().join("schema");
        std::os::unix::fs::symlink("/nowhere/customers.sql", root.join("customers.sql"))
            .expect("create symlink");

        let discovered =
            discover(&root, &["customers.sql".to_owned()], None).expect("discovery succeeds");
        assert_eq!(discovered.files.len(), 1);
        assert_eq!(discovered.excluded, [("customers.sql".to_owned(), 1)]);
    }

    /// A link that resolves through another link is still a file.
    #[cfg(unix)]
    #[test]
    fn follows_a_chain_of_symlinks() {
        let dir = tree(&[("shared/customers.sql", CUSTOMERS)]);
        let root = dir.path().join("schema");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::os::unix::fs::symlink(
            dir.path().join("shared/customers.sql"),
            dir.path().join("shared/link.sql"),
        )
        .expect("create symlink");
        std::os::unix::fs::symlink(
            dir.path().join("shared/link.sql"),
            root.join("customers.sql"),
        )
        .expect("create symlink");

        assert_eq!(found(&root), ["customers.sql"]);
    }
}
