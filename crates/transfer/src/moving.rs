//! A transfer somebody can watch and stop.
//!
//! `transfer` above runs to completion and reports one number at the end, which
//! is everything a test needs and nothing a person watching a million rows move
//! needs: no count until it is over, and no way to change their mind. This is
//! the same work, one batch per call, with the count readable between calls and
//! a stop that reaches both halves.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dbconn::{Cursor, CursorCancel, DbResult, Driver};
use dbsql::Dialect;

use crate::target::TargetWriter;

/// What one call moved, and whether there is more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// A batch went across. The number is the running total, not this batch:
    /// the total is what a progress line says and what a caller would otherwise
    /// have to add up itself, wrongly, the first time a retry is added.
    Moved(u64),
    /// The source is exhausted and everything it had is on the target.
    Done(u64),
    /// Somebody pressed Stop. The rows already sent are still there — a
    /// transfer is not a transaction, and pretending otherwise would need one
    /// the target may not have.
    Stopped(u64),
}

impl Step {
    pub fn rows(self) -> u64 {
        match self {
            Step::Moved(n) | Step::Done(n) | Step::Stopped(n) => n,
        }
    }
}

/// Stops a transfer, from a thread that is not the one running it.
///
/// Its own object rather than a method on `Transfer`, for the reason the FFI's
/// cursor keeps its canceller in a field of its own: this is used at exactly the
/// moment the transfer is borrowed — a step is in flight, or it would not be
/// worth stopping — so the two must not need the same thing.
///
/// **Both halves.** The flag stops the loop and the source cancel stops a fetch
/// that is waiting on the source, and neither of those reaches a write that is
/// already in flight on the target: an `INSERT` of ten thousand rows into a
/// table with an index is a wait of its own, and until now nothing could
/// interrupt it. `Driver::cancel` on the target is what closes that gap. It
/// travels on a connection of its own, which is what makes it safe to send while
/// the write is running.
#[derive(Clone)]
pub struct Stopper {
    asked: Arc<AtomicBool>,
    source: Arc<dyn CursorCancel>,
    target: Arc<dyn Driver>,
}

impl Stopper {
    /// Asks both ends to stop, and reports whether the requests were delivered.
    ///
    /// Delivered is not stopped, the same distinction `Driver::cancel` draws: a
    /// fetch that had already finished leaves nothing to interrupt. What is
    /// promised is that the next `Transfer::step` will not send anything.
    ///
    /// The flag is set first and unconditionally. A cancel that the source or
    /// the target refuses is still somebody having pressed Stop, and a transfer
    /// that carried on because the refusal was returned early would be the one
    /// failure this cannot afford.
    pub async fn stop(&self) -> DbResult<()> {
        self.asked.store(true, Ordering::SeqCst);
        let source = self.source.cancel().await;
        let target = self.target.cancel().await;
        source.and(target)
    }

    pub fn was_asked(&self) -> bool {
        self.asked.load(Ordering::SeqCst)
    }
}

/// A transfer in progress: the target, what has gone across, and where to stop.
///
/// The source is passed to each `step` rather than held here, because the caller
/// already owns it — the FFI holds a cursor it handed out — and a second owner
/// of a cursor is a second place it can be closed from.
pub struct Transfer {
    target: Arc<dyn Driver>,
    dialect: &'static Dialect,
    table: String,
    /// Built from the first batch's schema rather than the cursor's, for the
    /// reason `transfer` gives: a result with no rows sends nothing at all.
    writer: Option<TargetWriter>,
    moved: u64,
    asked: Arc<AtomicBool>,
}

impl Transfer {
    pub fn new(target: Arc<dyn Driver>, dialect: &'static Dialect, table: String) -> Self {
        Self {
            target,
            dialect,
            table,
            writer: None,
            moved: 0,
            asked: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The handle for whoever draws the Stop button.
    ///
    /// Takes the source's canceller here rather than at `new`, because that is
    /// where the caller has it: a cursor hands out its canceller, and the object
    /// that hands out a stop for the whole transfer should be the one that owns
    /// both ends of it.
    pub fn stopper(&self, source: Arc<dyn CursorCancel>) -> Stopper {
        Stopper {
            asked: self.asked.clone(),
            source,
            target: self.target.clone(),
        }
    }

    /// How many rows are on the target, as of the last completed step.
    pub fn moved(&self) -> u64 {
        self.moved
    }

    /// Fetches one batch and sends it.
    ///
    /// Checked for a stop twice, before each half, because each half is a wait:
    /// a Stop pressed while the fetch was in flight must not be answered by
    /// sending the batch that fetch was already carrying.
    pub async fn step(&mut self, source: &mut dyn Cursor) -> DbResult<Step> {
        if self.asked.load(Ordering::SeqCst) {
            return Ok(Step::Stopped(self.moved));
        }
        let Some(batch) = source.fetch().await? else {
            return Ok(Step::Done(self.moved));
        };
        if self.asked.load(Ordering::SeqCst) {
            return Ok(Step::Stopped(self.moved));
        }

        let writer = self.writer.get_or_insert_with(|| {
            TargetWriter::new(self.dialect, self.table.clone(), batch.schema_ref())
        });
        self.moved += writer.write(self.target.as_ref(), &batch).await?;
        Ok(Step::Moved(self.moved))
    }
}
