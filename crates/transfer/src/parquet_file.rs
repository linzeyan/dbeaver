//! Parquet, written straight from Arrow.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow::error::ArrowError;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use std::io::Write;

/// Turns record batches into a Parquet file.
///
/// The schema is not known until the first batch arrives — a result's schema
/// comes from the server with the data — so the underlying writer, which needs
/// it up front, is built on first use rather than in the constructor. An export
/// that produced no batches therefore writes no file, which is the honest
/// outcome: Parquet has no way to say "these columns, no rows" without having
/// been told the columns.
pub struct ParquetWriter<W: Write + Send> {
    inner: Option<ArrowWriter<W>>,
    /// Held until the first batch names the schema.
    pending: Option<W>,
}

impl<W: Write + Send> ParquetWriter<W> {
    pub fn new(into: W) -> Self {
        Self {
            inner: None,
            pending: Some(into),
        }
    }

    /// Compression on by default, which is the point of choosing this format:
    /// somebody exporting to Parquet rather than CSV is asking for the columnar
    /// file, and an uncompressed one throws away most of what they came for.
    /// zstd at its lowest level, because the levels above it cost noticeably
    /// more CPU for a few percent — and the exit criterion for this phase is
    /// that export stays I/O-bound.
    fn properties() -> WriterProperties {
        WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::default()))
            .build()
    }

    pub fn write(&mut self, batch: &RecordBatch) -> Result<(), ArrowError> {
        if self.inner.is_none() {
            let into = self
                .pending
                .take()
                .expect("pending is Some until inner is built, and this is that moment");
            let schema: SchemaRef = batch.schema();
            self.inner = Some(
                ArrowWriter::try_new(into, schema, Some(Self::properties()))
                    .map_err(|e| ArrowError::ExternalError(Box::new(e)))?,
            );
        }
        // Unwrap is sound: the branch above built it if it was missing.
        self.inner
            .as_mut()
            .unwrap()
            .write(batch)
            .map_err(|e| ArrowError::ExternalError(Box::new(e)))
    }

    /// Writes the footer, without which the file is not readable at all.
    pub fn finish(self) -> Result<(), ArrowError> {
        let Some(writer) = self.inner else {
            return Ok(());
        };
        writer
            .close()
            .map(|_| ())
            .map_err(|e| ArrowError::ExternalError(Box::new(e)))
    }
}
