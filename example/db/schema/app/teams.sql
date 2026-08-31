-- teams and employees reference each other: a same-schema cycle no
-- hand-ordered include list can express, and FK-lift handles for free.
CREATE TABLE teams (
    id      bigint PRIMARY KEY,
    name    text   NOT NULL,
    lead_id bigint REFERENCES employees (id)
);

CREATE TABLE employees (
    id      bigint PRIMARY KEY,
    name    text   NOT NULL,
    team_id bigint REFERENCES teams (id)
);
