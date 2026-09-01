//! SQL Server, read out of `ext.mssql`.
//!
//! A table takes the same shared path PostgreSQL's does —
//! `SQLServerTable.getObjectDefinitionText` → `DBStructUtils.generateTableDDL` →
//! `SQLTableManager.getTableDDL` — so the order is that method's and the parts
//! come from `ext.mssql`'s managers. A view does not: `SQLServerView` keeps the
//! source `sys.sql_modules` stored and prints it back, which is SQLite's shape.
//!
//! What is *not* here, because SQL Server's managers do not put it there.
//! There is no trigger section: `SQLServerTableManager.addObjectExtraActions`
//! appends column comments for a table being created and extended properties
//! under an option the source viewer does not set, and nothing else — where
//! `PostgreTableManagerBase` appends triggers. And a CHECK constraint is not
//! declared inside the parentheses: `SQLServerCheckConstraintManager` is a plain
//! `SQLObjectEditor` with no nested declaration, so its own create action —
//! `ALTER TABLE … WITH NOCHECK ADD CONSTRAINT … CHECK (…)` — follows the
//! `CREATE TABLE`, before the indexes, which is the order `getTableDDL`
//! aggregates them in.

use crate::{ColumnKind, Renderer, Script, TableChange, create_table_text};
use arrow::datatypes::Schema;
use async_trait::async_trait;
use dbconn::{
    ColumnInfo, Computed, ConstraintInfo, ConstraintKind, DbError, DbResult, Driver, IndexInfo,
    RelationInfo, RelationKind, RelationshipInfo,
};

pub(crate) static MSSQL: MsSql = MsSql;

pub(crate) struct MsSql;

#[async_trait]
impl Renderer for MsSql {
    /// Tables and views, and a refusal for the rest.
    ///
    /// SQL Server has no materialized view — an indexed view is a view with an
    /// index on it — and no partitioned table as a kind of its own, so the
    /// remaining arms describe objects this database does not have.
    async fn definition(&self, driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
        match relation.kind {
            RelationKind::Table => table(driver, relation).await,
            RelationKind::View => view(driver, relation).await,
            kind => Err(DbError::new(format!(
                "{} is a {kind:?}, and SQL Server has no such object",
                qualified(&relation.schema, &relation.name)
            ))),
        }
    }

    /// SQL Server's words for the kinds a file can ask for.
    fn create_table(&self, table: &str, columns: &Schema) -> DbResult<String> {
        create_table_text(&dbsql::MSSQL, table, columns, word, "")
    }

    /// None of the three yet.
    ///
    /// The rename is the reason to wait rather than the drop. SQL Server has no
    /// `RENAME` statement at all — `SQLServerBaseTableManager` calls
    /// `sp_rename`, a stored procedure taking the object as a *string literal*,
    /// which is a different escaping problem from every other renderer here and
    /// one worth writing against upstream rather than from memory.
    fn table_change(&self, _relation: &RelationInfo, _change: TableChange<'_>) -> DbResult<String> {
        Err(DbError::new(
            "changing a table has not been written for SQL Server yet",
        ))
    }

    /// None are written, so the items are not drawn at all.
    fn changes_relations(&self) -> bool {
        false
    }
}

/// A table, as `SQLTableManager.getTableDDL` orders it.
async fn table(driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
    let schema = relation.schema.as_str();
    let name = relation.name.as_str();
    let qualified_name = qualified(schema, name);

    let columns = driver.columns(schema, name).await?;
    let indexes = driver.indexes(schema, name).await?;
    let constraints = driver.constraints(schema, name).await?;
    let foreign_keys = driver.foreign_keys(schema, name).await?;

    let mut script = Script::new();

    // Commented out, so that reading the DDL tab and pressing Execute does not
    // drop the table you were looking at — upstream wraps the whole DROP in
    // `SQLDatabasePersistActionComment`.
    script.comment("Drop table");
    script.comment(&format!("DROP TABLE {qualified_name};"));

    // Columns, the primary key, the unique keys, then the foreign keys: the
    // order `getTableDDL` aggregates the nested commands in, which for
    // `SQLServerTableManager` is columns, unique keys, check constraints,
    // foreign keys, indexes — of which the middle one contributes nothing here
    // and the last contributes nothing nested.
    let mut items: Vec<String> = columns.iter().map(column).collect();
    // The primary key is rebuilt from its index, because `Driver::constraints`
    // deliberately excludes primary and foreign keys — each has a section of its
    // own in the structure pane. In SQL Server a key and the index behind it are
    // one object with one name, so `sys.indexes.is_primary_key` names the
    // constraint as surely as `sys.key_constraints` would.
    if let Some(key) = indexes.iter().find(|index| index.is_primary) {
        items.push(format!(
            "CONSTRAINT {} PRIMARY KEY ({})",
            quote(&key.name),
            key.columns.join(", ")
        ));
    }
    items.extend(
        constraints
            .iter()
            .filter(|constraint| constraint.kind == ConstraintKind::Unique)
            // The driver renders `UNIQUE (a, b)` from `sys.index_columns`, so
            // the keyword is already in the definition and only the name is
            // added — the same arrangement the PostgreSQL renderer has with
            // `pg_get_constraintdef`.
            .map(|constraint| {
                format!(
                    "CONSTRAINT {} {}",
                    quote(&constraint.name),
                    constraint.definition
                )
            }),
    );
    items.extend(foreign_keys.iter().map(foreign_key));

    let declarations: Vec<String> = items.iter().map(|item| format!("\t{item}")).collect();
    script.statement(&format!(
        "CREATE TABLE {qualified_name} (\n{}\n)",
        declarations.join(",\n")
    ));

    for check in constraints
        .iter()
        .filter(|constraint| constraint.kind == ConstraintKind::Check)
    {
        script.statement(&check_constraint(&qualified_name, check));
    }

    // Upstream emits every index it is given, its generic `isIncludeIndexInDDL`
    // dropping only hidden and inherited ones — and against SQL Server that
    // produces a script that cannot run, because the index behind a primary key
    // or a unique constraint has the constraint's own name and is created by the
    // statement above. Skipped here for that reason, which is also what
    // `PostgreTableManagerBase.isIncludeIndexInDDL` does upstream on the one
    // database whose maintainers noticed.
    let backed_by_constraint: Vec<&str> = constraints
        .iter()
        .filter(|constraint| constraint.kind == ConstraintKind::Unique)
        .map(|constraint| constraint.name.as_str())
        .collect();
    for index in &indexes {
        if index.is_primary || backed_by_constraint.contains(&index.name.as_str()) {
            continue;
        }
        script.statement(&create_index(&qualified_name, index));
    }

    Ok(script.finish())
}

