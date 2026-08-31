-- Category-2 objects: a domain the tables below are written in. Unqualified,
-- so it belongs to the default schema (app) like everything else here.
CREATE DOMAIN email_address AS text CHECK (VALUE ~ '@');
