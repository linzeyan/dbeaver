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

/// Renders a driver error the way the server stated it.
///
/// `tokio_postgres::Error` displays as the bare string "db error"; everything a
/// user needs is in the attached `DbError`. Without this the UI surfaces an
/// error banner that says nothing, which is worse than no banner.
fn describe(e: &tokio_postgres::Error) -> String {
    let Some(db) = e.as_db_error() else {
        return e.to_string();
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
        Ok(Self { client })
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
}

pub struct ArrowStream {
    schema: SchemaRef,
    types: Vec<ColumnType>,
    rows: Pin<Box<RowStream>>,
    batch_rows: usize,
    exhausted: bool,
}

impl ArrowStream {
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
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
