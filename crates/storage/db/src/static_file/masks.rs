use super::{ReceiptMask, SidecarMask, TransactionMask};
use crate::{
    add_static_file_mask,
    static_file::mask::{ColumnSelectorOne, ColumnSelectorTwo, HeaderMask},
    HeaderTerminalDifficulties, RawValue, Receipts, Transactions,
};
use reth_db_api::table::Table;
use reth_primitives::{BlockHash, Header};
use reth_primitives_traits::BlobSidecars;

// HEADER MASKS
add_static_file_mask!(HeaderMask, Header, 0b001);
add_static_file_mask!(HeaderMask, <HeaderTerminalDifficulties as Table>::Value, 0b010);
add_static_file_mask!(HeaderMask, BlockHash, 0b100);
add_static_file_mask!(HeaderMask, Header, BlockHash, 0b101);
add_static_file_mask!(HeaderMask, <HeaderTerminalDifficulties as Table>::Value, BlockHash, 0b110);

// RECEIPT MASKS
add_static_file_mask!(ReceiptMask, <Receipts as Table>::Value, 0b1);

// TRANSACTION MASKS
add_static_file_mask!(TransactionMask, <Transactions as Table>::Value, 0b1);
add_static_file_mask!(TransactionMask, RawValue<<Transactions as Table>::Value>, 0b1);

// SIDECARS MASKS
add_static_file_mask!(SidecarMask, BlobSidecars, 0b01);
add_static_file_mask!(SidecarMask, BlockHash, 0b10);
add_static_file_mask!(SidecarMask, BlobSidecars, BlockHash, 0b11);
