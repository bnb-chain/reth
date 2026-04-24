//! Pure decision logic for aligning MDBX to `TrieDB` pathdb at startup.
//!
//! The I/O wrapper lives in `common.rs::align_mdbx_to_triedb_at_startup`. Keep
//! the decision function free of I/O so its behaviour can be exhaustively
//! covered by unit tests without a database fixture.

use alloy_primitives::{BlockNumber, B256};

/// Hard cap on how many blocks startup alignment is willing to unwind MDBX.
/// Larger gaps almost always mean misconfiguration (wrong pathdb directory,
/// mixed chains) and must be resolved by the operator, not by silent recovery.
pub(crate) const MAX_STARTUP_UNWIND_BLOCKS: u64 = 1024;

/// Outcome of evaluating startup alignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AlignmentOutcome {
    /// `mdbx_tip == pathdb_block`. No work required.
    Aligned,
    /// `mdbx_tip > pathdb_block` and all safety checks passed. Caller unwinds
    /// MDBX + static files + stage checkpoints down to `to`.
    NeedsUnwind { to: BlockNumber },
    /// `mdbx_tip < pathdb_block`. Invariant violation; caller fails hard.
    TriedbAhead { pathdb_block: BlockNumber, mdbx_tip: BlockNumber },
    /// `mdbx_tip - pathdb_block > max_unwind`. Caller fails hard.
    ExceedsLimit { mdbx_tip: BlockNumber, pathdb_block: BlockNumber, gap: u64, limit: u64 },
    /// `pathdb_root != mdbx_header.state_root()` at `pathdb_block`. Caller fails hard.
    RootMismatch { block: BlockNumber, triedb_root: B256, mdbx_root: B256 },
}

/// Evaluate alignment from inputs only; no I/O.
///
/// `mdbx_root_at_pathdb_block` is the MDBX header's state root at
/// `pathdb_block`. The caller need only fetch it when `mdbx_tip > pathdb_block`
/// (this function ignores the value on the equal / `<` branches).
///
/// `ExceedsLimit` takes precedence over `RootMismatch` — a very large gap
/// signals misconfiguration and must be investigated by the operator before
/// trusting either backend.
pub(crate) fn decide_startup_alignment(
    pathdb_block: BlockNumber,
    pathdb_root: B256,
    mdbx_tip: BlockNumber,
    mdbx_root_at_pathdb_block: B256,
    max_unwind: u64,
) -> AlignmentOutcome {
    use core::cmp::Ordering;

    match mdbx_tip.cmp(&pathdb_block) {
        Ordering::Equal => AlignmentOutcome::Aligned,
        Ordering::Less => AlignmentOutcome::TriedbAhead { pathdb_block, mdbx_tip },
        Ordering::Greater => {
            let gap = mdbx_tip - pathdb_block;
            if gap > max_unwind {
                return AlignmentOutcome::ExceedsLimit {
                    mdbx_tip,
                    pathdb_block,
                    gap,
                    limit: max_unwind,
                };
            }
            if pathdb_root != mdbx_root_at_pathdb_block {
                return AlignmentOutcome::RootMismatch {
                    block: pathdb_block,
                    triedb_root: pathdb_root,
                    mdbx_root: mdbx_root_at_pathdb_block,
                };
            }
            AlignmentOutcome::NeedsUnwind { to: pathdb_block }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    #[test]
    fn equal_tips_return_aligned() {
        let r = root(1);
        assert_eq!(
            decide_startup_alignment(100, r, 100, r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::Aligned
        );
    }

    #[test]
    fn gap_within_limit_and_root_matches_returns_needs_unwind() {
        let r = root(3);
        assert_eq!(
            decide_startup_alignment(100, r, 105, r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::NeedsUnwind { to: 100 }
        );
    }

    #[test]
    fn gap_exceeds_limit_returns_exceeds_limit() {
        let r = root(5);
        let pathdb = 100;
        let tip = pathdb + MAX_STARTUP_UNWIND_BLOCKS + 1;
        assert_eq!(
            decide_startup_alignment(pathdb, r, tip, r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::ExceedsLimit {
                mdbx_tip: tip,
                pathdb_block: pathdb,
                gap: MAX_STARTUP_UNWIND_BLOCKS + 1,
                limit: MAX_STARTUP_UNWIND_BLOCKS,
            }
        );
    }

    #[test]
    fn root_mismatch_takes_precedence_over_unwind() {
        let triedb_r = root(6);
        let mdbx_r = root(7);
        assert_eq!(
            decide_startup_alignment(100, triedb_r, 105, mdbx_r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::RootMismatch { block: 100, triedb_root: triedb_r, mdbx_root: mdbx_r }
        );
    }
}
