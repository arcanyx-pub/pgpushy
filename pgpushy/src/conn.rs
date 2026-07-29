//! Connection resolution (spec §6.3, §6.4).
//!
//! pgpushy connects to the target itself for the read-only inspection, and
//! pgschema connects independently for every plan and apply. If those two
//! resolved their settings separately — an ambient `PGSERVICE`, a default
//! applied by one library and not the other — pgpushy could report a clean
//! inspection of one database while pgschema reconciled another.
//!
//! Spec §6.3 closes that by construction rather than by comparison: **pgpushy
//! resolves every parameter and passes all of them explicitly, so pgschema
//! resolves nothing.** That is why [`Resolved::command_env`] strips `PG*` from
//! the subprocess environment instead of letting it through — an inherited
//! variable is exactly the input that could make the child disagree.

use anyhow::{Result, bail};
use clap::Args;

/// Connection flags, mirroring pgschema's own names (spec §8.3).
#[derive(Args, Debug, Clone)]
pub struct ConnectionArgs {
    /// Database server host.
    #[arg(long, value_name = "HOST")]
    pub host: Option<String>,

    /// Database server port.
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,

    /// Database name.
    #[arg(long = "db", value_name = "NAME")]
    pub dbname: Option<String>,

    /// Database user.
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,

    /// Password. Prefer `PGPASSWORD`; a password on the command line is
    /// visible in the process list.
    #[arg(long, value_name = "PASSWORD")]
    pub password: Option<String>,

    /// TLS mode, as libpq understands it.
    #[arg(long, value_name = "MODE")]
    pub sslmode: Option<String>,
}

/// A fully resolved connection: one answer, used by both pgpushy and pgschema.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    pub password: Option<String>,
    pub sslmode: String,
}

/// Environment variables libpq and pgx read, which must not reach the child
/// unresolved.
///
/// Anything here that pgpushy has already folded into [`Resolved`] is passed
/// back explicitly; anything it has not is removed rather than forwarded, so
/// that the child cannot resolve a parameter differently from the parent.
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
    /// Fold CLI flags and `PG*` into one answer.
    ///
    /// Precedence is spec §10: CLI flag, then environment, then default. The
    /// configuration file joins between environment and default when it lands.
    pub fn from(args: &ConnectionArgs) -> Result<Self> {
        // pgpushy does not interpret these, and passing them through would let
        // pgschema resolve a connection pgpushy never saw. Refusing is the
        // honest response: silently ignoring them would connect somewhere the
        // operator did not ask for.
        for var in ["PGSERVICE", "PGSERVICEFILE"] {
            if std::env::var_os(var).is_some() {
                bail!(
                    "{var} is set, and pgpushy does not yet interpret connection services\n\
                     \n\
                     pgpushy resolves the connection itself and passes it to pgschema \
                     explicitly, so a setting it cannot read would be silently dropped.\n\
                     Pass the connection with --host/--port/--db/--user instead."
                );
            }
        }

        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());

        let port = match args.port {
            Some(port) => port,
            None => match env("PGPORT") {
                Some(value) => value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("PGPORT is not a port number: {value:?}"))?,
                None => 5432,
            },
        };

        let user = args
            .user
            .clone()
            .or_else(|| env("PGUSER"))
            .or_else(|| env("USER"))
            .unwrap_or_else(|| "postgres".to_owned());

        // libpq defaults the database to the user name; matching that avoids a
        // surprise for anyone used to psql.
        let dbname = args
            .dbname
            .clone()
            .or_else(|| env("PGDATABASE"))
            .unwrap_or_else(|| user.clone());

        Ok(Self {
            host: args
                .host
                .clone()
                .or_else(|| env("PGHOST"))
                .unwrap_or_else(|| "localhost".to_owned()),
            port,
            dbname,
            user,
            password: args.password.clone().or_else(|| env("PGPASSWORD")),
            sslmode: args
                .sslmode
                .clone()
                .or_else(|| env("PGSSLMODE"))
                .unwrap_or_else(|| "prefer".to_owned()),
        })
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

    fn args() -> ConnectionArgs {
        ConnectionArgs {
            host: Some("db.example".into()),
            port: Some(6543),
            dbname: Some("shop".into()),
            user: Some("joe".into()),
            password: Some("s3cret".into()),
            sslmode: Some("require".into()),
        }
    }

    #[test]
    fn renders_a_conninfo_string() {
        let resolved = Resolved::from(&args()).unwrap();
        assert_eq!(
            resolved.conninfo(),
            "host=db.example port=6543 dbname=shop user=joe sslmode=require password=s3cret",
        );
    }

    /// Every parameter pgpushy resolved is passed on, so pgschema has nothing
    /// left to work out for itself (spec §6.3).
    #[test]
    fn forwards_every_parameter_to_pgschema() {
        let resolved = Resolved::from(&args()).unwrap();
        let flags = resolved.pgschema_flags().join(" ");
        assert_eq!(
            flags,
            "--host db.example --port 6543 --db shop --user joe --sslmode require",
        );
        assert!(
            !flags.contains("s3cret"),
            "the password must not be a flag: {flags}"
        );
    }

    #[test]
    fn quotes_values_that_would_break_conninfo_parsing() {
        assert_eq!(quote("simple"), "simple");
        assert_eq!(quote("with space"), "'with space'");
        assert_eq!(quote("it's"), r"'it\'s'");
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn describes_the_target_without_the_password() {
        let described = Resolved::from(&args()).unwrap().describe();
        assert_eq!(described, "joe@db.example:6543/shop");
        assert!(!described.contains("s3cret"));
    }
}
