//! Phase 0 PostgreSQL read path: connect, execute, stream Arrow record batches.
//!
//! Deliberately narrow. There is no `Driver` trait here — with one driver, the
//! abstraction would be invented rather than derived. Phase 1 defines it once
//! there are two implementations to derive it from.

mod arrow_map;
mod metadata;

pub use metadata::{
    ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, RelationInfo, RelationKind,
    RelationshipInfo, SchemaInfo, TriggerInfo,
};

use arrow::array::RecordBatch;
use arrow::datatypes::{Schema, SchemaRef};
use arrow_map::{ColBuilder, ColumnType, arrow_field};
use futures_util::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_postgres::error::{ErrorPosition, SqlState};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, NoTls, RowStream};

#[derive(Debug, thiserror::Error)]
pub enum PgError {
    #[error("{}", describe(.0))]
    Postgres(#[from] tokio_postgres::Error),
    #[error("column {column:?} has unsupported type {pg_type}")]
    UnsupportedType { column: String, pg_type: String },
    #[error("numeric value {0} does not fit the column's fixed scale")]
    NumericOverflow(String),
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
}

impl PgError {
    /// Where in the statement the server says the trouble is: a 1-based index,
    /// counted in characters, into the SQL that was sent.
    ///
    /// The message alone says what is wrong and never where, which for a syntax
    /// error is most of the answer missing. A front end that has the number can
    /// put the caret on the character.
    ///
    /// `Internal` positions are dropped rather than passed on. They index a
    /// query the server generated on our behalf — a PL/pgSQL body, say — not the
    /// text we handed it, so applying one to an editor points confidently at the
    /// wrong character. No position is better than a wrong one.
    pub fn statement_position(&self) -> Option<u32> {
        let PgError::Postgres(e) = self else {
            return None;
        };
        match e.as_db_error()?.position()? {
            ErrorPosition::Original(p) => Some(*p),
            ErrorPosition::Internal { .. } => None,
        }
    }

