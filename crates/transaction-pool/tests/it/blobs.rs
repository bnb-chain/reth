//! Blob transaction tests

use reth_transaction_pool::{
    blobstore::InMemoryBlobStore,
    error::{Eip4844PoolTransactionError, InvalidPoolTransactionError, PoolErrorKind},
    test_utils::{MockTransaction, MockTransactionFactory, TestPoolBuilder},
    validate::EthTransactionValidatorBuilder,
    AddedTransactionOutcome, CoinbaseTipOrdering, Pool, PoolConfig, PoolTransaction,
    TransactionOrigin, TransactionPool,
};
use alloy_primitives::U256;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

#[tokio::test(flavor = "multi_thread")]
async fn blobs_exclusive() {
    let txpool = TestPoolBuilder::default();
    let mut mock_tx_factory = MockTransactionFactory::default();
    let blob_tx = mock_tx_factory.create_eip4844();

    let AddedTransactionOutcome { hash, .. } = txpool
        .add_transaction(TransactionOrigin::External, blob_tx.transaction.clone())
        .await
        .unwrap();
    assert_eq!(hash, *blob_tx.transaction.get_hash());

    let mut best_txns = txpool.best_transactions();
    assert_eq!(best_txns.next().unwrap().transaction.get_hash(), blob_tx.transaction.get_hash());
    assert!(best_txns.next().is_none());

    let eip1559_tx =
        MockTransaction::eip1559().set_sender(blob_tx.transaction.sender()).inc_price_by(10_000);

    let res =
        txpool.add_transaction(TransactionOrigin::External, eip1559_tx.clone()).await.unwrap_err();

    assert_eq!(res.hash, *eip1559_tx.get_hash());
    match res.kind {
        PoolErrorKind::ExistingConflictingTransactionType(addr, tx_type) => {
            assert_eq!(addr, eip1559_tx.sender());
            assert_eq!(tx_type, eip1559_tx.tx_type());
        }
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reject_blob_tx_with_zero_blob_fee() {
    // Create a mock provider with account balance
    let provider = MockEthProvider::default();
    let mut mock_tx_factory = MockTransactionFactory::default();
    let blob_tx = mock_tx_factory.create_eip4844();
    
    // Add account with sufficient balance
    provider.add_account(
        blob_tx.transaction.sender(),
        ExtendedAccount::new(*blob_tx.transaction.get_nonce(), U256::MAX),
    );

    // Create a validator that does proper validation
    let blob_store = InMemoryBlobStore::default();
    let validator = EthTransactionValidatorBuilder::new(provider)
        .build(blob_store.clone());

    // Create a transaction pool with the real validator
    let txpool = Pool::new(
        validator,
        CoinbaseTipOrdering::default(),
        blob_store,
        PoolConfig::default(),
    );

    // Create a blob transaction with zero max_fee_per_blob_gas
    let zero_fee_tx = blob_tx.transaction.with_blob_fee(0);

    let res = txpool.add_transaction(TransactionOrigin::External, zero_fee_tx).await;

    // Should be rejected due to zero blob fee
    assert!(res.is_err());
    let err = res.unwrap_err();

    match err.kind {
        PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::Eip4844(
            Eip4844PoolTransactionError::ZeroBlobFee,
        )) => {
            // Expected: InvalidPoolTransactionError::Eip4844(ZeroBlobFee)
        }
        _ => panic!("Expected Eip4844(ZeroBlobFee) error for zero blob fee, got: {:?}", err.kind),
    }
}
