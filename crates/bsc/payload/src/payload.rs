//! Payload related types

//! Bsc builder support

use std::convert::Infallible;

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV3, ExecutionPayloadEnvelopeV4,
    ExecutionPayloadV1, PayloadAttributes, PayloadId,
};
use reth_chain_state::ExecutedBlock;
use reth_payload_builder::EthPayloadBuilderAttributes;
use reth_payload_primitives::{BuiltPayload, PayloadBuilderAttributes};
use reth_primitives::{BlobTransactionSidecar, SealedBlock, Withdrawals};
use reth_rpc_types_compat::engine::payload::{
    block_to_payload_v1, block_to_payload_v3, block_to_payload_v4,
    convert_block_to_payload_field_v2,
};

/// Bsc Payload Builder Attributes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BscPayloadBuilderAttributes {
    /// Inner ethereum payload builder attributes
    pub payload_attributes: EthPayloadBuilderAttributes,
}

impl PayloadBuilderAttributes for BscPayloadBuilderAttributes {
    type RpcPayloadAttributes = PayloadAttributes;
    type Error = Infallible;

    /// Creates a new payload builder for the given parent block and the attributes.
    ///
    /// Derives the unique [`PayloadId`] for the given parent and attributes
    fn try_new(parent: B256, attributes: PayloadAttributes) -> Result<Self, Infallible> {
        Ok(Self { payload_attributes: EthPayloadBuilderAttributes::try_new(parent, attributes)? })
    }

    fn payload_id(&self) -> PayloadId {
        self.payload_attributes.id
    }

    fn parent(&self) -> B256 {
        self.payload_attributes.parent
    }

    fn timestamp(&self) -> u64 {
        self.payload_attributes.timestamp
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.payload_attributes.parent_beacon_block_root
    }

    fn suggested_fee_recipient(&self) -> Address {
        self.payload_attributes.suggested_fee_recipient
    }

    fn prev_randao(&self) -> B256 {
        self.payload_attributes.prev_randao
    }

    fn withdrawals(&self) -> &Withdrawals {
        &self.payload_attributes.withdrawals
    }
}

/// Contains the built payload.
#[derive(Debug, Clone)]
pub struct BscBuiltPayload {
    /// Identifier of the payload
    pub(crate) id: PayloadId,
    /// The built block
    pub(crate) block: SealedBlock,
    /// Block execution data for the payload, if any.
    pub(crate) executed_block: Option<ExecutedBlock>,
    /// The fees of the block
    pub(crate) fees: U256,
    /// The blobs, proofs, and commitments in the block. If the block is pre-cancun, this will be
    /// empty.
    pub(crate) sidecars: Vec<BlobTransactionSidecar>,
}

// === impl BuiltPayload ===

impl BscBuiltPayload {
    /// Initializes the payload with the given initial block.
    pub const fn new(
        id: PayloadId,
        block: SealedBlock,
        fees: U256,
        executed_block: Option<ExecutedBlock>,
    ) -> Self {
        Self { id, block, executed_block, fees, sidecars: Vec::new() }
    }

    /// Returns the identifier of the payload.
    pub const fn id(&self) -> PayloadId {
        self.id
    }

    /// Returns the built block(sealed)
    pub const fn block(&self) -> &SealedBlock {
        &self.block
    }

    /// Fees of the block
    pub const fn fees(&self) -> U256 {
        self.fees
    }

    /// Adds sidecars to the payload.
    pub fn extend_sidecars(&mut self, sidecars: Vec<BlobTransactionSidecar>) {
        self.sidecars.extend(sidecars)
    }
}

impl BuiltPayload for BscBuiltPayload {
    fn block(&self) -> &SealedBlock {
        &self.block
    }

    fn fees(&self) -> U256 {
        self.fees
    }

    fn executed_block(&self) -> Option<ExecutedBlock> {
        self.executed_block.clone()
    }
}

impl BuiltPayload for &BscBuiltPayload {
    fn block(&self) -> &SealedBlock {
        (**self).block()
    }

    fn fees(&self) -> U256 {
        (**self).fees()
    }

    fn executed_block(&self) -> Option<ExecutedBlock> {
        self.executed_block.clone()
    }
}

// V1 engine_getPayloadV1 response
impl From<BscBuiltPayload> for ExecutionPayloadV1 {
    fn from(value: BscBuiltPayload) -> Self {
        block_to_payload_v1(value.block)
    }
}

// V2 engine_getPayloadV2 response
impl From<BscBuiltPayload> for ExecutionPayloadEnvelopeV2 {
    fn from(value: BscBuiltPayload) -> Self {
        let BscBuiltPayload { block, fees, .. } = value;

        Self { block_value: fees, execution_payload: convert_block_to_payload_field_v2(block) }
    }
}

impl From<BscBuiltPayload> for ExecutionPayloadEnvelopeV3 {
    fn from(value: BscBuiltPayload) -> Self {
        let BscBuiltPayload { block, fees, sidecars, .. } = value;

        Self {
            execution_payload: block_to_payload_v3(block),
            block_value: fees,
            // From the engine API spec:
            //
            // > Client software **MAY** use any heuristics to decide whether to set
            // `shouldOverrideBuilder` flag or not. If client software does not implement any
            // heuristic this flag **SHOULD** be set to `false`.
            //
            // Spec:
            // <https://github.com/ethereum/execution-apis/blob/fe8e13c288c592ec154ce25c534e26cb7ce0530d/src/engine/cancun.md#specification-2>
            should_override_builder: false,
            blobs_bundle: sidecars.into_iter().map(Into::into).collect::<Vec<_>>().into(),
        }
    }
}

impl From<BscBuiltPayload> for ExecutionPayloadEnvelopeV4 {
    fn from(value: BscBuiltPayload) -> Self {
        let BscBuiltPayload { block, fees, sidecars, .. } = value;

        Self {
            execution_payload: block_to_payload_v4(block),
            block_value: fees,
            // From the engine API spec:
            //
            // > Client software **MAY** use any heuristics to decide whether to set
            // `shouldOverrideBuilder` flag or not. If client software does not implement any
            // heuristic this flag **SHOULD** be set to `false`.
            //
            // Spec:
            // <https://github.com/ethereum/execution-apis/blob/fe8e13c288c592ec154ce25c534e26cb7ce0530d/src/engine/cancun.md#specification-2>
            should_override_builder: false,
            blobs_bundle: sidecars.into_iter().map(Into::into).collect::<Vec<_>>().into(),
        }
    }
}
