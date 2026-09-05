//! PostgreSQL DDL, against hand-built metadata and against the server.
//!
//! Both halves assert the same strings, and that is the arrangement rather than
//! a coincidence. The constants below are what upstream emits — established by
//! reading the Java named on each one, not by running this and writing down what
//! came out. The tests with a fake driver check that this crate turns metadata
//! into that text; the ignored ones check that a real `bench` database produces
//! exactly the metadata the fake claims it does. A change that breaks the second
//! group only means the fixture drifted; a change that breaks the first means
//! the renderer did.
//!
//! Where this deliberately differs from upstream, the difference is stated on
//! the constant it shows up in, with the reason.

use dbconn::{
    Browse, Capabilities, ColumnInfo, Computed, ConstraintInfo, ConstraintKind, Cursor,
    DatabaseInfo, DbResult, Driver, IndexInfo, RelationInfo, RelationKind, RelationshipInfo,
    ResultStream, SchemaInfo, ServerInfo, ServerProcesses, TriggerInfo, TxStep, UniqueKeyInfo,
};
use driver_postgres::PgSource;

const BENCH: &str = "postgres://bench:bench@127.0.0.1:55432/bench";

// ---------------------------------------------------------------------------
// What upstream emits
// ---------------------------------------------------------------------------

/// `public.bench_child`, the fixture that has one of everything.
///
/// Read from `SQLTableManager.getTableDDL`, which is what
/// `PostgreTable.getObjectDefinitionText` reaches through
/// `DBStructUtils.generateTableDDL`. The order is that method's: the drop
/// commented out, the `CREATE TABLE`, then what could not go inside it —
/// indexes, whose manager declares nothing nested, and the triggers
/// `PostgreTableManagerBase.addObjectExtraActions` appends under a heading.
///
/// Deliberately different from upstream in three places:
///
/// - `CHECK (qty > 0)` where upstream has `CHECK ((qty > 0))`.
///   `PostgreTableConstraintBase.getObjectDefinitionText` calls
///   `pg_get_constraintdef(oid)` and the driver calls it with the pretty flag,
///   which is the flag that drops the redundant parentheses. Same predicate.
/// - Constraints come out primary key, then in the driver's order — kind, then
///   name. Upstream's constraint cache is `ORDER BY c.oid`, which is creation
///   order, and no oid reaches this crate.
/// - Triggers are the server's own text. Upstream runs `pg_get_triggerdef`
///   through `SQLFormatUtils.formatSQL`, which lower-cases it and rewraps it
///   across five lines; the statement is the same one. The `ON bench_child` is
///   unqualified because the driver asks for the pretty rendering, which omits a
///   schema that is on the search path — upstream's unpretty call gets
///   `ON public.bench_child`.
const BENCH_CHILD: &str = "-- Drop table

-- DROP TABLE public.bench_child;

CREATE TABLE public.bench_child (
\torder_id int4 NOT NULL,
\tline_no int2 NOT NULL,
\tparent_id int4 NOT NULL,
\tsku text NOT NULL,
\temail text NULL,
\tqty int4 DEFAULT 1 NOT NULL,
\tshipped_at timestamp NULL,
\tCONSTRAINT bench_child_pkey PRIMARY KEY (order_id, line_no),
\tCONSTRAINT bench_child_qty_positive CHECK (qty > 0),
\tCONSTRAINT bench_child_order_line_uniq UNIQUE (order_id, line_no, sku),
\tCONSTRAINT bench_child_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES public.bench_wide(id) ON DELETE CASCADE
);
CREATE INDEX bench_child_email_lower_idx ON public.bench_child USING btree (lower(email));
CREATE INDEX bench_child_pending_idx ON public.bench_child USING btree (order_id) WHERE (shipped_at IS NULL);
CREATE UNIQUE INDEX bench_child_sku_key ON public.bench_child USING btree (sku);

-- Table Triggers

CREATE TRIGGER bench_child_after_delete AFTER DELETE ON bench_child FOR EACH STATEMENT EXECUTE FUNCTION bench_child_touch();
CREATE TRIGGER bench_child_before_write BEFORE INSERT OR UPDATE ON bench_child FOR EACH ROW EXECUTE FUNCTION bench_child_touch();";

