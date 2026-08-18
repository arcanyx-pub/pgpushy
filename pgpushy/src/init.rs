//! `pgpushy init` — write a starter configuration.
//!
//! Configuration is required (spec §10.1), which is the right call for a tool
//! that reconciles a whole database but makes the first run an error message.
//! This turns that into one command.
//!
//! It deliberately does **not** try to be clever. It guesses the source root
//! from where the `*.sql` files actually are, and leaves the environment for
//! the operator to fill in — because guessing which database to reconcile is
//! precisely the thing §10.2 says pgpushy must never do.

use crate::config::FILE_NAME;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Write a starter `pgpushy.toml`.
pub fn init(out: Option<&Path>) -> Result<()> {
    let path = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(FILE_NAME));

    // Never overwrite: a configuration file is the one thing in the project
    // whose loss would be silent and expensive.
    if path.exists() {
        bail!(
            "{} already exists\n\
             \n\
             pgpushy will not overwrite it. Delete it first, or pass --out to write \
             somewhere else.",
            path.display(),
        );
    }

    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let source_root = guess_source_root(directory);

    std::fs::write(&path, template(source_root.as_deref()))
        .with_context(|| format!("writing {}", path.display()))?;

    report(&path, source_root.as_deref());
    Ok(())
}

/// Where the SQL seems to live, relative to the configuration file.
///
/// `None` means "right here", which is also the default when the key is
/// omitted — so a flat project gets a file with nothing to correct.
///
/// Only the shallowest directory containing `*.sql` is reported. Guessing
/// deeper would be guessing at intent: pgpushy walks the tree recursively, so
/// the shallowest common root is always the safe answer.
fn guess_source_root(directory: &Path) -> Option<String> {
    if contains_sql(directory) {
        return None;
    }

    // One level down covers `db/`, `schema/`, `sql/` and the like. Two levels
    // covers `db/schema/`, which is common enough to be worth finding.
    let mut candidates: Vec<PathBuf> = Vec::new();
    for depth_one in subdirectories(directory) {
        if contains_sql(&depth_one) {
            candidates.push(depth_one);
            continue;
        }
        for depth_two in subdirectories(&depth_one) {
            if contains_sql(&depth_two) {
                candidates.push(depth_two);
            }
        }
    }

    // Exactly one answer, or none. Two candidate roots is ambiguous, and a
    // wrong guess here silently narrows the desired state — the hazard §10.1
    // exists to prevent.
    if candidates.len() != 1 {
        return None;
    }

    let relative = candidates[0].strip_prefix(directory).ok()?;
    Some(
        relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn subdirectories(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .collect();
    out.sort();
    out
}

fn contains_sql(directory: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_type().is_ok_and(|kind| kind.is_file())
            && entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
            && !entry.file_name().to_string_lossy().starts_with('.')
    })
}

fn template(source_root: Option<&str>) -> String {
    let root = match source_root {
        Some(root) => format!("source_root = {root:?}\n"),
        None => "# source_root defaults to this file's directory.\n\
                 # Set it if your SQL lives somewhere else:\n\
                 # source_root = \"db/schema\"\n"
            .to_owned(),
    };

    format!(
        "# pgpushy configuration. See https://github.com/arcanyx-pub/pgpushy\n\
         \n\
         {root}\
         \n\
         # Schema that unqualified objects belong to.\n\
         # default_schema = \"public\"\n\
         \n\
         # Files that are not desired state — seed data, fixtures.\n\
         # exclude = [\"seeds/**\", \"**/*.test.sql\"]\n\
         \n\
         # Restrict what pgpushy may reconcile. When set, a schema your SQL uses\n\
         # but this list omits becomes an error instead of being managed silently.\n\
         # managed_schemas = [\"public\"]\n\
         \n\
         # Targets. `pgpushy plan --env local` selects one; --env is always\n\
         # required, so adding a second environment can never change what an\n\
         # existing command reconciles.\n\
         [env.local]\n\
         # host = \"localhost\"\n\
         # port = 5432\n\
         db   = \"CHANGEME\"\n\
         user = \"CHANGEME\"\n\
         # sslmode = \"prefer\"\n\
         \n\
         # Prefer PGPASSWORD in the environment. A password here works, but\n\
         # pgpushy warns whenever it is the one actually used.\n\
         # password = \"...\"\n"
    )
}

fn report(path: &Path, source_root: Option<&str>) {
    println!("  wrote {}", path.display());
    match source_root {
        Some(root) => println!("  source_root: {root} (found *.sql there)"),
        None => println!("  source_root: this directory"),
    }
    println!();
    println!("  Next: set db and user in [env.local], then");
    println!("    pgpushy validate          # checks your SQL, connects to nothing");
    println!("    pgpushy plan --env local  # shows what would change");
}

#[cfg(test)]
mod tests {
    use super::guess_source_root;
    use tempfile::TempDir;

    fn tree(paths: &[&str]) -> TempDir {
        let dir = TempDir::new().expect("temp dir");
        for path in paths {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().expect("has a parent")).expect("mkdir");
            std::fs::write(&full, "-- sql").expect("write");
        }
        dir
    }

    #[test]
    fn sql_beside_the_config_needs_no_source_root() {
        let dir = tree(&["customers.sql"]);
        assert_eq!(guess_source_root(dir.path()), None);
    }

    #[test]
    fn finds_a_single_directory_one_level_down() {
        let dir = tree(&["db/customers.sql"]);
        assert_eq!(guess_source_root(dir.path()).as_deref(), Some("db"));
    }

    #[test]
    fn finds_the_common_two_level_layout() {
        let dir = tree(&["db/schema/customers.sql"]);
        assert_eq!(guess_source_root(dir.path()).as_deref(), Some("db/schema"));
    }

    /// Two candidates is ambiguous, and a wrong guess silently narrows the
    /// desired state — so pgpushy declines and leaves the key commented out.
    #[test]
    fn declines_to_guess_between_two_candidates() {
        let dir = tree(&["db/customers.sql", "other/orders.sql"]);
        assert_eq!(guess_source_root(dir.path()), None);
    }

    #[test]
    fn ignores_hidden_directories() {
        let dir = tree(&[".git/hooks/thing.sql", "db/customers.sql"]);
        assert_eq!(guess_source_root(dir.path()).as_deref(), Some("db"));
    }

    #[test]
    fn an_empty_project_gets_no_guess() {
        let dir = TempDir::new().expect("temp dir");
        assert_eq!(guess_source_root(dir.path()), None);
    }
}
