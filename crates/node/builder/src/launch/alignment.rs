//! Pure decision logic for aligning MDBX to TrieDB at startup.
//!
//! See `docs/superpowers/specs/2026-04-17-triedb-mdbx-startup-alignment-design.md`
//! for context. The I/O wrapper lives in `common.rs`.

use alloy_primitives::{BlockNumber, B256};

/// Hard cap on how many blocks we are willing to unwind MDBX during startup
/// alignment. Larger gaps almost certainly indicate a misconfiguration (wrong
/// pathdb directory, mixed chains) rather than a normal crash window, and must
/// be resolved by the operator.
pub(crate) const MAX_STARTUP_UNWIND_BLOCKS: u64 = 1024;

/// Outcome of evaluating the startup MDBX/TrieDB alignment state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AlignmentOutcome {
    /// `mdbx_tip == pathdb_block`. No work required.
    Aligned,
    /// `mdbx_tip > pathdb_block` and all safety checks passed. Caller must
    /// unwind MDBX (and static files + stage checkpoints) down to
    /// `pathdb_block`.
    NeedsUnwind {
        /// Block to unwind MDBX to (exclusive upper bound for removal).
        to: BlockNumber,
    },
    /// `mdbx_tip < pathdb_block`. Invariant violation; caller must fail hard.
    TriedbAhead {
        pathdb_block: BlockNumber,
        mdbx_tip: BlockNumber,
    },
    /// `mdbx_tip - pathdb_block > MAX_STARTUP_UNWIND_BLOCKS`. Caller must fail
    /// hard and tell the operator to investigate.
    ExceedsLimit {
        mdbx_tip: BlockNumber,
        pathdb_block: BlockNumber,
        gap: u64,
        limit: u64,
    },
    /// `pathdb_root != mdbx_header.state_root()` at `pathdb_block`. Caller
    /// must fail hard; data integrity problem.
    RootMismatch {
        block: BlockNumber,
        triedb_root: B256,
        mdbx_root: B256,
    },
}

/// Evaluate alignment purely from its inputs. No I/O.
///
/// `mdbx_root_at_pathdb_block` is `None` only when MDBX does not have a header
/// at `pathdb_block` (unexpected — caller should convert to a `HeaderNotFound`
/// before invoking this function). If it is provided but does not match
/// `pathdb_root`, the returned outcome is `RootMismatch`.
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
    fn mdbx_behind_returns_triedb_ahead() {
        let r = root(2);
        assert_eq!(
            decide_startup_alignment(100, r, 95, r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::TriedbAhead { pathdb_block: 100, mdbx_tip: 95 }
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
    fn gap_at_exactly_limit_is_allowed() {
        let r = root(4);
        let tip = MAX_STARTUP_UNWIND_BLOCKS;
        assert_eq!(
            decide_startup_alignment(0, r, tip, r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::NeedsUnwind { to: 0 }
        );
    }

    #[test]
    fn gap_exceeds_limit_returns_exceeds_limit() {
        let r = root(5);
        let tip = MAX_STARTUP_UNWIND_BLOCKS + 1;
        assert_eq!(
            decide_startup_alignment(0, r, tip, r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::ExceedsLimit {
                mdbx_tip: tip,
                pathdb_block: 0,
                gap: tip,
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
            AlignmentOutcome::RootMismatch {
                block: 100,
                triedb_root: triedb_r,
                mdbx_root: mdbx_r,
            }
        );
    }

    #[test]
    fn exceeds_limit_takes_precedence_over_root_mismatch() {
        // If the gap is absurdly large we refuse to act, regardless of whether
        // the pathdb root looks corrupt — the operator should investigate both.
        let triedb_r = root(8);
        let mdbx_r = root(9);
        let tip = MAX_STARTUP_UNWIND_BLOCKS + 1;
        assert_eq!(
            decide_startup_alignment(0, triedb_r, tip, mdbx_r, MAX_STARTUP_UNWIND_BLOCKS),
            AlignmentOutcome::ExceedsLimit {
                mdbx_tip: tip,
                pathdb_block: 0,
                gap: tip,
                limit: MAX_STARTUP_UNWIND_BLOCKS,
            }
        );
    }
}
