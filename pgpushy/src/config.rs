//! `pgpushy.toml` (spec §10).
//!
//! pgpushy reconciles an entire database, so everything that decides *what*
//! gets reconciled — which files are desired state, which schemas are managed,
//! which server is on the other end — is required and explicit rather than
//! inferred from where the command was run.
//!
//! Two rules follow, and both cost convenience on purpose:
//!
//! - **The file is required** (§10.1). An optional file plus a source root
//!   defaulting to the working directory would mean that running from the
//!   wrong directory silently treats a fragment of the tree as the whole
//!   desired state — and everything outside that fragment is then
//!   desired-state-absent, which is to say scheduled for deletion.
//! - **Project structure is not settable by flag** (§10.1). The source root,
//!   default schema, managed-schema declaration and exclusions all describe the
//!   project, and each is a way to change what gets reconciled. A flag that
//!   silently narrows the desired state is the same hazard as a missing file.
//!
//! `--config` selects which project. `--env` selects which target. Nothing
//! else about either is a flag.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The name looked for when `--config` is not given.
pub const FILE_NAME: &str = "pgpushy.toml";

/// `pgpushy.toml`, as written.
///
/// `deny_unknown_fields` throughout: a mistyped key is invisible from
/// behavior — pgpushy would act as though the setting were absent — so silence
/// is the one response that cannot be recovered from.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    /// Root of the source tree. Relative to this file's own directory, and
    /// defaulting to it.
    pub source_root: Option<PathBuf>,
    /// Schema unqualified objects belong to. Defaults to `public`.
    pub default_schema: Option<String>,
    /// The authoritative managed-schema declaration (spec §4.4).
    pub managed_schemas: Option<Vec<String>>,
    /// Globs of paths not to read (spec §4.1).
    pub exclude: Option<Vec<String>>,

    #[serde(default)]
    pub pgschema: Pgschema,
    /// Named targets, selected by `--env` (spec §10.2).
    #[serde(default)]
    pub env: BTreeMap<String, Environment>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pgschema {
    /// An explicit pgschema binary (the BYO backend).
    pub path: Option<PathBuf>,
    /// Which backend to use. Only `byo` exists so far.
    pub backend: Option<String>,
    /// The pinned version, for the managed backend.
    pub version: Option<String>,
}

/// One named target.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Required: there is no safe default for which database to reconcile.
    pub db: Option<String>,
    /// Required: there is no safe default for who to connect as.
    pub user: Option<String>,
    pub sslmode: Option<String>,
    /// Permitted, discouraged, and warned about loudly when actually used
    /// (spec §10.2).
    pub password: Option<String>,
}

/// A loaded configuration and where it came from.
#[derive(Debug)]
pub struct Loaded {
    pub file: File,
    pub path: PathBuf,
}

impl Loaded {
    /// The directory relative paths resolve against: the file's own.
    ///
    /// Not the working directory, so that `--config ../other/pgpushy.toml`
    /// means what it looks like.
    pub fn base(&self) -> &Path {
        self.path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
    }

    /// Everything about the source tree, resolved.
    pub fn settings(&self) -> Settings {
        let file = &self.file;
        Settings {
            source_root: match &file.source_root {
                Some(root) => self.base().join(root),
                // Defaulting to the file's directory rather than the working
                // directory is what makes "wrong directory" harmless: the
                // source tree is anchored to the project, not to the caller.
                None => self.base().to_path_buf(),
            },
            default_schema: file
                .default_schema
                .clone()
                .unwrap_or_else(|| "public".to_owned()),
            managed_schemas: file.managed_schemas.clone(),
            exclude: file.exclude.clone().unwrap_or_default(),
        }
    }

    /// The pgschema binary, if one is configured. `None` means look on `PATH`.
    pub fn pgschema_path(&self, flag: Option<&Path>) -> Option<PathBuf> {
        flag.map(Path::to_path_buf).or_else(|| {
            self.file
                .pgschema
                .path
                .as_ref()
                .map(|path| self.base().join(path))
        })
    }

