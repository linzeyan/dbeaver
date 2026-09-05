-- One schema holding every shape the diagram has to draw, so a screenshot of it
-- shows all of them at once rather than four boxes in a row.
--
-- Read by `make screenshot-diagram`. Dropped and remade each time, for the reason
-- `seed-diff-schemas.sql` is: a fixture that accumulated across runs would make
-- the picture depend on how often it had been taken.
--
-- A plausible order-taking schema rather than contrived objects, because what a
-- reader has to be able to do with the picture is follow it — which is easier to
-- judge against tables whose names say what they hold.

DROP SCHEMA IF EXISTS erd_demo CASCADE;
CREATE SCHEMA erd_demo;

CREATE TABLE erd_demo.customer (id integer PRIMARY KEY, name text NOT NULL);
CREATE TABLE erd_demo.address (id integer PRIMARY KEY, line1 text, city text);

-- Two keys between the same pair of tables. The box has to list both columns,
-- and the two lines have to be visibly two.
CREATE TABLE erd_demo.orders (
    id integer PRIMARY KEY,
    customer_id integer NOT NULL REFERENCES erd_demo.customer (id),
    billing_address_id integer REFERENCES erd_demo.address (id),
    shipping_address_id integer REFERENCES erd_demo.address (id),
    placed timestamptz NOT NULL);

-- A table that points at itself: the loop, drawn on the box rather than as a
-- line between two points that are the same point.
CREATE TABLE erd_demo.category (
    id integer PRIMARY KEY,
    parent_id integer REFERENCES erd_demo.category (id),
    name text NOT NULL);

CREATE TABLE erd_demo.product (
    id integer PRIMARY KEY,
    category_id integer REFERENCES erd_demo.category (id),
    sku text NOT NULL,
    UNIQUE (id, sku));

-- A composite key, so the picture has a box listing two columns for one line.
CREATE TABLE erd_demo.order_line (
    order_id integer REFERENCES erd_demo.orders (id),
    product_id integer,
    sku text,
    qty integer NOT NULL DEFAULT 1,
    PRIMARY KEY (order_id, product_id),
    FOREIGN KEY (product_id, sku) REFERENCES erd_demo.product (id, sku));

-- Two tables nothing points at and that point at nothing. They get no box, and
-- the sentence under the canvas is what says they were read.
CREATE TABLE erd_demo.audit_log (id integer PRIMARY KEY, at timestamptz NOT NULL);
CREATE TABLE erd_demo.settings (key text PRIMARY KEY, value text);

-- Never asked for its keys at all: a view can be on neither end of one.
CREATE VIEW erd_demo.recent_orders AS
    SELECT id, customer_id FROM erd_demo.orders WHERE placed > now() - interval '7 days';