    /// Whether the server stopped this statement because somebody asked it to.
    ///
    /// A cancelled statement fails like any other, and the difference matters to
    /// whoever is looking at the screen: "canceling statement due to user
    /// request" in an error banner reads as a fault, when it is the button they
    /// just pressed working. The caller having issued the cancel is not enough
    /// to tell them apart — a statement can fail on its own merits in the same
    /// moment — so the answer comes from the SQLSTATE the server sent rather
    /// than from what this side happens to remember doing.
    pub fn is_cancelled(&self) -> bool {
        let PgError::Postgres(e) = self else {
            return false;
        };
        e.as_db_error()
            .is_some_and(|db| *db.code() == SqlState::QUERY_CANCELED)
    }
}

/// An error that never reached the server, with the reason it did not.
///
/// A failure before the connection exists carries no `DbError`, and what
/// tokio-postgres displays for one names the stage rather than the cause:
/// "error connecting to server" is every possible connection failure at once —
/// wrong port, no route, no server, TLS refused — and a connection dialog
/// showing it leaves the user to guess which. The reason is in the source
/// chain, so the chain is what gets rendered.
fn with_causes(e: &tokio_postgres::Error) -> String {
    use std::error::Error;
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(next) = cause {
        out.push_str(": ");
        out.push_str(&next.to_string());
        cause = next.source();
    }
    out
}

/// Renders a driver error the way the server stated it.
///
/// `tokio_postgres::Error` displays as the bare string "db error"; everything a
/// user needs is in the attached `DbError`. Without this the UI surfaces an
/// error banner that says nothing, which is worse than no banner.
fn describe(e: &tokio_postgres::Error) -> String {
    let Some(db) = e.as_db_error() else {
        return with_causes(e);
    };
    let mut out = db.message().to_string();
    if let Some(detail) = db.detail() {
        out.push_str(" — ");
        out.push_str(detail);
    }
    if let Some(hint) = db.hint() {
        out.push_str(" (");
        out.push_str(hint);
        out.push(')');
    }
    out
}

pub struct PgSource {
    client: Client,
    conn_str: String,
}

impl PgSource {
    pub async fn connect(conn_str: &str) -> Result<Self, PgError> {
        let (client, connection) = tokio_postgres::connect(conn_str, NoTls).await?;
        // The connection future drives the socket and must outlive us. Phase 0
        // has no reconnect story; a dropped connection surfaces as a query error.
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("postgres connection closed: {e}");
            }
        });
        Ok(Self {
            client,
            conn_str: conn_str.to_string(),
        })
    }

    /// Asks the server to abandon whatever this connection is currently running.
    ///
    /// The request travels on a connection of its own, which is why this can be
    /// called while the socket is busy streaming a result: the protocol has no
    /// way to interleave one, so a cancel sent in-band would sit in the queue
    /// behind the statement it is trying to stop.
    ///
    /// Best-effort by design. The server may finish before the request lands, or
    /// the statement may be between commands with nothing to cancel, and neither
    /// is an error — success here means the request was delivered, not that
    /// anything was interrupted. What actually happened shows up as the running
    /// statement failing with `is_cancelled`, or not failing at all.
    pub async fn cancel(&self) -> Result<(), PgError> {
        self.client.cancel_token().cancel_query(NoTls).await?;
        Ok(())
    }

    /// Non-system schemas, for the navigator root.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, PgError> {
        metadata::schemas(&self.client).await
    }

    /// Tables, views, and other relations within a schema.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, PgError> {
        metadata::relations(&self.client, schema).await
    }

    /// Column definitions for one relation.
    pub async fn columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>, PgError> {
        metadata::columns(&self.client, schema, relation).await
    }

    /// The statement a view is defined by; `None` for a relation that has none.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, PgError> {
        metadata::definition(&self.client, schema, relation).await
    }

    /// Indexes on one relation, primary key first.
    pub async fn indexes(&self, schema: &str, relation: &str) -> Result<Vec<IndexInfo>, PgError> {
        metadata::indexes(&self.client, schema, relation).await
    }

    /// Foreign keys declared by one relation.
    pub async fn foreign_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, PgError> {
        metadata::foreign_keys(&self.client, schema, relation).await
    }

    /// Foreign keys other relations declare against this one.
    pub async fn referenced_by(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, PgError> {
        metadata::referenced_by(&self.client, schema, relation).await
    }

    /// CHECK, UNIQUE, and EXCLUDE constraints.
    pub async fn constraints(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ConstraintInfo>, PgError> {
        metadata::constraints(&self.client, schema, relation).await
    }

    /// User-defined triggers, excluding constraint enforcement machinery.
    pub async fn triggers(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<TriggerInfo>, PgError> {
        metadata::triggers(&self.client, schema, relation).await
    }

    /// Prepare `sql` and begin streaming results as Arrow batches of
    /// `batch_rows` rows.
    ///
    /// Resolves once the server acknowledges the bind, which is later than it
    /// reads: the server buffers its output and flushes at the end of the
    /// command, so on a slow statement this waits out the whole execution and
    /// then returns a stream whose first batch has already arrived. Execution
    /// failures — and a `cancel` that lands mid-statement — therefore still
    /// surface from `next_batch`, not from here.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<ArrowStream, PgError> {
        let stmt = self.client.prepare(sql).await?;

        let types: Vec<ColumnType> = stmt
            .columns()
            .iter()
            .map(|c| ColumnType {
                pg_type: c.type_().clone(),
                modifier: c.type_modifier(),
            })
            .collect();
        let fields = stmt
            .columns()
            .iter()
            .zip(&types)
            .map(|(c, t)| arrow_field(c.name(), t))
            .collect::<Result<Vec<_>, _>>()?;
        let schema = Arc::new(Schema::new(fields));

        let no_params: [&(dyn ToSql + Sync); 0] = [];
        let rows = self
            .client
            .query_raw(&stmt, no_params.iter().copied())
            .await?;

        Ok(ArrowStream {
            schema,
            types,
            rows: Box::pin(rows),
            batch_rows,
            exhausted: false,
        })
    }

    /// Open a cursor over `sql` and return a handle to fetch pages.
    ///
    /// A cursor occupies its connection while open, so the handle owns a
    /// connection of its own for the lifetime of the cursor. The connection
    /// is closed when the cursor is dropped, ensuring that no changes are
    /// committed.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Cursor, PgError> {
        // Create a fresh connection for the cursor
        let (client, connection) = tokio_postgres::connect(&self.conn_str, NoTls).await?;
        // The connection future drives the socket and must outlive us.
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("postgres cursor connection closed: {e}");
            }
        });

        // Create a unique cursor name using atomic counter
        static CURSOR_COUNTER: AtomicU64 = AtomicU64::new(0);
        let cursor_id = CURSOR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let cursor_name = format!("cursor_{}", cursor_id);

        // Begin transaction and declare the cursor
        client.batch_execute("BEGIN").await?;

        let declare_sql = format!("DECLARE {} CURSOR FOR {}", cursor_name, sql);
        client.batch_execute(&declare_sql).await?;

        // Prepare the schema information by fetching column info from the statement
        let stmt = client.prepare(sql).await?;

        let types: Vec<ColumnType> = stmt
            .columns()
            .iter()
            .map(|c| ColumnType {
                pg_type: c.type_().clone(),
                modifier: c.type_modifier(),
            })
            .collect();
        let fields = stmt
            .columns()
            .iter()
            .zip(&types)
            .map(|(c, t)| arrow_field(c.name(), t))
            .collect::<Result<Vec<_>, _>>()?;
        let schema = Arc::new(Schema::new(fields));

        Ok(Cursor {
            client,
            schema,
            types,
            batch_rows,
            cursor_name,
        })
    }
}

pub struct ArrowStream {
    schema: SchemaRef,
    types: Vec<ColumnType>,
    rows: Pin<Box<RowStream>>,
    batch_rows: usize,
    exhausted: bool,
}

