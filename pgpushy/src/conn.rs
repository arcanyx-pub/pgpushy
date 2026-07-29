//! Turning a named environment into a connection (spec §6.3, §6.4, §10.2).
//!
//! pgpushy connects to the target itself for the read-only inspection, and
//! pgschema connects independently for every plan and apply. If those two
//! resolved their settings separately, pgpushy could report a clean inspection
//! of one database while pgschema reconciled another.
//!
//! Spec §6.3 closes that by construction rather than by comparison: **pgpushy
//! resolves every parameter and passes all of them explicitly, so pgschema
//! resolves nothing.** That is why [`Resolved::command_env`] strips `PG*` from
//! the subprocess environment instead of letting it through.
//!
//! The target itself comes from the `[env.<name>]` block and nowhere else.
//! `PG*` deliberately does **not** override it: the whole point of naming a
//! target is that it is unambiguous, and an ambient `PGHOST` that silently
//! redirected `--env prod` would defeat that at exactly the moment it matters.
//! `PGPASSWORD` is the one exception, because a secret should not live in a
//! version-controlled file.

use crate::config::Target;
use anyhow::{Result, bail};

/// A fully resolved connection: one answer, used by both pgpushy and pgschema.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The environment this came from, for output.
    pub env: String,
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    pub password: Option<String>,
    pub password_source: PasswordSource,
    pub sslmode: String,
}

/// Where the password actually in use came from.
///
/// Tracked because spec §10.2 warns about a password read from `pgpushy.toml` —
/// but only when it is the one being used. A file password that `PGPASSWORD`
/// overrode is not a risk anyone needs telling about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordSource {
    Environment,
    File,
    None,
}

/// Environment variables libpq and pgx read, which must not reach the child
/// unresolved.
///
/// The password pgpushy resolved is put back explicitly; everything else is
/// removed rather than forwarded, so that the child cannot resolve a parameter
/// differently from the parent.
const PG_ENV_VARS: &[&str] = &[
    "PGHOST",
    "PGHOSTADDR",
    "PGPORT",
    "PGDATABASE",
    "PGUSER",
    "PGPASSWORD",
    "PGPASSFILE",
    "PGSERVICE",
    "PGSERVICEFILE",
    "PGSSLMODE",
    "PGSSLCERT",
    "PGSSLKEY",
    "PGSSLROOTCERT",
    "PGSSLCRL",
    "PGREQUIRESSL",
    "PGOPTIONS",
    "PGAPPNAME",
    "PGCONNECT_TIMEOUT",
    "PGCLIENTENCODING",
    "PGTARGETSESSIONATTRS",
    "PGCHANNELBINDING",
    "PGKRBSRVNAME",
    "PGGSSLIB",
    "PGREQUIREPEER",
];

impl Resolved {
    /// Resolve a named environment into a connection.
    ///
    /// Only the password consults the process environment. Everything that
    /// says *which* database this is comes from the named target.
    pub fn from(target: &Target) -> Result<Self> {
        // These would let the environment rather than the configuration decide
        // the target. pgpushy cannot interpret them, and silently dropping one
        // would mean connecting somewhere the operator did not name.
        for var in ["PGSERVICE", "PGSERVICEFILE"] {
            if std::env::var_os(var).is_some() {
                bail!(
                    "{var} is set, and pgpushy does not interpret connection services\n\
                     \n\
                     The target comes from the [env.{}] block in your configuration, and \
                     pgpushy passes it to pgschema explicitly. A setting it cannot read \
                     would be silently dropped.\n\
                     Unset {var}, or put the connection in the environment block.",
                    target.name,
                );
            }
        }

        Ok(Self::build(
            target,
            std::env::var("PGPASSWORD")
                .ok()
                .filter(|value| !value.is_empty()),
        ))
    }

    /// The resolution itself, with the environment's contribution passed in.
    ///
    /// Split out from [`Resolved::from`] so it can be tested without mutating
    /// the process environment — which `forbid(unsafe_code)` rules out anyway,
    /// and which would make these tests order-dependent besides.
    fn build(target: &Target, env_password: Option<String>) -> Self {
        let (password, password_source) = match env_password {
            Some(password) => (Some(password), PasswordSource::Environment),
            None => match target.password.clone() {
                Some(password) => (Some(password), PasswordSource::File),
                None => (None, PasswordSource::None),
            },
        };

        Self {
            env: target.name.clone(),
            host: target.host.clone(),
            port: target.port,
            dbname: target.db.clone(),
            user: target.user.clone(),
            password,
            password_source,
            sslmode: target.sslmode.clone(),
        }
    }