/// `public.bench_wide`, which is here for the type spellings and nothing else.
///
/// Every name on the right of a column is `pg_type.typname`, because upstream
/// prints the type object and `PostgreDataType`'s name is that column
/// (`PostgreDataTypeModifier` in `PostgreTableColumnManager`). `numeric(18, 4)`
/// carries a space after the comma that `format_type` does not write, because
/// upstream builds the modifier itself in
/// `PostgreNumericTypeHandler.getTypeModifiersString`.
///
/// One deliberate difference, and it is in `name text NULL`. Upstream quotes
/// that column: `JDBCSQLDialect.loadDataTypesFromDatabase` puts every local data
/// type name into the dialect's keyword table as `DBPKeywordType.TYPE`, `name`
/// is a PostgreSQL type, and `AbstractSQLDialect.mustBeQuoted` quotes a keyword.
/// `dbsql::Dialect` carries no type names, so this crate leaves it bare. Both
/// name the same column — `name` is not reserved — and closing the gap means
/// adding PostgreSQL's type names to the dialect table in `crates/sql`.
const BENCH_WIDE: &str = "-- Drop table

-- DROP TABLE public.bench_wide;

CREATE TABLE public.bench_wide (
\tid int4 NOT NULL,
\tbig_val int8 NULL,
\tint_val int4 NULL,
\tnum_val numeric(18, 4) NULL,
\treal_val float4 NULL,
\tdbl_val float8 NULL,
\tname text NULL,
\thash_hex text NULL,
\tpayload text NULL,
\tcategory text NULL,
\tflag bool NULL,
\tcreated_at timestamp NULL,
\tcreated_on date NULL,
\tcreated_time time NULL,
\tuuid_val uuid NULL,
\tsmall_val int2 NULL,
\tnullable_text text NULL,
\tnullable_int int4 NULL,
\tjson_val jsonb NULL,
\tbytes_val bytea NULL,
\tCONSTRAINT bench_wide_pkey PRIMARY KEY (id)
);";

/// `public.no_key`: no key, no index, no constraint, no trigger.
///
/// The whole of it is the skeleton — `SQLTableManager.beginCreateTableStatement`
/// through the closing bracket — plus the drop header, which
/// `SQLTableManager.getTableDDL` writes for every persisted table and not only
/// for interesting ones.
const NO_KEY: &str = "-- Drop table

-- DROP TABLE public.no_key;

CREATE TABLE public.no_key (
\tn int4 NULL,
\tlabel text NULL
);";

/// `reporting.daily_totals`, which is here because it is not in `public`.
///
/// `DBUtils.getEntityScriptName` defaults `OPTION_FULLY_QUALIFIED_NAMES` to
/// true, so the schema is written even when it is the default one; the failure
/// that catches is a script that recreates the table wherever `search_path`
/// happens to point.
const DAILY_TOTALS: &str = "-- Drop table

-- DROP TABLE reporting.daily_totals;

CREATE TABLE reporting.daily_totals (
\tday date NOT NULL,
\torders int4 NOT NULL,
\trevenue numeric(12, 2) NOT NULL,
\tCONSTRAINT daily_totals_pkey PRIMARY KEY (day)
);";

/// `public.bench_open_lines`, from `PostgreUtils.getViewDDL`.
///
/// `CREATE OR REPLACE` because that function uses the replacing form for a plain
/// view. The body is `pg_get_viewdef`'s, indentation and all, with the trailing
/// semicolon stripped and one put back at the end — upstream strips it in the
/// same place and for the same reason.
const BENCH_OPEN_LINES: &str = "CREATE OR REPLACE VIEW public.bench_open_lines
AS SELECT c.order_id,
    c.line_no,
    c.sku,
    c.qty,
    w.category,
    w.created_on AS ordered_on
   FROM bench_child c
     JOIN bench_wide w ON w.id = c.parent_id
  WHERE c.shipped_at IS NULL;";

// ---------------------------------------------------------------------------
// A database that answers from memory
// ---------------------------------------------------------------------------

/// One relation's metadata, standing still.
///
/// The names it is asked about are ignored: a fixture holds one relation, so a
/// call that reached the wrong one would have to be a call this crate should not
/// be making at all.
#[derive(Default)]
struct Fixture {
    columns: Vec<ColumnInfo>,
    indexes: Vec<IndexInfo>,
    constraints: Vec<ConstraintInfo>,
    foreign_keys: Vec<RelationshipInfo>,
    triggers: Vec<TriggerInfo>,
    definition: Option<String>,
}

