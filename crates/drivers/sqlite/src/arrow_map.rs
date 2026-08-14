//! SQLite values -> Arrow types, and the column builders that carry them across.
//!
//! The mapping is harder than PostgreSQL's for one reason: SQLite does not have
//! column types. A declaration gives a column an *affinity*, which is a
//! preference applied when a value is stored and not a promise about what comes
//! back out, and a column with no declaration at all — every expression in a
//! `SELECT` list — has not even that. Arrow needs one type per column for the
//! whole result, so this file is where a preference is turned into a decision.
//!
//! Two rules, and the reasons they are not the obvious ones:
//!
//! A declared affinity decides the column, except NUMERIC. NUMERIC is the
//! affinity every declaration SQLite does not recognise falls into, `BOOLEAN`,
//! `DATE` and `DATETIME` among them, and it constrains nothing about what a row
//! holds. Reading a number out of a column whose values are dates written as
//! text would fill the grid with errors over a database SQLite considers
//! perfectly ordinary.
//!
//! A column with no usable affinity is decided by its first value. That is the
//! only evidence there is, and it is right for the case that matters —
//! `SELECT count(*)` is an integer, not text.

use arrow::array::{ArrayRef, BinaryBuilder, Float64Builder, Int64Builder, StringBuilder};
use arrow::datatypes::DataType;
use rusqlite::Row;
use rusqlite::types::ValueRef;
use std::borrow::Cow;
use std::sync::Arc;

use crate::SqliteError;

/// Above this an integer no longer survives the trip through `f64`, so a value
/// beyond it is reported rather than silently rounded.
const F64_EXACT_INTEGER: i64 = 1 << 53;

/// The Arrow type one result column is read as, for the whole result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Int64,
    Float64,
    Utf8,
    Binary,
}

impl ColumnType {
    pub fn data_type(self) -> DataType {
        match self {
            Self::Int64 => DataType::Int64,
            Self::Float64 => DataType::Float64,
            Self::Utf8 => DataType::Utf8,
            Self::Binary => DataType::Binary,
        }
    }

    /// The storage class this reads as, spelled the way SQLite spells it, so
    /// that a mismatch reports both sides in the user's vocabulary.
    fn storage_class(self) -> &'static str {
        match self {
            Self::Int64 => "INTEGER",
            Self::Float64 => "REAL",
            Self::Utf8 => "TEXT",
            Self::Binary => "BLOB",
        }
    }
}

/// The type a declared column reads as, or `None` where the declaration decides
/// nothing.
///
/// The rules are SQLite's own, from §3.1 of its datatype documentation, applied
/// in the order it applies them — with one deliberate departure. SQLite's third
/// rule gives BLOB affinity to a column declared with an empty type; here an
/// absent declaration answers `None` instead, so the value decides. A column
/// declared without a type holds whatever was inserted, which in practice is
/// text far more often than bytes, and BLOB affinity is a statement about
/// storage conversion rather than about what a row contains.
pub fn affinity(declared: &str) -> Option<ColumnType> {
    let d = declared.to_ascii_uppercase();
    if d.contains("INT") {
        Some(ColumnType::Int64)
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        Some(ColumnType::Utf8)
    } else if d.contains("BLOB") {
        Some(ColumnType::Binary)
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        Some(ColumnType::Float64)
    } else {
        // NUMERIC affinity, which decides nothing. See the module comment.
        None
    }
}

/// Settles every column's type, given what each was declared as and the first
/// row of the result if there is one.
pub fn resolve(
    declared: &[Option<String>],
    first: Option<&Row<'_>>,
) -> Result<Vec<ColumnType>, SqliteError> {
    declared
        .iter()
        .enumerate()
        .map(|(idx, decl)| {
            if let Some(from_declaration) = decl.as_deref().and_then(affinity) {
                return Ok(from_declaration);
            }
            let Some(row) = first else {
                // An empty result with nothing declared. Text is the one type
                // every storage class can be rendered into, so a later batch —
                // there is none, but the schema outlives the result — cannot be
                // contradicted by it.
                return Ok(ColumnType::Utf8);
            };
            Ok(match row.get_ref(idx)? {
                ValueRef::Integer(_) => ColumnType::Int64,
                ValueRef::Real(_) => ColumnType::Float64,
                ValueRef::Blob(_) => ColumnType::Binary,
                // Text, and Null: a first value that says nothing leaves text,
                // for the reason above.
                ValueRef::Text(_) | ValueRef::Null => ColumnType::Utf8,
            })
        })
        .collect()
}

