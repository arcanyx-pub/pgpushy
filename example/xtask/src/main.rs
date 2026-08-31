//! Emit snowdrop-id-postgres's published SQL for `pgpushy generate`.
//!
//! The [[generate]] entries in ../pgpushy.toml run this binary through
//! `cargo run -p xtask`, so the SQL that lands in db/ always comes from the
//! exact crate version Cargo.lock pins — never from whatever happens to be
//! installed. `pgpushy generate --check` fails the moment a dependency bump
//! changes the emission, forcing the change into a reviewed diff.

use snowdrop_id_postgres::PgMachineIdLease;
use std::process::ExitCode;

/// The schema the machine-ID leases live in: isolated, so the application
/// role needs nothing but DML on the lease table.
const SCHEMA: &str = "snowdrop";

fn main() -> ExitCode {
    let sql = match std::env::args().nth(1).as_deref() {
        Some("snowdrop-schema") => PgMachineIdLease::schema_sql_with_schema(SCHEMA),
        Some("snowdrop-seeding") => PgMachineIdLease::seeding_sql_with_schema(SCHEMA),
        _ => {
            eprintln!("usage: xtask <snowdrop-schema|snowdrop-seeding>");
            return ExitCode::from(2);
        }
    };
    match sql {
        Ok(sql) => {
            println!("{sql}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}
