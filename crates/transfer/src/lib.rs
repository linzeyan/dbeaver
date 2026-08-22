//! Writing a result out, a batch at a time.
//!
//! In Rust rather than in the front end because the front end had to have the
//! whole result in memory before it could write a byte of it: it exported what
//! the grid had loaded, so a large table was a scroll-to-the-bottom away from
//! being exportable at all. A batch at a time has no such ceiling, and leaves
//! the cost where it belongs — in the socket and the disk, not in formatting.

mod delimited;
mod import;
mod moving;
mod parquet_file;
mod sql_script;
mod target;

pub use delimited::{DelimitedReader, DelimitedWriter};
pub use import::import;
pub use moving::{Step, Stopper, Transfer};
pub use parquet_file::ParquetWriter;
pub use sql_script::SqlWriter;
pub use target::TargetWriter;

use arrow::array::RecordBatch;
use arrow::error::ArrowError;
use dbconn::DbResult;
use dbsql::Dialect;
use std::io::Write;

/// A format a result can be written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Tsv,
    /// One JSON object per line, which is the shape that streams. A single
    /// top-level array would have to be closed by whoever wrote it, so a
    /// transfer stopped part way through leaves a file no parser will open —
    /// and stopping part way through is a button this application has.
    JsonLines,
    Parquet,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::JsonLines => "jsonl",
            Format::Parquet => "parquet",
        }
    }

    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "csv" => Some(Format::Csv),
            "tsv" => Some(Format::Tsv),
            "jsonl" | "ndjson" => Some(Format::JsonLines),
            "parquet" => Some(Format::Parquet),
            _ => None,
        }
    }
}

/// A destination that takes batches and finishes into a file.
///
/// An enum and not a trait object: the three writers have nothing in common to
/// abstract over — one holds a byte buffer, one holds a parquet footer under
/// construction — and a trait would exist only so this module could match on it
/// anyway.
enum Sink<W: Write + Send> {
    Delimited(DelimitedWriter<W>),
    Json(arrow::json::LineDelimitedWriter<W>),
    Parquet(ParquetWriter<W>),
}

/// Writes every batch `batches` yields, and reports how many rows that was.
///
/// Takes an iterator rather than a slice, so the caller can hand over a cursor
/// that is still fetching. Nothing here holds a batch after writing it, which
/// is the property that makes the size of the result irrelevant.
pub fn export<W, I>(batches: I, format: Format, into: W) -> Result<u64, ArrowError>
where
    W: Write + Send,
    I: IntoIterator<Item = Result<RecordBatch, ArrowError>>,
{
    let mut sink = match format {
        Format::Csv => Sink::Delimited(DelimitedWriter::new(into, b',')),
        Format::Tsv => Sink::Delimited(DelimitedWriter::new(into, b'\t')),
        Format::JsonLines => Sink::Json(arrow::json::LineDelimitedWriter::new(into)),
        Format::Parquet => Sink::Parquet(ParquetWriter::new(into)),
    };

    let mut rows = 0u64;
    for batch in batches {
        let batch = batch?;
        rows += batch.num_rows() as u64;
        match &mut sink {
            Sink::Delimited(w) => w.write(&batch)?,
            Sink::Json(w) => w.write(&batch)?,
            Sink::Parquet(w) => w.write(&batch)?,
        }
    }

    // Every sink is finished explicitly. Parquet's footer and the delimited
    // writer's last buffer are both written here, and both can fail on a full
    // disk — a failure a `Drop` would have to swallow, leaving a file that
    // looks complete and is not.
    match sink {
        Sink::Delimited(w) => {
            w.finish()?;
        }
        Sink::Json(mut w) => {
            w.finish()?;
        }
        Sink::Parquet(w) => w.finish()?,
    }
    Ok(rows)
}

/// Writes every batch as `INSERT` statements into `table`, and reports how many
/// rows that was.
///
/// Its own entry point rather than a fifth `Format`, because it is the only one
/// that cannot be chosen by a file extension alone: an `INSERT` needs a table to
/// name and a dialect to spell it in. Folding those into `export` would give
/// four of the five formats two arguments that mean nothing to them.
pub fn export_sql<W, I>(
    batches: I,
    dialect: &'static Dialect,
    table: String,
    into: W,
) -> Result<u64, ArrowError>
where
    W: Write,
    I: IntoIterator<Item = Result<RecordBatch, ArrowError>>,
{
    let mut writer = SqlWriter::new(into, dialect, table);
    let mut rows = 0u64;
    for batch in batches {
        let batch = batch?;
        rows += batch.num_rows() as u64;
        writer.write(&batch)?;
    }
    writer.finish()?;
    Ok(rows)
}

/// Fetch one batch at a time from `source` and send each to `target`.
///
/// Builds the `TargetWriter` from the first batch's schema, not from
/// `source.schema()`, so an empty result sends nothing at all rather than an
/// INSERT with no rows. Nothing is held between batches, which is what makes
/// the size of the result irrelevant.
pub async fn transfer(
    source: &mut dyn dbconn::Cursor,
    target: &dyn dbconn::Driver,
    dialect: &'static Dialect,
    table: String,
) -> DbResult<u64> {
    let mut total = 0u64;
    let mut writer: Option<TargetWriter> = None;

    while let Some(batch) = source.fetch().await? {
        let writer = writer
            .get_or_insert_with(|| TargetWriter::new(dialect, table.clone(), batch.schema_ref()));
        total += writer.write(target, &batch).await?;
    }

    Ok(total)
}