/// One builder per column. An enum rather than `Box<dyn ArrayBuilder>` so the
/// per-value append stays a static call — this is the inner loop over every
/// cell in the result.
pub enum ColBuilder {
    Int64(Int64Builder),
    Float64(Float64Builder),
    Utf8(StringBuilder),
    Binary(BinaryBuilder),
}

impl ColBuilder {
    pub fn new(column: ColumnType, capacity: usize) -> Self {
        match column {
            ColumnType::Int64 => Self::Int64(Int64Builder::with_capacity(capacity)),
            ColumnType::Float64 => Self::Float64(Float64Builder::with_capacity(capacity)),
            ColumnType::Utf8 => Self::Utf8(StringBuilder::with_capacity(capacity, capacity * 24)),
            ColumnType::Binary => {
                Self::Binary(BinaryBuilder::with_capacity(capacity, capacity * 32))
            }
        }
    }

    /// Appends `row`'s value for column `idx`, converting it to the type the
    /// column was resolved to.
    ///
    /// `name` is carried only so that a value that cannot be converted names the
    /// column it was in. A message that says a REAL turned up where an INTEGER
    /// was expected, without saying where, leaves the user to find it by opening
    /// the table.
    pub fn append(&mut self, name: &str, row: &Row<'_>, idx: usize) -> Result<(), SqliteError> {
        let value = row.get_ref(idx)?;
        if matches!(value, ValueRef::Null) {
            match self {
                Self::Int64(b) => b.append_null(),
                Self::Float64(b) => b.append_null(),
                Self::Utf8(b) => b.append_null(),
                Self::Binary(b) => b.append_null(),
            }
            return Ok(());
        }
        match self {
            Self::Int64(b) => b.append_value(as_integer(name, value)?),
            Self::Float64(b) => b.append_value(as_real(name, value)?),
            Self::Utf8(b) => b.append_value(as_text(name, value)?),
            Self::Binary(b) => b.append_value(as_blob(name, value)?),
        }
        Ok(())
    }

    pub fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Int64(b) => Arc::new(b.finish()),
            Self::Float64(b) => Arc::new(b.finish()),
            Self::Utf8(b) => Arc::new(b.finish()),
            Self::Binary(b) => Arc::new(b.finish()),
        }
    }
}

/// The storage class of a value, spelled the way SQLite spells it.
fn class_of(value: ValueRef<'_>) -> &'static str {
    match value {
        ValueRef::Null => "NULL",
        ValueRef::Integer(_) => "INTEGER",
        ValueRef::Real(_) => "REAL",
        ValueRef::Text(_) => "TEXT",
        ValueRef::Blob(_) => "BLOB",
    }
}

fn mismatch(name: &str, value: ValueRef<'_>, expected: ColumnType) -> SqliteError {
    SqliteError::TypeMismatch {
        column: name.to_string(),
        found: class_of(value),
        expected: expected.storage_class(),
    }
}

/// Conversions here are the ones that lose nothing. A column with INTEGER
/// affinity can hold a REAL — SQLite converts on the way in only when the
/// conversion is exact, and leaves the value alone when it is not — so applying
/// the same test on the way out is what keeps 1.5 from arriving as 1. Anything
/// that would have to round or reinterpret is reported instead, because a grid
/// showing a wrong number is worse than one showing an error.
fn as_integer(name: &str, value: ValueRef<'_>) -> Result<i64, SqliteError> {
    match value {
        ValueRef::Integer(i) => Ok(i),
        ValueRef::Real(f) if f.fract() == 0.0 && f.abs() < F64_EXACT_INTEGER as f64 => Ok(f as i64),
        ValueRef::Text(t) => std::str::from_utf8(t)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| mismatch(name, value, ColumnType::Int64)),
        _ => Err(mismatch(name, value, ColumnType::Int64)),
    }
}

fn as_real(name: &str, value: ValueRef<'_>) -> Result<f64, SqliteError> {
    match value {
        ValueRef::Real(f) => Ok(f),
        ValueRef::Integer(i) if i.abs() < F64_EXACT_INTEGER => Ok(i as f64),
        ValueRef::Text(t) => std::str::from_utf8(t)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| mismatch(name, value, ColumnType::Float64)),
        _ => Err(mismatch(name, value, ColumnType::Float64)),
    }
}

fn as_text<'a>(name: &str, value: ValueRef<'a>) -> Result<Cow<'a, str>, SqliteError> {
    match value {
        ValueRef::Text(t) => std::str::from_utf8(t)
            .map(Cow::Borrowed)
            .map_err(|_| mismatch(name, value, ColumnType::Utf8)),
        ValueRef::Integer(i) => Ok(Cow::Owned(i.to_string())),
        ValueRef::Real(f) => Ok(Cow::Owned(f.to_string())),
        // A blob rendered as text would be characters this side invented. SQLite
        // itself refuses the same conversion in `CAST`, and hex is a rendering
        // rather than a value.
        _ => Err(mismatch(name, value, ColumnType::Utf8)),
    }
}

