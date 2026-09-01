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

use crate::config::{ResolvedPlanDatabase, Target};
use crate::tls::Security;
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
    pub sslmode: SslMode,
    /// An external plan database, if the environment named one (spec §10.4).
    pub plan_db: Option<PlanConnection>,
    /// The environment's lock timeout, before any flag overrides it (§10.5).
    pub lock_timeout: Option<String>,
    /// Spec §9.1: whether a destructive plan may exit 0.
    pub allow_destructive: bool,
}

/// A resolved external plan database.
///
/// Kept separate from [`Resolved`] rather than folded in: pgpushy never
/// connects here itself. This is only ever forwarded to pgschema, which is why
/// it has a flag rendering and no driver configuration — and why its `sslmode`
/// stays a string: the mode governs a connection pgschema alone makes, and
/// pgschema interprets it.
#[derive(Debug, Clone)]
pub struct PlanConnection {
    pub host: String,
    pub port: u16,
    pub db: String,
    pub user: String,
    pub sslmode: String,
    pub password: Option<String>,
}

impl PlanConnection {
    fn from(plan: &ResolvedPlanDatabase, env_password: Option<String>) -> Self {
        Self {
            host: plan.host.clone(),
            port: plan.port,
            db: plan.db.clone(),
            user: plan.user.clone(),
            sslmode: plan.sslmode.clone(),
            password: env_password.or_else(|| plan.password.clone()),
        }
    }

    /// Everything but the password, which travels through the environment.
    fn flags(&self) -> Vec<String> {
        vec![
            "--plan-host".into(),
            self.host.clone(),
            "--plan-port".into(),
            self.port.to_string(),
            "--plan-db".into(),
            self.db.clone(),
            "--plan-user".into(),
            self.user.clone(),
            "--plan-sslmode".into(),
            self.sslmode.clone(),
        ]
    }

    /// How the plan database is named in output, without the password.
    pub fn describe(&self) -> String {
        format!("{}@{}:{}/{}", self.user, self.host, self.port, self.db)
    }

    /// The driver configuration for pgpushy's own read-only look at the plan
    /// database (spec §10.4), built the way the target's is and interpreting
    /// `sslmode` the same way (§6.4).
    pub fn pg_config(&self) -> Result<(postgres::Config, SslMode)> {
        let sslmode = SslMode::parse(&self.sslmode, "<name>.plan_db")?;
        let mut config = postgres::Config::new();
        config
            .host(&self.host)
            .port(self.port)
            .dbname(&self.db)
            .user(&self.user)
            .ssl_mode(Security::for_mode(sslmode).fallback);
        if let Some(password) = &self.password {
            config.password(password);
        }
        Ok((config, sslmode))
    }
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

/// A libpq `sslmode`, as pgpushy understands it (spec §6.4).
///
/// Resolved into this enum before anything connects, so an unusable mode is a
/// configuration error rather than a connection failure — and so the string
/// pgschema is handed is one pgpushy has also understood.
///
/// The Postgres driver models three of these five and rejects `verify-ca` and
/// `verify-full` outright, which is why pgpushy interprets the mode itself and
/// [`crate::tls`] maps it to what the driver and the TLS stack each need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    /// Interpret an environment's `sslmode`.
    ///
    /// Spelled exactly as libpq spells it, because pgschema compares the same
    /// way: accepting a spelling pgschema would reject would produce a run that
    /// inspects the target and then cannot delegate.
    fn parse(value: &str, env: &str) -> Result<Self> {
        match value {
            "disable" => Ok(Self::Disable),
            "prefer" => Ok(Self::Prefer),
            "require" => Ok(Self::Require),
            "verify-ca" => Ok(Self::VerifyCa),
            "verify-full" => Ok(Self::VerifyFull),
            other => bail!(
                "[env.{env}]: sslmode = {other:?} is not a mode pgpushy understands\n\
                 \n\
                 Write one of: disable, prefer, require, verify-ca, verify-full — \
                 lower case, as libpq spells them.\n\
                 require encrypts without checking the target's certificate; \
                 verify-ca additionally checks that the certificate chains to a \
                 trusted authority; verify-full also checks that it was issued for \
                 the host being connected to."
            ),
        }
    }

    /// The spelling pgschema is handed, which is the one that was written.
    ///
    /// pgschema implements all five itself, so the mode crosses to it unchanged
    /// rather than reduced to whatever pgpushy's own driver could express.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }
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
    // pgschema reads these for its plan database. Stripped for the same reason
    // as the rest: pgpushy decides where the comparison model is built, and an
    // inherited variable could move it somewhere else (spec §8.3).
    "PGSCHEMA_PLAN_HOST",
    "PGSCHEMA_PLAN_PORT",
    "PGSCHEMA_PLAN_DB",
    "PGSCHEMA_PLAN_USER",
    "PGSCHEMA_PLAN_PASSWORD",
    "PGSCHEMA_PLAN_SSLMODE",
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

