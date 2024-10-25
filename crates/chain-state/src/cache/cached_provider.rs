use alloy_primitives::{Address, BlockNumber, Bytes, StorageKey, StorageValue, B256};
use reth_errors::ProviderResult;
use reth_primitives::{Account, Bytecode};
use reth_storage_api::{
    AccountReader, BlockHashReader, StateProofProvider, StateProvider, StateProviderBox,
    StateRootProvider, StorageRootProvider,
};
use reth_trie::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof, StorageProof,
    TrieInput,
};

/// Cached state provider struct
#[allow(missing_debug_implementations)]
pub struct CachedStateProvider {
    pub(crate) underlying: Box<dyn StateProvider>,
}

impl CachedStateProvider {
    /// Create a new `CachedStateProvider`
    pub fn new(underlying: Box<dyn StateProvider>) -> Self {
        Self { underlying }
    }

    /// Turn this state provider into a [`StateProviderBox`]
    pub fn boxed(self) -> StateProviderBox {
        Box::new(self)
    }
}

impl BlockHashReader for CachedStateProvider {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        BlockHashReader::block_hash(&self.underlying, number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        let hashes = self.underlying.canonical_hashes_range(start, end)?;
        Ok(hashes)
    }
}

impl AccountReader for CachedStateProvider {
    fn basic_account(&self, address: Address) -> ProviderResult<Option<Account>> {
        // Check cache first
        if let Some(v) = crate::cache::get_account(&address) {
            return Ok(Some(v))
        }
        // Fallback to underlying provider
        if let Some(value) = AccountReader::basic_account(&self.underlying, address)? {
            crate::cache::insert_account(address, value);
            return Ok(Some(value))
        }
        Ok(None)
    }
}

impl StateRootProvider for CachedStateProvider {
    fn state_root(&self, state: HashedPostState) -> ProviderResult<B256> {
        self.state_root_from_nodes(TrieInput::from_state(state))
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        self.underlying.state_root_from_nodes(input)
    }

    fn state_root_with_updates(
        &self,
        state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.state_root_from_nodes_with_updates(TrieInput::from_state(state))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.underlying.state_root_from_nodes_with_updates(input)
    }
}

impl StorageRootProvider for CachedStateProvider {
    fn storage_root(&self, address: Address, storage: HashedStorage) -> ProviderResult<B256> {
        self.underlying.storage_root(address, storage)
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        self.underlying.storage_proof(address, slot, hashed_storage)
    }
}

impl StateProofProvider for CachedStateProvider {
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        self.underlying.proof(input, address, slots)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: alloy_primitives::map::HashMap<B256, alloy_primitives::map::HashSet<B256>>,
    ) -> ProviderResult<MultiProof> {
        self.underlying.multiproof(input, targets)
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
    ) -> ProviderResult<alloy_primitives::map::HashMap<B256, Bytes>> {
        self.underlying.witness(input, target)
    }
}

impl StateProvider for CachedStateProvider {
    fn storage(
        &self,
        address: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let key = (address, storage_key);
        // Check cache first
        if let Some(v) = crate::cache::get_storage(&key) {
            return Ok(Some(v))
        }
        // Fallback to underlying provider
        if let Some(value) = StateProvider::storage(&self.underlying, address, storage_key)? {
            crate::cache::insert_storage(key, value);
            return Ok(Some(value))
        }
        Ok(None)
    }

