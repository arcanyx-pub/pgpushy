//! `pgpushy.toml` and the precedence rules around it (spec §10).
//!
//! The file is **optional convenience, never required**: CLI flags and `PG*`
//! alone must always work. It is read from the current working directory and
//! *not* searched for in parent directories; `--config` names one explicitly.
//!
//! Precedence is CLI flag → environment → file → built-in default. Note what
//! that means for connections: an ambient `PGHOST` beats the project's
//! `pgpushy.toml`. That is deliberate and matches `psql`, but it surprises
//! people who expect a project file to be the more specific setting.
//!
//! For a flag to lose to nothing, pgpushy has to be able to tell "not given"
//! from "given the default value" — which is why the CLI carries `Option`s and
//! defaults are applied here, at the end, rather than by clap.

use crate::cli::{PgschemaArgs, SourceArgs};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The name looked for when `--config` is not given.
const FILE_NAME: &str = "pgpushy.toml";

/// `pgpushy.toml`, as written.
///
/// `deny_unknown_fields` throughout: a mistyped key in a configuration file is
/// nearly impossible to spot from behavior — pgpushy would simply act as
/// though the setting were absent — so it is better to say so.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    /// Root of the source tree, relative to the file's own directory.
    pub source_root: Option<PathBuf>,
    /// Schema unqualified objects belong to.
    pub default_schema: Option<String>,
    /// The authoritative managed-schema declaration (spec §4.4).
    pub managed_schemas: Option<Vec<String>>,
    /// Globs of paths not to read (spec §4.1).
    pub exclude: Option<Vec<String>>,

    #[serde(default)]
    pub pgschema: Pgschema,
    #[serde(default)]
    pub connection: Connection,
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub db: Option<String>,
    pub user: Option<String>,
    pub sslmode: Option<String>,
    /// Permitted, discouraged, and warned about loudly when actually used
    /// (spec §10).
    pub password: Option<String>,
}

/// A loaded configuration, plus where it came from.
#[derive(Debug)]
pub struct Loaded {
    pub file: File,
    /// The file that was read, or `None` if there wasn't one.
    pub path: Option<PathBuf>,
}

impl Loaded {
    /// The directory relative paths in the file resolve against.
    ///
    /// The file's own directory rather than the working directory, so that
    /// `--config ../other/pgpushy.toml` means what it looks like it means.
    fn base(&self) -> PathBuf {
        self.path
            .as_ref()
            .and_then(|path| path.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Read `pgpushy.toml`, if there is one.
///
/// An explicit `--config` that does not exist is an error; an absent default
/// `pgpushy.toml` is not, because the file is optional.
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
                return Ok(Loaded {
                    file: File::default(),
                    path: None,
                });
            }
            default
        }
    };

    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let file: File =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    check_unsupported(&file, &path)?;

    Ok(Loaded {
        file,
        path: Some(path),
    })
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

/// Everything about the source tree, resolved.
///
/// Deliberately does not carry the pgschema binary: `validate` needs all of
/// this and none of that, so the two are resolved separately (see
/// [`pgschema_path`]).
#[derive(Debug)]
pub struct Settings {
    pub source_root: PathBuf,
    pub default_schema: String,
    pub managed_schemas: Option<Vec<String>>,
    pub exclude: Vec<String>,
}

impl Settings {
    /// Fold CLI flags over the file, then over the built-in defaults.
    ///
    /// List-valued settings (`exclude`, `managed_schemas`) **replace** rather
    /// than append: passing `--exclude` on the command line means "these are
    /// the exclusions", not "these as well as the file's". Appending would
    /// make it impossible to narrow a project's exclusions for one run.
    pub fn resolve(args: &SourceArgs, loaded: &Loaded) -> Self {
        let base = loaded.base();
        let file = &loaded.file;

        let source_root = args
            .source_root
            .clone()
            .or_else(|| file.source_root.as_ref().map(|root| base.join(root)))
            .unwrap_or_else(|| PathBuf::from("."));

        let managed_schemas = if args.managed_schemas.is_empty() {
            file.managed_schemas.clone()
        } else {
            Some(args.managed_schemas.clone())
        };

        let exclude = if args.exclude.is_empty() {
            file.exclude.clone().unwrap_or_default()
        } else {
            args.exclude.clone()
        };

        Self {
            source_root,
            default_schema: args
                .default_schema
                .clone()
                .or_else(|| file.default_schema.clone())
                .unwrap_or_else(|| "public".to_owned()),
            managed_schemas,
            exclude,
        }
    }
}

