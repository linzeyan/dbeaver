-- Two schemas that differ in every way the comparison sheet has to draw, so a
-- screenshot of it shows all of them at once rather than a column of one verdict.
--
-- Read by `make screenshot-diff`. Dropped and remade each time: a fixture that
-- accumulated across runs would make the picture depend on how often it had been
-- taken.
--
-- The pair is deliberately a plausible migration — staging one release ahead of
-- production — rather than a set of contrived objects. What a reader has to be
-- able to do with the picture is tell at a glance which side is which, and that
-- is easier to judge against differences somebody might really be looking at.

DROP SCHEMA IF EXISTS diff_prod CASCADE;
DROP SCHEMA IF EXISTS diff_staging CASCADE;
CREATE SCHEMA diff_prod;
CREATE SCHEMA diff_staging;

CREATE TABLE diff_prod.customer (id integer PRIMARY KEY, name text NOT NULL);
CREATE TABLE diff_staging.customer (id integer PRIMARY KEY, name text NOT NULL);

-- The table both sides have, holding a changed column, a widened one and an
-- added one.
CREATE TABLE diff_prod.invoice (
    id integer PRIMARY KEY,
    customer_id integer NOT NULL,
    sku varchar(32) NOT NULL,
    qty integer NOT NULL DEFAULT 1,
    total numeric(12, 2),
    issued timestamptz NOT NULL);

CREATE TABLE diff_staging.invoice (
    id integer PRIMARY KEY,
    customer_id integer NOT NULL,
    sku varchar(64) NOT NULL,
    qty integer NOT NULL DEFAULT 1,
    total numeric(12, 2),
    issued timestamptz,
    currency char(3));

-- Same name, different index: unique on one side only.
CREATE INDEX invoice_sku_idx ON diff_prod.invoice (sku);
CREATE UNIQUE INDEX invoice_sku_idx ON diff_staging.invoice (sku);

-- Same name, different check.
ALTER TABLE diff_prod.invoice ADD CONSTRAINT invoice_qty_check CHECK (qty > 0);
ALTER TABLE diff_staging.invoice ADD CONSTRAINT invoice_qty_check CHECK (qty >= 0);

-- A foreign key only the newer side has.
ALTER TABLE diff_staging.invoice ADD CONSTRAINT invoice_customer_fk
    FOREIGN KEY (customer_id) REFERENCES diff_staging.customer (id) ON DELETE CASCADE;

-- A table on each side that the other does not have.
CREATE TABLE diff_prod.legacy_note (id integer, note text);
CREATE TABLE diff_staging.audit_log (id integer, at timestamptz NOT NULL);

-- A view both sides have and agree about, so the report is not made only of
-- differences: a schema where everything differs says nothing about which of
-- them matter.
CREATE VIEW diff_prod.paid AS SELECT id, total FROM diff_prod.invoice;
CREATE VIEW diff_staging.paid AS SELECT id, total FROM diff_staging.invoice;
