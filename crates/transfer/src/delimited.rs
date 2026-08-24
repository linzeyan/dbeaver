//! CSV and TSV, written and read a batch at a time.
//!
//! Both directions live in one file because they share one contract — the
//! quoting rules and the NULL-versus-empty distinction — and a reader kept
//! apart from its writer is how the two drift until a round trip loses data.

use arrow::array::{ArrayRef, RecordBatch, StringArray};
use arrow::compute::{CastOptions, cast_with_options};
use arrow::datatypes::{DataType, Schema, SchemaRef};
use arrow::error::ArrowError;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use std::io::{BufReader, Read, Write};
use std::sync::Arc;

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

/// Rows read per batch. The reader's memory is one batch regardless of the
/// file, which is the property `import` promises.
const BATCH_ROWS: usize = 4096;

/// What ended a field.
enum End {
    Delimiter,
    Record,
    Eof,
}

/// One record's fields; `None` is an unquoted empty field, the file's NULL.
type Fields = Vec<Option<String>>;

/// Turns delimited text back into record batches, against a schema the caller
/// already has.
///
/// Written here rather than taken from `arrow::csv` for the mirror image of the
/// writer's reason: the `csv` crate underneath `arrow::csv` resolves quoting
/// while it parses, so by the time Arrow sees a field, `` and `""` are the same
/// empty string — and the reader calls both NULL. Whether a blank was quoted is
/// the one bit this reader exists to keep: an unquoted empty field is NULL, a
/// quoted one is an empty string, exactly as the writer above spends them.
///
/// The distinction only has somewhere to land in a string column. In any other
/// type an empty field can only be NULL, whichever way it was written.
///
/// Values are parsed by Arrow's own cast kernels rather than by hand: this
/// reader decides where fields begin and end and which of them are null, and
/// hands the text over. `safe: false`, so a cell that does not parse fails the
/// import by name instead of quietly becoming one more NULL.
pub struct DelimitedReader<R: Read> {
    input: std::io::Bytes<BufReader<R>>,
    /// One byte of lookahead, for CRLF and for the byte after a closing quote.
    peeked: Option<u8>,
    delimiter: u8,
    schema: SchemaRef,
    /// 1-based, counting the header; a quoted field may span several. Errors
    /// carry the line a record started on, because "somewhere in the file" is
    /// not an answer anyone can act on.
    line: u64,
    header_done: bool,
    finished: bool,
}

impl<R: Read> DelimitedReader<R> {
    /// Refuses binary columns up front: the writer renders them as hex text,
    /// and reading that hex back as raw bytes would be corruption with a row
    /// count that looks right. Parquet carries binary faithfully.
    pub fn new(input: R, delimiter: u8, schema: SchemaRef) -> Result<Self, ArrowError> {
        for field in schema.fields() {
            if matches!(
                field.data_type(),
                DataType::Binary
                    | DataType::LargeBinary
                    | DataType::BinaryView
                    | DataType::FixedSizeBinary(_)
            ) {
                return Err(ArrowError::CsvError(format!(
                    "column {} holds binary data, which a delimited file cannot \
                     carry faithfully; Parquet can",
                    field.name()
                )));
            }
        }
        Ok(Self {
            input: BufReader::new(input).bytes(),
            peeked: None,
            delimiter,
            schema,
            line: 1,
            header_done: false,
            finished: false,
        })
    }

    fn next_byte(&mut self) -> Result<Option<u8>, ArrowError> {
        if let Some(b) = self.peeked.take() {
            return Ok(Some(b));
        }
        match self.input.next() {
            None => Ok(None),
            Some(Ok(b)) => Ok(Some(b)),
            Some(Err(e)) => Err(ArrowError::CsvError(format!("reading failed: {e}"))),
        }
    }

    /// Consumes the LF of a CRLF pair, if there is one. A lone CR is a line
    /// ending too — old files exist and are not worth refusing.
    fn eat_lf(&mut self) -> Result<(), ArrowError> {
        self.line += 1;
        let next = self.next_byte()?;
        if next != Some(b'\n') {
            self.peeked = next;
        }
        Ok(())
    }

    /// One field, and what ended it. `None` is an unquoted empty field — the
    /// writer's spelling of NULL.
    fn field(&mut self, start_line: u64) -> Result<(Option<String>, End), ArrowError> {
        let mut bytes = Vec::new();
        match self.next_byte()? {
            None => return Ok((None, End::Eof)),
            Some(b) if b == self.delimiter => return Ok((None, End::Delimiter)),
            Some(b'\n') => {
                self.line += 1;
                return Ok((None, End::Record));
            }
            Some(b'\r') => {
                self.eat_lf()?;
                return Ok((None, End::Record));
            }
            Some(b'"') => return self.quoted_field(start_line),
            Some(b) => bytes.push(b),
        }
        loop {
            let end = match self.next_byte()? {
                None => End::Eof,
                Some(b) if b == self.delimiter => End::Delimiter,
                Some(b'\n') => {
                    self.line += 1;
                    End::Record
                }
                Some(b'\r') => {
                    self.eat_lf()?;
                    End::Record
                }
                Some(b) => {
                    bytes.push(b);
                    continue;
                }
            };
            return Ok((Some(text(bytes, start_line)?), end));
        }
    }