/// A view, which is the source the server kept.
///
/// `SQLServerView.getObjectDefinitionText` reads `sys.sql_modules` and rewrites
/// the leading `CREATE` to `ALTER` on the way out
/// (`SQLServerUtils.changeCreateToAlterDDL`), because upstream's Source tab is an
/// editor for the view and `ALTER` is what saving it should send. This pane
/// states what would recreate the object, and an `ALTER VIEW` of a view that is
/// not there creates nothing — so the source is printed as it was stored.
async fn view(driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
    let qualified_name = qualified(&relation.schema, &relation.name);
    let source = driver
        .definition(&relation.schema, &relation.name)
        .await?
        .ok_or_else(|| {
            // Two different absences, and the message covers both: a view
            // created `WITH ENCRYPTION` has a `sys.sql_modules` row whose
            // definition is NULL, and a view that has been dropped has no row.
            DbError::new(format!(
                "{qualified_name} is listed as a view but the server will not show its source"
            ))
        })?;
    Ok(source.trim().to_string())
}

/// One column declaration.
///
/// `SQLServerTableColumnManager.getSupportedModifiers` orders them: type,
/// `IDENTITY`, `COLLATE`, default, nullability. The default is printed because
/// the source viewer sets `OPTION_DDL_SOURCE`, which is the flag
/// `SQLServerDefaultModifier` reads before writing one for a persisted column.
///
/// Two of those five are never written, for want of the facts: `IDENTITY(seed,
/// increment)` (from `sys.identity_columns`) and `COLLATE` (from
/// `sys.columns.collation_name`), neither of which reaches `ColumnInfo`.
///
/// A computed column takes none of those five. `getSupportedModifiers` answers
/// `{ComputedModifier, NotNullModifier}` for a column that has a computed
/// definition, so what is written is the expression and nothing else — no type,
/// no default, no `NULL` — which is also the only form SQL Server takes back: a
/// type in front of `AS` is a syntax error.
fn column(column: &ColumnInfo) -> String {
    // The flag and the expression are read from one catalog row and mean
    // something only together: a column marked computed with no expression to
    // compute is not a column this can write, and inventing one would be
    // inventing the table.
    if let (Some(computed), Some(expression)) = (column.computed, &column.default_value) {
        return computed_column(column, computed, expression);
    }

    let mut declaration = format!("{} {}", quote(&column.name), column.data_type);
    if let Some(default) = &column.default_value {
        // `sys.default_constraints.definition` arrives parenthesised — `((1))` —
        // and upstream prints it as it stands, so the doubled brackets in the
        // output are the server's own.
        declaration.push_str(&format!(" DEFAULT {default}"));
    }
    declaration.push_str(if column.nullable {
        " NULL"
    } else {
        " NOT NULL"
    });
    declaration
}

/// A computed column, as `ComputedModifier` then `NotNullModifier` write one.
///
/// The expression keeps the brackets `sys.computed_columns.definition` stores it
/// with — `([qty]*(2))` — as the check constraint above keeps its own, because
/// upstream appends the catalog string untouched.
///
/// `NOT NULL` is written only for a persisted column, and there upstream is not
/// followed: its `NotNullModifier` writes the words whenever the column is
/// required, and SQL Server refuses them on a column it does not store — "CHECK,
/// FOREIGN KEY, and NOT NULL constraints require that computed columns be
/// persisted" (Msg 8183). The catalog reaches that state on its own, without
/// anybody having declared it: a non-persisted `isnull([a],(0))` reports
/// `is_nullable = 0`, because the server derives nullability from the expression.
/// So the column upstream's rule would describe cannot be created, and a script
/// that stops halfway is worse than one that omits a word the server would have
/// derived anyway.
fn computed_column(column: &ColumnInfo, computed: Computed, expression: &str) -> String {
    let mut declaration = format!("{} AS {expression}", quote(&column.name));
    if computed == Computed::Stored {
        declaration.push_str(" PERSISTED");
        if !column.nullable {
            declaration.push_str(" NOT NULL");
        }
    }
    declaration
}

