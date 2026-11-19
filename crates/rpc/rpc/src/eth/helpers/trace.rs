//! Contains RPC handler implementations specific to tracing.

use reth_rpc_convert::RpcConvert;
use reth_rpc_eth_api::{
    helpers::{Call, Trace},
    FromEvmError, RpcNodeCore,
};
use reth_rpc_eth_types::EthApiError;

use crate::EthApi;

impl<N, Rpc> Trace for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    reth_evm::EvmEnvFor<<Self as RpcNodeCore>::Evm>: reth_rpc_eth_types::BlockOverridesExt,
    reth_evm::EvmEnvFor<N::Evm>: reth_rpc_eth_types::BlockOverridesExt,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    Self: Call,
{
}
