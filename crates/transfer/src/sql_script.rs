//! A result written as `INSERT` statements.

use arrow::array::RecordBatch;
use arrow::datatypes::{DataType, Schema};
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
pub(crate) const ROWS_PER_STATEMENT: usize = 200;

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
    insert: Option<Insert>,
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
            insert: None,
            buffer: Vec::with_capacity(FLUSH_BYTES * 2),
        }
    }

    pub fn write(&mut self, batch: &RecordBatch) -> Result<(), ArrowError> {
        if self.insert.is_none() {
            self.insert = Some(Insert::new(
                self.dialect,
                self.table.clone(),
                batch.schema().as_ref(),
            ));
        }
        let mut offset = 0;
        while offset < batch.num_rows() {
            let rows = std::cmp::min(ROWS_PER_STATEMENT, batch.num_rows() - offset);
            // Set immediately above when it was absent. Scoped so the borrow is
            // over before `flush_buffer` wants `self` back.
            let statement = {
                let insert = self.insert.as_ref().unwrap();
                insert.statement(self.dialect, batch, offset, rows)?
            };
            write_str(&mut self.buffer, &statement);
            if self.buffer.len() >= FLUSH_BYTES {
                self.flush_buffer()?;
            }
            offset += rows;
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

/// One table's `INSERT` statements, rendered from batches.
///
/// Separate from the writer above because a file is not the only destination:
/// a database-to-database transfer sends these same statements to a server. Two
/// copies of "how a value becomes SQL" would be two copies to keep in step, and
/// the one that drifted would be the one nobody had a test for.
pub(crate) struct Insert {
    table: String,
    columns: String,
}

impl Insert {
    pub(crate) fn new(dialect: &Dialect, table: String, schema: &Schema) -> Self {
        let columns: String = schema
            .fields()
            .iter()
            .map(|f| dialect.quote(f.name()))
            .collect::<Vec<_>>()
            .join(", ");
        Self { table, columns }
    }

    pub(crate) fn statement(
        &self,
        dialect: &Dialect,
        batch: &RecordBatch,
        offset: usize,
        rows: usize,
    ) -> Result<String, ArrowError> {
        let options = FormatOptions::default();
        let formatters: Vec<_> = batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &options))
            .collect::<Result<Vec<_>, _>>()?;

        let schema = batch.schema();
        let mut text = String::new();
        let mut result = format!("INSERT INTO {} ({}) VALUES\n", self.table, self.columns);

        for (i, row) in (offset..).take(rows).enumerate() {
            if i > 0 {
                result.push_str(",\n");
            }
            result.push('(');
            for (c, formatter) in formatters.iter().enumerate() {
                if c > 0 {
                    result.push_str(", ");
                }
                if batch.column(c).is_null(row) {
                    result.push_str("NULL");
                    continue;
                }
                text.clear();
                formatter.value(row).write(&mut text)?;
                let literal = if unquoted(schema.field(c).data_type()) {
                    text.clone()
                } else {
                    dialect.string_literal(&text)
                };
                result.push_str(&literal);
            }
            result.push(')');
        }

        result.push_str(";\n");
        Ok(result)
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
