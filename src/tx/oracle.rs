use super::conflict_manager::ConflictChecker;
use core::ops::AddAssign;
use std::borrow::Cow;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use wmark::{Closer, WaterMark, WaterMarkError};

#[derive(Debug)]
pub(super) struct OracleInner<C> {
    next_txn_ts: u64,

    last_cleanup_ts: u64,

    /// Contains all committed writes (contains fingerprints
    /// of keys written and their latest commit counter).
    pub(super) committed_txns: Vec<CommittedTxn<C>>,
}

pub(super) enum CreateCommitTimestampResult<C> {
    Timestamp(u64),
    Conflict(Option<C>),
}

#[derive(Debug)]
pub enum Error {
    /// Watermark-related error
    WaterMark(WaterMarkError),
    /// Poisoned write serialization lock
    Poisoned,
}

impl std::error::Error for Error {}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl From<WaterMarkError> for Error {
    fn from(value: WaterMarkError) -> Self {
        Self::WaterMark(value)
    }
}

impl<T> From<PoisonError<T>> for Error {
    fn from(_value: PoisonError<T>) -> Self {
        Self::Poisoned
    }
}

#[derive(Debug)]
pub(super) struct Oracle<C> {
    // write_serialize_lock is for ensuring that transactions go to the write
    // channel in the same order as their commit timestamps.
    pub(super) write_serialize_lock: Mutex<()>,

    pub(super) inner: Mutex<OracleInner<C>>,

    /// Used by DB
    pub(super) read_mark: WaterMark,

    /// Used to block new transaction, so all previous commits are visible to a new read.
    pub(super) txn_mark: WaterMark,

    /// closer is used to stop watermarks.
    closer: Closer,
}

impl Oracle<ConflictChecker> {
    pub(super) fn new_commit_ts(
        &self,
        done_read: &mut bool,
        read_ts: u64,
        conflict_manager: ConflictChecker,
    ) -> Result<CreateCommitTimestampResult<ConflictChecker>, Error> {
        let ts = {
            let mut inner = self.inner.lock()?;

            for committed_txn in &inner.committed_txns {
                // If the committed_txn.ts is less than txn.read_ts that implies that the
                // committed_txn finished before the current transaction started.
                // We don't need to check for conflict in that case.
                // This change assumes linearizability. Lack of linearizability could
                // cause the read ts of a new txn to be lower than the commit ts of
                // a txn before it (@mrjn).
                if committed_txn.ts <= read_ts {
                    continue;
                }

                if conflict_manager.has_conflict(&committed_txn.conflict_manager) {
                    return Ok(CreateCommitTimestampResult::Conflict(Some(
                        conflict_manager,
                    )));
                }
            }

            let ts = {
                if !*done_read {
                    self.read_mark.done(read_ts)?;
                    *done_read = true;
                }

                self.cleanup_committed_transactions(true, &mut inner)?;

                // This is the general case, when user doesn't specify the read and commit ts.
                let ts = inner.next_txn_ts;
                inner.next_txn_ts += 1;
                self.txn_mark.begin(ts)?;
                ts
            };

            assert!(ts >= inner.last_cleanup_ts);

            // We should ensure that txns are not added to o.committedTxns slice when
            // conflict detection is disabled otherwise this slice would keep growing.
            inner.committed_txns.push(CommittedTxn {
                ts,
                conflict_manager,
            });
            ts
        };

        Ok(CreateCommitTimestampResult::Timestamp(ts))
    }

    fn cleanup_committed_transactions(
        &self,
        detect_conflicts: bool,
        inner: &mut MutexGuard<OracleInner<ConflictChecker>>,
    ) -> Result<(), Error> {
        if !detect_conflicts {
            // When detectConflicts is set to false, we do not store any
            // committedTxns and so there's nothing to clean up.
            return Ok(());
        }

        let max_read_ts = self.read_mark.done_until()?;

        assert!(max_read_ts >= inner.last_cleanup_ts);

        // do not run clean up if the max_read_ts (read timestamp of the
        // oldest transaction that is still in flight) has not increased
        if max_read_ts == inner.last_cleanup_ts {
            return Ok(());
        }

        inner.last_cleanup_ts = max_read_ts;

        inner.committed_txns.retain(|txn| txn.ts > max_read_ts);

        Ok(())
    }
}

impl<C> Oracle<C> {
    pub fn new(
        read_mark_name: Cow<'static, str>,
        txn_mark_name: Cow<'static, str>,
        next_txn_ts: u64,
    ) -> Self {
        let closer = Closer::new(2);
        let mut orc = Self {
            write_serialize_lock: Mutex::new(()),
            inner: Mutex::new(OracleInner {
                next_txn_ts,
                last_cleanup_ts: 0,
                committed_txns: Vec::new(),
            }),
            read_mark: WaterMark::new(read_mark_name),
            txn_mark: WaterMark::new(txn_mark_name),
            closer,
        };

        orc.read_mark.init(orc.closer.clone());
        orc.txn_mark.init(orc.closer.clone());
        orc
    }

    pub(super) fn read_ts(&self) -> Result<u64, Error> {
        let read_ts = {
            let read_ts = {
                let inner = self.inner.lock()?;
                inner.next_txn_ts - 1
            };
            self.read_mark.begin(read_ts)?;
            read_ts
        };

        // Wait for all txns which have no conflicts, have been assigned a commit
        // timestamp and are going through the write to value log and LSM tree
        // process. Not waiting here could mean that some txns which have been
        // committed would not be read.
        self.txn_mark.wait_for_mark(read_ts)?;
        Ok(read_ts)
    }

    pub(super) fn increment_next_ts(&self) -> Result<(), Error> {
        self.inner.lock()?.next_txn_ts.add_assign(1);
        Ok(())
    }

    // TODO: is this important?
    // pub(super) fn discard_at_or_below(&self) -> Result<u64, Error> {
    //     self.read_mark.done_until().map_err(Error::from)
    // }

    // TODO: is this important?
    // pub(super) fn done_read(&self, read_ts: u64) -> Result<(), Error> {
    //     self.read_mark.done(read_ts).map_err(Error::from)?;
    //     Ok(())
    // }

    pub(super) fn done_commit(&self, cts: u64) -> Result<(), Error> {
        self.txn_mark.done(cts).map_err(Error::from)?;
        Ok(())
    }

    fn stop(&self) {
        self.closer.signal_and_wait();
    }
}

impl<S> Drop for Oracle<S> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug)]
pub(super) struct CommittedTxn<C> {
    ts: u64,
    /// Keeps track of the entries written at timestamp ts.
    conflict_manager: C,
}