    /// The rest of a field whose opening quote was just consumed. Delimiters
    /// and line endings inside are literal; `""` is one literal quote.
    fn quoted_field(&mut self, start_line: u64) -> Result<(Option<String>, End), ArrowError> {
        let mut bytes = Vec::new();
        loop {
            match self.next_byte()? {
                None => {
                    return Err(ArrowError::CsvError(format!(
                        "line {start_line}: a quote is opened and never closed"
                    )));
                }
                Some(b'"') => {
                    let end = match self.next_byte()? {
                        Some(b'"') => {
                            bytes.push(b'"');
                            continue;
                        }
                        None => End::Eof,
                        Some(b) if b == self.delimiter => End::Delimiter,
                        Some(b'\n') => {
                            self.line += 1;
                            End::Record
                        }
                        Some(b'\r') => {
                            self.eat_lf()?;
                            End::Record
                        }
                        Some(_) => {
                            return Err(ArrowError::CsvError(format!(
                                "line {start_line}: text after a closing quote"
                            )));
                        }
                    };
                    // Quoted, so even nothing at all is an empty string and
                    // not a NULL. This branch is the reader's whole reason.
                    return Ok((Some(text(bytes, start_line)?), end));
                }
                Some(b'\n') => {
                    self.line += 1;
                    bytes.push(b'\n');
                }
                Some(b) => bytes.push(b),
            }
        }
    }

    /// One record: the line it started on and its fields. `None` at a clean
    /// end of file.
    fn record(&mut self) -> Result<Option<(u64, Fields)>, ArrowError> {
        let first = self.next_byte()?;
        let Some(first) = first else { return Ok(None) };
        self.peeked = Some(first);

        let start_line = self.line;
        let mut fields = Vec::new();
        loop {
            let (value, end) = self.field(start_line)?;
            fields.push(value);
            match end {
                End::Delimiter => continue,
                End::Record | End::Eof => break,
            }
        }
        Ok(Some((start_line, fields)))
    }

    /// A record checked against the schema's width, or the error saying which
    /// line disagreed.
    fn checked_record(&mut self) -> Result<Option<Fields>, ArrowError> {
        let Some((start_line, fields)) = self.record()? else {
            return Ok(None);
        };
        let expected = self.schema.fields().len();
        if fields.len() != expected {
            return Err(ArrowError::CsvError(format!(
                "line {start_line}: expected {expected} fields, found {}",
                fields.len()
            )));
        }
        Ok(Some(fields))
    }

    fn batch(&self, rows: &[Fields]) -> Result<RecordBatch, ArrowError> {
        let options = CastOptions {
            safe: false,
            format_options: FormatOptions::default(),
        };
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(self.schema.fields().len());
        for (index, field) in self.schema.fields().iter().enumerate() {
            let column: ArrayRef = if field.data_type() == &DataType::Utf8 {
                let strings: StringArray = rows.iter().map(|r| r[index].as_deref()).collect();
                Arc::new(strings)
            } else {
                // A quoted blank folds into NULL here: outside a string column
                // the distinction has nowhere to land, and `''` handed to the
                // cast would be refused as an unparseable number rather than
                // read as the nothing it is.
                let strings: StringArray = rows
                    .iter()
                    .map(|r| r[index].as_deref().filter(|s| !s.is_empty()))
                    .collect();
                cast_with_options(&strings, field.data_type(), &options)
                    .map_err(|e| ArrowError::CsvError(format!("column {}: {e}", field.name())))?
            };
            columns.push(column);
        }
        RecordBatch::try_new(Arc::clone(&self.schema), columns)
    }
}

fn text(bytes: Vec<u8>, start_line: u64) -> Result<String, ArrowError> {
    String::from_utf8(bytes)
        .map_err(|_| ArrowError::CsvError(format!("line {start_line}: not valid UTF-8")))
}

/// The names in a delimited file's first record.
///
/// Built by hand rather than through `DelimitedReader::new`, because there is no
/// schema yet — this is what runs before there is one, so that somebody can be
/// shown which of the file's columns is going where. Nothing below the record
/// splitter is touched: no width is checked and no value is cast.
///
/// An empty field in a header comes back as an empty name, which is what the
/// file says. Inventing "column 3" here would put a name in the picker that is
/// not in the file.
pub fn header_names<R: Read>(input: R, delimiter: u8) -> Result<Vec<String>, ArrowError> {
    let mut reader = DelimitedReader {
        input: BufReader::new(input).bytes(),
        peeked: None,
        delimiter,
        schema: Arc::new(Schema::empty()),
        line: 1,
        header_done: false,
        finished: false,
    };
    let Some((_, fields)) = reader.record()? else {
        return Ok(Vec::new());
    };
    Ok(fields.into_iter().map(|f| f.unwrap_or_default()).collect())
}

impl<R: Read> Iterator for DelimitedReader<R> {
    type Item = Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if !self.header_done {
            // The header is skipped by position, not matched by name — the
            // columns are wherever the file puts them, exactly as written. Its
            // width is still checked, because a file in the wrong format
            // usually announces itself right here, before any value fails.
            match self.checked_record() {
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
                Ok(None) => {
                    self.finished = true;
                    return None;
                }
                Ok(Some(_)) => self.header_done = true,
            }
        }
        let mut rows = Vec::new();
        while rows.len() < BATCH_ROWS {
            match self.checked_record() {
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
                Ok(None) => {
                    self.finished = true;
                    break;
                }
                Ok(Some(fields)) => rows.push(fields),
            }
        }
        if rows.is_empty() {
            return None;
        }
        let batch = self.batch(&rows);
        if batch.is_err() {
            self.finished = true;
        }
        Some(batch)
    }
}