    /// Select a named environment (spec §10.2).
    ///
    /// A name is always required, even when only one environment exists:
    /// selecting the sole one automatically would make adding a second silently
    /// change what an existing command reconciles.
    pub fn environment(&self, name: &str) -> Result<Target> {
        let Some(env) = self.file.env.get(name) else {
            let available = self.file.env.keys().cloned().collect::<Vec<_>>();
            if available.is_empty() {
                bail!(
                    "{}: no environments are defined\n\
                     \n\
                     plan and apply need a target. Add one:\n\
                     \n    \
                     [env.{name}]\n    \
                     db   = \"your_database\"\n    \
                     user = \"your_user\"",
                    self.path.display(),
                );
            }
            bail!(
                "{}: no environment named {name:?}\n\
                 \n\
                 Defined environments: {}",
                self.path.display(),
                available.join(", "),
            );
        };

        // No default for either: reconciling the wrong database, or as the
        // wrong role, are exactly the mistakes naming the environment is
        // supposed to prevent.
        let (Some(db), Some(user)) = (env.db.clone(), env.user.clone()) else {
            let mut missing = Vec::new();
            if env.db.is_none() {
                missing.push("db");
            }
            if env.user.is_none() {
                missing.push("user");
            }
            bail!(
                "{}: [env.{name}] is missing {}\n\
                 \n\
                 An environment must say which database to reconcile and who to \
                 connect as; there is no safe default for either.",
                self.path.display(),
                missing.join(" and "),
            );
        };

        Ok(Target {
            name: name.to_owned(),
            host: env.host.clone().unwrap_or_else(|| "localhost".to_owned()),
            port: env.port.unwrap_or(5432),
            db,
            user,
            sslmode: env.sslmode.clone().unwrap_or_else(|| "prefer".to_owned()),
            password: env.password.clone(),
        })
    }
}

/// A named target, fully resolved from its `[env.<name>]` block.
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub db: String,
    pub user: String,
    pub sslmode: String,
    /// The environment's own password, before `PGPASSWORD` is considered.
    pub password: Option<String>,
}

/// Everything about the source tree, resolved.
#[derive(Debug)]
pub struct Settings {
    pub source_root: PathBuf,
    pub default_schema: String,
    pub managed_schemas: Option<Vec<String>>,
    pub exclude: Vec<String>,
}

/// Read `pgpushy.toml`. Required (spec §10.1).
pub fn load(explicit: Option<&Path>) -> Result<Loaded> {
    let path = match explicit {
        Some(path) => {
            if !path.exists() {
                bail!("no configuration file at {}", path.display());
            }
            path.to_path_buf()
        }
        None => {
            let default = PathBuf::from(FILE_NAME);
            if !default.exists() {
                bail!("{}", missing_file_message());
            }
            default
        }
    };

    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let file: File =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    check_unsupported(&file, &path)?;

    Ok(Loaded { file, path })
}

/// What to say when there is no configuration file.
///
/// The first thing many people will see, so it explains the requirement rather
/// than merely stating it, and shows the smallest file that works.
fn missing_file_message() -> String {
    let cwd = std::env::current_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_else(|_| ".".into());

    format!(
        "no {FILE_NAME} in {cwd}\n\
         \n\
         pgpushy reconciles a whole database, so it will not guess which files are\n\
         desired state or which server to reconcile them against — running from the\n\
         wrong directory would otherwise treat part of a source tree as all of it,\n\
         and plan to drop everything else.\n\
         \n\
         Create {FILE_NAME} beside your schema files:\n\
         \n    \
         # source_root defaults to this file's directory\n    \
         \n    \
         [env.local]\n    \
         db   = \"your_database\"\n    \
         user = \"your_user\"\n\
         \n\
         Then run `pgpushy plan --env local`, or pass --config <path> to use a file\n\
         somewhere else. {FILE_NAME} is not searched for in parent directories."
    )
}

