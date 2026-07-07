//! Storage metadata models.

use reth_codecs::{add_arbitrary_tests, Compact};
use serde::{Deserialize, Serialize};

/// Storage configuration settings for this node.
///
/// Controls whether this node uses v2 storage layout (static files + `RocksDB` routing)
/// or v1/legacy layout (everything in MDBX).
///
/// These should be set during `init_genesis` or `init_db` depending on whether we want dictate
/// behaviour of new or old nodes respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Compact, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(compact)]
pub struct StorageSettings {
    /// Whether this node uses v2 storage layout.
    ///
    /// When `true`, enables all v2 storage features:
    /// - Receipts and transaction senders in static files
    /// - History indices in `RocksDB` (accounts, storages, transaction hashes)
    /// - Account and storage changesets in static files
    /// - Hashed state tables as canonical state representation (see [`Self::hashed_state`])
    ///
    /// When `false`, uses v1/legacy layout (everything in MDBX).
    pub storage_v2: bool,
    /// Whether the v2 layout uses hashed state tables (`HashedAccounts`/`HashedStorages`)
    /// as the canonical state representation.
    ///
    /// Chains that carry no Merkle-Patricia trie (state commitment computed outside the
    /// trie, e.g. a lattice hash over plain keys) can set this to `false` to keep
    /// `PlainAccountState`/`PlainStorageState` canonical while still routing receipts,
    /// senders and changesets to static files and history indices to `RocksDB`.
    ///
    /// Ignored when `storage_v2` is `false`. Defaults to `true` (upstream v2 behaviour)
    /// so that settings persisted before this field existed keep their semantics.
    #[serde(default = "serde_default_true")]
    pub hashed_state: bool,
}

/// `serde` default for [`StorageSettings::hashed_state`] — settings persisted before the
/// field existed were always hashed-state canonical.
const fn serde_default_true() -> bool {
    true
}

impl StorageSettings {
    /// Returns the default base `StorageSettings`.
    pub const fn base() -> Self {
        Self::v2()
    }

    /// Creates `StorageSettings` for v2 nodes with all storage features enabled:
    /// - Receipts and transaction senders in static files
    /// - History indices in `RocksDB` (storages, accounts, transaction hashes)
    /// - Account and storage changesets in static files
    /// - Hashed state as canonical state representation
    ///
    /// Use this when the `--storage.v2` CLI flag is set.
    pub const fn v2() -> Self {
        Self { storage_v2: true, hashed_state: true }
    }

    /// Creates `StorageSettings` for v2 nodes that keep plain state tables canonical:
    /// v2 routing (static files + `RocksDB`) without the hashed-state representation.
    ///
    /// For chains whose state commitment is not derived from the Merkle-Patricia trie,
    /// so state must stay addressable by plain key.
    pub const fn v2_with_plain_state() -> Self {
        Self { storage_v2: true, hashed_state: false }
    }

    /// Creates `StorageSettings` for v1/legacy nodes.
    ///
    /// This keeps all data in MDBX, matching the original storage layout.
    pub const fn v1() -> Self {
        Self { storage_v2: false, hashed_state: false }
    }

    /// Returns `true` if this node uses v2 storage layout.
    pub const fn is_v2(&self) -> bool {
        self.storage_v2
    }

    /// Whether receipts are stored in static files.
    pub const fn receipts_in_static_files(&self) -> bool {
        self.storage_v2
    }

    /// Whether transaction senders are stored in static files.
    pub const fn transaction_senders_in_static_files(&self) -> bool {
        self.storage_v2
    }

    /// Whether storages history is stored in `RocksDB`.
    pub const fn storages_history_in_rocksdb(&self) -> bool {
        self.storage_v2
    }

    /// Whether transaction hash numbers are stored in `RocksDB`.
    pub const fn transaction_hash_numbers_in_rocksdb(&self) -> bool {
        self.storage_v2
    }

    /// Whether account history is stored in `RocksDB`.
    pub const fn account_history_in_rocksdb(&self) -> bool {
        self.storage_v2
    }

    /// Whether to use hashed state tables (`HashedAccounts`/`HashedStorages`) as the canonical
    /// state representation instead of plain state tables. Implied by v2 storage layout unless
    /// [`Self::hashed_state`] opts out.
    pub const fn use_hashed_state(&self) -> bool {
        self.storage_v2 && self.hashed_state
    }

    /// Whether block persistence should write hashed-state / trie data at all.
    ///
    /// `true` on v1 (hashed + trie tables back the MPT state root) and on full
    /// v2 (hashed state is canonical). `false` only for plain-state v2
    /// ([`Self::v2_with_plain_state`]): the chain's state commitment is not
    /// trie-derived, so hashed/trie writes would populate tables nothing reads.
    pub const fn writes_hashed_state(&self) -> bool {
        !self.storage_v2 || self.hashed_state
    }

    /// Returns `true` if any tables are configured to be stored in `RocksDB`.
    pub const fn any_in_rocksdb(&self) -> bool {
        self.storage_v2
    }
}
