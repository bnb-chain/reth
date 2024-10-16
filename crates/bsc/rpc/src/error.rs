//! RPC errors specific to BSC.

use reth_provider::ProviderError;
use reth_rpc_eth_api::AsEthApiError;
use reth_rpc_eth_types::EthApiError;
use reth_rpc_server_types::result::internal_rpc_err;

/// Bsc specific errors, that extend [`EthApiError`].
#[derive(Debug, thiserror::Error)]
pub enum BscEthApiError {
    #[error(transparent)]
    Eth(#[from] EthApiError),
    /// When load account from db failed
    #[error("load account failed")]
    LoadAccountFailed,
    /// Provider error
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

impl AsEthApiError for BscEthApiError {
    fn as_err(&self) -> Option<&EthApiError> {
        match self {
            Self::Eth(err) => Some(err),
            _ => None,
        }
    }
}

impl From<BscEthApiError> for jsonrpsee_types::error::ErrorObject<'static> {
    fn from(err: BscEthApiError) -> Self {
        match err {
            BscEthApiError::Eth(err) => err.into(),
            BscEthApiError::LoadAccountFailed => internal_rpc_err(err.to_string()),
            BscEthApiError::Provider(err) => err.into(),
        }
    }
}
