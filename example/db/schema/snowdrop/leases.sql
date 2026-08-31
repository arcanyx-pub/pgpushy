-- Generated source. Do not edit; regenerate with `pgpushy generate`.
-- Command: cargo run --quiet -p xtask -- snowdrop-schema

CREATE SCHEMA IF NOT EXISTS "snowdrop"; CREATE TABLE IF NOT EXISTS "snowdrop"."snowdrop_machine_id_leases" (machine_id SMALLINT PRIMARY KEY, claimed_at TIMESTAMPTZ, reclaimable_after TIMESTAMPTZ) WITH (fillfactor = 70);
