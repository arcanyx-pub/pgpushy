CREATE TABLE customers (
    id    bigint PRIMARY KEY,
    email email_address NOT NULL,
    CONSTRAINT customers_email_key UNIQUE (email)
);

COMMENT ON TABLE customers IS 'One row per registered customer.';
COMMENT ON COLUMN customers.email IS 'Checked by the email_address domain.';
