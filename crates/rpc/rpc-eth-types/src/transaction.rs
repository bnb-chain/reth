//! Helper types for `reth_rpc_eth_api::EthApiServer` implementation.
//!
//! Transaction wrapper that labels transaction with its origin.

use alloy_primitives::B256;
use alloy_rpc_types_eth::TransactionInfo;
use reth_ethereum_primitives::TransactionSigned;
use reth_primitives_traits::{NodePrimitives, Recovered, SignedTransaction};
use reth_rpc_convert::{RpcConvert, RpcTransaction};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Represents from where a transaction was fetched.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TransactionSource<T = TransactionSigned> {
    /// Transaction exists in the pool (Pending)
    Pool(Recovered<T>),
    /// Transaction already included in a block
    ///
    /// This can be a historical block or a pending block (received from the CL)
    Block {
        /// Transaction fetched via provider
        transaction: Recovered<T>,
        /// Index of the transaction in the block
        index: u64,
        /// Hash of the block.
        block_hash: B256,
        /// Number of the block.
        block_number: u64,
        /// base fee of the block.
        base_fee: Option<u64>,
    },
}

// === impl TransactionSource ===

impl<T: SignedTransaction> TransactionSource<T> {
    /// Consumes the type and returns the wrapped transaction.
    pub fn into_recovered(self) -> Recovered<T> {
        self.into()
    }

    /// Conversion into network specific transaction type.
    pub fn into_transaction<Builder>(
        self,
        resp_builder: &Builder,
    ) -> Result<RpcTransaction<Builder::Network>, Builder::Error>
    where
        Builder: RpcConvert<Primitives: NodePrimitives<SignedTx = T>>,
    {
        match self {
            Self::Pool(tx) => resp_builder.fill_pending(tx),
            Self::Block { transaction, index, block_hash, block_number, base_fee } => {
                let tx_info = TransactionInfo {
                    hash: Some(transaction.trie_hash()),
                    index: Some(index),
                    block_hash: Some(block_hash),
                    block_number: Some(block_number),
                    base_fee,
                };

                resp_builder.fill(transaction, tx_info)
            }
        }
    }

    /// Returns the transaction and block related info, if not pending
    pub fn split(self) -> (Recovered<T>, TransactionInfo) {
        match self {
            Self::Pool(tx) => {
                let hash = tx.trie_hash();
                (tx, TransactionInfo { hash: Some(hash), ..Default::default() })
            }
            Self::Block { transaction, index, block_hash, block_number, base_fee } => {
                let hash = transaction.trie_hash();
                (
                    transaction,
                    TransactionInfo {
                        hash: Some(hash),
                        index: Some(index),
                        block_hash: Some(block_hash),
                        block_number: Some(block_number),
                        base_fee,
                    },
                )
            }
        }
    }
}

impl<T> From<TransactionSource<T>> for Recovered<T> {
    fn from(value: TransactionSource<T>) -> Self {
        match value {
            TransactionSource::Pool(tx) => tx,
            TransactionSource::Block { transaction, .. } => transaction,
        }
    }
}

/// Response structure for transaction data and receipt with custom field names
#[derive(Debug, Clone)]
pub struct TransactionDataAndReceipt<TX, RX> {
    /// Transaction data (corresponds to "txData" in JSON)
    pub tx_data: Option<TX>,
    /// Transaction receipt
    pub receipt: Option<RX>,
}

impl<TX, RX> Serialize for TransactionDataAndReceipt<TX, RX>
where
    TX: Serialize,
    RX: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("TransactionDataAndReceipt", 2)?;
        state.serialize_field("txData", &self.tx_data)?;
        state.serialize_field("receipt", &self.receipt)?;
        state.end()
    }
}

impl<'de, TX, RX> Deserialize<'de> for TransactionDataAndReceipt<TX, RX>
where
    TX: Deserialize<'de>,
    RX: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};
        use std::fmt;

        struct TransactionDataAndReceiptVisitor<TX, RX>(std::marker::PhantomData<(TX, RX)>);

        impl<'de, TX, RX> Visitor<'de> for TransactionDataAndReceiptVisitor<TX, RX>
        where
            TX: Deserialize<'de>,
            RX: Deserialize<'de>,
        {
            type Value = TransactionDataAndReceipt<TX, RX>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("struct TransactionDataAndReceipt")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut tx_data = None;
                let mut receipt = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "txData" => {
                            if tx_data.is_some() {
                                return Err(serde::de::Error::duplicate_field("txData"));
                            }
                            tx_data = Some(map.next_value()?);
                        }
                        "receipt" => {
                            if receipt.is_some() {
                                return Err(serde::de::Error::duplicate_field("receipt"));
                            }
                            receipt = Some(map.next_value()?);
                        }
                        _ => {
                            // Ignore unknown fields
                            let _ = map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }

                Ok(TransactionDataAndReceipt {
                    tx_data: tx_data.unwrap_or(None),
                    receipt: receipt.unwrap_or(None),
                })
            }
        }

        deserializer.deserialize_struct(
            "TransactionDataAndReceipt",
            &["txData", "receipt"],
            TransactionDataAndReceiptVisitor(std::marker::PhantomData),
        )
    }
}
