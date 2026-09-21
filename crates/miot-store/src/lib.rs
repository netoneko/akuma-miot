//! The block log, its compaction points, and the one rule for disagreement.
//!
//! # What it stores
//!
//! Blocks by height, and **state only at compaction points**. Both are opaque
//! bytes — this crate has no idea what a block contains, which is why it needs
//! neither FRAME nor SCALE and tests in milliseconds.
//!
//! # Compaction is the recovery boundary
//!
//! State is *not* snapshotted per block. It is written at compaction, and
//! nowhere else. That makes the set of places a cat can rewind to small,
//! known, and agreed — which is the point.
//!
//! This is the same boundary meow already had. Its compaction marker was where
//! a cold agent stopped paging history (`LITTER_STATE_MACHINE.md`); here it is
//! also where a diverged cat rewinds to. One concept, two uses, and the second
//! falls out of the first.
//!
//! # Conflict resolution: the leader wins, back to the last compaction
//!
//! No fork-choice rule, no longest-chain, no voting. **The leader's chain is
//! canonical by definition.** A cat that finds its log diverging rewinds to the
//! **latest compaction at or below the fork point** — or to genesis if there
//! is none — discards everything above it, and replays the leader's blocks
//! from there.
//!
//! The rewind is deliberately *coarser* than the divergence. Rewinding to the
//! exact fork point would need a state snapshot at every height; rewinding to a
//! compaction needs one per epoch, and the cost is replaying a few blocks that
//! were never in dispute. That trade is right at this scale and it keeps the
//! recovery target to a handful of well-known heights instead of all of them.
//!
//! Right for a litter, badly wrong for a public chain — the difference is the
//! trust model. This is one operator's swarm in one trust domain
//! (`LITTER_STATE_MACHINE.md`: *"cooperative agents that already share one
//! trust domain"*), so the machinery that exists to stop a leader lying buys
//! nothing.
//!
//! **A cat can lose work this way**, and that is accepted rather than
//! regretted. It is the trade the lease already makes: the protocol prefers the
//! litter making progress over preserving one member's contribution. An
//! extrinsic in a discarded block is gone; if the cat still cares, it submits
//! it again.

use parity_db::{Db, Options};
use std::path::Path;

/// Blocks, by height.
const COL_BLOCK: u8 = 0;
/// State, by height — written ONLY at compaction points.
const COL_CHECKPOINT: u8 = 1;
/// Singletons.
const COL_META: u8 = 2;
const N_COLS: u8 = 3;

const KEY_HEAD: &[u8] = b"head";
const KEY_LAST_CP: &[u8] = b"last_checkpoint";

#[derive(Debug)]
pub enum Error {
    Db(parity_db::Error),
    /// The log is append-only and contiguous; a gap makes replay meaningless.
    NotContiguous { expected: u64, got: u64 },
    /// A compaction above the head, or below one we already have.
    BadCheckpoint { head: u64, last: u64, got: u64 },
}

impl From<parity_db::Error> for Error {
    fn from(e: parity_db::Error) -> Self {
        Error::Db(e)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Db(e) => write!(f, "parity-db: {e}"),
            Error::NotContiguous { expected, got } => {
                write!(f, "block {got} is not contiguous (expected {expected})")
            }
            Error::BadCheckpoint { head, last, got } => {
                write!(f, "checkpoint {got} invalid (head {head}, last checkpoint {last})")
            }
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

fn key(n: u64) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

fn be(v: Vec<u8>) -> Option<u64> {
    v.try_into().ok().map(u64::from_be_bytes)
}

/// Where a rewind landed, and what to do from there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewind {
    /// The compaction height we returned to. 0 means genesis.
    pub height: u64,
    /// The state at that compaction. `None` at genesis — start from nothing.
    pub state: Option<Vec<u8>>,
    /// How many of our own blocks were discarded.
    pub dropped: u64,
}

pub struct Store {
    db: Db,
    head: u64,
    last_checkpoint: u64,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Db::open_or_create(&Options::with_columns(path.as_ref(), N_COLS))?;
        let head = db.get(COL_META, KEY_HEAD)?.and_then(be).unwrap_or(0);
        let last_checkpoint = db.get(COL_META, KEY_LAST_CP)?.and_then(be).unwrap_or(0);
        Ok(Store { db, head, last_checkpoint })
    }

    /// Highest block stored. 0 means empty — heights start at 1.
    pub fn head(&self) -> u64 {
        self.head
    }

    /// Height of the latest compaction. 0 means none yet: genesis.
    pub fn last_checkpoint(&self) -> u64 {
        self.last_checkpoint
    }

