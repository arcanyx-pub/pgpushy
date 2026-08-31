-- Generated source. Do not edit; regenerate with `pgpushy generate`.
-- Command: cargo run --quiet -p xtask -- snowdrop-seeding

INSERT INTO "snowdrop"."snowdrop_machine_id_leases" (machine_id) SELECT generate_series(0, 1023) ON CONFLICT (machine_id) DO NOTHING;