    fn bytecode_by_hash(&self, code_hash: B256) -> ProviderResult<Option<Bytecode>> {
        // Check cache first
        if let Some(v) = crate::cache::get_code(&code_hash) {
            return Ok(Some(v))
        }
        // Fallback to underlying provider
        if let Some(value) = StateProvider::bytecode_by_hash(&self.underlying, code_hash)? {
            crate::cache::insert_code(code_hash, value.clone());
            return Ok(Some(value))
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::plain_state::{clear_plain_state, write_plain_state, PLAIN_ACCOUNTS};
    use alloy_primitives::{map::HashMap, U256};
    use reth_primitives::revm_primitives::{AccountInfo, KECCAK_EMPTY};
    use reth_provider::{
        providers::ConsistentDbView, test_utils, test_utils::create_test_provider_factory,
    };
    use reth_revm::db::{AccountStatus, BundleState};
    use reth_storage_api::TryIntoHistoricalStateProvider;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_basic_account_and_clear() {
        let factory = create_test_provider_factory();

        let consistent_view = ConsistentDbView::new_with_latest_tip(factory.clone()).unwrap();
        let state_provider = consistent_view
            .provider_ro()
            .unwrap()
            .disable_long_read_transaction_safety()
            .try_into_history_at_block(1);
        let cached_state_provider = CachedStateProvider::new(state_provider.unwrap());

        let account = Address::random();
        let result =
            reth_storage_api::AccountReader::basic_account(&cached_state_provider, account)
                .unwrap();
        assert_eq!(result.is_none(), true);

        PLAIN_ACCOUNTS
            .insert(account, Account { nonce: 100, balance: U256::ZERO, bytecode_hash: None });
        let result =
            reth_storage_api::AccountReader::basic_account(&cached_state_provider, account)
                .unwrap();
        assert_eq!(result.unwrap().nonce, 100);

        // clear account
        clear_plain_state();
        let result =
            reth_storage_api::AccountReader::basic_account(&cached_state_provider, account)
                .unwrap();
        assert_eq!(result.is_none(), true);
    }

    #[test]
    #[serial]
    fn test_apply_bundle_state() {
        let factory = test_utils::create_test_provider_factory();
        let consistent_view = ConsistentDbView::new_with_latest_tip(factory.clone()).unwrap();
        let state_provider = consistent_view
            .provider_ro()
            .unwrap()
            .disable_long_read_transaction_safety()
            .try_into_history_at_block(1);
        let cached_state_provider = CachedStateProvider::new(state_provider.unwrap());

        // apply bundle state to set cache
        let account1 = Address::random();
        let account2 = Address::random();
        let bundle_state = BundleState::new(
            vec![
                (
                    account1,
                    None,
                    Some(AccountInfo {
                        nonce: 1,
                        balance: U256::from(10),
                        code_hash: KECCAK_EMPTY,
                        code: None,
                    }),
                    HashMap::from_iter([
                        (U256::from(2), (U256::from(0), U256::from(10))),
                        (U256::from(5), (U256::from(0), U256::from(15))),
                    ]),
                ),
                (
                    account2,
                    None,
                    Some(AccountInfo {
                        nonce: 1,
                        balance: U256::from(10),
                        code_hash: KECCAK_EMPTY,
                        code: None,
                    }),
                    HashMap::from_iter([]),
                ),
            ],
            vec![vec![
                (
                    account1,
                    Some(None),
                    vec![(U256::from(2), U256::from(0)), (U256::from(5), U256::from(0))],
                ),
                (account2, Some(None), vec![]),
            ]],
            vec![],
        );
        write_plain_state(bundle_state);

        let account1_result =
            reth_storage_api::AccountReader::basic_account(&cached_state_provider, account1)
                .unwrap();
        assert_eq!(account1_result.unwrap().nonce, 1);
        let storage1_result = reth_storage_api::StateProvider::storage(
            &cached_state_provider,
            account1,
            B256::with_last_byte(2),
        )
        .unwrap();
        assert_eq!(storage1_result.unwrap(), U256::from(10));
        let storage2_result = reth_storage_api::StateProvider::storage(
            &cached_state_provider,
            account1,
            B256::with_last_byte(5),
        )
        .unwrap();
        assert_eq!(storage2_result.unwrap(), U256::from(15));

        let account2_result =
            reth_storage_api::AccountReader::basic_account(&cached_state_provider, account2)
                .unwrap();
        assert_eq!(account2_result.unwrap().nonce, 1);

        // apply bundle state to set clear cache
        let account3 = Address::random();
        let mut bundle_state = BundleState::new(
            vec![(
                account3,
                Some(AccountInfo {
                    nonce: 3,
                    balance: U256::from(10),
                    code_hash: KECCAK_EMPTY,
                    code: None,
                }),
                None,
                HashMap::from_iter([
                    (U256::from(2), (U256::from(0), U256::from(10))),
                    (U256::from(5), (U256::from(0), U256::from(15))),
                ]),
            )],
            vec![vec![(
                account3,
                Some(None),
                vec![(U256::from(2), U256::from(0)), (U256::from(5), U256::from(0))],
            )]],
            vec![],
        );
        bundle_state.state.get_mut(&account3).unwrap().status = AccountStatus::Destroyed;
        write_plain_state(bundle_state);

        let account1_result =
            reth_storage_api::AccountReader::basic_account(&cached_state_provider, account1)
                .unwrap();
        assert_eq!(account1_result.unwrap().nonce, 1);
        let storage1_result = reth_storage_api::StateProvider::storage(
            &cached_state_provider,
            account1,
            B256::with_last_byte(2),
        )
        .unwrap();
        assert_eq!(storage1_result.is_none(), true);
        let storage2_result = reth_storage_api::StateProvider::storage(
            &cached_state_provider,
            account1,
            B256::with_last_byte(5),
        )
        .unwrap();
        assert_eq!(storage2_result.is_none(), true);

        let account2_result =
            reth_storage_api::AccountReader::basic_account(&cached_state_provider, account2)
                .unwrap();
        assert_eq!(account2_result.unwrap().nonce, 1);
    }
}
