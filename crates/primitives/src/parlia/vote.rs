use alloy_primitives::{keccak256, wrap_fixed_bytes, BlockNumber, B256};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use bytes::Bytes;
#[cfg(any(test, feature = "reth-codec"))]
use reth_codecs::{impl_compact_for_wrapped_bytes, Compact};
use serde::{Deserialize, Serialize};

/// max attestation extra length
pub const MAX_ATTESTATION_EXTRA_LENGTH: usize = 256;
/// validators bit set in vote attestation
pub type ValidatorsBitSet = u64;

wrap_fixed_bytes!(
    /// VoteAddress represents the BLS public key of the validator.
    pub struct VoteAddress<48>;
);

wrap_fixed_bytes!(
    /// VoteSignature represents the BLS signature of the validator.
    pub struct VoteSignature<96>;
);

#[cfg(feature = "reth-codec")]
impl_compact_for_wrapped_bytes!(VoteAddress, VoteSignature);

/// `VoteData` represents the vote range that validator voted for fast finality.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Default, RlpEncodable, RlpDecodable, Serialize, Deserialize,
)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[cfg_attr(any(test, feature = "reth-codec"), derive(Compact))]
#[cfg_attr(any(test, feature = "reth-codec"), reth_codecs::add_arbitrary_tests(compact))]
pub struct VoteData {
    /// The source block number should be the latest justified block number.
    pub source_number: BlockNumber,
    /// The block hash of the source block.
    pub source_hash: B256,
    /// The target block number which validator wants to vote for.
    pub target_number: BlockNumber,
    /// The block hash of the target block.
    pub target_hash: B256,
}

impl VoteData {
    /// hash, get the hash of the rlp outcome of the `VoteData`
    pub fn hash(&self) -> B256 {
        keccak256(alloy_rlp::encode(self))
    }
}

/// `VoteEnvelope` a single vote from validator.
#[derive(Clone, Debug, PartialEq, Eq, RlpEncodable, RlpDecodable, Serialize, Deserialize)]
pub struct VoteEnvelope {
    /// The BLS public key of the validator.
    pub vote_address: VoteAddress,
    /// The BLS signature of the validator.
    pub signature: VoteSignature,
    /// The vote data for fast finality.
    pub data: VoteData,
}

impl VoteEnvelope {
    /// hash, get the hash of the rlp outcome of the `VoteEnvelope`
    pub fn hash(&self) -> B256 {
        keccak256(alloy_rlp::encode(self))
    }
}

/// `VoteAttestation` represents the votes of super majority validators.
#[derive(Clone, Debug, PartialEq, Eq, RlpEncodable, RlpDecodable, Serialize, Deserialize)]
pub struct VoteAttestation {
    /// The bitset marks the voted validators.
    pub vote_address_set: ValidatorsBitSet,
    /// The aggregated BLS signature of the voted validators' signatures.
    pub agg_signature: VoteSignature,
    /// The vote data for fast finality.
    pub data: VoteData,
    /// Reserved for future usage.
    pub extra: Bytes,
}
