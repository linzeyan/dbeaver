//! PostgreSQL, read out of `ext.postgresql`.
//!
//! The path upstream takes for a table is
//! `PostgreTable.getObjectDefinitionText` → `DBStructUtils.generateTableDDL` →
//! `SQLTableManager.getTableDDL`, and for a view it is
//! `PostgreViewBase.getObjectDefinitionText` → `PostgreUtils.getViewDDL`. Those
//! two functions are what this file reproduces; each clause below names the Java
//! that decides it.
//!
//! Four options change what upstream emits, and all four are off in the DDL tab
//! this is compared against. `OPTION_INCLUDE_COMMENTS` and
//! `OPTION_INCLUDE_PERMISSIONS` come from preferences with no registered
//! default (`SQLSourceViewer.getShowColumnComments`, `getShowPermissions`), so
//! they read false; `OPTION_SCRIPT_FORMAT_COMPACT` is never set by the viewer;
//! and `OPTION_SKIP_DROPS` is set only where the DDL is fed to something other
//! than a reader, which today means the AI plugins keeping `-- DROP` out of a
//! prompt. Nothing here is parameterised by any of them: the metadata carries
//! no comments and no grants to print under the first two, and the other two
//! produce a shape nobody reads.

use crate::{Renderer, Script};
use async_trait::async_trait;
use dbconn::{
    ColumnInfo, ConstraintKind, DbError, DbResult, Driver, IndexInfo, RelationInfo, RelationKind,
    RelationshipInfo,
};

pub(crate) static POSTGRES: Postgres = Postgres;

pub(crate) struct Postgres;

#[async_trait]
impl Renderer for Postgres {
    /// Tables and views, and a refusal for the rest.
    ///
    /// A materialized view ends in `WITH DATA` or `WITH NO DATA`
    /// (`PostgreMViewManager.appendViewDeclarationPostfix`, from
    /// `pg_class.relispopulated`) and a partitioned table carries a
    /// `PARTITION BY` clause (`PostgreServerExtensionBase.getTableModifiers`).
    /// Neither fact reaches this crate — `RelationInfo` has a kind and a row
    /// estimate and nothing else — and both change what the statement *does*, so
    /// the answer is a refusal naming the kind rather than a statement that
    /// looks complete and is not.
    async fn definition(&self, driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
        match relation.kind {
            RelationKind::Table => table(driver, relation).await,
            RelationKind::View => view(driver, relation).await,
            kind => Err(DbError::new(format!(
                "{} is a {kind:?}, whose DDL needs facts this metadata does not carry",
                qualified(&relation.schema, &relation.name)
            ))),
        }
    }
}

