use crate::{
    evm::CustomTxEnv,
    primitives::{CustomHeader, CustomTransaction},
};
use alloy_consensus::error::ValueError;
use alloy_network::TxSigner;
use alloy_primitives::U256;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::{OpTransactionReceipt, OpTransactionRequest};
use reth_op::rpc::RpcTypes;
use reth_primitives_traits::SealedHeader;
use reth_rpc_api::eth::{
    transaction::TryIntoTxEnv, EthTxEnvError, SignTxRequestError, SignableTxRequest, TryIntoSimTx,
};
use reth_rpc_convert::{transaction::FromConsensusHeader, CustomRpcHeader};
use revm::context::{BlockEnv, CfgEnv};

#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct CustomRpcTypes;

impl RpcTypes for CustomRpcTypes {
    type Header = CustomRpcHeader<CustomHeader>;
    type Receipt = OpTransactionReceipt;
    type TransactionRequest = OpTransactionRequest;
    type TransactionResponse = op_alloy_rpc_types::Transaction<CustomTransaction>;
}

impl FromConsensusHeader<CustomHeader> for CustomRpcHeader<CustomHeader> {
    fn from_consensus_header(
        header: SealedHeader<CustomHeader>,
        block_size: usize,
        td: Option<U256>,
    ) -> Self {
        let header_hash = header.hash();
        let consensus_header = header.into_header();
        let milli_timestamp = Some(U256::from(
            reth_rpc_convert::custom_header::calculate_millisecond_timestamp(&consensus_header),
        ));

        Self {
            hash: header_hash,
            inner: consensus_header,
            total_difficulty: td,
            size: Some(U256::from(block_size)),
            milli_timestamp,
        }
    }
}

impl TryIntoSimTx<CustomTransaction> for OpTransactionRequest {
    fn try_into_sim_tx(self) -> Result<CustomTransaction, ValueError<Self>> {
        Ok(CustomTransaction::Op(self.try_into_sim_tx()?))
    }
}

impl TryIntoTxEnv<CustomTxEnv> for OpTransactionRequest {
    type Err = EthTxEnvError;

    fn try_into_tx_env<Spec>(
        self,
        cfg_env: &CfgEnv<Spec>,
        block_env: &BlockEnv,
    ) -> Result<CustomTxEnv, Self::Err> {
        Ok(CustomTxEnv::Op(self.try_into_tx_env(cfg_env, block_env)?))
    }
}

impl SignableTxRequest<CustomTransaction> for OpTransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl TxSigner<alloy_primitives::Signature> + Send,
    ) -> Result<CustomTransaction, SignTxRequestError> {
        Ok(CustomTransaction::Op(
            SignableTxRequest::<OpTxEnvelope>::try_build_and_sign(self, signer).await?,
        ))
    }
}
