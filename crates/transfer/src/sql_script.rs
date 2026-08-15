//! A result written as `INSERT` statements.

use arrow::array::RecordBatch;
use arrow::datatypes::DataType;
use arrow::error::ArrowError;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use dbsql::Dialect;
use std::io::Write;

/// How many rows share one `INSERT`.
///
/// One statement per row is the slowest thing a database can be asked to do,
/// and one statement for a million rows exceeds what most of them will parse.
/// A few hundred is where neither is true, and it is also small enough that a
/// failure names a readable part of the file.
const ROWS_PER_STATEMENT: usize = 200;

/// Turns record batches into `INSERT` statements for one table.
///
/// The dialect is the whole reason this is not a text format like the others:
/// an identifier and a string literal are spelled differently on each database,
/// and a script written in the wrong spelling fails on the first row with an
/// apostrophe in it — or worse, does not fail.
pub struct SqlWriter<W: Write> {
    inner: W,
    dialect: &'static Dialect,
    table: String,
    /// Built once from the first batch, because the column list is repeated on
    /// every statement and re-quoting it per row is work for nothing.
    columns: Option<String>,
    buffer: Vec<u8>,
}

const FLUSH_BYTES: usize = 256 * 1024;

impl<W: Write> SqlWriter<W> {
    /// `table` is written as given. It is a name the caller chose — the source
    /// relation, or one typed into the export panel — and quoting it here would
    /// mangle the qualified names (`public.orders`) that are the common case.
    pub fn new(inner: W, dialect: &'static Dialect, table: String) -> Self {
        Self {
            inner,
            dialect,
            table,
            columns: None,
            buffer: Vec::with_capacity(FLUSH_BYTES * 2),
        }
    }

    pub fn write(&mut self, batch: &RecordBatch) -> Result<(), ArrowError> {
        if self.columns.is_none() {
            let names: Vec<String> = batch
                .schema()
                .fields()
                .iter()
                .map(|f| self.dialect.quote(f.name()))
                .collect();
            self.columns = Some(names.join(", "));
        }
        // Set immediately above when it was absent.
        let columns = self.columns.clone().unwrap();

        let options = FormatOptions::default();
        let formatters = batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &options))
            .collect::<Result<Vec<_>, _>>()?;

        let schema = batch.schema();
        let mut text = String::new();
        for row in 0..batch.num_rows() {
            if row % ROWS_PER_STATEMENT == 0 {
                if row > 0 {
                    self.buffer.extend_from_slice(b";\n");
                }
                write_str(
                    &mut self.buffer,
                    &format!("INSERT INTO {} ({}) VALUES\n", self.table, columns),
                );
            } else {
                self.buffer.extend_from_slice(b",\n");
            }

            self.buffer.push(b'(');
            for (c, formatter) in formatters.iter().enumerate() {
                if c > 0 {
                    self.buffer.extend_from_slice(b", ");
                }
                if batch.column(c).is_null(row) {
                    self.buffer.extend_from_slice(b"NULL");
                    continue;
                }
                text.clear();
                formatter.value(row).write(&mut text)?;
                let literal = if unquoted(schema.field(c).data_type()) {
                    text.clone()
                } else {
                    self.dialect.string_literal(&text)
                };
                write_str(&mut self.buffer, &literal);
            }
            self.buffer.push(b')');

            if self.buffer.len() >= FLUSH_BYTES {
                self.flush_buffer()?;
            }
        }

        if batch.num_rows() > 0 {
            self.buffer.extend_from_slice(b";\n");
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<(), ArrowError> {
        self.flush_buffer()?;
        self.inner.flush()?;
        Ok(())
    }

    fn flush_buffer(&mut self) -> Result<(), ArrowError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.inner.write_all(&self.buffer)?;
        self.buffer.clear();
        Ok(())
    }
}

fn write_str(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
}

/// Whether a value of this type is written without quotes.
///
/// Quoting a number is not always harmless — a strict database refuses to
/// compare `'42'` with an integer column — and everything else is safer
/// quoted: a date, a timestamp and an interval all arrive intact as strings,
/// and every database will read them back into the column's own type.
///
/// Booleans are quoted for the same reason the editor quotes them: `true` is a
/// literal on PostgreSQL and an error on databases that spell it `1`, whereas
/// `'true'` is accepted by both.
fn unquoted(data_type: &DataType) -> bool {
    use DataType::*;
    matches!(
        data_type,
        Int8 | Int16
            | Int32
            | Int64
            | UInt8
            | UInt16
            | UInt32
            | UInt64
            | Float16
            | Float32
            | Float64
            | Decimal128(_, _)
            | Decimal256(_, _)
    )
}