/// The pgschema binary a command should use, if one was configured.
///
/// `None` means fall back to a `PATH` lookup.
pub fn pgschema_path(args: &PgschemaArgs, loaded: &Loaded) -> Option<PathBuf> {
    args.pgschema_path.clone().or_else(|| {
        loaded
            .file
            .pgschema
            .path
            .as_ref()
            .map(|path| loaded.base().join(path))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> File {
        toml::from_str(toml).expect("valid config")
    }

    fn loaded(toml: &str) -> Loaded {
        Loaded {
            file: parse(toml),
            path: Some(PathBuf::from("proj/pgpushy.toml")),
        }
    }

    fn no_args() -> SourceArgs {
        SourceArgs {
            source_root: None,
            default_schema: None,
            managed_schemas: Vec::new(),
            exclude: Vec::new(),
        }
    }

    #[test]
    fn an_empty_config_yields_the_built_in_defaults() {
        let settings = Settings::resolve(
            &no_args(),
            &Loaded {
                file: File::default(),
                path: None,
            },
        );
        assert_eq!(settings.source_root, PathBuf::from("."));
        assert_eq!(settings.default_schema, "public");
        assert!(settings.managed_schemas.is_none());
        assert!(settings.exclude.is_empty());
    }

    #[test]
    fn the_file_supplies_what_the_flags_do_not() {
        let settings = Settings::resolve(
            &no_args(),
            &loaded(
                r#"
                source_root = "db/schema"
                default_schema = "app"
                managed_schemas = ["app", "billing"]
                exclude = ["seeds/**"]
                "#,
            ),
        );

        assert_eq!(settings.source_root, PathBuf::from("proj/db/schema"));
        assert_eq!(settings.default_schema, "app");
        assert_eq!(settings.managed_schemas.unwrap(), vec!["app", "billing"]);
        assert_eq!(settings.exclude, vec!["seeds/**"]);
    }

    #[test]
    fn a_flag_beats_the_file() {
        let mut args = no_args();
        args.default_schema = Some("from_flag".into());
        args.source_root = Some(PathBuf::from("/elsewhere"));

        let settings = Settings::resolve(
            &args,
            &loaded("source_root = \"db\"\ndefault_schema = \"from_file\""),
        );

        assert_eq!(settings.default_schema, "from_flag");
        assert_eq!(settings.source_root, PathBuf::from("/elsewhere"));
    }

    /// Replace, not append — otherwise a project's exclusions could never be
    /// narrowed for a single run.
    #[test]
    fn list_flags_replace_the_files_lists() {
        let mut args = no_args();
        args.exclude = vec!["only-this/**".into()];

        let settings = Settings::resolve(&args, &loaded(r#"exclude = ["a/**", "b/**"]"#));

        assert_eq!(settings.exclude, vec!["only-this/**"]);
    }

    /// Paths in the file are relative to the file, so `--config` pointing
    /// elsewhere means what it looks like.
    #[test]
    fn file_paths_resolve_against_the_files_own_directory() {
        let loaded = Loaded {
            file: parse("source_root = \"schema\"\n[pgschema]\npath = \"bin/pgschema\""),
            path: Some(PathBuf::from("/projects/app/pgpushy.toml")),
        };

        assert_eq!(
            Settings::resolve(&no_args(), &loaded).source_root,
            PathBuf::from("/projects/app/schema"),
        );
        assert_eq!(
            pgschema_path(
                &PgschemaArgs {
                    pgschema_path: None
                },
                &loaded
            )
            .unwrap(),
            PathBuf::from("/projects/app/bin/pgschema"),
        );
    }

    #[test]
    fn a_pgschema_path_flag_beats_the_file() {
        let loaded = loaded("[pgschema]\npath = \"from/file\"");
        let args = PgschemaArgs {
            pgschema_path: Some(PathBuf::from("/from/flag")),
        };
        assert_eq!(
            pgschema_path(&args, &loaded).unwrap(),
            PathBuf::from("/from/flag")
        );
    }

    #[test]
    fn a_mistyped_key_is_an_error_rather_than_silence() {
        let err = toml::from_str::<File>("exclude_patterns = [\"x\"]").unwrap_err();
        assert!(err.to_string().contains("exclude_patterns"), "{err}");
    }

    #[test]
    fn settings_that_are_not_built_yet_say_so() {
        let path = Path::new("pgpushy.toml");

        let managed = parse("[pgschema]\nbackend = \"managed\"");
        let err = check_unsupported(&managed, path).unwrap_err().to_string();
        assert!(err.contains("not available yet"), "{err}");

        let pinned = parse("[pgschema]\nversion = \"1.12.0\"");
        let err = check_unsupported(&pinned, path).unwrap_err().to_string();
        assert!(err.contains("managed backend"), "{err}");

        // The one backend that does exist is accepted.
        assert!(check_unsupported(&parse("[pgschema]\nbackend = \"byo\""), path).is_ok());
    }
}
