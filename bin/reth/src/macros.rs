//! Helper macros

/// Creates the block executor type based on the configured feature.
///
/// Note(mattsse): This is incredibly horrible and will be replaced
#[cfg(all(not(feature = "optimism"), not(feature = "bsc")))]
macro_rules! block_executor {
    ($chain_spec:expr) => {
        reth_node_ethereum::EthExecutorProvider::ethereum($chain_spec)
    };
}

#[cfg(feature = "optimism")]
macro_rules! block_executor {
    ($chain_spec:expr) => {
        reth_node_optimism::OpExecutorProvider::optimism($chain_spec)
    };
}

#[cfg(feature = "bsc")]
macro_rules! block_executor {
    ($chain_spec:expr) => {
        // In some cases provider is not available
        // And we don't really need a bsc executor provider
        reth_node_ethereum::EthExecutorProvider::ethereum($chain_spec)
    };
    ($chain_spec:expr, $provider:expr) => {
        reth_node_bsc::BscExecutorProvider::bsc($chain_spec, $provider)
    };
}

pub(crate) use block_executor;