/// A table, as `SQLTableManager.getTableDDL` orders it.
///
/// The order is the whole of that method: the drop, commented out; the
/// `CREATE TABLE` with everything that can be declared inside its parentheses;
/// then everything that cannot — indexes, because `SQLIndexManager` declares
/// nothing nested and so its commands are executed separately
/// (`SQLTableManager.addStructObjectCreateActions`), and triggers, which
/// `PostgreTableManagerBase.addObjectExtraActions` appends after them.
async fn table(driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
    let schema = relation.schema.as_str();
    let name = relation.name.as_str();
    let qualified_name = qualified(schema, name);

    let columns = driver.columns(schema, name).await?;
    let indexes = driver.indexes(schema, name).await?;
    let constraints = driver.constraints(schema, name).await?;
    let foreign_keys = driver.foreign_keys(schema, name).await?;
    let triggers = driver.triggers(schema, name).await?;

    let mut script = Script::new();

    // Two comments and not one statement: upstream wraps the whole DROP in
    // `SQLDatabasePersistActionComment` so that reading the DDL tab and pressing
    // Execute does not drop the table you were looking at.
    script.comment("Drop table");
    script.comment(&format!("DROP TABLE {qualified_name};"));

    // Columns, then the constraint types in the order `PostgreTableManager`'s
    // CHILD_TYPES lists them — columns, constraints, foreign keys — which is
    // what `SQLStructEditor.getNestedOrderedCommands` sorts by.
    let mut items: Vec<String> = columns.iter().map(column).collect();
    // The primary key is rebuilt from its index rather than read as a
    // constraint, because `Driver::constraints` deliberately excludes primary
    // and foreign keys: the structure pane gives each of those a section of its
    // own. `pg_index.indisprimary` names the same object — a primary key's
    // constraint and its index always share a name — and its key columns are
    // the key columns of the constraint.
    if let Some(key) = indexes.iter().find(|index| index.is_primary) {
        items.push(format!(
            "CONSTRAINT {} PRIMARY KEY ({})",
            quote(&key.name),
            key.columns.join(", ")
        ));
    }
    items.extend(constraints.iter().map(|constraint| {
        // `PostgreConstraintManager.getNestedDeclaration` prefers the server's
        // own rendering for anything already persisted, which is what
        // `ConstraintInfo::definition` holds, so a CHECK expression is never
        // reassembled here.
        format!(
            "CONSTRAINT {} {}",
            quote(&constraint.name),
            constraint.definition
        )
    }));
    items.extend(foreign_keys.iter().map(foreign_key));

    let declarations: Vec<String> = items.iter().map(|item| format!("\t{item}")).collect();
    script.statement(&format!(
        "CREATE TABLE {qualified_name} (\n{}\n)",
        declarations.join(",\n")
    ));

    // An index that exists only because a unique constraint asked for one is
    // already in the statement above, so emitting it again would create it
    // twice. That is `PostgreTableManagerBase.isIncludeIndexInDDL` reading
    // `PostgreIndex.isPrimaryKeyIndex`, which is set for the index behind any
    // unique constraint and not only behind the primary key. An exclusion
    // constraint's index is not unique, so upstream prints it separately as well
    // as inline, and so does this.
    let backed_by_constraint: Vec<&str> = constraints
        .iter()
        .filter(|constraint| constraint.kind == ConstraintKind::Unique)
        .map(|constraint| constraint.name.as_str())
        .collect();
    for index in &indexes {
        if index.is_primary
            || (index.is_unique && backed_by_constraint.contains(&index.name.as_str()))
        {
            continue;
        }
        script.statement(&create_index(&qualified_name, index));
    }

    // Triggers reach a table's DDL because the source viewer sets
    // `OPTION_DDL_SOURCE` (`SQLSourceViewer.getSourceOptions`), which is the
    // flag `addObjectExtraActions` reads; the heading over them is a second
    // preference, `ModelPreferences.META_EXTRA_DDL_INFO`, which defaults to
    // true.
    //
    // `definition` is optional in the shared shape because SQLite records
    // nothing else about a trigger; PostgreSQL always fills it. A trigger with
    // no text has nothing to emit and must not pull the heading in after it.
    if triggers.iter().any(|trigger| trigger.definition.is_some()) {
        script.comment("Table Triggers");
        for statement in triggers.iter().filter_map(|t| t.definition.as_deref()) {
            // A disabled trigger is printed as though it fires, which is what
            // upstream does — `pg_get_triggerdef` does not say, and the
            // `ALTER TABLE … DISABLE TRIGGER` that would say it is not part of
            // upstream's output. The structure pane is where that fact is shown.
            script.statement(statement);
        }
    }

    Ok(script.finish())
}

/// A view, as `PostgreUtils.getViewDDL` writes one.
///
/// `CREATE OR REPLACE` and not `CREATE`: upstream uses the replacing form for a
/// plain view and the plain form for a materialized one, which this crate does
/// not render.
///
/// The body is the server's, reformatted by nobody. `pg_get_viewdef` returns it
/// indented and terminated, and upstream trims it and strips the trailing
/// semicolons before putting its own back, because two of them is a syntax
/// error in some clients and a lone empty statement in the rest.
async fn view(driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
    let schema = relation.schema.as_str();
    let name = relation.name.as_str();
    let qualified_name = qualified(schema, name);

    let body = driver.definition(schema, name).await?.ok_or_else(|| {
        DbError::new(format!(
            "{qualified_name} is listed as a view but the server has no definition for it"
        ))
    })?;

    let mut script = Script::new();
    script.statement(&format!(
        "CREATE OR REPLACE VIEW {qualified_name}\nAS {}",
        body.trim().trim_end_matches(';')
    ));
    Ok(script.finish())
}