fn as_blob<'a>(name: &str, value: ValueRef<'a>) -> Result<&'a [u8], SqliteError> {
    match value {
        ValueRef::Blob(b) => Ok(b),
        // SQLite stores text as bytes, so this direction loses nothing.
        ValueRef::Text(t) => Ok(t),
        _ => Err(mismatch(name, value, ColumnType::Binary)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_follows_sqlites_own_rules_in_order() {
        // The order matters: "INT" is tested before "CHAR", so VARCHAR(20) is
        // text while INTEGER is a number, and a declaration containing both
        // resolves the way SQLite resolves it.
        assert_eq!(affinity("INTEGER"), Some(ColumnType::Int64));
        assert_eq!(affinity("BIGINT"), Some(ColumnType::Int64));
        assert_eq!(affinity("UNSIGNED BIG INT"), Some(ColumnType::Int64));
        assert_eq!(affinity("VARCHAR(20)"), Some(ColumnType::Utf8));
        assert_eq!(affinity("NVARCHAR(10)"), Some(ColumnType::Utf8));
        assert_eq!(affinity("CLOB"), Some(ColumnType::Utf8));
        assert_eq!(affinity("BLOB"), Some(ColumnType::Binary));
        assert_eq!(affinity("REAL"), Some(ColumnType::Float64));
        assert_eq!(affinity("DOUBLE PRECISION"), Some(ColumnType::Float64));
        assert_eq!(affinity("FLOAT"), Some(ColumnType::Float64));
        // "POINT" contains "INT" and SQLite reads it as an integer column. Odd,
        // documented, and not ours to correct.
        assert_eq!(affinity("POINT"), Some(ColumnType::Int64));
    }

    #[test]
    fn affinity_is_case_insensitive() {
        assert_eq!(affinity("integer"), Some(ColumnType::Int64));
        assert_eq!(affinity("Varchar(8)"), Some(ColumnType::Utf8));
    }

    #[test]
    fn numeric_affinity_decides_nothing() {
        // The case this rule exists for: a DATETIME column in SQLite holds
        // whatever was written into it, and text is the usual answer. Reading it
        // as a number would report an error on a database SQLite is happy with.
        for declared in [
            "NUMERIC",
            "DECIMAL(10,5)",
            "BOOLEAN",
            "DATE",
            "DATETIME",
            "",
        ] {
            assert_eq!(affinity(declared), None, "{declared} should not decide");
        }
    }

    #[test]
    fn an_integer_that_survives_f64_converts_and_one_that_does_not_is_reported() {
        assert_eq!(
            as_real("n", ValueRef::Integer(1 << 52)).unwrap(),
            2f64.powi(52)
        );
        let err = as_real("n", ValueRef::Integer((1 << 53) + 1)).unwrap_err();
        // Silently rounding here is how a client shows two different ids as the
        // same number.
        assert!(
            matches!(err, SqliteError::TypeMismatch { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_real_with_a_fraction_is_not_read_as_an_integer() {
        assert_eq!(as_integer("n", ValueRef::Real(3.0)).unwrap(), 3);
        let err = as_integer("n", ValueRef::Real(1.5)).unwrap_err();
        match err {
            SqliteError::TypeMismatch {
                column,
                found,
                expected,
            } => {
                assert_eq!(column, "n");
                assert_eq!(found, "REAL");
                assert_eq!(expected, "INTEGER");
            }
            other => panic!("expected a mismatch naming the column, got {other:?}"),
        }
    }

    #[test]
    fn numbers_render_into_a_text_column_and_blobs_do_not() {
        assert_eq!(as_text("c", ValueRef::Integer(42)).unwrap(), "42");
        assert_eq!(as_text("c", ValueRef::Text(b"hi")).unwrap(), "hi");
        // Hex would be a rendering this side invented, not the value.
        assert!(as_text("c", ValueRef::Blob(&[0xff])).is_err());
    }

    #[test]
    fn text_reaches_a_blob_column_as_its_bytes() {
        // Lossless in this direction: SQLite stores text as bytes already.
        assert_eq!(as_blob("c", ValueRef::Text(b"hi")).unwrap(), b"hi");
        assert!(as_blob("c", ValueRef::Real(1.0)).is_err());
    }
}