#[async_trait::async_trait]
impl Driver for Fixture {
    async fn server_info(&self) -> DbResult<ServerInfo> {
        unreachable!("DDL is rendered for a relation the caller already has")
    }
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(None)
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        unreachable!("DDL is rendered for a relation the caller already has")
    }

    async fn relations(&self, _: &str) -> DbResult<Vec<RelationInfo>> {
        unreachable!("DDL is rendered for a relation the caller already has")
    }

    async fn columns(&self, _: &str, _: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(self.columns.clone())
    }

    async fn definition(&self, _: &str, _: &str) -> DbResult<Option<String>> {
        Ok(self.definition.clone())
    }

    async fn indexes(&self, _: &str, _: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(self.indexes.clone())
    }

    async fn unique_keys(&self, _: &str, _: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        unreachable!("a unique key reaches the script through constraints()")
    }

    async fn foreign_keys(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(self.foreign_keys.clone())
    }

    async fn referenced_by(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        // Inbound references belong to other tables' DDL, so asking for them
        // here would be a bug worth failing on rather than answering.
        unreachable!("a table's own DDL does not name what points at it")
    }

    async fn constraints(&self, _: &str, _: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(self.constraints.clone())
    }

    async fn triggers(&self, _: &str, _: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(self.triggers.clone())
    }

    fn browse(&self, _: &Browse<'_>) -> String {
        unreachable!("DDL is rendered from metadata, never from a browse")
    }
    async fn query(&self, _: &str, _: usize) -> DbResult<Box<dyn ResultStream>> {
        unreachable!("DDL is rendered from metadata, never from a query")
    }

    async fn cursor(&self, _: &str, _: usize) -> DbResult<Box<dyn Cursor>> {
        unreachable!("DDL is rendered from metadata, never from a query")
    }

    async fn cancel(&self) -> DbResult<()> {
        unreachable!("nothing here is long enough to cancel")
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: false,
            cancel_stops_the_statement: false,
            switches_database: false,
            schema_is_the_database: false,
            // A rendered statement is read off the metadata it was
            // handed; this double is never asked for routines.
            reports_routines: false,
            // Nor sequences, for the same reason as the line above.
            reports_sequences: false,
            server_processes: ServerProcesses::Unreported,
            reports_variables: false,
            // DDL is rendered from metadata; this double is never asked to
            // write a row.
            writes_rows: false,
        }
    }

    async fn transaction(&self, _: &TxStep) -> DbResult<()> {
        unreachable!("reading a table's shape opens no transaction")
    }
}

fn relation(schema: &str, name: &str, kind: RelationKind) -> RelationInfo {
    RelationInfo {
        schema: schema.to_string(),
        name: name.to_string(),
        kind,
        estimated_rows: None,
    }
}

/// Columns in catalog order, from `(name, type, nullable, default)`.
///
/// `is_primary_key` is left false throughout, because the renderer does not read
/// it: a column that says it belongs to the key does not say to which key or in
/// what position, and a `PRIMARY KEY (a, b)` needs both.
fn columns(spec: &[(&str, &str, bool, Option<&str>)]) -> Vec<ColumnInfo> {
    spec.iter()
        .enumerate()
        .map(|(i, (name, data_type, nullable, default))| ColumnInfo {
            name: (*name).to_string(),
            data_type: (*data_type).to_string(),
            nullable: *nullable,
            position: i as i32 + 1,
            is_primary_key: false,
            default_value: default.map(str::to_string),
            computed: None,
        })
        .collect()
}

fn index(name: &str, keys: &[&str], unique: bool, primary: bool) -> IndexInfo {
    IndexInfo {
        name: name.to_string(),
        columns: keys.iter().map(|k| (*k).to_string()).collect(),
        is_unique: unique,
        is_primary: primary,
        method: "btree".to_string(),
        predicate: None,
    }
}

fn constraint(name: &str, kind: ConstraintKind, definition: &str) -> ConstraintInfo {
    ConstraintInfo {
        name: name.to_string(),
        kind,
        definition: definition.to_string(),
    }
}

