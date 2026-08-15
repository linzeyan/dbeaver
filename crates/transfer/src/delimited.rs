//! CSV and TSV, written a batch at a time.

use arrow::array::RecordBatch;
use arrow::error::ArrowError;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use std::io::Write;

/// Quotes one field the way RFC 4180 says to.
///
/// A value carrying the delimiter, a double quote, CR or LF is wrapped in
/// quotes with its own quotes doubled. Approximately-correct quoting is how one
/// address column silently shifts every field after it one place to the left,
/// in a file nobody re-reads.
///
/// NULL and the empty string are written differently — nothing at all versus a
/// quoted empty field — because they are different values and the difference is
/// one people write WHERE clauses against. It is also what PostgreSQL's own
/// COPY … FORMAT csv emits, so readers on the other side already know how to
/// take it. This is the reason the writer is here rather than `arrow::csv`,
/// whose null representation is a plain empty field: it cannot say which of the
/// two a blank was.
fn field(value: Option<&str>, delimiter: u8, out: &mut Vec<u8>) {
    let Some(value) = value else { return };
    if value.is_empty() {
        out.extend_from_slice(b"\"\"");
        return;
    }
    // Over bytes rather than chars, which is sound because every byte tested
    // for is ASCII and UTF-8 never puts an ASCII byte inside a multi-byte
    // sequence.
    let needs_quoting = value
        .bytes()
        .any(|b| b == delimiter || b == b'"' || b == b'\r' || b == b'\n');
    if !needs_quoting {
        out.extend_from_slice(value.as_bytes());
        return;
    }
    out.push(b'"');
    for b in value.bytes() {
        if b == b'"' {
            out.push(b'"');
        }
        out.push(b);
    }
    out.push(b'"');
}

/// Writes one record, terminated.
///
/// LF, not the CRLF the RFC nominates. Every parser worth writing a file for
/// accepts LF, and a stray CR is the kind of thing that costs somebody an
/// afternoon. Quoting is where the RFC has to be followed to the letter; the
/// line ending is where it does not.
fn row<'a>(values: impl Iterator<Item = Option<&'a str>>, delimiter: u8, out: &mut Vec<u8>) {
    for (index, value) in values.enumerate() {
        if index > 0 {
            out.push(delimiter);
        }
        field(value, delimiter, out);
    }
    out.push(b'\n');
}

/// Turns record batches into delimited text.
///
/// Holds the header state rather than taking it per call, because the header is
/// written once and the batches arrive one at a time — a caller made to
/// remember which batch was first would get it wrong on the empty result, where
/// there is no first batch and the header still has to be written.
pub struct DelimitedWriter<W: Write> {
    inner: W,
    delimiter: u8,
    wrote_header: bool,
    /// Reused across batches so a million rows do not cost a million
    /// allocations. Flushed to `inner` once it passes `FLUSH_BYTES`.
    buffer: Vec<u8>,
}

/// Bytes buffered before a write. Large enough that a million rows cost a few
/// hundred syscalls, small enough that the peak allocation is nothing.
const FLUSH_BYTES: usize = 256 * 1024;

impl<W: Write> DelimitedWriter<W> {
    pub fn new(inner: W, delimiter: u8) -> Self {
        Self {
            inner,
            delimiter,
            wrote_header: false,
            buffer: Vec::with_capacity(FLUSH_BYTES * 2),
        }
    }

    /// Writes `batch`, preceded by the header if this is the first call.
    pub fn write(&mut self, batch: &RecordBatch) -> Result<(), ArrowError> {
        if !self.wrote_header {
            let names = batch.schema();
            row(
                names.fields().iter().map(|f| Some(f.name().as_str())),
                self.delimiter,
                &mut self.buffer,
            );
            self.wrote_header = true;
        }

        // Options are irrelevant to how nulls come out here: this asks the
        // column itself and writes nothing for them, rather than letting the
        // formatter render one as a string. Its answer would be the empty
        // string, which `field` would faithfully quote into `""` — the
        // representation reserved for an empty value that is not null.
        let options = FormatOptions::default();
        let formatters = batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &options))
            .collect::<Result<Vec<_>, _>>()?;

        let mut text = String::new();
        for r in 0..batch.num_rows() {
            for (c, formatter) in formatters.iter().enumerate() {
                if c > 0 {
                    self.buffer.push(self.delimiter);
                }
                if batch.column(c).is_null(r) {
                    continue;
                }
                text.clear();
                // `write` rather than `Display`, which under the default
                // options renders a formatting failure as the literal text
                // "ERROR: …" and returns Ok. In a file nobody re-reads, an
                // export that quietly writes that in a cell is worse than one
                // that refuses to save.
                formatter.value(r).write(&mut text)?;
                field(Some(&text), self.delimiter, &mut self.buffer);
            }
            self.buffer.push(b'\n');

            if self.buffer.len() >= FLUSH_BYTES {
                self.flush_buffer()?;
            }
        }
        Ok(())
    }

    /// Flushes what is buffered and hands back the writer.
    ///
    /// Consuming rather than relying on `Drop`, because the last write is the
    /// one that can fail on a full disk and a `Drop` has nowhere to report it.
    /// A save that quietly wrote all but the final buffer is worse than one
    /// that says the disk is full.
    pub fn finish(mut self) -> Result<W, ArrowError> {
        self.flush_buffer()?;
        self.inner.flush()?;
        Ok(self.inner)
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
