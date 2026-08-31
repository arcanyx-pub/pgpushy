-- Reference data the application requires. DO UPDATE with the guard makes
-- the listed labels authoritative: a drifted label is corrected on the next
-- apply, and once converged, the probe's second pass touches nothing.
INSERT INTO app.order_statuses AS s (code, label) VALUES
    ('pending',   'Awaiting payment'),
    ('paid',      'Paid'),
    ('shipped',   'Shipped'),
    ('cancelled', 'Cancelled')
ON CONFLICT (code) DO UPDATE SET label = excluded.label
WHERE s.label IS DISTINCT FROM excluded.label;