/// The metadata the `bench` database holds for `public.bench_child`.
///
/// In the order the PostgreSQL driver returns each list, because the renderer
/// preserves it and the ignored test below is what proves the order is real:
/// indexes by `indisprimary DESC, relname`, constraints by `contype, conname`,
/// triggers and foreign keys by name.
fn bench_child() -> Fixture {
    Fixture {
        columns: columns(&[
            ("order_id", "integer", false, None),
            ("line_no", "smallint", false, None),
            ("parent_id", "integer", false, None),
            ("sku", "text", false, None),
            ("email", "text", true, None),
            ("qty", "integer", false, Some("1")),
            ("shipped_at", "timestamp without time zone", true, None),
        ]),
        indexes: vec![
            index("bench_child_pkey", &["order_id", "line_no"], true, true),
            index(
                "bench_child_email_lower_idx",
                &["lower(email)"],
                false,
                false,
            ),
            index(
                "bench_child_order_line_uniq",
                &["order_id", "line_no", "sku"],
                true,
                false,
            ),
            IndexInfo {
                predicate: Some("(shipped_at IS NULL)".to_string()),
                ..index("bench_child_pending_idx", &["order_id"], false, false)
            },
            index("bench_child_sku_key", &["sku"], true, false),
        ],
        constraints: vec![
            constraint(
                "bench_child_qty_positive",
                ConstraintKind::Check,
                "CHECK (qty > 0)",
            ),
            constraint(
                "bench_child_order_line_uniq",
                ConstraintKind::Unique,
                "UNIQUE (order_id, line_no, sku)",
            ),
        ],
        foreign_keys: vec![RelationshipInfo {
            name: "bench_child_parent_id_fkey".to_string(),
            local_columns: vec!["parent_id".to_string()],
            other_schema: "public".to_string(),
            other_table: "bench_wide".to_string(),
            other_columns: vec!["id".to_string()],
            on_update: "NO ACTION".to_string(),
            on_delete: "CASCADE".to_string(),
        }],
        triggers: vec![
            TriggerInfo {
                name: "bench_child_after_delete".to_string(),
                timing: Some("AFTER".to_string()),
                events: vec!["DELETE".to_string()],
                level: Some("STATEMENT".to_string()),
                function: Some("bench_child_touch".to_string()),
                enabled: false,
                definition: Some(
                    "CREATE TRIGGER bench_child_after_delete AFTER DELETE ON bench_child \
                     FOR EACH STATEMENT EXECUTE FUNCTION bench_child_touch()"
                        .to_string(),
                ),
            },
            TriggerInfo {
                name: "bench_child_before_write".to_string(),
                timing: Some("BEFORE".to_string()),
                events: vec!["INSERT".to_string(), "UPDATE".to_string()],
                level: Some("ROW".to_string()),
                function: Some("bench_child_touch".to_string()),
                enabled: true,
                definition: Some(
                    "CREATE TRIGGER bench_child_before_write BEFORE INSERT OR UPDATE \
                     ON bench_child FOR EACH ROW EXECUTE FUNCTION bench_child_touch()"
                        .to_string(),
                ),
            },
        ],
        definition: None,
    }
}

