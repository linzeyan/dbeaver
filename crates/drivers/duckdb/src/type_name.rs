//! What DuckDB calls a result column's type.
//!
//! The Arrow schema cannot answer this. `VARCHAR`, `JSON` and an `ENUM` all
//! arrive as `Utf8` once `arrow_map` has had them, `DECIMAL(12,2)` arrives as
//! `Decimal128(12, 2)` under a name nobody types into a `CREATE TABLE`, and a
//! `STRUCT` arrives as the text this driver rendered it into. The type a column
//! was declared with is a separate fact, and this is where it is read.
//!
//! From the executed statement rather than from the catalogue, because a result
//! is not a relation: `SELECT a + b AS total FROM t JOIN u ...` has columns no
//! `information_schema` row describes, and the front end that has no navigator
//! has nothing to ask anyway. `duckdb_column_logical_type` answers for whatever
//! the planner settled on, which is the same thing DuckDB's own shell prints
//! above the column.
//!
//! ## What is not spelled out here
//!
//! A container's contents. DuckDB writes `INTEGER[]`, `STRUCT(qty INTEGER)` and
//! `MAP(VARCHAR, INTEGER)`; this writes `LIST`, `STRUCT` and `MAP`. Three
//! reasons, in the order they matter:
//!
//! - every one of these columns has already been rendered to text by
//!   `arrow_map`, and `duckdb.rendered_from` records the shape it had. The full
//!   spelling belongs with whatever eventually undoes that rendering, next to
//!   the rest of what it needs to know.
//! - the C API hands over children for `LIST`, `STRUCT` and `MAP` and nothing
//!   for `ENUM`'s members or an `ARRAY`'s length. Spelling three of the five
//!   fully and two of them by their family name is a label whose precision
//!   depends on which container it is, which is worse than one that is uniformly
//!   coarse.
//! - a header line thirteen characters wide cuts `STRUCT(qty INTE…` anyway.
//!
//! The family name is what DuckDB calls the type id, so it is short of the whole
//! answer rather than a different one. That is the line this file holds: it says
//! less than DuckDB would, never something else.
//!
//! The catalogue does spell them out — `metadata::columns` reads `INTEGER[3]`
//! and `STRUCT(qty INTEGER, unit VARCHAR)` straight out of `information_schema`,
//! where DuckDB itself did the printing. Where a front end has both, that one is
//! the better answer, and this one is for the results no catalogue describes.

use duckdb::Statement;
use duckdb::core::{LogicalTypeHandle, LogicalTypeId};

/// Every column's declared type, indexed with the statement's columns.
///
/// An empty string where there is no answer to give, which is not the same as
/// no entry: the caller pairs this with the schema by position, and a column
/// that has to be skipped still has to be counted.
pub fn declared_types(stmt: &Statement<'_>) -> Vec<String> {
    (0..stmt.column_count())
        .map(|i| sql_name(&stmt.column_logical_type(i)))
        .collect()
}

