//! Sending INSERT statements to a database instead of a file.

use arrow::array::RecordBatch;
use arrow::datatypes::Schema;
use dbconn::{DbError, DbResult, Driver};
use dbsql::Dialect;

use crate::sql_script::{Insert, ROWS_PER_STATEMENT};

/// A table on another database, written to by statement.
///
/// Statements and not a bulk-load path because there is no bulk-load path every
/// database shares: `COPY`, `LOAD DATA` and `BULK INSERT` are three protocols
/// with three sets of privileges. An `INSERT` runs everywhere this application
/// can already connect, which is the point of routing it through the `Driver`
/// trait rather than through any one driver.
pub struct TargetWriter {
    insert: Insert,
    dialect: &'static Dialect,
}

impl TargetWriter {
    /// `table` is written as given, as it is by the file writer — see
    /// `SqlWriter::new` for why a qualified name must not be quoted here.
    pub fn new(dialect: &'static Dialect, table: String, schema: &Schema) -> Self {
        Self {
            insert: Insert::new(dialect, table, schema),
            dialect,
        }
    }

    /// Sends one batch, and reports how many rows that was.
    ///
    /// One statement at a time, awaited before the next is sent. Firing them
    /// concurrently would be faster and would also reorder them, and rows that
    /// arrive out of order are rows a foreign key or a unique index can refuse
    /// for reasons the source never had.
    pub async fn write(&self, target: &dyn Driver, batch: &RecordBatch) -> DbResult<u64> {
        let mut total = 0u64;
        let mut offset = 0;

        while offset < batch.num_rows() {
            let rows = std::cmp::min(ROWS_PER_STATEMENT, batch.num_rows() - offset);
            let statement = self
                .insert
                .statement(self.dialect, batch, offset, rows)
                .map_err(|e| DbError::new(e.to_string()))?;

            let mut stream = target.query(&statement, 1).await?;
            // Drained, not dropped. A statement that violates a constraint
            // fails when the server executes it, not when it accepts it, so a
            // stream left unread reports success for rows the server refused —
            // and the transfer would report a total it did not write.
            while stream.next_batch().await?.is_some() {}

            total += rows as u64;
            offset += rows;
        }

        Ok(total)
    }
}
