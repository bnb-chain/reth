//! Implement BSC upgrade message which is required during handshake with other BSC clients, e.g.,
//! geth.

use alloy_rlp::{RlpDecodable, RlpEncodable};
use reth_codecs_derive::derive_arbitrary;
use serde::{Deserialize, Serialize};

/// UpdateStatus packet introduced in BSC to notify peers whether to broadcast transaction or not.
/// It is used during the p2p handshake.
#[derive_arbitrary(rlp)]
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct UpgradeStatus {
    /// Extension for support customized features for BSC.
    pub extension: UpgradeStatusExtension,
}

/// The extension to define whether to enable or disable the flag.
/// This flag currently is ignored, and will be supported later.
#[derive_arbitrary(rlp)]
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct UpgradeStatusExtension {
    // TODO: support disable_peer_tx_broadcast flag
    /// To notify a peer to disable the broadcast of transactions or not.
    pub disable_peer_tx_broadcast: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rlp::Encodable;
    use reth_primitives::hex;

    #[test]
    fn test_encode_upgrade_status() {
        let extension = UpgradeStatusExtension { disable_peer_tx_broadcast: true };
        let mut buffer = Vec::<u8>::new();
        extension.encode(&mut buffer);
        assert_eq!("c101", hex::encode(buffer.clone()));

        let upgrade_status = UpgradeStatus { extension };
        let mut buffer = Vec::<u8>::new();
        upgrade_status.encode(&mut buffer);
        assert_eq!("c2c101", hex::encode(buffer.clone()));
    }
}
