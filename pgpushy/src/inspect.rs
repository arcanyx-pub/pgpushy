//! Read-only target inspection (spec §6).
//!
//! Inspection and seed execution (spec §8.8) are the only places pgpushy
//! touches the target directly, and every statement issued *here* is a
//! `SELECT`. Spec §6 hangs a guarantee on that: pgpushy issues no DDL of its
//! own, so schema changes all flow through pgschema, and `plan` cannot mutate
//! the target even incidentally.
//!
//! One connection answers everything the commands need before delegating.

use crate::conn::Resolved;
use crate::tls;
use anyhow::{Context, Result};
use pgpushy_core::SchemaName;
use postgres::{Client, NoTls};

/// What the target looks like right now.
pub struct Inspection {
    /// Managed schemas that do not exist on the target (spec §6.1).
    pub missing_schemas: Vec<SchemaName>,
    /// Foreign keys crossing between two managed schemas (spec §6.2).
    pub cross_schema_foreign_keys: Vec<CrossSchemaForeignKey>,
    /// Policies and RLS-enabled tables in managed schemas (spec §6.5).
    pub policies: Vec<PolicyOrRls>,
    /// Which database pgpushy actually reached (spec §6.3).
    pub identity: Identity,
}

/// A foreign key on the target whose two ends are in different managed schemas.
///
/// Carries what it *depends on* rather than just what it points at: the
/// referenced columns, and the unique or primary key constraint backing them.
/// Those are the objects whose removal the apply order cannot accommodate
/// (spec §6.2), so they are what the check in [`crate::hazard`] matches
/// against.
#[derive(Debug, Clone)]
pub struct CrossSchemaForeignKey {
    pub from_schema: SchemaName,
    pub from_table: String,
    pub name: String,
    pub to_schema: SchemaName,
    pub to_table: String,
    /// The referenced columns, in key order.
    pub to_columns: Vec<String>,
    /// The unique or primary key constraint the foreign key depends on.
    pub to_constraint: Option<String>,
}

/// A policy, or an RLS-enabled table, in a managed schema (spec §6.5).
///
/// pgschema's ignore file has no section for either, and §4.3 admits no way
/// to describe them, so reconciliation would drop the policy and disable the
/// security — which §8.4 forbids. They are refused by name instead.
#[derive(Debug, Clone)]
pub struct PolicyOrRls {
    pub schema: SchemaName,
    pub table: String,
    /// The policy's name, or `None` for the row-level-security flag itself.
    pub policy: Option<String>,
}

/// Enough to identify the target unambiguously in output.
///
/// The cluster's system identifier is included because host and port can be
/// misleading — a tunnel, a proxy, or a connection pooler all make two
/// different-looking addresses reach the same cluster, or one address reach
/// different ones.
pub struct Identity {
    pub database: String,
    pub server: String,
    pub system_identifier: String,
}

impl std::fmt::Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} on {} (cluster {})",
            self.database, self.server, self.system_identifier
        )
    }
}

/// Open pgpushy's own connection to the target.
///
/// Shared with seed execution (spec §8.8), which is what makes §6.3 hold for
/// it too: the seeds land on the same database the inspection saw.
pub fn connect(connection: &Resolved) -> Result<Client> {
    let config = connection.pg_config();
    // The two halves of `sslmode` (spec §6.4): the config carries what happens
    // when the server declines TLS, and the connector carries how much of the
    // certificate is checked once it does not. `None` is `disable` alone — the
    // driver then never even asks for TLS.
    match tls::connector(connection.sslmode)? {
        Some(tls) => config.connect(tls),
        None => config.connect(NoTls),
    }
    .with_context(|| format!("connecting to {}", connection.describe()))
}

/// Inspect the target. Read-only.
pub fn inspect(connection: &Resolved, managed: &[SchemaName]) -> Result<Inspection> {
    let mut client = connect(connection)?;

    let identity = read_identity(&mut client)?;
    let missing_schemas = missing_schemas(&mut client, managed)?;
    let cross_schema_foreign_keys = cross_schema_foreign_keys(&mut client, managed)?;
    let policies = policies(&mut client, managed)?;

    Ok(Inspection {
        missing_schemas,
        cross_schema_foreign_keys,
        policies,
        identity,
    })
}

/// Policies and RLS-enabled tables in the managed schemas (spec §6.5).
fn policies(client: &mut Client, managed: &[SchemaName]) -> Result<Vec<PolicyOrRls>> {
    if managed.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<&str> = managed.iter().map(SchemaName::as_str).collect();
    let rows = client
        .query(
            "SELECT n.nspname, c.relname, p.polname::text
             FROM pg_policy p
             JOIN pg_class c     ON c.oid = p.polrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = ANY($1)
             UNION ALL
             SELECT n.nspname, c.relname, NULL
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relrowsecurity AND c.relkind = 'r' AND n.nspname = ANY($1)
             ORDER BY 1, 2, 3",
            &[&names],
        )
        .context("reading policies and row-level security from pg_policy")?;

    Ok(rows
        .iter()
        .map(|row| PolicyOrRls {
            schema: SchemaName::new(row.get::<_, String>(0)),
            table: row.get(1),
            policy: row.get(2),
        })
        .collect())
}

