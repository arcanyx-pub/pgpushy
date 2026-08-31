-- This file sorts before the tables it references would need it to — which
-- is the point: pgpushy lifts the foreign keys, so file order never matters.
CREATE TABLE orders (
    id          bigint PRIMARY KEY,
    customer_id bigint NOT NULL REFERENCES customers (id),
    status      text   NOT NULL REFERENCES order_statuses (code),
    placed_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX orders_customer_idx ON orders (customer_id);
COMMENT ON INDEX orders_customer_idx IS 'The lookup the app makes constantly.';