async fn rendered(driver: &dyn Driver, relation: &RelationInfo) -> String {
    dbddl::definition(driver, &dbsql::POSTGRES, relation)
        .await
        .expect("rendering failed")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The whole of a table, from metadata that never touched a socket.
///
/// One assertion over the whole string rather than a search for fragments,
/// because everything this crate can get wrong is a matter of arrangement:
/// where the comma goes, which clause precedes which, how many newlines separate
/// two statements. A test that checked the pieces were present would pass on
/// output nobody could execute.
#[tokio::test]
async fn a_table_is_rendered_with_its_keys_indexes_and_triggers() {
    let ddl = rendered(
        &bench_child(),
        &relation("public", "bench_child", RelationKind::Table),
    )
    .await;
    assert_eq!(ddl, BENCH_CHILD);
}

/// A table with nothing but columns still gets the drop header.
///
/// Catches the temptation to write the header only when there is something
/// interesting under it: `SQLTableManager.getTableDDL` emits it for every
/// persisted table, and a DDL tab whose first two lines come and go depending on
/// the table is a worse tab.
#[tokio::test]
async fn a_table_with_no_key_at_all_is_still_dropped_before_it_is_created() {
    let fixture = Fixture {
        columns: columns(&[("n", "integer", true, None), ("label", "text", true, None)]),
        ..Fixture::default()
    };
    let ddl = rendered(&fixture, &relation("public", "no_key", RelationKind::Table)).await;
    assert_eq!(ddl, NO_KEY);
}

/// The index behind a unique constraint is not created twice.
///
/// PostgreSQL builds an index for every unique constraint and the driver lists
/// it like any other, so the naive rendering emits the constraint inside the
/// table and the same index again after it — a script that fails on the second
/// statement. `PostgreTableManagerBase.isIncludeIndexInDDL` drops it by way of
/// `PostgreIndex.isPrimaryKeyIndex`, which is set for the index behind any
/// unique constraint and not only behind the primary key.
#[tokio::test]
async fn an_index_that_only_exists_to_enforce_a_constraint_is_not_created_again() {
    let ddl = rendered(
        &bench_child(),
        &relation("public", "bench_child", RelationKind::Table),
    )
    .await;
    assert!(
        !ddl.contains("CREATE UNIQUE INDEX bench_child_order_line_uniq"),
        "the unique constraint's own index was emitted as well:\n{ddl}"
    );
    assert!(
        !ddl.contains("CREATE UNIQUE INDEX bench_child_pkey"),
        "the primary key's index was emitted as well:\n{ddl}"
    );
    assert!(
        ddl.contains("CREATE UNIQUE INDEX bench_child_sku_key"),
        "a unique index nothing declared as a constraint was dropped:\n{ddl}"
    );
}

/// A generated column is a computation, not a default.
///
/// `pg_get_expr` hands back a generation expression in the same shape it hands
/// back a default, so before `attgenerated` was read this rendered as
/// `total numeric DEFAULT ((qty * price))` — which PostgreSQL refuses outright,
/// because a default may not reference another column. Wrong in the way that is
/// hardest to notice: it reads as a table somebody could have written.
#[tokio::test]
async fn a_generated_column_is_written_as_a_generation() {
    let mut spec = columns(&[
        ("qty", "integer", false, None),
        ("price", "numeric(12,2)", false, None),
        // As `pg_get_expr` renders it, parentheses and all.
        ("total", "numeric(12,2)", true, Some("(qty * price)")),
    ]);
    spec[2].computed = Some(Computed::Stored);
    let fixture = Fixture {
        columns: spec,
        ..Fixture::default()
    };

    let ddl = rendered(&fixture, &relation("public", "t", RelationKind::Table)).await;
    assert!(
        ddl.contains("total numeric(12, 2) GENERATED ALWAYS AS ((qty * price)) STORED NULL"),
        "got:\n{ddl}"
    );
    assert!(
        !ddl.contains("DEFAULT"),
        "the expression came out as a default:\n{ddl}"
    );
}

/// And the server takes back what was written for it.
///
/// The strongest statement available about generated columns, and the reason it
/// is worth a scratch schema: rendering `GENERATED ALWAYS AS` proves the words
/// were chosen, and executing it proves they were the right ones. The old
/// `DEFAULT` form failed this at the server rather than at an assertion.
#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_generated_column_renders_into_a_table_the_server_accepts() {
    let source = bench().await;
    // Its own schema rather than a table in `public`: the benchmark database is
    // shared with every other suite, and a fixture left behind is a fixture the
    // next reader has to explain.
    run(&source, "DROP SCHEMA IF EXISTS ddl_generated CASCADE").await;
    run(&source, "CREATE SCHEMA ddl_generated").await;
    run(
        &source,
        "CREATE TABLE ddl_generated.invoice (
             id integer NOT NULL,
             qty integer NOT NULL,
             price numeric(12,2) NOT NULL,
             total numeric(12,2) GENERATED ALWAYS AS (qty * price) STORED
         )",
    )
    .await;

    let script = from_server(&source, "ddl_generated", "invoice").await;
    assert!(
        script.contains("GENERATED ALWAYS AS") && !script.contains("DEFAULT"),
        "the catalog's own generated column did not come back as one:\n{script}"
    );

    run(&source, "CREATE SCHEMA ddl_generated_replay").await;
    run(
        &source,
        &script.replace("ddl_generated.", "ddl_generated_replay."),
    )
    .await;
    let copy = from_server(&source, "ddl_generated_replay", "invoice").await;
    assert_eq!(
        copy,
        script.replace("ddl_generated.", "ddl_generated_replay."),
        "the table built from the script does not render as the script"
    );

    run(&source, "DROP SCHEMA ddl_generated CASCADE").await;
    run(&source, "DROP SCHEMA ddl_generated_replay CASCADE").await;
}

async fn run(source: &PgSource, sql: &str) {
    let mut stream = source
        .query(sql, 1)
        .await
        .unwrap_or_else(|e| panic!("statement failed: {e}\n{sql}"));
    while stream
        .next_batch()
        .await
        .unwrap_or_else(|e| panic!("statement failed: {e}\n{sql}"))
        .is_some()
    {}
}

