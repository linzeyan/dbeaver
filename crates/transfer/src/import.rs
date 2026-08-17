//! A file's rows put into a table that already exists.
//!
//! The half that is not obvious is that nothing here guesses a type. A CSV
//! column of `1e5`, or of `2024-01-02`, is a number or a date or neither
//! depending on what it is being read into — and the table being read into
//! already knows. So the schema is asked of the target database and handed to
//! the reader, and the reader parses to order rather than to taste. Inference
//! would only be a way of getting the answer the table already had.
//!
//! The cost of that choice is written down: the table has to exist first. There
//! is no `CREATE TABLE` here, because building one from a file needs exactly the
//! inference this avoids, and a statement that guessed a column's type is one
//! somebody should read before it runs.

use crate::{DelimitedReader, Format, TargetWriter};
use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow::error::ArrowError;
use dbconn::{Browse, DbError, DbResult, Driver};
use dbsql::Dialect;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

/// Reads `path` into `table` on `target`, and reports how many rows that was.
///
/// A batch is read, sent and dropped, so a file larger than memory is no
/// different from a small one — the same property `transfer` has, and the reason
/// both live in this crate rather than in the caller.
pub async fn import(
    path: &Path,
    format: Format,
    target: &dyn Driver,
    dialect: &'static Dialect,
    table: String,
) -> DbResult<u64> {
    let schema = table_schema(target, dialect, &table).await?;
    let file = File::open(path).map_err(|e| DbError::new(e.to_string()))?;
    let batches = reader(file, format, Arc::clone(&schema))?;
    let writer = TargetWriter::new(dialect, table, &schema);

    let mut rows = 0u64;
    for batch in batches {
        let batch = batch.map_err(|e| DbError::new(e.to_string()))?;
        rows += writer.write(target, &batch).await?;
    }
    Ok(rows)
}

/// What the target says its own columns are.
///
/// A zero-row `SELECT` rather than the catalog, because the catalog answers in
/// the database's type names and this needs Arrow types. The driver already does
/// that translation for every result it returns; asking it this way reuses the
/// translation instead of adding a second one that would have to agree with it.
async fn table_schema(
    target: &dyn Driver,
    dialect: &'static Dialect,
    table: &str,
) -> DbResult<SchemaRef> {
    // `sql_named` takes a name already written, so the table reaches the
    // statement exactly as the caller spelled it — the same treatment
    // `TargetWriter` gives it, and the reason a qualified name survives.
    let probe = Browse {
        schema: "",
        relation: "",
        filter: None,
        order: None,
        keys: &[],
        limit: Some(0),
    }
    .sql_named(dialect, table);
    let stream = target.query(&probe, 1).await?;
    Ok(stream.schema())
}

type Batches = Box<dyn Iterator<Item = Result<RecordBatch, ArrowError>>>;

fn reader(file: File, format: Format, schema: SchemaRef) -> DbResult<Batches> {
    let batches: Batches = match format {
        // This crate's own reader, not `arrow::csv`, for the same reason the
        // writer is this crate's own: `arrow::csv` cannot tell an unquoted
        // blank from `""`, so the NULL-versus-empty distinction the writer
        // preserves would be lost on the way back in.
        Format::Csv => Box::new(
            DelimitedReader::new(file, b',', schema).map_err(|e| DbError::new(e.to_string()))?,
        ),
        Format::Tsv => Box::new(
            DelimitedReader::new(file, b'\t', schema).map_err(|e| DbError::new(e.to_string()))?,
        ),
        Format::JsonLines => Box::new(
            arrow::json::ReaderBuilder::new(schema)
                .build(std::io::BufReader::new(file))
                .map_err(|e| DbError::new(e.to_string()))?,
        ),
        // Parquet carries its own schema and ignores the one above. A column set
        // that does not match the table is left to the server to refuse: it is
        // the one that knows what its own table will accept, and a check here
        // would be a second opinion that can only be wrong in new ways.
        Format::Parquet => Box::new(
            parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
                .map_err(|e| DbError::new(e.to_string()))?
                .build()
                .map_err(|e| DbError::new(e.to_string()))?,
        ),
    };
    Ok(batches)
}
