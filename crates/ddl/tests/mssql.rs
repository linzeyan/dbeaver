//! SQL Server DDL, against hand-built metadata and against the server.
//!
//! The same arrangement as the PostgreSQL tests: the constants below are what
//! upstream emits, read out of the Java named on each one, and both halves
//! assert them. The fake driver checks that this crate turns metadata into that
//! text; the ignored tests check that a real server produces exactly the
//! metadata the fake claims it does. A change that breaks the second group means
//! the fixture drifted; one that breaks the first means the renderer did.

use dbconn::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Cursor, DbResult, Driver, IndexInfo, RelationInfo,
    RelationKind, RelationshipInfo, ResultStream, SchemaInfo, TriggerInfo, TxStep,
};
use driver_mssql::MsSqlSource;
use tokio::sync::OnceCell;

const HOST: &str = "Server=tcp:localhost,51433;User Id=sa;Password=Str0ng!Passw0rd;\
                    Encrypt=true;TrustServerCertificate=true;Application Name=dbclient-ddl-tests";
const DATABASE: &str = "dbclient_ddl";

// ---------------------------------------------------------------------------
// What upstream emits
// ---------------------------------------------------------------------------

/// `dbo.parts`, from `SQLTableManager.getTableDDL` with `ext.mssql`'s managers.
///
/// The drop commented out, the `CREATE TABLE` with everything that can be
/// declared inside its parentheses, then everything that cannot: the check
/// constraint, which `SQLServerCheckConstraintManager` writes as an `ALTER TABLE`
/// of its own, and the indexes.
///
/// Deliberately different from upstream in three places, each recorded on the
/// line that shows it:
///
/// - `id int NOT NULL` where upstream has `id int IDENTITY(1,1) NOT NULL`.
///   `SQLServerTableColumnManager.IdentityModifier` reads
///   `sys.identity_columns`, which no metadata type carries.
/// - No `CREATE UNIQUE NONCLUSTERED INDEX parts_sku_key`, which upstream's
///   generic `isIncludeIndexInDDL` would emit after having already declared the
///   constraint that made it — a script that stops with "there is already an
///   object named parts_sku_key".
/// - The doubled brackets in `CHECK (([qty]>(0)))` and `DEFAULT ((1))` are the
///   server's own text, printed as upstream prints it.
const PARTS: &str = "-- Drop table

-- DROP TABLE dbo.parts;

CREATE TABLE dbo.parts (
\tid int NOT NULL,
\tsku varchar(32) NOT NULL,
\tqty int DEFAULT ((1)) NOT NULL,
\tparent_id int NULL,
\tCONSTRAINT parts_pkey PRIMARY KEY (id),
\tCONSTRAINT parts_sku_key UNIQUE (sku),
\tCONSTRAINT parts_parent_fkey FOREIGN KEY (parent_id) REFERENCES dbo.parts_parent(id) ON DELETE CASCADE
);
ALTER TABLE dbo.parts WITH NOCHECK ADD CONSTRAINT parts_qty_positive CHECK (([qty]>(0)));
CREATE NONCLUSTERED INDEX parts_qty_idx ON dbo.parts (qty);";

/// `dbo.open_parts`, from `SQLServerView.getObjectDefinitionText`.
///
/// The source `sys.sql_modules` kept, whitespace and all. Upstream rewrites the
/// leading `CREATE` to `ALTER` on the way out, for the reason `crate::mssql`
/// gives, and this does not.
const OPEN_PARTS: &str = "CREATE VIEW dbo.open_parts AS
    SELECT id, sku FROM dbo.parts WHERE qty > 0";

// ---------------------------------------------------------------------------
// A database that answers from hand-built metadata
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Fixture {
    columns: Vec<ColumnInfo>,
    indexes: Vec<IndexInfo>,
    constraints: Vec<ConstraintInfo>,
    foreign_keys: Vec<RelationshipInfo>,
    definition: Option<String>,
}

#[async_trait::async_trait]
impl Driver for Fixture {
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

    async fn foreign_keys(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(self.foreign_keys.clone())
    }

    async fn referenced_by(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("a table's own DDL does not name what points at it")
    }

    async fn constraints(&self, _: &str, _: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(self.constraints.clone())
    }