    /// A libpq-style connection string for pgpushy's own driver.
    pub fn conninfo(&self) -> String {
        let mut parts = vec![
            format!("host={}", quote(&self.host)),
            format!("port={}", self.port),
            format!("dbname={}", quote(&self.dbname)),
            format!("user={}", quote(&self.user)),
            format!("sslmode={}", quote(&self.sslmode)),
        ];
        if let Some(password) = &self.password {
            parts.push(format!("password={}", quote(password)));
        }
        parts.join(" ")
    }

    /// The flags to hand pgschema, so it resolves nothing for itself.
    pub fn pgschema_flags(&self) -> Vec<String> {
        vec![
            "--host".into(),
            self.host.clone(),
            "--port".into(),
            self.port.to_string(),
            "--db".into(),
            self.dbname.clone(),
            "--user".into(),
            self.user.clone(),
            "--sslmode".into(),
            self.sslmode.clone(),
        ]
    }

    /// Apply the resolved connection to a subprocess environment.
    ///
    /// The password travels here rather than as a flag, so it stays out of the
    /// process list. Everything else `PG*` is removed: an inherited variable is
    /// precisely what could make pgschema disagree with pgpushy about which
    /// database it is talking to.
    pub fn command_env(&self, command: &mut std::process::Command) {
        for var in PG_ENV_VARS {
            command.env_remove(var);
        }
        if let Some(password) = &self.password {
            command.env("PGPASSWORD", password);
        }
    }

    /// How the target is named in output, without the password.
    pub fn describe(&self) -> String {
        format!("{}@{}:{}/{}", self.user, self.host, self.port, self.dbname)
    }
}

/// Quote a conninfo value if it contains anything that would break parsing.
fn quote(value: &str) -> String {
    if value.is_empty() || value.contains([' ', '\'', '\\']) {
        let escaped = value.replace('\\', r"\\").replace('\'', r"\'");
        format!("'{escaped}'")
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            name: "prod".into(),
            host: "db.example".into(),
            port: 6543,
            db: "shop".into(),
            user: "joe".into(),
            sslmode: "require".into(),
            password: Some("s3cret".into()),
        }
    }

    #[test]
    fn renders_a_conninfo_string() {
        let resolved = Resolved::build(&target(), None);
        assert_eq!(
            resolved.conninfo(),
            "host=db.example port=6543 dbname=shop user=joe sslmode=require password=s3cret",
        );
    }

    /// Every parameter pgpushy resolved is passed on, so pgschema has nothing
    /// left to work out for itself (spec §6.3).
    #[test]
    fn forwards_every_parameter_to_pgschema() {
        let flags = Resolved::build(&target(), None).pgschema_flags().join(" ");
        assert_eq!(
            flags,
            "--host db.example --port 6543 --db shop --user joe --sslmode require"
        );
        assert!(
            !flags.contains("s3cret"),
            "the password must not be a flag: {flags}"
        );
    }

    #[test]
    fn the_environments_password_is_used_when_nothing_overrides_it() {
        let resolved = Resolved::build(&target(), None);
        assert_eq!(resolved.password.as_deref(), Some("s3cret"));
        assert_eq!(resolved.password_source, PasswordSource::File);
    }

    /// A secret does not belong in a version-controlled file, so the
    /// environment wins — and there is then nothing to warn about.
    #[test]
    fn pgpassword_overrides_the_environments_password() {
        let resolved = Resolved::build(&target(), Some("from-env".into()));
        assert_eq!(resolved.password.as_deref(), Some("from-env"));
        assert_eq!(resolved.password_source, PasswordSource::Environment);
    }

    #[test]
    fn no_password_anywhere_is_not_an_error() {
        let mut target = target();
        target.password = None;
        let resolved = Resolved::build(&target, None);
        assert!(resolved.password.is_none());
        assert_eq!(resolved.password_source, PasswordSource::None);
    }

    #[test]
    fn describes_the_target_without_the_password() {
        let described = Resolved::build(&target(), None).describe();
        assert_eq!(described, "joe@db.example:6543/shop");
        assert!(!described.contains("s3cret"));
    }

    #[test]
    fn quotes_values_that_would_break_conninfo_parsing() {
        assert_eq!(quote("simple"), "simple");
        assert_eq!(quote("with space"), "'with space'");
        assert_eq!(quote("it's"), r"'it\'s'");
        assert_eq!(quote(""), "''");
    }
}