/// Reject settings that are specified but not yet built.
///
/// A dedicated message beats "unknown field": these keys are in the spec, so
/// someone reading it will reasonably try them, and "not implemented yet"
/// answers the question they actually have.
fn check_unsupported(file: &File, path: &Path) -> Result<()> {
    if let Some(backend) = &file.pgschema.backend
        && backend != "byo"
    {
        bail!(
            "{}: pgschema backend {backend:?} is not available yet\n\
             \n\
             Only \"byo\" — a pgschema binary you supply — is implemented. The managed \
             backend, which downloads and verifies a pinned version, is a fast-follow.\n\
             Set [pgschema] path, or pass --pgschema-path.",
            path.display(),
        );
    }
    if file.pgschema.version.is_some() {
        bail!(
            "{}: [pgschema] version only applies to the managed backend, which is not \
             available yet\n\
             \n\
             With the BYO backend pgpushy uses whatever binary you point it at, and checks \
             that its version meets the supported minimum.",
            path.display(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(toml: &str) -> Loaded {
        Loaded {
            file: toml::from_str(toml).expect("valid config"),
            path: PathBuf::from("/projects/app/pgpushy.toml"),
        }
    }

    #[test]
    fn the_source_root_defaults_to_the_config_files_directory() {
        let settings = loaded("").settings();
        assert_eq!(settings.source_root, PathBuf::from("/projects/app"));
        assert_eq!(settings.default_schema, "public");
        assert!(settings.managed_schemas.is_none());
        assert!(settings.exclude.is_empty());
    }

    #[test]
    fn relative_paths_resolve_against_that_directory() {
        let loaded = loaded("source_root = \"db/schema\"\n[pgschema]\npath = \"bin/pgschema\"");
        assert_eq!(
            loaded.settings().source_root,
            PathBuf::from("/projects/app/db/schema")
        );
        assert_eq!(
            loaded.pgschema_path(None).unwrap(),
            PathBuf::from("/projects/app/bin/pgschema")
        );
    }

    /// The pgschema binary differs per machine and cannot change what is
    /// reconciled, so unlike project structure it stays a flag.
    #[test]
    fn a_pgschema_path_flag_beats_the_file() {
        let loaded = loaded("[pgschema]\npath = \"bin/pgschema\"");
        let flag = PathBuf::from("/usr/local/bin/pgschema");
        assert_eq!(loaded.pgschema_path(Some(&flag)).unwrap(), flag);
    }

    #[test]
    fn an_environment_resolves_with_conventional_defaults() {
        let target = loaded("[env.local]\ndb = \"shop\"\nuser = \"joe\"")
            .environment("local")
            .expect("ok");

        assert_eq!(target.host, "localhost");
        assert_eq!(target.port, 5432);
        assert_eq!(target.sslmode, "prefer");
        assert_eq!(target.db, "shop");
        assert_eq!(target.user, "joe");
    }

    /// Reconciling the wrong database, or as the wrong role, are the mistakes
    /// naming an environment exists to prevent.
    #[test]
    fn an_environment_must_say_which_database_and_who() {
        let err = loaded("[env.local]\nhost = \"db\"")
            .environment("local")
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing db and user"), "{err}");

        let err = loaded("[env.local]\ndb = \"shop\"")
            .environment("local")
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing user"), "{err}");
    }

    #[test]
    fn an_unknown_environment_lists_the_defined_ones() {
        let err =
            loaded("[env.local]\ndb = \"a\"\nuser = \"u\"\n[env.prod]\ndb = \"b\"\nuser = \"u\"")
                .environment("staging")
                .unwrap_err()
                .to_string();

        assert!(err.contains("no environment named \"staging\""), "{err}");
        assert!(err.contains("local, prod"), "{err}");
    }

    #[test]
    fn a_file_with_no_environments_says_how_to_add_one() {
        let err = loaded("source_root = \"db\"")
            .environment("local")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no environments are defined"), "{err}");
        assert!(err.contains("[env.local]"), "{err}");
    }

    /// Even with exactly one environment: selecting it automatically would make
    /// adding a second silently change what an existing command reconciles.
    #[test]
    fn the_sole_environment_is_still_not_selected_automatically() {
        let loaded = loaded("[env.only]\ndb = \"a\"\nuser = \"u\"");
        assert!(loaded.environment("other").is_err());
        assert!(loaded.environment("only").is_ok());
    }

    #[test]
    fn a_mistyped_key_is_an_error_rather_than_silence() {
        let err = toml::from_str::<File>("exclude_patterns = [\"x\"]").unwrap_err();
        assert!(err.to_string().contains("exclude_patterns"), "{err}");

        let err = toml::from_str::<File>("[env.local]\ndatabase = \"x\"").unwrap_err();
        assert!(err.to_string().contains("database"), "{err}");
    }

    #[test]
    fn settings_that_are_not_built_yet_say_so() {
        let path = Path::new("pgpushy.toml");
        let parse = |t: &str| toml::from_str::<File>(t).expect("valid");

        let err = check_unsupported(&parse("[pgschema]\nbackend = \"managed\""), path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not available yet"), "{err}");

        let err = check_unsupported(&parse("[pgschema]\nversion = \"1.12.0\""), path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("managed backend"), "{err}");

        assert!(check_unsupported(&parse("[pgschema]\nbackend = \"byo\""), path).is_ok());
    }
}