/// One column declaration.
///
/// The clause order is `PostgreTableColumnManager.getSupportedModifiers`: type,
/// default, then nullability. It reads backwards — `qty int4 DEFAULT 1 NOT NULL`
/// — and it is upstream's order, and PostgreSQL accepts column constraints in
/// any order, so there is nothing to fix.
///
/// Nullability is always stated, including the `NULL` that means nothing, because
/// `PostgreServerExtensionBase.supportsColumnsRequiring` is true and so the
/// modifier chosen is `NullNotNullModifier` rather than `NotNullModifier`.
///
/// The three modifiers between default and nullability — identity, collation and
/// generated-always — are not written: nothing in `ColumnInfo` carries them, so
/// a column that has one comes out short. Upstream reads them from
/// `pg_attribute.attidentity`, `pg_collation` and `pg_attrdef`.
fn column(column: &ColumnInfo) -> String {
    let mut declaration = format!(
        "{} {}",
        quote(&column.name),
        catalog_type(&column.data_type)
    );
    if let Some(default) = &column.default_value {
        // Never quoted, although `SQLTableColumnManager.BaseDefaultModifier`
        // can quote one: it does so when a string or datetime column's default
        // does not look like an expression, and `pg_get_expr` always renders a
        // literal with its own quotes, so the branch cannot fire for
        // PostgreSQL.
        declaration.push_str(&format!(" DEFAULT {default}"));
    }
    declaration.push_str(if column.nullable {
        " NULL"
    } else {
        " NOT NULL"
    });
    declaration
}

/// One foreign key, as `SQLForeignKeyManager.getNestedDeclaration` writes it.
///
/// This is the one part of a table's DDL that upstream builds itself rather than
/// asking the server for, which is why the columns are separated by a bare comma
/// where the constraints above use a comma and a space: those come from
/// `pg_get_constraintdef` and this does not.
///
/// `NO ACTION` disappears rather than being written out, because
/// `DBSForeignKeyModifyRule.NO_ACTION` has no clause and
/// `appendUpdateDeleteRule` skips a rule with an empty one. Delete before
/// update, as it appends them.
///
/// Two clauses upstream can add are missing here for want of the facts:
/// `MATCH FULL` (`PostgreForeignKeyManager.appendUpdateDeleteRule`, from
/// `pg_constraint.confmatchtype`) and `DEFERRABLE`/`INITIALLY DEFERRED` (from
/// `condeferrable` and `condeferred`).
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

/// One index, assembled to say what `pg_get_indexdef` would have said.
///
/// Upstream prints exactly that — `PostgreIndex.getObjectDefinitionText` runs
/// `pg_get_indexdef(oid)` and prints the result — and this crate cannot, because
/// `IndexInfo` carries the parts and not the statement. The parts are enough for
/// the shape that function produces: `CREATE [UNIQUE] INDEX name ON schema.table
/// USING method (keys) [WHERE predicate]`, with the index name unqualified and
/// the table qualified, which is how the server writes it.
///
/// The key expressions are the server's own, one per key column
/// (`pg_get_indexdef(oid, n, true)`), so an operator class, a `DESC` or a
/// `NULLS FIRST` arrives already written into the key rather than needing a
/// field of its own.
fn create_index(qualified_table: &str, index: &IndexInfo) -> String {
    let unique = if index.is_unique { "UNIQUE " } else { "" };
    let mut statement = format!(
        "CREATE {unique}INDEX {} ON {qualified_table} USING {} ({})",
        quote(&index.name),
        index.method,
        index.columns.join(", ")
    );
    if let Some(predicate) = &index.predicate {
        // `pg_get_expr` parenthesises the whole predicate, which is also how
        // `pg_get_indexdef` renders it: `WHERE (shipped_at IS NULL)`.
        statement.push_str(&format!(" WHERE {predicate}"));
    }
    statement
}

