-- Work in progress: views are outside pgpushy's statement set, and this file
-- is excluded by pgpushy.toml's "**/*.draft.sql" pattern — so it can sit in
-- the tree without failing validate.
CREATE VIEW wishlist AS SELECT id FROM customers;