/// A cursor over a PostgreSQL query result.
///
/// A cursor occupies its connection while open, so the handle owns a
/// connection of its own for the lifetime of the cursor. The connection
/// is closed when the cursor is dropped, ensuring that no changes are
/// committed.
pub struct Cursor {
    client: Client,
    schema: SchemaRef,
    types: Vec<ColumnType>,
    batch_rows: usize,
    cursor_name: String,
}

impl Cursor {
    /// Fetch the next batch of rows from the cursor.
    ///
    /// Returns `Ok(None)` when the cursor has reached the end of the result set.
    /// Returns an error if the fetch fails.
    pub async fn fetch(&mut self) -> Result<Option<RecordBatch>, PgError> {
        // Use FETCH FORWARD to get the next batch of rows
        let sql = format!(
            "FETCH FORWARD {} FROM {}",
            self.batch_rows, self.cursor_name
        );
        let rows = self.client.query(&sql, &[]).await?;

        if rows.is_empty() {
            return Ok(None);
        }

        let mut builders: Vec<ColBuilder> = self
            .types
            .iter()
            .map(|t| ColBuilder::new(t, self.batch_rows))
            .collect();

        let mut n = 0usize;
        for row in rows {
            for (idx, b) in builders.iter_mut().enumerate() {
                b.append(&row, idx)?;
            }
            n += 1;
        }

        if n == 0 {
            return Ok(None);
        }

        let arrays = builders.iter_mut().map(|b| b.finish()).collect();
        Ok(Some(RecordBatch::try_new(
            Arc::clone(&self.schema),
            arrays,
        )?))
    }

    /// Close the cursor explicitly.
    ///
    /// This is optional as the cursor will be closed automatically when dropped.
    pub async fn close(&mut self) -> Result<(), PgError> {
        let sql = format!("CLOSE {}", self.cursor_name);
        self.client.batch_execute(&sql).await?;
        // Rollback the transaction to close it properly
        self.client.batch_execute("ROLLBACK").await?;
        Ok(())
    }
}

impl ArrowStream {
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Rows the server said this statement affected, or `None` until the result
    /// has been read to the end.
    ///
    /// A statement that returns no result set — an `UPDATE`, a `CREATE` — still
    /// did something, and this count is the only thing it says about itself. It
    /// rides on the `CommandComplete` that terminates the result, so there is
    /// nothing to read until `next_batch` has answered `None`; a number reported
    /// before then would be a guess dressed as an answer.
    ///
    /// The verb does not come with it. tokio-postgres parses the trailing count
    /// out of the command tag and drops the rest, so `UPDATE 3` reaches us as 3
    /// and `CREATE TABLE` as 0. Recovering the verb by re-reading the SQL we
    /// sent would be this side inventing a fact the server did not state, which
    /// is how a `CREATE` ends up labelled by somebody's regex for `INSERT`.
    pub fn rows_affected(&self) -> Option<u64> {
        self.rows.rows_affected()
    }

    /// Next batch, or `None` once the result is fully consumed.
    ///
    /// Builders are allocated per batch. Reusing them across batches would save
    /// allocations but force a copy out of the shared buffer on `finish`, which
    /// is the opposite of what this path exists to demonstrate.
    pub async fn next_batch(&mut self) -> Result<Option<RecordBatch>, PgError> {
        if self.exhausted {
            return Ok(None);
        }

        let mut builders: Vec<ColBuilder> = self
            .types
            .iter()
            .map(|t| ColBuilder::new(t, self.batch_rows))
            .collect();

        let mut n = 0usize;
        while n < self.batch_rows {
            match self.rows.next().await {
                Some(row) => {
                    let row = row?;
                    for (idx, b) in builders.iter_mut().enumerate() {
                        b.append(&row, idx)?;
                    }
                    n += 1;
                }
                None => {
                    self.exhausted = true;
                    break;
                }
            }
        }

        if n == 0 {
            return Ok(None);
        }

        let arrays = builders.iter_mut().map(|b| b.finish()).collect();
        Ok(Some(RecordBatch::try_new(
            Arc::clone(&self.schema),
            arrays,
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs no database — it needs the absence of one, which is why it can run
    /// in the unit suite. Port 1 is reserved and nothing on a developer machine
    /// or a CI runner listens there.
    #[tokio::test]
    async fn a_connection_that_never_happened_says_why_not() {
        let err = PgSource::connect("host=127.0.0.1 port=1 user=nobody dbname=nothing")
            .await
            .err()
            .expect("nothing is listening on port 1");
        let message = err.to_string();
        // The stage on its own — which is all tokio-postgres displays — fits
        // every connection failure there is, so a dialog showing it tells the
        // user nothing they did not already know from the dialog being up.
        assert!(
            message.len() > "error connecting to server".len(),
            "the message stops at the stage and never says the cause: {message}"
        );
        assert!(
            message.to_lowercase().contains("refused"),
            "expected the refusal to survive into the message, got: {message}"
        );
    }
}
