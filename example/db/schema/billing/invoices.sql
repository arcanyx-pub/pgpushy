-- A second managed schema, written qualified. The foreign key into
-- app.orders is what puts billing after app in the apply order.
CREATE TABLE billing.invoices (
    id        bigint PRIMARY KEY,
    order_id  bigint NOT NULL,
    number    bigint NOT NULL,
    issued_on date   NOT NULL,
    CONSTRAINT invoices_number_key UNIQUE (number)
);

-- The ALTER form pg_dump emits, and the one ALTER pgpushy accepts: a
-- foreign key added after the fact. An imported tree needs no rewriting.
ALTER TABLE billing.invoices
    ADD CONSTRAINT invoices_order_fkey FOREIGN KEY (order_id)
    REFERENCES app.orders (id);

-- A standalone sequence, drawn from by application code when numbering
-- invoices. (A sequence as a column DEFAULT is the one shape pgpushy
-- rejects, because pgschema cannot converge it — spec §12.8.)
CREATE SEQUENCE billing.invoice_numbers START WITH 1000;
COMMENT ON SEQUENCE billing.invoice_numbers IS 'Invoice numbering, drawn by the app.';
