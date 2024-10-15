use alloy_primitives::{map::HashMap, B256, U256};
use reth_primitives::{parlia::Snapshot, Request};
use revm::db::BundleState;

/// A helper type for ethereum block inputs that consists of a block and the total difficulty.
#[derive(Debug)]
pub struct BlockExecutionInput<'a, Block, Header> {
    /// The block to execute.
    pub block: &'a Block,
    /// The total difficulty of the block.
    pub total_difficulty: U256,
    /// The headers of the block's ancestor
    pub ancestor_headers: Option<&'a HashMap<B256, Header>>,
}

impl<'a, Block, Header> BlockExecutionInput<'a, Block, Header> {
    /// Creates a new input.
    pub const fn new(
        block: &'a Block,
        total_difficulty: U256,
        ancestor_headers: Option<&'a HashMap<B256, Header>>,
    ) -> Self {
        Self { block, total_difficulty, ancestor_headers }
    }
}

impl<'a, Block, Header> From<(&'a Block, U256, Option<&'a HashMap<B256, Header>>)>
    for BlockExecutionInput<'a, Block, Header>
{
    fn from(
        (block, total_difficulty, ancestor_headers): (
            &'a Block,
            U256,
            Option<&'a HashMap<B256, Header>>,
        ),
    ) -> Self {
        Self::new(block, total_difficulty, ancestor_headers)
    }
}

/// The output of an ethereum block.
///
/// Contains the state changes, transaction receipts, and total gas used in the block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockExecutionOutput<T> {
    /// The changed state of the block after execution.
    pub state: BundleState,
    /// All the receipts of the transactions in the block.
    pub receipts: Vec<T>,
    /// All the EIP-7685 requests of the transactions in the block.
    pub requests: Vec<Request>,
    /// The total gas used by the block.
    pub gas_used: u64,

    /// Parlia snapshot.
    pub snapshot: Option<Snapshot>,
}