/// Managed schemas that are non-empty in the plan database (spec §10.4).
///
/// A previous run's closure members accumulate there and collide with this
/// run's, midway through the loop, as pgschema's error. Read-only: pgpushy
/// issues no DDL to the plan database either, following pgschema's own lead —
/// the remedy is the operator's drop-and-recreate. An empty leftover schema
/// is deliberately not reported: a project with no cross-schema references
/// re-plans against the same plan database indefinitely (verified), and
/// refusing it would break what works.
pub fn plan_db_leftovers(
    plan: &crate::conn::PlanConnection,
    managed: &[SchemaName],
) -> Result<Vec<SchemaName>> {
    let (config, sslmode) = plan.pg_config()?;
    let mut client = match tls::connector(sslmode)? {
        Some(tls) => config.connect(tls),
        None => config.connect(NoTls),
    }
    .with_context(|| format!("connecting to the plan database {}", plan.describe()))?;

    let names: Vec<&str> = managed.iter().map(SchemaName::as_str).collect();
    let rows = client
        .query(
            "SELECT DISTINCT nspname FROM (
                 SELECT n.nspname
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = ANY($1)
                 UNION ALL
                 SELECT n.nspname
                 FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
                 WHERE n.nspname = ANY($1) AND t.typtype IN ('d', 'e')
             ) s ORDER BY nspname",
            &[&names],
        )
        .context("reading leftover state from the plan database")?;

    Ok(rows
        .iter()
        .map(|row| SchemaName::new(row.get::<_, String>(0)))
        .collect())
}

/// Foreign keys on the target that cross between two managed schemas.
///
/// Both ends must be managed: a foreign key reaching into a schema pgpushy
/// does not reconcile cannot be affected by an ordering pgpushy chooses,
/// because pgpushy never applies anything to the other end.
fn cross_schema_foreign_keys(
    client: &mut Client,
    managed: &[SchemaName],
) -> Result<Vec<CrossSchemaForeignKey>> {
    if managed.len() < 2 {
        return Ok(Vec::new());
    }

    let names: Vec<&str> = managed.iter().map(SchemaName::as_str).collect();
    let rows = client
        .query(
            "SELECT fn.nspname,
                    fc.relname,
                    con.conname,
                    tn.nspname,
                    tc.relname,
                    ARRAY(
                        SELECT a.attname
                        FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord)
                        JOIN pg_attribute a
                          ON a.attrelid = con.confrelid AND a.attnum = k.attnum
                        ORDER BY k.ord
                    ),
                    (SELECT u.conname
                       FROM pg_constraint u
                      WHERE u.conindid = con.conindid
                        AND u.contype IN (\'p\', \'u\')
                      LIMIT 1)
             FROM pg_constraint con
             JOIN pg_class fc     ON fc.oid = con.conrelid
             JOIN pg_namespace fn ON fn.oid = fc.relnamespace
             JOIN pg_class tc     ON tc.oid = con.confrelid
             JOIN pg_namespace tn ON tn.oid = tc.relnamespace
             WHERE con.contype = \'f\'
               AND fn.nspname <> tn.nspname
               AND fn.nspname = ANY($1)
               AND tn.nspname = ANY($1)
             ORDER BY 1, 2, 3",
            &[&names],
        )
        .context("reading cross-schema foreign keys from pg_constraint")?;

    Ok(rows
        .iter()
        .map(|row| CrossSchemaForeignKey {
            from_schema: SchemaName::new(row.get::<_, String>(0)),
            from_table: row.get(1),
            name: row.get(2),
            to_schema: SchemaName::new(row.get::<_, String>(3)),
            to_table: row.get(4),
            to_columns: row.get(5),
            to_constraint: row.get(6),
        })
        .collect())
}

/// Which of the managed schemas are absent.
///
/// Reported as a complete list rather than the first one found: an operator
/// introducing three schemas should create three, not discover them one run at
/// a time (spec §6.1).
fn missing_schemas(client: &mut Client, managed: &[SchemaName]) -> Result<Vec<SchemaName>> {
    if managed.is_empty() {
        return Ok(Vec::new());
    }

    let names: Vec<&str> = managed.iter().map(SchemaName::as_str).collect();
    let rows = client
        .query(
            "SELECT nspname FROM pg_namespace WHERE nspname = ANY($1)",
            &[&names],
        )
        .context("reading schemas from pg_namespace")?;

    let present: std::collections::BTreeSet<String> =
        rows.iter().map(|row| row.get::<_, String>(0)).collect();

    Ok(managed
        .iter()
        .filter(|schema| !present.contains(schema.as_str()))
        .cloned()
        .collect())
}

fn read_identity(client: &mut Client) -> Result<Identity> {
    let row = client
        .query_one(
            "SELECT current_database(),
                    COALESCE(host(inet_server_addr())::text, 'local'),
                    COALESCE(inet_server_port(), 0),
                    (SELECT system_identifier::text FROM pg_control_system())",
            &[],
        )
        .context("reading the target's identity")?;

    let host: String = row.get(1);
    let port: i32 = row.get(2);

    Ok(Identity {
        database: row.get(0),
        server: if port == 0 {
            host
        } else {
            format!("{host}:{port}")
        },
        system_identifier: row.get(3),
    })
}