        let from_env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        Self::build(
            target,
            from_env("PGPASSWORD"),
            // A separate variable, because the plan database is a separate
            // server with separate credentials (spec §10.4).
            from_env("PGPUSHY_PLAN_PASSWORD"),
        )
    }

    /// The resolution itself, with the environment's contribution passed in.
    ///
    /// Split out from [`Resolved::from`] so it can be tested without mutating
    /// the process environment — which `forbid(unsafe_code)` rules out anyway,
    /// and which would make these tests order-dependent besides.
    fn build(
        target: &Target,
        env_password: Option<String>,
        plan_password: Option<String>,
    ) -> Result<Self> {
        let (password, password_source) = match env_password {
            Some(password) => (Some(password), PasswordSource::Environment),
            None => match target.password.clone() {
                Some(password) => (Some(password), PasswordSource::File),
                None => (None, PasswordSource::None),
            },
        };

        Ok(Self {
            env: target.name.clone(),
            host: target.host.clone(),
            port: target.port,
            dbname: target.db.clone(),
            user: target.user.clone(),
            password,
            password_source,
            sslmode: SslMode::parse(&target.sslmode, &target.name)?,
            plan_db: target
                .plan_db
                .as_ref()
                .map(|plan| PlanConnection::from(plan, plan_password)),
            lock_timeout: target.lock_timeout.clone(),
            allow_destructive: target.allow_destructive,
        })
    }

    /// The driver configuration for pgpushy's own connection.
    ///
    /// Built field by field rather than from a connection string, because a
    /// string is parsed by the driver — whose `sslmode` grammar covers three of
    /// the five modes and hard-errors on the other two. The round trip would
    /// hand the driver back the one setting spec §6.4 gives to pgpushy.
    ///
    /// This carries the mode's fallback half; its verification half is the
    /// connector [`crate::tls::connector`] returns for the same mode.
    pub fn pg_config(&self) -> postgres::Config {
        let mut config = postgres::Config::new();
        config
            .host(&self.host)
            .port(self.port)
            .dbname(&self.dbname)
            .user(&self.user)
            .ssl_mode(Security::for_mode(self.sslmode).fallback);
        if let Some(password) = &self.password {
            config.password(password);
        }
        config
    }

    /// The flags to hand pgschema, so it resolves nothing for itself.
    pub fn pgschema_flags(&self) -> Vec<String> {
        let mut flags = vec![
            "--host".into(),
            self.host.clone(),
            "--port".into(),
            self.port.to_string(),
            "--db".into(),
            self.dbname.clone(),
            "--user".into(),
            self.user.clone(),
            "--sslmode".into(),
            self.sslmode.as_str().to_owned(),
        ];
        if let Some(plan) = &self.plan_db {
            flags.extend(plan.flags());
        }
        flags
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
        // Same reasoning, and the same channel pgschema documents for it. A
        // `--plan-password` flag would put the secret in the process list, and
        // in anything `--verbose` prints.
        if let Some(password) = self
            .plan_db
            .as_ref()
            .and_then(|plan| plan.password.as_ref())
        {
            command.env("PGSCHEMA_PLAN_PASSWORD", password);
        }
        absolutise_certificate_paths(command);
    }

    /// How the target is named in output, without the password.
    pub fn describe(&self) -> String {
        format!("{}@{}:{}/{}", self.user, self.host, self.port, self.dbname)
    }
}