/// A type keeps its meaning while it changes its spelling.
///
/// The failure this catches is a modifier lost or misplaced while the name
/// around it is being rewritten — `format_type` writes `timestamp(3) without
/// time zone`, with the modifier in the middle of the name being looked up, so a
/// rewrite that splits on the first parenthesis and stops loses `without time
/// zone` and silently changes the column's type.
#[tokio::test]
async fn a_type_is_spelled_the_way_the_catalog_spells_it() {
    let fixture = Fixture {
        columns: columns(&[
            ("a", "integer", true, None),
            ("b", "character varying(64)", true, None),
            ("c", "numeric(18,4)", true, None),
            ("d", "timestamp(3) without time zone", true, None),
            ("e", "double precision", true, None),
            ("f", "jsonb", true, None),
        ]),
        ..Fixture::default()
    };
    let ddl = rendered(&fixture, &relation("public", "t", RelationKind::Table)).await;
    let declared: Vec<&str> = ddl
        .lines()
        .filter(|line| line.starts_with('\t'))
        .map(|line| line.trim().trim_end_matches(','))
        .collect();
    assert_eq!(
        declared,
        [
            "a int4 NULL",
            "b varchar(64) NULL",
            "c numeric(18, 4) NULL",
            "d timestamp(3) NULL",
            "e float8 NULL",
            "f jsonb NULL",
        ]
    );
}

/// A view is the statement the server gave, wrapped and nothing else.
///
/// The failure this catches is the doubled semicolon: `pg_get_viewdef` returns
/// its text terminated, and `PostgreUtils.getViewDDL` strips that before adding
/// its own, so a renderer that only appends produces `…IS NULL;;`.
#[tokio::test]
async fn a_view_is_rendered_from_the_definition_the_server_gave() {
    let fixture = Fixture {
        definition: Some(
            " SELECT c.order_id,\n    c.sku\n   FROM bench_child c\n  WHERE c.shipped_at IS NULL;"
                .to_string(),
        ),
        ..Fixture::default()
    };
    let ddl = rendered(
        &fixture,
        &relation("public", "bench_open_lines", RelationKind::View),
    )
    .await;
    assert_eq!(
        ddl,
        "CREATE OR REPLACE VIEW public.bench_open_lines\n\
         AS SELECT c.order_id,\n    c.sku\n   FROM bench_child c\n  WHERE c.shipped_at IS NULL;"
    );
}

/// A kind whose DDL cannot be stated is refused by name.
///
/// A materialized view ends in `WITH DATA` or `WITH NO DATA` and nothing in the
/// metadata says which, so the alternatives are a refusal and a statement that
/// might silently populate — or fail to populate — a view. This test exists so
/// that "we do not guess" stays a decision instead of becoming an oversight the
/// first time somebody makes the match arm fall through.
#[tokio::test]
async fn a_materialized_view_is_refused_rather_than_guessed_at() {
    let fixture = Fixture {
        definition: Some("SELECT 1;".to_string()),
        ..Fixture::default()
    };
    let error = dbddl::definition(
        &fixture,
        &dbsql::POSTGRES,
        &relation(
            "public",
            "bench_category_totals",
            RelationKind::MaterializedView,
        ),
    )
    .await
    .expect_err("a materialized view was rendered from metadata that cannot describe one");
    assert!(
        error.to_string().contains("bench_category_totals"),
        "the refusal does not say which object it is about: {error}"
    );
}

// ---------------------------------------------------------------------------
// Against the server
// ---------------------------------------------------------------------------

async fn bench() -> PgSource {
    PgSource::connect(BENCH)
        .await
        .expect("benchmark database unreachable; run `make db-seed`")
}

async fn from_server(source: &PgSource, schema: &str, name: &str) -> String {
    let relation = source
        .relations(schema)
        .await
        .expect("listing relations failed")
        .into_iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("{schema}.{name} is not in the benchmark database"));
    rendered(source, &relation).await
}

/// The rich fixture, rendered from the catalog rather than from a fake.
///
/// This is the test the phase-4 criterion is about: the string it compares
/// against was written from the Java, so a difference here is a difference from
/// upstream. It also pins the driver's orderings — a change to `ORDER BY` in
/// `crates/drivers/postgres` moves constraints or indexes around in generated
/// DDL, and there is nowhere else that would notice.
#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn the_child_table_renders_from_the_server_exactly_as_upstream_writes_it() {
    let source = bench().await;
    assert_eq!(
        from_server(&source, "public", "bench_child").await,
        BENCH_CHILD
    );
}

