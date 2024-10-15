use once_cell::sync::Lazy;
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition};

/// Dev hardforks
pub static DEV_HARDFORKS: Lazy<ChainHardforks> = Lazy::new(|| {
    ChainHardforks::new(vec![
        (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)),
        (crate::BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)),
        (crate::BscHardfork::Niels.boxed(), ForkCondition::Block(0)),
        (crate::BscHardfork::MirrorSync.boxed(), ForkCondition::Block(1)),
        (crate::BscHardfork::Bruno.boxed(), ForkCondition::Block(1)),
        (crate::BscHardfork::Euler.boxed(), ForkCondition::Block(2)),
        (crate::BscHardfork::Nano.boxed(), ForkCondition::Block(3)),
        (crate::BscHardfork::Moran.boxed(), ForkCondition::Block(3)),
        (crate::BscHardfork::Gibbs.boxed(), ForkCondition::Block(4)),
        (crate::BscHardfork::Planck.boxed(), ForkCondition::Block(5)),
        (crate::BscHardfork::Luban.boxed(), ForkCondition::Block(6)),
        (crate::BscHardfork::Plato.boxed(), ForkCondition::Block(7)),
        (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(8)),
        (EthereumHardfork::London.boxed(), ForkCondition::Block(8)),
        (crate::BscHardfork::Hertz.boxed(), ForkCondition::Block(8)),
        (crate::BscHardfork::HertzFix.boxed(), ForkCondition::Block(8)),
        (EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(1722442622)),
        (crate::BscHardfork::Kepler.boxed(), ForkCondition::Timestamp(1722442622)),
        (crate::BscHardfork::Feynman.boxed(), ForkCondition::Timestamp(1722442622)),
        (crate::BscHardfork::FeynmanFix.boxed(), ForkCondition::Timestamp(1722442622)),
        (EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(1722442622)),
        (crate::BscHardfork::Haber.boxed(), ForkCondition::Timestamp(1722442622)),
        (crate::BscHardfork::HaberFix.boxed(), ForkCondition::Timestamp(1722442622)),
        (crate::BscHardfork::Bohr.boxed(), ForkCondition::Timestamp(1722442622)),
    ])
});