/// The type name written the way upstream writes it.
///
/// Upstream prints the type *object* — `PostgreDataTypeModifier` appends
/// `PostgreDataType.getFullyQualifiedName`, and that name is `pg_type.typname` —
/// so a column declared `integer` comes out as `int4`. `ColumnInfo::data_type`
/// holds `format_type()` instead, which prefers the SQL-standard spelling,
/// because the structure pane shows it and `character varying(64)` is what the
/// table says. The two are the same set of types under different names, and
/// upstream keeps the map between them: `PostgreConstants.DATA_TYPE_ALIASES`,
/// reproduced below for the spellings `format_type` can produce.
///
/// The modifier is lifted out and put back rather than pattern-matched, because
/// `format_type` puts it in the middle of the name it splits — `timestamp(3)
/// without time zone` — and only the name outside it is looked up. Putting it
/// back inserts a space after the comma, which is what
/// `PostgreNumericTypeHandler.getTypeModifiersString` emits for `numeric(18, 4)`
/// and the only place a modifier has two parts.
///
/// Three shapes deliberately pass through as `format_type` gave them. An array
/// is `integer[]` here and `_int4` upstream, which is the array type's own
/// catalog name; both are legal and this one is readable. An interval keeps its
/// field qualifier as written — `interval day to second(3)` against upstream's
/// `interval DAY TO SECOND(3)`, which
/// `PostgreIntervalTypeHandler.getTypeModifiersString` rebuilds from the type
/// modifier in upper case. And `"char"` stays quoted: upstream maps it to the
/// catalog name `char`, which PostgreSQL then reads back as `bpchar` — a
/// different type — so reproducing that would be reproducing a bug.
fn catalog_type(declared: &str) -> String {
    let (name, modifier) = match (declared.find('('), declared.find(')')) {
        (Some(open), Some(close)) if open < close => (
            format!("{}{}", &declared[..open], &declared[close + 1..]),
            Some(&declared[open + 1..close]),
        ),
        _ => (declared.to_string(), None),
    };

    let catalog = CATALOG_TYPE_NAMES
        .iter()
        .find(|(standard, _)| *standard == name)
        .map_or(name.as_str(), |(_, catalog)| *catalog);

    match modifier {
        None => catalog.to_string(),
        Some(modifier) => {
            let parts: Vec<&str> = modifier.split(',').map(str::trim).collect();
            format!("{catalog}({})", parts.join(", "))
        }
    }
}

/// `PostgreConstants.DATA_TYPE_ALIASES`, restricted to what `format_type` emits.
///
/// Upstream's map has entries for spellings a catalog never produces — `int`,
/// `char varying`, every `interval` field qualifier — because it is also used to
/// resolve a name a user typed. Carrying them here would be carrying rows
/// nothing can reach.
const CATALOG_TYPE_NAMES: &[(&str, &str)] = &[
    ("bigint", "int8"),
    ("bit varying", "varbit"),
    ("boolean", "bool"),
    ("character", "bpchar"),
    ("character varying", "varchar"),
    ("double precision", "float8"),
    ("integer", "int4"),
    ("real", "float4"),
    ("smallint", "int2"),
    ("time with time zone", "timetz"),
    ("time without time zone", "time"),
    ("timestamp with time zone", "timestamptz"),
    ("timestamp without time zone", "timestamp"),
];

/// `schema.name`, both quoted only where PostgreSQL needs them quoted.
///
/// Qualified always, because `DBUtils.getEntityScriptName` defaults
/// `OPTION_FULLY_QUALIFIED_NAMES` to true and nothing in the DDL path turns it
/// off. It also matters more than it looks: a DDL script that names no schema
/// creates the table wherever `search_path` happens to point.
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
    dbsql::POSTGRES.quote(name)
}