/// Twenty columns, for the twenty type spellings.
///
/// Hand-built metadata can only prove the mapping is applied; only the server
/// proves the strings going into it are the strings `format_type` produces.
#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn every_column_type_in_the_wide_table_is_named_as_the_catalog_names_it() {
    let source = bench().await;
    assert_eq!(
        from_server(&source, "public", "bench_wide").await,
        BENCH_WIDE
    );
}

/// A table with no primary key renders anyway.
#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_table_with_no_primary_key_renders_from_the_server() {
    let source = bench().await;
    assert_eq!(from_server(&source, "public", "no_key").await, NO_KEY);
}

/// A relation outside `public` names its schema.
#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_table_in_another_schema_is_written_with_that_schema() {
    let source = bench().await;
    assert_eq!(
        from_server(&source, "reporting", "daily_totals").await,
        DAILY_TOTALS
    );
}

/// The view, with the server's own formatting of the body preserved.
#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn the_view_renders_from_the_definition_the_server_holds() {
    let source = bench().await;
    assert_eq!(
        from_server(&source, "public", "bench_open_lines").await,
        BENCH_OPEN_LINES
    );
}

/// The materialized view in the benchmark database is refused, not rendered.
#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn the_materialized_view_in_the_database_is_refused() {
    let source = bench().await;
    let relation = source
        .relations("public")
        .await
        .expect("listing relations failed")
        .into_iter()
        .find(|r| r.name == "bench_category_totals")
        .expect("bench_category_totals is not in the benchmark database");
    assert_eq!(relation.kind, RelationKind::MaterializedView);
    dbddl::definition(&source, &dbsql::POSTGRES, &relation)
        .await
        .expect_err("a materialized view was rendered without knowing whether it holds data");
}

// ---------------------------------------------------------------------------
// A table made for a file
// ---------------------------------------------------------------------------

/// The seven kinds a file can ask for, and a name that has to be quoted.
fn a_files_columns() -> arrow::datatypes::Schema {
    use arrow::datatypes::{DataType, Field, TimeUnit};
    arrow::datatypes::Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("Order Date", DataType::Date32, true),
        Field::new("amount", DataType::Decimal128(12, 2), true),
        Field::new("ratio", DataType::Float64, true),
        Field::new("paid", DataType::Boolean, true),
        Field::new("note", DataType::Utf8, true),
        Field::new(
            "seen_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
    ])
}

/// The statement written for a file's columns is one PostgreSQL runs.
///
/// The golden strings in the crate's own tests say what each database is *told*;
/// only the server says whether it understood. What is checked is the column
/// list read back out of the table that was made, in the catalog's own words, so
/// a type word the server merely tolerated would show up here as something other
/// than what was asked for.
#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn a_table_made_for_a_files_columns_is_one_postgresql_runs() {
    let source = bench().await;
    let statement = dbddl::create_table(
        &dbsql::POSTGRES,
        "public.ddl_from_a_file",
        &a_files_columns(),
    )
    .expect("PostgreSQL would not write a table for a file's columns");
    run(&source, "DROP TABLE IF EXISTS public.ddl_from_a_file").await;
    run(&source, &statement).await;

    let columns: Vec<(String, String)> = source
        .columns("public", "ddl_from_a_file")
        .await
        .expect("listing the new table's columns failed")
        .into_iter()
        .map(|column| (column.name, column.data_type))
        .collect();
    run(&source, "DROP TABLE public.ddl_from_a_file").await;
    assert_eq!(
        columns,
        vec![
            ("id".to_string(), "bigint".to_string()),
            ("Order Date".to_string(), "date".to_string()),
            ("amount".to_string(), "numeric(12,2)".to_string()),
            ("ratio".to_string(), "double precision".to_string()),
            ("paid".to_string(), "boolean".to_string()),
            ("note".to_string(), "text".to_string()),
            (
                "seen_at".to_string(),
                "timestamp without time zone".to_string()
            ),
        ]
    );
}

// ---------------------------------------------------------------------------
// Constraints, against the server that has to take them
// ---------------------------------------------------------------------------

