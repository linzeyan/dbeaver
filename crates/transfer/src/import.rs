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

use crate::moving::Step;
use crate::{DelimitedReader, Format, TargetWriter, delimited};
use arrow::array::RecordBatch;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::error::ArrowError;
use dbconn::{Browse, DbError, DbResult, Driver};
use dbsql::Dialect;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Which column of the table each column of the file feeds.
///
/// One entry per column of the file, in the file's own order: the name of the
/// table column it goes into, or `None` for a column nobody asked to read. The
/// file's order rather than the table's because that is the order the file can
/// be shown in — a mapping is drawn beside the header it describes, and a table
/// column nothing feeds is simply absent from this list.
pub type ColumnMapping = Vec<Option<String>>;

/// What the file calls its own columns.
///
/// Read before an import is started, so that a mapping can be chosen against
/// something real rather than typed from memory. Cheap by construction: the
/// header of a delimited file is one record, JSON Lines is inferred from the
/// first line, and Parquet keeps its schema in the footer.
///
/// An empty answer is possible and is not an error — an empty file has no
/// columns to name. What the caller does with that is show a file with nothing
/// in it, which is better than an error message about a file that is merely
/// empty.
pub fn file_columns(path: &Path, format: Format) -> DbResult<Vec<String>> {
    let file = File::open(path).map_err(|e| DbError::new(e.to_string()))?;
    let names = match format {
        Format::Csv => delimited::head(file, b',', 0).map_err(arrow_error)?.0,
        Format::Tsv => delimited::head(file, b'\t', 0).map_err(arrow_error)?.0,
        // Inferred from one line rather than from the whole file: what is wanted
        // here is the keys, and a second line that carries a key the first did
        // not is a file this reader could not have imported anyway — the schema
        // it reads with is fixed before the first batch.
        Format::JsonLines => {
            let (schema, _) =
                arrow::json::reader::infer_json_schema(std::io::BufReader::new(file), Some(1))
                    .map_err(arrow_error)?;
            schema.fields().iter().map(|f| f.name().clone()).collect()
        }
        Format::Parquet => {
            parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
                .map_err(|e| DbError::new(e.to_string()))?
                .schema()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect()
        }
    };
    Ok(names)
}

fn arrow_error(e: ArrowError) -> DbError {
    DbError::new(e.to_string())
}

/// An import somebody can watch and stop.
///
/// `import` above runs to completion and reports one number at the end, which
/// is everything a test needs and nothing a person watching a two-gigabyte CSV
/// needs: no count until it is over, and no way to change their mind. This is
/// the same work, one batch per call, with the count readable between calls.
///
/// `Step` is the transfer's, deliberately. The two operations answer the same
/// three things — a batch went, the source is spent, somebody stopped it — and
/// the FFI already turns those into 1/0/-2 for one of them.
pub struct Import {
    target: Arc<dyn Driver>,
    /// Opened at `open` and not at the first step, so a file that is not there,
    /// or a table that is not, is refused before anything on screen has claimed
    /// to be reading it.
    batches: Batches,
    writer: TargetWriter,
    /// Which of the batch's columns go to the server, in the order the INSERT
    /// names them. `None` where every column goes, unchanged — an unmapped
    /// import reads the file into the table's own columns by position, which is
    /// what this did before there was anything to map.
    projection: Option<Vec<usize>>,
    loaded: u64,
    asked: Arc<AtomicBool>,
}