    pub fn is_empty(&self) -> bool {
        self.head == 0
    }

    /// Append one block. State is not written here — see [`Store::compact`].
    pub fn append(&mut self, height: u64, block: &[u8]) -> Result<()> {
        if height != self.head + 1 {
            return Err(Error::NotContiguous { expected: self.head + 1, got: height });
        }
        self.db.commit(vec![
            (COL_BLOCK, key(height), Some(block.to_vec())),
            (COL_META, KEY_HEAD.to_vec(), Some(height.to_be_bytes().to_vec())),
        ])?;
        self.head = height;
        Ok(())
    }

    /// Record a compaction: the state as of `height` becomes a rewind target.
    ///
    /// Blocks below it are dropped in the same commit — after a compaction
    /// there is nothing to replay from below it, so keeping them would be
    /// keeping history nobody can use.
    pub fn compact(&mut self, height: u64, state: &[u8]) -> Result<u64> {
        if height > self.head || height <= self.last_checkpoint {
            return Err(Error::BadCheckpoint {
                head: self.head,
                last: self.last_checkpoint,
                got: height,
            });
        }
        let mut ops = vec![
            (COL_CHECKPOINT, key(height), Some(state.to_vec())),
            (COL_META, KEY_LAST_CP.to_vec(), Some(height.to_be_bytes().to_vec())),
        ];
        // The previous checkpoint's state, and every block up to this one, are
        // now unreachable: a rewind can only land here or later.
        if self.last_checkpoint > 0 {
            ops.push((COL_CHECKPOINT, key(self.last_checkpoint), None));
        }
        let mut pruned = 0;
        for h in (self.last_checkpoint + 1)..=height {
            ops.push((COL_BLOCK, key(h), None));
            pruned += 1;
        }
        self.db.commit(ops)?;
        self.last_checkpoint = height;
        Ok(pruned)
    }

    pub fn block(&self, height: u64) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(COL_BLOCK, &key(height))?)
    }

    /// The state at the latest compaction, if there is one.
    pub fn checkpoint_state(&self) -> Result<Option<Vec<u8>>> {
        if self.last_checkpoint == 0 {
            return Ok(None);
        }
        Ok(self.db.get(COL_CHECKPOINT, &key(self.last_checkpoint))?)
    }

    /// **Leader wins.** Rewind to the latest compaction at or below
    /// `fork_point`, or to genesis if there is none.
    ///
    /// Everything above the landing height is discarded — our own blocks, and
    /// any state above it. The caller adopts [`Rewind::state`] and replays the
    /// leader's blocks from `height + 1`.
    ///
    /// A fork *below* the last compaction lands at genesis: we cannot restore a
    /// state we no longer hold, so the honest answer is to rebuild from
    /// nothing rather than pretend. That is rare by construction — compaction
    /// is a point the litter has already agreed on.
    pub fn rewind_for_fork(&mut self, fork_point: u64) -> Result<Rewind> {
        let land = if fork_point >= self.last_checkpoint { self.last_checkpoint } else { 0 };
        let dropped = self.head.saturating_sub(land);

        let mut ops: Vec<(u8, Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        for h in (land + 1)..=self.head {
            ops.push((COL_BLOCK, key(h), None));
        }
        if land == 0 && self.last_checkpoint > 0 {
            ops.push((COL_CHECKPOINT, key(self.last_checkpoint), None));
            ops.push((COL_META, KEY_LAST_CP.to_vec(), Some(0u64.to_be_bytes().to_vec())));
        }
        ops.push((COL_META, KEY_HEAD.to_vec(), Some(land.to_be_bytes().to_vec())));
        self.db.commit(ops)?;

        let was_cp = self.last_checkpoint;
        self.head = land;
        if land == 0 {
            self.last_checkpoint = 0;
        }
        let state = if land == 0 {
            None
        } else {
            self.db.get(COL_CHECKPOINT, &key(was_cp))?
        };
        Ok(Rewind { height: land, state, dropped })
    }

    /// Where our log and the leader's diverge: the last height we agree on.
    ///
    /// `theirs` is the leader's blocks (or their hashes) from `from` upward.
    /// Comparison is on bytes, so the caller decides which.
    pub fn fork_point(&self, from: u64, theirs: &[Vec<u8>]) -> Result<u64> {
        let mut agreed = from.saturating_sub(1);
        for (i, t) in theirs.iter().enumerate() {
            let h = from + i as u64;
            match self.block(h)? {
                Some(ours) if &ours == t => agreed = h,
                _ => break,
            }
        }
        Ok(agreed)
    }
}

#[cfg(test)]
mod tests;
