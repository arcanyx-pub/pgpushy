//! Read-only target inspection (spec §6).
//!
//! This is the **only** place pgpushy touches the target directly, and every
//! statement it issues here is a `SELECT`. Spec §6 hangs a guarantee on that:
//! pgpushy issues no DDL of its own, so schema and content changes all flow
//! through pgschema, and `plan` cannot mutate the target even incidentally.
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

/// Inspect the target. Read-only.
pub fn inspect(connection: &Resolved, managed: &[SchemaName]) -> Result<Inspection> {
    let config = connection.pg_config();
    // The two halves of `sslmode` (spec §6.4): the config carries what happens
    // when the server declines TLS, and the connector carries how much of the
    // certificate is checked once it does not. `None` is `disable` alone — the
    // driver then never even asks for TLS.
    let mut client = match tls::connector(connection.sslmode)? {
        Some(tls) => config.connect(tls),
        None => config.connect(NoTls),
    }
    .with_context(|| format!("connecting to {}", connection.describe()))?;

    let identity = read_identity(&mut client)?;
    let missing_schemas = missing_schemas(&mut client, managed)?;
    let cross_schema_foreign_keys = cross_schema_foreign_keys(&mut client, managed)?;

    Ok(Inspection {
        missing_schemas,
        cross_schema_foreign_keys,
        identity,
    })
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