    async fn triggers(&self, _: &str, _: &str) -> DbResult<Vec<TriggerInfo>> {
        unreachable!("SQL Server's table DDL has no trigger section")
    }

    async fn query(&self, _: &str, _: usize) -> DbResult<Box<dyn ResultStream>> {
        unreachable!("this renderer builds its statement out of metadata")
    }

    async fn cursor(&self, _: &str, _: usize) -> DbResult<Box<dyn Cursor>> {
        unreachable!("this renderer builds its statement out of metadata")
    }

    async fn cancel(&self) -> DbResult<()> {
        unreachable!("nothing here is long enough to cancel")
    }

    fn transactional(&self) -> bool {
        false
    }

    async fn transaction(&self, _: &TxStep) -> DbResult<()> {
        unreachable!("reading metadata opens no transaction")
    }
}

fn column(name: &str, data_type: &str, nullable: bool, default: Option<&str>) -> ColumnInfo {
    ColumnInfo {
        name: name.to_string(),
        data_type: data_type.to_string(),
        nullable,
        position: 1,
        is_primary_key: false,
        default_value: default.map(str::to_string),
    }
}

fn index(name: &str, columns: &[&str], is_unique: bool, is_primary: bool) -> IndexInfo {
    IndexInfo {
        name: name.to_string(),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        is_unique,
        is_primary,
        method: "NONCLUSTERED".to_string(),
        predicate: None,
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

/// The fixture the constants above describe, answered from memory.
fn parts() -> Fixture {
    Fixture {
        columns: vec![
            column("id", "int", false, None),
            column("sku", "varchar(32)", false, None),
            column("qty", "int", false, Some("((1))")),
            column("parent_id", "int", true, None),
        ],
        indexes: vec![
            // The primary key's index and the unique constraint's, both of which
            // the CREATE TABLE already declares, and one ordinary index.
            index("parts_pkey", &["id"], true, true),
            index("parts_sku_key", &["sku"], true, false),
            index("parts_qty_idx", &["qty"], false, false),
        ],
        constraints: vec![
            ConstraintInfo {
                name: "parts_qty_positive".to_string(),
                kind: ConstraintKind::Check,
                definition: "([qty]>(0))".to_string(),
            },
            ConstraintInfo {
                name: "parts_sku_key".to_string(),
                kind: ConstraintKind::Unique,
                definition: "UNIQUE (sku)".to_string(),
            },
        ],
        foreign_keys: vec![RelationshipInfo {
            name: "parts_parent_fkey".to_string(),
            local_columns: vec!["parent_id".to_string()],
            other_schema: "dbo".to_string(),
            other_table: "parts_parent".to_string(),
            other_columns: vec!["id".to_string()],
            on_update: "NO ACTION".to_string(),
            on_delete: "CASCADE".to_string(),
        }],
        definition: None,
    }
}

async fn rendered(driver: &dyn Driver, relation: &RelationInfo) -> String {
    dbddl::definition(driver, &dbsql::MSSQL, relation)
        .await
        .expect("rendering failed")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_table_is_assembled_the_way_upstream_assembles_one() {
    let ddl = rendered(&parts(), &relation("dbo", "parts", RelationKind::Table)).await;
    assert_eq!(ddl, PARTS);
}

/// A view is the source the server kept, and nothing is built around it.
#[tokio::test]
async fn a_view_is_the_source_the_server_kept() {
    let fixture = Fixture {
        definition: Some(format!("{OPEN_PARTS}\n")),
        ..Fixture::default()
    };
    let ddl = rendered(&fixture, &relation("dbo", "open_parts", RelationKind::View)).await;
    assert_eq!(ddl, OPEN_PARTS);
}

/// A view whose source the server will not show is refused, not left blank.
#[tokio::test]
async fn a_view_with_no_source_is_refused_rather_than_left_blank() {
    let error = dbddl::definition(
        &Fixture::default(),
        &dbsql::MSSQL,
        &relation("dbo", "encrypted", RelationKind::View),
    )
    .await
    .expect_err("a view with no source rendered as something");
    assert!(
        error.to_string().contains("encrypted"),
        "the refusal does not say which object it is about: {error}"
    );
}

/// A kind SQL Server does not have is refused rather than rendered.
#[tokio::test]
async fn a_kind_sql_server_does_not_have_is_refused() {
    let error = dbddl::definition(
        &Fixture::default(),
        &dbsql::MSSQL,
        &relation("dbo", "rollup", RelationKind::MaterializedView),
    )
    .await
    .expect_err("a materialized view rendered as something");
    assert!(error.to_string().contains("MaterializedView"), "{error}");
}

// ---------------------------------------------------------------------------
// Against the server
// ---------------------------------------------------------------------------

static FIXTURE: OnceCell<()> = OnceCell::const_new();

/// The fixture, built through this driver rather than through `tiberius`.
///
/// The other direction from the driver's own tests, and for a different reason:
/// what is under test here is the renderer, and the driver's read path is the
/// thing the live half exists to include. A statement sent through it to build
/// the table is not evidence about anything this file asserts.
async fn live() -> MsSqlSource {
    FIXTURE.get_or_init(seed).await;
    MsSqlSource::connect(&format!("{HOST};Database={DATABASE}"))
        .await
        .expect("SQL Server unreachable; run 'make db-up-mssql'")
}

async fn seed() {
    let master = MsSqlSource::connect(&format!("{HOST};Database=master"))
        .await
        .expect("SQL Server unreachable; run 'make db-up-mssql'");
    run(
        &master,
        &format!("IF DB_ID('{DATABASE}') IS NULL CREATE DATABASE {DATABASE}"),
    )
    .await;

    let db = MsSqlSource::connect(&format!("{HOST};Database={DATABASE}"))
        .await
        .expect("the fixture database should be reachable once created");
    for statement in [
        "IF OBJECT_ID('dbo.open_parts') IS NOT NULL DROP VIEW dbo.open_parts",
        "IF OBJECT_ID('dbo.parts') IS NOT NULL DROP TABLE dbo.parts",
        "IF OBJECT_ID('dbo.parts_parent') IS NOT NULL DROP TABLE dbo.parts_parent",
        "CREATE TABLE dbo.parts_parent (id int NOT NULL CONSTRAINT parts_parent_pkey PRIMARY KEY)",
        "CREATE TABLE dbo.parts (
            id int NOT NULL IDENTITY(1,1),
            sku varchar(32) NOT NULL,
            qty int NOT NULL CONSTRAINT parts_qty_default DEFAULT 1,
            parent_id int NULL,
            CONSTRAINT parts_pkey PRIMARY KEY (id),
            CONSTRAINT parts_sku_key UNIQUE (sku),
            CONSTRAINT parts_qty_positive CHECK (qty > 0),
            CONSTRAINT parts_parent_fkey FOREIGN KEY (parent_id)
                REFERENCES dbo.parts_parent (id) ON DELETE CASCADE
        )",
        "CREATE NONCLUSTERED INDEX parts_qty_idx ON dbo.parts (qty)",
        "CREATE VIEW dbo.open_parts AS
    SELECT id, sku FROM dbo.parts WHERE qty > 0",
    ] {
        run(&db, statement).await;
    }
}

async fn run(source: &MsSqlSource, sql: &str) {
    let mut stream = source
        .query(sql, 1)
        .await
        .unwrap_or_else(|e| panic!("fixture statement failed: {e}\n{sql}"));
    while stream
        .next_batch()
        .await
        .unwrap_or_else(|e| panic!("fixture statement failed: {e}\n{sql}"))
        .is_some()
    {}
}

async fn listed(source: &MsSqlSource, name: &str) -> RelationInfo {
    source
        .relations("dbo")
        .await
        .expect("listing the fixture database")
        .into_iter()
        .find(|relation| relation.name == name)
        .unwrap_or_else(|| panic!("{name} is not in the fixture database"))
}

/// The server's metadata renders to the same script the fake's does.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn a_table_on_the_server_renders_to_what_the_fake_renders() {
    let source = live().await;
    let relation = listed(&source, "parts").await;
    let ddl = rendered(&source, &relation).await;
    assert_eq!(ddl, PARTS);
}

/// And a view's source comes back as it was typed.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn a_view_on_the_server_renders_as_it_was_typed() {
    let source = live().await;
    let relation = listed(&source, "open_parts").await;
    assert_eq!(relation.kind, RelationKind::View);
    let ddl = rendered(&source, &relation).await;
    assert_eq!(ddl, OPEN_PARTS);
}