/// Make the certificate-path variables mean the same thing to both connections.
///
/// pgpushy and pgschema each build their own TLS trust from `SSL_CERT_FILE` and
/// `SSL_CERT_DIR`, and both read them from the environment. pgpushy resolves a
/// relative one against the operator's working directory, while pgschema runs
/// in a directory pgpushy chooses — so the same relative path would name two
/// different files, and pgpushy would verify a certificate pgschema then
/// rejects. Spec §6.3 requires that divergence to be impossible by
/// construction rather than diagnosed afterwards, so the child is handed the
/// path pgpushy itself resolved.
fn absolutise_certificate_paths(command: &mut std::process::Command) {
    for var in ["SSL_CERT_FILE", "SSL_CERT_DIR"] {
        let Ok(value) = std::env::var(var) else {
            continue;
        };
        if let Ok(absolute) = std::path::absolute(&value) {
            command.env(var, absolute);
        }
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
            lock_timeout: None,
            allow_destructive: false,
            plan_db: None,
        }
    }

    /// Everything pgpushy's own driver needs, set on the config rather than
    /// spelled into a string the driver would have to parse back.
    #[test]
    fn builds_the_drivers_configuration_from_the_resolved_target() {
        let config = Resolved::build(&target(), None, None).unwrap().pg_config();
        assert_eq!(
            config.get_hosts(),
            [postgres::config::Host::Tcp("db.example".to_owned())]
        );
        assert_eq!(config.get_ports(), [6543]);
        assert_eq!(config.get_dbname(), Some("shop"));
        assert_eq!(config.get_user(), Some("joe"));
        assert_eq!(config.get_password(), Some(b"s3cret".as_slice()));
        assert_eq!(config.get_ssl_mode(), postgres::config::SslMode::Require);
    }

    /// The driver has no mode for the verifying two, so they reach it as
    /// `require` — mandatory encryption — and [`crate::tls`] adds the checking.
    #[test]
    fn the_verifying_modes_reach_the_driver_as_require() {
        for mode in ["verify-ca", "verify-full"] {
            let mut target = target();
            target.sslmode = mode.into();
            let resolved = Resolved::build(&target, None, None).unwrap();
            assert_eq!(
                resolved.pg_config().get_ssl_mode(),
                postgres::config::SslMode::Require,
                "sslmode={mode}",
            );
        }
    }

    /// Every parameter pgpushy resolved is passed on, so pgschema has nothing
    /// left to work out for itself (spec §6.3).
    #[test]
    fn forwards_every_parameter_to_pgschema() {
        let flags = Resolved::build(&target(), None, None)
            .unwrap()
            .pgschema_flags()
            .join(" ");
        assert_eq!(
            flags,
            "--host db.example --port 6543 --db shop --user joe --sslmode require"
        );
        assert!(
            !flags.contains("s3cret"),
            "the password must not be a flag: {flags}"
        );
    }

    /// All five modes resolve (spec §6.4), and each crosses to pgschema as the
    /// word that was written: pgschema implements all five, so nothing here has
    /// to be reduced to what pgpushy's own driver can express.
    #[test]
    fn every_mode_the_spec_lists_resolves_and_reaches_pgschema_unchanged() {
        for mode in ["disable", "prefer", "require", "verify-ca", "verify-full"] {
            let mut target = target();
            target.sslmode = mode.into();
            let resolved = Resolved::build(&target, None, None)
                .unwrap_or_else(|error| panic!("sslmode={mode} should resolve: {error}"));

            assert_eq!(resolved.sslmode.as_str(), mode);
            let flags = resolved.pgschema_flags();
            let position = flags
                .iter()
                .position(|flag| flag == "--sslmode")
                .expect("--sslmode is always passed");
            assert_eq!(flags[position + 1], mode);
        }
    }

    /// An unusable mode fails at resolution, before pgpushy has connected to
    /// anything — and the message says what may be written instead (spec §6.4).
    #[test]
    fn an_unrecognized_mode_is_refused_and_lists_all_five() {
        let mut target = target();
        target.sslmode = "verify_full".into();
        let error = Resolved::build(&target, None, None)
            .expect_err("verify_full is not a libpq spelling")
            .to_string();

        assert!(error.contains("verify_full"), "{error}");
        assert!(error.contains("[env.prod]"), "{error}");
        for mode in ["disable", "prefer", "require", "verify-ca", "verify-full"] {
            assert!(error.contains(mode), "{mode} is not named: {error}");
        }
    }

    /// The plan database's flags never carry its password (spec §10.4): it
    /// travels as PGSCHEMA_PLAN_PASSWORD, out of the process list.
    #[test]
    fn plan_database_flags_never_carry_the_password() {
        let mut target = target();
        target.plan_db = Some(ResolvedPlanDatabase {
            host: "plan.example".into(),
            port: 5432,
            db: "plan".into(),
            user: "planner".into(),
            sslmode: "disable".into(),
            password: Some("zzz-plan-secret-zzz".into()),
        });
        let flags = Resolved::build(&target, None, None)
            .unwrap()
            .pgschema_flags()
            .join(" ");
        assert!(flags.contains("--plan-db plan"), "{flags}");
        assert!(!flags.contains("--plan-password"), "{flags}");
        assert!(!flags.contains("zzz-plan-secret-zzz"), "{flags}");
    }

    #[test]
    fn the_environments_password_is_used_when_nothing_overrides_it() {
        let resolved = Resolved::build(&target(), None, None).unwrap();
        assert_eq!(resolved.password.as_deref(), Some("s3cret"));
        assert_eq!(resolved.password_source, PasswordSource::File);
    }

    /// A secret does not belong in a version-controlled file, so the
    /// environment wins — and there is then nothing to warn about.
    #[test]
    fn pgpassword_overrides_the_environments_password() {
        let resolved = Resolved::build(&target(), Some("from-env".into()), None).unwrap();
        assert_eq!(resolved.password.as_deref(), Some("from-env"));
        assert_eq!(resolved.password_source, PasswordSource::Environment);
    }

    #[test]
    fn no_password_anywhere_is_not_an_error() {
        let mut target = target();
        target.password = None;
        let resolved = Resolved::build(&target, None, None).unwrap();
        assert!(resolved.password.is_none());
        assert_eq!(resolved.password_source, PasswordSource::None);
    }

    #[test]
    fn describes_the_target_without_the_password() {
        let described = Resolved::build(&target(), None, None).unwrap().describe();
        assert_eq!(described, "joe@db.example:6543/shop");
        assert!(!described.contains("s3cret"));
    }
}
