//! Read-only target inspection (spec §6).
//!
//! This is the **only** place pgpushy touches the target directly, and every
//! statement it issues here is a `SELECT`. Spec §6 hangs a guarantee on that:
//! pgpushy issues no DDL of its own, so schema and content changes all flow
//! through pgschema, and `plan` cannot mutate the target even incidentally.
//!
//! One connection answers everything the commands need before delegating.

use crate::conn::Resolved;
use anyhow::{Context, Result};
use pgpushy_core::SchemaName;
use postgres::{Client, NoTls};

/// What the target looks like right now.
pub struct Inspection {
    /// Managed schemas that do not exist on the target (spec §6.1).
    pub missing_schemas: Vec<SchemaName>,
    /// Which database pgpushy actually reached (spec §6.3).
    pub identity: Identity,
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
    let mut client = Client::connect(&connection.conninfo(), NoTls)
        .with_context(|| format!("connecting to {}", connection.describe()))?;

    let identity = read_identity(&mut client)?;
    let missing_schemas = missing_schemas(&mut client, managed)?;

    Ok(Inspection {
        missing_schemas,
        identity,
    })
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
