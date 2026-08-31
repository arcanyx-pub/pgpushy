-- A lookup table whose rows are reference data: the shape lives here, the
-- rows live in ../../seeds/order_statuses.sql.
CREATE TABLE order_statuses (
    code  text PRIMARY KEY,
    label text NOT NULL
);
