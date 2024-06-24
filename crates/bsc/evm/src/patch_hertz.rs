use crate::execute::BscEvmExecutor;
use reth_errors::ProviderError;
use reth_evm::ConfigureEvm;
use reth_primitives::{address, b256, Address, Header, TransactionSigned, B256, U256};
use reth_revm::{
    db::{states::CacheAccount, AccountStatus::Destroyed},
    State,
};
use revm_primitives::db::Database;
use std::{collections::HashMap, str::FromStr};
use tracing::log::trace;

impl<EvmConfig> BscEvmExecutor<EvmConfig>
where
    EvmConfig: ConfigureEvm,
{
    pub(crate) fn patch_mainnet<DB>(
        &self,
        header: &Header,
        transaction: &TransactionSigned,
        state: &mut State<DB>,
    ) where
        DB: Database<Error = ProviderError>,
    {
        let patches = vec![
            // patch 1: BlockNum 33851236, txIndex 89(patch before tx 89)
            (
                b256!("022296e50021d7225b75f3873e7bc5a2bf6376a08079b4368f9dee81946d623b"),
                b256!("7eba4edc7c1806d6ee1691d43513838931de5c94f9da56ec865721b402f775b0"),
                address!("00000000001f8b68515EfB546542397d3293CCfd"),
                HashMap::from([
                    (
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x00000000000000000000000052db206170b430da8223651d28830e56ba3cdc04",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000002",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x000000000000000000000000bb45f138499734bf5c0948d490c65903676ea1de",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x65c95177950b486c2071bf2304da1427b9136564150fb97266ffb318b03a71cc",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x245e58a02bec784ccbdb9e022a84af83227a4125a22a5e68fcc596c7e436434e",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x1c4534c86090a60a9120f34c7b15254913c00bda3d4b276d6edb65c9f48a913f",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000004",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000019",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x8a35acfbc15ff81a39ae7d344fd709f28e8600b4aa8c65c6b64bfe7fe36bd1b4",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000000",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x8a35acfbc15ff81a39ae7d344fd709f28e8600b4aa8c65c6b64bfe7fe36bd1b5",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000000",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x8a35acfbc15ff81a39ae7d344fd709f28e8600b4aa8c65c6b64bfe7fe36bd1b6",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000000",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000005",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x00000000000000000000000000000000000000000000000000000000000fc248",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000006",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x00000000000000000000000000000000000000000000000000000000000fc132",
                        )
                        .unwrap(),
                    ),
                ]),
            ),
            // patch 2: BlockNum 33851236, txIndex 90(patch before tx 90)
            (
                b256!("022296e50021d7225b75f3873e7bc5a2bf6376a08079b4368f9dee81946d623b"),
                b256!("5217324f0711af744fe8e12d73f13fdb11805c8e29c0c095ac747b7e4563e935"),
                address!("00000000001f8b68515EfB546542397d3293CCfd"),
                HashMap::from([
                    (
                        U256::from_str(
                            "0xbcfc62ca570bdb58cf9828ac51ae8d7e063a1cc0fa1aee57691220a7cd78b1c8",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x30dce49ce1a4014301bf21aad0ee16893e4dcc4a4e4be8aa10e442dd13259837",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xc0582628d787ee16fe03c8e5b5f5644d3b81989686f8312280b7a1f733145525",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xfca5cf22ff2e8d58aece8e4370cce33cd0144d48d00f40a5841df4a42527694b",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xb189302b37865d2ae522a492ff1f61a5addc1db44acbdcc4b6814c312c815f46",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xfe1f1986775fc2ac905aeaecc7b1aa8b0d6722b852c90e26edacd2dac7382489",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x36052a8ddb27fecd20e2e09da15494a0f2186bf8db36deebbbe701993f8c4aae",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x4959a566d8396b889ff4bc20e18d2497602e01e5c6013af5af7a7c4657ece3e2",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xe0b5aeb100569add952966f803cb67aca86dc6ec8b638f5a49f9e0760efa9a7a",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x632467ad388b91583f956f76488afc42846e283c962cbb215d288033ffc4fb71",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x9ad4e69f52519f7b7b8ee5ae3326d57061b429428ea0c056dd32e7a7102e79a7",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x35e130c7071699eae5288b12374ef157a15e4294e2b3a352160b7c1cd4641d82",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xa0d8279f845f63979dc292228adfa0bda117de27e44d90ac2adcd44465b225e7",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x9a100b70ffda9ed9769becdadca2b2936b217e3da4c9b9817bad30d85eab25ff",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x28d67156746295d901005e2d95ce589e7093decb638f8c132d9971fd0a37e176",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x297c4e115b5df76bcd5a1654b8032661680a1803e30a0774cb42bb01891e6d97",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x5f71b88f1032d27d8866948fc9c49525f3e584bdd52a66de6060a7b1f767326f",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xe6d8ddf6a0bbeb4840f48f0c4ffda9affa4675354bdb7d721235297f5a094f54",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x30ba10aef6238bf19667aaa988b18b72adb4724c016e19eb64bbb52808d1a842",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0x9c6806a4d6a99e4869b9a4aaf80b0a3bf5f5240a1d6032ed82edf0e86f2a2467",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xe8480d613bbf3b979aee2de4487496167735bb73df024d988e1795b3c7fa559a",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                    (
                        U256::from_str(
                            "0xebfaec01f898f7f0e2abdb4b0aee3dfbf5ec2b287b1e92f9b62940f85d5f5bac",
                        )
                        .unwrap(),
                        U256::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                    ),
                ]),
            ),
        ];

        apply_patch(header, transaction, state, patches);
    }

    pub(crate) fn patch_chapel<DB>(
        &self,
        header: &Header,
        transaction: &TransactionSigned,
        state: &mut State<DB>,
    ) where
        DB: Database<Error = ProviderError>,
    {
        let patches = vec![
            // patch 1: BlockNum 35547779, txIndex 196(patch before tx 196)
            (
                b256!("1237cb09a7d08c187a78e777853b70be28a41bb188c5341987408623c1a4f4aa"),
                b256!("7ce9a3cf77108fcc85c1e84e88e363e3335eca515dfcf2feb2011729878b13a7"),
                address!("89791428868131eb109e42340ad01eb8987526b2"),
                HashMap::from([(
                    U256::from_str(
                        "0xf1e9242398de526b8dd9c25d38e65fbb01926b8940377762d7884b8b0dcdc3b0",
                    )
                    .unwrap(),
                    U256::from_str(
                        "0x0000000000000000000000000000000000000000000000f6a7831804efd2cd0a",
                    )
                    .unwrap(),
                )]),
            ),
            // patch 2: BlockNum 35548081, txIndex 486(patch before tx 486)
            (
                b256!("cdd38b3681c8f3f1da5569a893231466ab35f47d58ba85dbd7d9217f304983bf"),
                b256!("e3895eb95605d6b43ceec7876e6ff5d1c903e572bf83a08675cb684c047a695c"),
                address!("89791428868131eb109e42340ad01eb8987526b2"),
                HashMap::from([(
                    U256::from_str(
                        "0xf1e9242398de526b8dd9c25d38e65fbb01926b8940377762d7884b8b0dcdc3b0",
                    )
                    .unwrap(),
                    U256::from_str(
                        "0x0000000000000000000000000000000000000000000000114be8ecea72b64003",
                    )
                    .unwrap(),
                )]),
            ),
        ];

        apply_patch(header, transaction, state, patches);
    }
}

fn apply_patch<DB>(
    header: &Header,
    transaction: &TransactionSigned,
    state: &mut State<DB>,
    patches: Vec<(B256, B256, Address, HashMap<U256, U256>)>,
) where
    DB: Database<Error = ProviderError>,
{
    for (block_hash, tx_hash, address, patch) in patches {
        if header.hash_slow() == block_hash && transaction.recalculate_hash() == tx_hash {
            trace!("patch evm state at block {:?} tx {:?}", block_hash, tx_hash);

            let account = state.load_cache_account(address).unwrap().clone();
            let (info, mut storage) = account.into_components().0.unwrap();
            patch.into_iter().for_each(|(key, value)| {
                storage.insert(key, value);
            });

            let mut account = CacheAccount::new_loaded(info, storage);
            account.status = Destroyed;
            state.cache.accounts.insert(address, account);
        }
    }
}