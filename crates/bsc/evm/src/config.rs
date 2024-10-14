use reth_bsc_forks::BscHardfork;
use reth_chainspec::ChainSpec;
use reth_ethereum_forks::{EthereumHardfork, Head};

/// Returns the spec id at the given timestamp.
///
/// Note: This is only intended to be used after the merge, when hardforks are activated by
/// timestamp.
pub fn revm_spec_by_timestamp_after_shanghai(
    chain_spec: &ChainSpec,
    timestamp: u64,
) -> revm_primitives::SpecId {
    if chain_spec.fork(BscHardfork::Bohr).active_at_timestamp(timestamp) {
        revm_primitives::BOHR
    } else if chain_spec.fork(BscHardfork::HaberFix).active_at_timestamp(timestamp) {
        revm_primitives::HABER_FIX
    } else if chain_spec.fork(BscHardfork::Haber).active_at_timestamp(timestamp) {
        revm_primitives::HABER
    } else if chain_spec.fork(BscHardfork::FeynmanFix).active_at_timestamp(timestamp) {
        revm_primitives::FEYNMAN_FIX
    } else if chain_spec.fork(BscHardfork::Feynman).active_at_timestamp(timestamp) {
        revm_primitives::FEYNMAN
    } else if chain_spec.fork(BscHardfork::Kepler).active_at_timestamp(timestamp) {
        revm_primitives::KEPLER
    } else {
        revm_primitives::SHANGHAI
    }
}

/// return `revm_spec` from spec configuration.
pub fn revm_spec(chain_spec: &ChainSpec, block: &Head) -> revm_primitives::SpecId {
    if chain_spec.fork(BscHardfork::Bohr).active_at_head(block) {
        revm_primitives::BOHR
    } else if chain_spec.fork(BscHardfork::HaberFix).active_at_head(block) {
        revm_primitives::HABER_FIX
    } else if chain_spec.fork(BscHardfork::Haber).active_at_head(block) {
        revm_primitives::HABER
    } else if chain_spec.fork(EthereumHardfork::Cancun).active_at_head(block) {
        revm_primitives::CANCUN
    } else if chain_spec.fork(BscHardfork::FeynmanFix).active_at_head(block) {
        revm_primitives::FEYNMAN_FIX
    } else if chain_spec.fork(BscHardfork::Feynman).active_at_head(block) {
        revm_primitives::FEYNMAN
    } else if chain_spec.fork(BscHardfork::Kepler).active_at_head(block) {
        revm_primitives::KEPLER
    } else if chain_spec.fork(EthereumHardfork::Shanghai).active_at_head(block) {
        revm_primitives::SHANGHAI
    } else if chain_spec.fork(BscHardfork::HertzFix).active_at_head(block) {
        revm_primitives::HERTZ_FIX
    } else if chain_spec.fork(BscHardfork::Hertz).active_at_head(block) {
        revm_primitives::HERTZ
    } else if chain_spec.fork(EthereumHardfork::London).active_at_head(block) {
        revm_primitives::LONDON
    } else if chain_spec.fork(EthereumHardfork::Berlin).active_at_head(block) {
        revm_primitives::BERLIN
    } else if chain_spec.fork(BscHardfork::Plato).active_at_head(block) {
        revm_primitives::PLATO
    } else if chain_spec.fork(BscHardfork::Luban).active_at_head(block) {
        revm_primitives::LUBAN
    } else if chain_spec.fork(BscHardfork::Planck).active_at_head(block) {
        revm_primitives::PLANCK
    } else if chain_spec.fork(BscHardfork::Gibbs).active_at_head(block) {
        // bsc mainnet and testnet have different order for Moran, Nano and Gibbs
        if chain_spec.fork(BscHardfork::Moran).active_at_head(block) {
            revm_primitives::MORAN
        } else if chain_spec.fork(BscHardfork::Nano).active_at_head(block) {
            revm_primitives::NANO
        } else {
            revm_primitives::EULER
        }
    } else if chain_spec.fork(BscHardfork::Moran).active_at_head(block) {
        revm_primitives::MORAN
    } else if chain_spec.fork(BscHardfork::Nano).active_at_head(block) {
        revm_primitives::NANO
    } else if chain_spec.fork(BscHardfork::Euler).active_at_head(block) {
        revm_primitives::EULER
    } else if chain_spec.fork(BscHardfork::Bruno).active_at_head(block) {
        revm_primitives::BRUNO
    } else if chain_spec.fork(BscHardfork::MirrorSync).active_at_head(block) {
        revm_primitives::MIRROR_SYNC
    } else if chain_spec.fork(BscHardfork::Niels).active_at_head(block) {
        revm_primitives::NIELS
    } else if chain_spec.fork(BscHardfork::Ramanujan).active_at_head(block) {
        revm_primitives::RAMANUJAN
    } else if chain_spec.fork(EthereumHardfork::MuirGlacier).active_at_head(block) {
        revm_primitives::MUIR_GLACIER
    } else if chain_spec.fork(EthereumHardfork::Istanbul).active_at_head(block) {
        revm_primitives::ISTANBUL
    } else if chain_spec.fork(EthereumHardfork::Petersburg).active_at_head(block) {
        revm_primitives::PETERSBURG
    } else if chain_spec.fork(EthereumHardfork::Constantinople).active_at_head(block) {
        revm_primitives::CONSTANTINOPLE
    } else if chain_spec.fork(EthereumHardfork::Byzantium).active_at_head(block) {
        revm_primitives::BYZANTIUM
    } else if chain_spec.fork(EthereumHardfork::Homestead).active_at_head(block) {
        revm_primitives::HOMESTEAD
    } else if chain_spec.fork(EthereumHardfork::Frontier).active_at_head(block) {
        revm_primitives::FRONTIER
    } else {
        panic!(
            "invalid hardfork chainspec: expected at least one hardfork, got {:?}",
            chain_spec.hardforks
        )
    }
}
