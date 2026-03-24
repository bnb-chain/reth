use crate::{
    evm::CustomTxEnv,
    primitives::{CustomHeader, CustomTransaction},
};
use alloy_consensus::error::ValueError;
use alloy_evm::EvmEnv;
use alloy_network::TxSigner;
use alloy_primitives::{Sealed, U256};
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::{OpTransactionReceipt, OpTransactionRequest};
use reth_op::rpc::RpcTypes;
use reth_primitives_traits::SealedHeader;
use reth_rpc_api::eth::{
    EthTxEnvError, SignTxRequestError, SignableTxRequest, TryIntoSimTx, TryIntoTxEnv,
};
use reth_rpc_convert::transaction::FromConsensusHeader;
use revm::context::BlockEnv;

#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct CustomRpcTypes;

impl RpcTypes for CustomRpcTypes {
    type Header = alloy_rpc_types_eth::Header<CustomHeader>;
    type Receipt = OpTransactionReceipt;
    type TransactionRequest = OpTransactionRequest;
    type TransactionResponse = op_alloy_rpc_types::Transaction<CustomTransaction>;
}

impl FromConsensusHeader<CustomHeader> for alloy_rpc_types_eth::Header<CustomHeader> {
    fn from_consensus_header(
        header: SealedHeader<CustomHeader>,
        block_size: usize,
        td: Option<U256>,
    ) -> Self {
        let header_hash = header.hash();
        let consensus_header = header.into_header();
        let sealed = Sealed::new_unchecked(consensus_header, header_hash);
        alloy_rpc_types_eth::Header::from_consensus(sealed, td, Some(U256::from(block_size)))
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
        evm_env: &EvmEnv<Spec, BlockEnv>,
    ) -> Result<CustomTxEnv, Self::Err> {
        Ok(CustomTxEnv::Op(self.try_into_tx_env(evm_env)?))
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