/// One type, spelled the way DuckDB spells it.
fn sql_name(logical: &LogicalTypeHandle) -> String {
    // An aliased type wins over the id underneath it, and `JSON` is why this
    // branch exists: DuckDB carries it as a `VARCHAR` wearing the name, so the
    // id alone would spell a JSON column `VARCHAR` and lose the one distinction
    // a type label on a text column is there to draw.
    //
    // Only aliases survive to here. `CREATE TYPE email AS VARCHAR` is resolved
    // away by the planner and arrives as a plain `VARCHAR`, and an `ENUM`
    // created by name arrives with no alias at all — DuckDB does not hand the
    // type's name over this API, so `ENUM` below is the whole of what there is
    // to say about it.
    if let Some(alias) = logical.get_alias() {
        return alias;
    }
    // `try_id` rather than `id`: this is a number from a library that adds types
    // faster than its Rust wrapper names them, and a type label is not worth a
    // panic. An unrecognised id says nothing, which the reader already has a
    // path for.
    let Ok(id) = logical.try_id() else {
        return String::new();
    };
    match id {
        // The width and scale are the whole point of the type: `DECIMAL(12,2)`
        // and `DECIMAL(4,2)` compare differently and round differently, and a
        // label that dropped them would be the type's name without the part of
        // it that decides anything.
        LogicalTypeId::Decimal => {
            format!(
                "DECIMAL({},{})",
                logical.decimal_width(),
                logical.decimal_scale()
            )
        }
        LogicalTypeId::Boolean => "BOOLEAN".into(),
        LogicalTypeId::Tinyint => "TINYINT".into(),
        LogicalTypeId::Smallint => "SMALLINT".into(),
        LogicalTypeId::Integer => "INTEGER".into(),
        LogicalTypeId::Bigint => "BIGINT".into(),
        LogicalTypeId::Hugeint => "HUGEINT".into(),
        LogicalTypeId::UTinyint => "UTINYINT".into(),
        LogicalTypeId::USmallint => "USMALLINT".into(),
        LogicalTypeId::UInteger => "UINTEGER".into(),
        LogicalTypeId::UBigint => "UBIGINT".into(),
        LogicalTypeId::UHugeint => "UHUGEINT".into(),
        LogicalTypeId::Float => "FLOAT".into(),
        LogicalTypeId::Double => "DOUBLE".into(),
        LogicalTypeId::Varchar => "VARCHAR".into(),
        LogicalTypeId::Blob => "BLOB".into(),
        LogicalTypeId::Bit => "BIT".into(),
        LogicalTypeId::Uuid => "UUID".into(),
        LogicalTypeId::Date => "DATE".into(),
        LogicalTypeId::Time => "TIME".into(),
        LogicalTypeId::TimeTZ => "TIME WITH TIME ZONE".into(),
        LogicalTypeId::Timestamp => "TIMESTAMP".into(),
        LogicalTypeId::TimestampTZ => "TIMESTAMP WITH TIME ZONE".into(),
        LogicalTypeId::TimestampS => "TIMESTAMP_S".into(),
        LogicalTypeId::TimestampMs => "TIMESTAMP_MS".into(),
        LogicalTypeId::TimestampNs => "TIMESTAMP_NS".into(),
        LogicalTypeId::Interval => "INTERVAL".into(),
        LogicalTypeId::Bignum => "BIGNUM".into(),
        LogicalTypeId::Geometry => "GEOMETRY".into(),
        LogicalTypeId::Variant => "VARIANT".into(),
        // The type of a column that can only ever be null. Not what `SELECT
        // NULL` produces — the planner settles that on `INTEGER` — but DuckDB
        // has the type and calls it this.
        LogicalTypeId::SqlNull => "NULL".into(),
        // The families, for the reason in the module header.
        LogicalTypeId::List => "LIST".into(),
        LogicalTypeId::Struct => "STRUCT".into(),
        LogicalTypeId::Map => "MAP".into(),
        LogicalTypeId::Array => "ARRAY".into(),
        LogicalTypeId::Union => "UNION".into(),
        LogicalTypeId::Enum => "ENUM".into(),
        // Nothing a result column can be. `Invalid` and `Any` are the planner's
        // own states, and the two literal types are what an unresolved constant
        // is before DuckDB decides what it is — by the time a column reaches a
        // result it has been decided. Saying nothing beats guessing which
        // concrete type each would have become.
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckdb::Connection;

    /// The declared types of one statement's columns.
    fn types_of(sql: &str) -> Vec<String> {
        let conn = Connection::open_in_memory().unwrap();
        let mut stmt = conn.prepare(sql).unwrap();
        // Executed for its effect on the statement: the logical types are the
        // planner's answer, and before execution there is no answer to read.
        drop(stmt.stream_arrow([]).unwrap());
        declared_types(&stmt)
    }

    /// The types the grid sees most, in DuckDB's own spelling rather than
    /// Arrow's. `VARCHAR` is the one that matters here: it and `JSON` and an
    /// `ENUM` are all `Utf8` by the time the schema crosses, so the Arrow name
    /// could not tell any of them apart.
    #[test]
    fn a_column_is_named_the_way_duckdb_names_it() {
        assert_eq!(
            types_of("SELECT 1::INTEGER a, 'x'::VARCHAR b, 2::BIGINT c, true d"),
            ["INTEGER", "VARCHAR", "BIGINT", "BOOLEAN"]
        );
    }

    /// With the parameters, which is why this is not a table of type names.
    #[test]
    fn a_decimal_carries_the_width_and_scale_it_was_declared_with() {
        assert_eq!(
            types_of("SELECT 1.5::DECIMAL(12,2) a, 1.5::DECIMAL(4,1) b"),
            ["DECIMAL(12,2)", "DECIMAL(4,1)"]
        );
    }

    /// An aliased type answers with its alias. Both of these columns are
    /// `VARCHAR` underneath and both arrive as `Utf8`, so this is the whole
    /// difference between a JSON column that says so and one that does not.
    #[test]
    fn an_aliased_type_answers_with_its_alias() {
        assert_eq!(
            types_of("SELECT '{}'::JSON a, '{}'::VARCHAR b"),
            ["JSON", "VARCHAR"]
        );
    }

    /// And a type whose name DuckDB keeps to itself says what it is made of.
    /// Asserted rather than left to be discovered: it reads like an oversight
    /// next to the alias above, and the reason it is not one is that
    /// `duckdb_logical_type_get_alias` answers nothing for a named `ENUM`.
    #[test]
    fn a_named_enum_says_only_that_it_is_an_enum() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TYPE mood AS ENUM ('ok', 'sad');")
            .unwrap();
        let mut stmt = conn.prepare("SELECT 'ok'::mood AS m").unwrap();
        drop(stmt.stream_arrow([]).unwrap());
        assert_eq!(declared_types(&stmt), ["ENUM"]);
    }

    /// The containers say what family they are and stop there, which is the one
    /// place this deliberately says less than DuckDB would.
    #[test]
    fn a_container_is_named_by_its_family() {
        assert_eq!(
            types_of("SELECT [1, 2] a, {'qty': 2} b, MAP {'k': 1} c"),
            ["LIST", "STRUCT", "MAP"]
        );
    }
}