/// The three constraints this build writes are three PostgreSQL runs, and the
/// three drops take them away again.
///
/// The golden strings in `dbddl`'s own tests say what the server is *told*. Only
/// the server says whether it understood, and a constraint is where "looks
/// right" is worth least: `ADD CONSTRAINT c UNIQUE (a,b)` and
/// `ADD CONSTRAINT c UNIQUE(a, b)` read the same on the page and one of them is
/// a rule the table now has.
///
/// Read back through `Driver::constraints` and `Driver::foreign_keys` rather
/// than through the connection that ran the statements, so what is checked is
/// the state the structure pane would draw afterwards — including the
/// `ON DELETE CASCADE`, which is the clause that is silently absent when it is
/// spelled wrong.
#[tokio::test]
#[ignore = "requires the benchmark database"]
async fn the_constraints_written_for_postgresql_are_ones_it_runs() {
    let source = bench().await;
    run(&source, "DROP SCHEMA IF EXISTS ddl_constraints CASCADE").await;
    run(&source, "CREATE SCHEMA ddl_constraints").await;
    run(
        &source,
        "CREATE TABLE ddl_constraints.customers (id integer PRIMARY KEY)",
    )
    .await;
    run(
        &source,
        "CREATE TABLE ddl_constraints.orders (
             sku text NOT NULL,
             \"Line No\" integer NOT NULL,
             qty integer NOT NULL,
             customer_id integer
         )",
    )
    .await;

    let orders = relation("ddl_constraints", "orders", RelationKind::Table);
    let unique = dbddl::NewConstraint::Unique {
        name: "orders_sku_key".into(),
        columns: vec!["sku".into(), "Line No".into()],
    };
    let check = dbddl::NewConstraint::Check {
        name: "orders_qty_check".into(),
        expression: "qty > 0".into(),
    };
    let foreign_key = dbddl::NewConstraint::ForeignKey {
        name: "orders_customer_fk".into(),
        columns: vec!["customer_id".into()],
        other_schema: "ddl_constraints".into(),
        other_table: "customers".into(),
        other_columns: vec!["id".into()],
        on_delete: dbddl::ReferentialAction::Cascade,
        on_update: dbddl::ReferentialAction::NoAction,
    };
    for constraint in [&unique, &check, &foreign_key] {
        let statement = dbddl::constraint_change(
            &dbsql::POSTGRES,
            &orders,
            dbddl::ConstraintChange::Create(constraint),
        )
        .unwrap_or_else(|e| panic!("PostgreSQL would not write {}: {e}", constraint.name()));
        run(&source, &statement).await;
    }

    let listed: Vec<(String, String)> = source
        .constraints("ddl_constraints", "orders")
        .await
        .expect("listing constraints failed")
        .into_iter()
        .map(|constraint| (constraint.name, constraint.definition))
        .collect();
    assert!(
        listed
            .iter()
            .any(|(name, definition)| name == "orders_sku_key" && definition.starts_with("UNIQUE")),
        "the unique constraint is not on the table: {listed:?}"
    );
    assert!(
        listed
            .iter()
            .any(|(name, definition)| name == "orders_qty_check" && definition.contains("qty > 0")),
        "the check constraint is not on the table: {listed:?}"
    );

    let keys = source
        .foreign_keys("ddl_constraints", "orders")
        .await
        .expect("listing foreign keys failed");
    let key = keys
        .iter()
        .find(|key| key.name == "orders_customer_fk")
        .unwrap_or_else(|| panic!("the foreign key is not on the table: {keys:?}"));
    assert_eq!(key.local_columns, vec!["customer_id".to_string()]);
    assert_eq!(key.other_table, "customers");
    assert_eq!(key.other_columns, vec!["id".to_string()]);
    // The clause that is invisible when it goes missing: a key written without
    // it is still a key, and it stops taking the rows with it.
    assert_eq!(key.on_delete, "CASCADE");
    assert_eq!(key.on_update, "NO ACTION");

    for (name, sort) in [
        ("orders_sku_key", dbddl::ConstraintSort::Unique),
        ("orders_qty_check", dbddl::ConstraintSort::Check),
        ("orders_customer_fk", dbddl::ConstraintSort::ForeignKey),
    ] {
        let statement = dbddl::constraint_change(
            &dbsql::POSTGRES,
            &orders,
            dbddl::ConstraintChange::Drop { name, sort },
        )
        .unwrap_or_else(|e| panic!("PostgreSQL would not write the drop of {name}: {e}"));
        run(&source, &statement).await;
    }

    assert!(
        source
            .constraints("ddl_constraints", "orders")
            .await
            .expect("listing constraints failed")
            .is_empty(),
        "a constraint survived its own drop"
    );
    assert!(
        source
            .foreign_keys("ddl_constraints", "orders")
            .await
            .expect("listing foreign keys failed")
            .is_empty(),
        "the foreign key survived its own drop"
    );

    run(&source, "DROP SCHEMA ddl_constraints CASCADE").await;
}