/// One foreign key, as `SQLForeignKeyManager.getNestedDeclaration` writes it.
///
/// The same shared method the PostgreSQL renderer reproduces, down to the bare
/// comma between column names, and the same omission of `NO ACTION`.
fn foreign_key(key: &RelationshipInfo) -> String {
    let mut declaration = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}({})",
        quote(&key.name),
        quoted_list(&key.local_columns),
        qualified(&key.other_schema, &key.other_table),
        quoted_list(&key.other_columns)
    );
    if key.on_delete != NO_ACTION {
        declaration.push_str(&format!(" ON DELETE {}", key.on_delete));
    }
    if key.on_update != NO_ACTION {
        declaration.push_str(&format!(" ON UPDATE {}", key.on_update));
    }
    declaration
}

/// What `RelationshipInfo` calls the referential action that changes nothing.
const NO_ACTION: &str = "NO ACTION";

/// One CHECK constraint, as `SQLServerCheckConstraintManager` adds one.
///
/// `WITH NOCHECK` is upstream's, and it is the right default for a constraint
/// being replayed onto a table that is about to be filled: it adds the rule
/// without demanding that rows already there satisfy it.
///
/// The expression keeps the brackets `sys.check_constraints` stores it with, so
/// the output has two pairs — `CHECK (([qty]>(0)))` — which is what upstream
/// emits, its `getCheckConstraintDefinition` returning the catalog string
/// untouched.
fn check_constraint(qualified_table: &str, constraint: &ConstraintInfo) -> String {
    format!(
        "ALTER TABLE {qualified_table} WITH NOCHECK ADD CONSTRAINT {} CHECK ({})",
        quote(&constraint.name),
        constraint.definition
    )
}

/// One index, as `SQLServerIndexManager` writes its create action.
///
/// `CREATE [UNIQUE] [CLUSTERED|NONCLUSTERED] INDEX name ON schema.table (keys)`,
/// with no `USING` — SQL Server names the storage kind before the word INDEX
/// where PostgreSQL names the access method after the table.
///
/// `IndexInfo::method` holds `sys.indexes.type_desc`, which says more than
/// upstream's two words: `NONCLUSTERED COLUMNSTORE`, `SPATIAL`, `XML`. Only the
/// two that are a valid modifier here are written, because the others belong to
/// a different `CREATE` syntax altogether and a script claiming to make a
/// columnstore index with this one would not run.
fn create_index(qualified_table: &str, index: &IndexInfo) -> String {
    let unique = if index.is_unique { "UNIQUE " } else { "" };
    let storage = match index.method.as_str() {
        kind @ ("CLUSTERED" | "NONCLUSTERED") => format!("{kind} "),
        _ => String::new(),
    };
    let mut statement = format!(
        "CREATE {unique}{storage}INDEX {} ON {qualified_table} ({})",
        quote(&index.name),
        index.columns.join(", ")
    );
    if let Some(predicate) = &index.predicate {
        // `sys.indexes.filter_definition` is stored parenthesised, as the check
        // constraint above is, and upstream appends it unchanged.
        statement.push_str(&format!(" WHERE {predicate}"));
    }
    statement
}

/// `schema.name`, both quoted only where SQL Server needs them quoted.
///
/// Two levels rather than three. SQL Server qualifies fully as
/// `database.schema.name`, and this driver holds a connection to one database
/// and lists relations under a schema, so the database is the one the script
/// would be run against — which is the same thing the connection already means.
fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote(schema), quote(name))
}

/// Column names as a foreign key lists them: quoted individually, comma, no
/// space — `SQLForeignKeyManager.getNestedDeclarationScript` appends `","`.
fn quoted_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| quote(name))
        .collect::<Vec<_>>()
        .join(",")
}

fn quote(name: &str) -> String {
    dbsql::MSSQL.quote(name)
}

fn word(kind: ColumnKind) -> String {
    match kind {
        ColumnKind::Bool => "bit".to_string(),
        ColumnKind::Int => "bigint".to_string(),
        ColumnKind::Float => "float".to_string(),
        ColumnKind::Decimal(precision, scale) => format!("decimal({precision}, {scale})"),
        // `nvarchar(max)` rather than a length: a file's longest value is not
        // known until the file has been read, and a length guessed from the
        // sample is an import that stops on the row that exceeds it.
        ColumnKind::Text => "nvarchar(max)".to_string(),
        ColumnKind::Date => "date".to_string(),
        // `datetime2` and not `datetime`, which rounds to 1/300 of a second and
        // begins in 1753.
        ColumnKind::Timestamp => "datetime2".to_string(),
    }
}