impl Import {
    /// Opens `path`, asks `table` what its columns are, and stops there.
    ///
    /// Everything that can fail before a row moves fails here: a missing file, a
    /// format nothing reads, a table that is not on the target. What is left for
    /// `step` is the work that takes time.
    pub async fn open(
        path: &Path,
        format: Format,
        target: Arc<dyn Driver>,
        dialect: &'static Dialect,
        table: String,
        mapping: Option<ColumnMapping>,
    ) -> DbResult<Self> {
        let columns = table_schema(target.as_ref(), dialect, &table).await?;
        let plan = match mapping {
            Some(mapping) => Some(Plan::of(path, format, &columns, &mapping)?),
            None => None,
        };
        // The file is read against the schema on the left and written against
        // the one on the right, and unmapped they are the same schema — which is
        // the whole of what "read it into the table by position" means.
        let (read, insert) = match &plan {
            Some(plan) => (Arc::clone(&plan.read), Arc::clone(&plan.insert)),
            None => (Arc::clone(&columns), Arc::clone(&columns)),
        };
        let file = File::open(path).map_err(|e| DbError::new(e.to_string()))?;
        let batches = reader(file, format, read)?;
        let writer = TargetWriter::new(dialect, table, &insert);
        Ok(Self {
            target,
            batches,
            writer,
            projection: plan.map(|p| p.projection),
            loaded: 0,
            asked: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The handle for whoever draws the Stop button.
    pub fn stopper(&self) -> ImportStopper {
        ImportStopper {
            asked: self.asked.clone(),
            target: self.target.clone(),
        }
    }

    /// How many rows are on the target, as of the last completed step.
    pub fn loaded(&self) -> u64 {
        self.loaded
    }

    /// Reads one batch and sends it.
    ///
    /// Checked for a stop twice, as the transfer's step is, and for one of the
    /// same two reasons: the write is a wait long enough to be worth
    /// interrupting. The read is not — it is a file — but a stop pressed during
    /// one must still not be answered by sending the batch it was carrying.
    pub async fn step(&mut self) -> DbResult<Step> {
        if self.asked.load(Ordering::SeqCst) {
            return Ok(Step::Stopped(self.loaded));
        }
        let Some(batch) = self.batches.next() else {
            return Ok(Step::Done(self.loaded));
        };
        let batch = batch.map_err(|e| DbError::new(e.to_string()))?;
        // Narrowed here rather than in the reader, because a skipped column
        // still has to be parsed: a delimited record is found by counting
        // delimiters, and a column nobody wants is still one the next one is
        // measured from.
        let batch = match &self.projection {
            Some(projection) => batch.project(projection).map_err(arrow_error)?,
            None => batch,
        };
        if self.asked.load(Ordering::SeqCst) {
            return Ok(Step::Stopped(self.loaded));
        }
        self.loaded += self.writer.write(self.target.as_ref(), &batch).await?;
        Ok(Step::Moved(self.loaded))
    }
}

/// What a mapping works out to: what to read the file as, what to send, and
/// which columns of the first make up the second.
///
/// Three things rather than one because they answer at three different moments —
/// the reader is built from the first, the INSERT's column list from the second,
/// and every batch is narrowed by the third — and working them out together is
/// what keeps them describing the same mapping.
struct Plan {
    read: SchemaRef,
    insert: SchemaRef,
    projection: Vec<usize>,
}

impl Plan {
    fn of(
        path: &Path,
        format: Format,
        columns: &Schema,
        mapping: &ColumnMapping,
    ) -> DbResult<Self> {
        let names = file_columns(path, format)?;
        if names.len() != mapping.len() {
            return Err(DbError::new(format!(
                "the mapping covers {} columns and the file has {}",
                mapping.len(),
                names.len()
            )));
        }

        let mut read = Vec::with_capacity(names.len());
        let mut insert = Vec::new();
        let mut projection = Vec::new();
        for (index, (name, target)) in names.iter().zip(mapping).enumerate() {
            let Some(target) = target else {
                // Named by the file and typed as text: nothing is done with it,
                // and text is the one type every field parses as, so a column
                // being skipped cannot fail the import on its way past.
                read.push(Field::new(name, DataType::Utf8, true));
                continue;
            };
            let column = columns.fields().iter().find(|f| f.name() == target);
            let Some(column) = column else {
                return Err(DbError::new(format!(
                    "the table has no column called {target:?}"
                )));
            };
            // Read under the file's name and the table's type. The name is what
            // JSON Lines matches on and what a delimited file ignores; the type
            // is what every format is parsed to.
            read.push(Field::new(name, column.data_type().clone(), true));
            // Nullable whatever the table says, because what this schema is used
            // for is the INSERT's column list — see `Insert::new`. A NOT NULL the
            // file has nothing for is the server's to refuse, with the column
            // name in the message.
            insert.push(column.as_ref().clone().with_nullable(true));
            projection.push(index);
        }

        if projection.is_empty() {
            return Err(DbError::new(
                "every column of the file is skipped, so there is nothing to read",
            ));
        }
        Ok(Self {
            read: Arc::new(Schema::new(read)),
            insert: Arc::new(Schema::new(insert)),
            projection,
        })
    }
}

/// Stops an import, from a thread that is not the one running it.
///
/// `Stopper` next door has a source half as well as a target half; this has only
/// the target's, because the source here is a file. A read already in flight
/// ends in microseconds, there is no server on the other end of it to tell, and
/// the descriptor is closed by dropping the import. What is worth interrupting
/// is the INSERT: ten thousand rows into a table with an index is a wait, and
/// `Driver::cancel` travels on a connection of its own to reach it.
///
/// Its own object rather than a method on `Import` for the reason `Stopper` is
/// one: it is used at exactly the moment the import is borrowed by a step.
#[derive(Clone)]
pub struct ImportStopper {
    asked: Arc<AtomicBool>,
    target: Arc<dyn Driver>,
}

impl ImportStopper {
    /// Asks the write to stop, and reports whether the request was delivered.
    ///
    /// Delivered is not stopped, as everywhere else here. What is promised is
    /// that the next `Import::step` sends nothing. The flag is set first and
    /// unconditionally: a cancel the target refuses is still somebody having
    /// pressed Stop, and the rows already written stay written — an import is
    /// not a transaction.
    pub async fn stop(&self) -> DbResult<()> {
        self.asked.store(true, Ordering::SeqCst);
        self.target.cancel().await
    }

    pub fn was_asked(&self) -> bool {
        self.asked.load(Ordering::SeqCst)
    }
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
