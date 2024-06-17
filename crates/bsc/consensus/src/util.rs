use crate::EXTRA_SEAL_LEN;
use alloy_rlp::Encodable;
use lazy_static::lazy_static;
use reth_primitives::{
    keccak256, Address, BufMut, BytesMut, Header, TransactionSigned, B256, B64, U256,
};
use std::str::FromStr;

lazy_static! {
    // preset contracts
    pub static ref VALIDATOR_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000001000").unwrap();
    pub static ref SLASH_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000001001").unwrap();
    pub static ref SYSTEM_REWARD_CONTRACT: Address = Address::from_str("0x0000000000000000000000000000000000001002").unwrap();
    pub static ref LIGHT_CLIENT_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000001003").unwrap();
    pub static ref TOKEN_HUB_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000001004").unwrap();
    pub static ref RELAYER_INCENTIVIZE_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000001005").unwrap();
    pub static ref RELAYER_HUB_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000001006").unwrap();
    pub static ref GOV_HUB_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000001007").unwrap();
    pub static ref CROSS_CHAIN_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000002000").unwrap();
    pub static ref STAKE_HUB_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000002002").unwrap();
    pub static ref BSC_GOVERNOR_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000002004").unwrap();
    pub static ref GOV_TOKEN_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000002005").unwrap();
    pub static ref BSC_TIMELOCK_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000002006").unwrap();
    pub static ref TOKEN_RECOVER_PORTAL_CONTRACT: Address =  Address::from_str("0x0000000000000000000000000000000000003000").unwrap();

    pub static ref SYSTEM_CONTRACTS: Vec<Address> = vec![
        *VALIDATOR_CONTRACT,
        *SLASH_CONTRACT,
        *SYSTEM_REWARD_CONTRACT,
        *LIGHT_CLIENT_CONTRACT,
        *TOKEN_HUB_CONTRACT,
        *RELAYER_INCENTIVIZE_CONTRACT,
        *RELAYER_HUB_CONTRACT,
        *GOV_HUB_CONTRACT,
        *CROSS_CHAIN_CONTRACT,
        *STAKE_HUB_CONTRACT,
        *BSC_GOVERNOR_CONTRACT,
        *GOV_TOKEN_CONTRACT,
        *BSC_TIMELOCK_CONTRACT,
        *TOKEN_RECOVER_PORTAL_CONTRACT,
    ];
}

const SECONDS_PER_DAY: u64 = 86400; // 24 * 60 * 60

pub const fn is_same_day_in_utc(first: u64, second: u64) -> bool {
    first / SECONDS_PER_DAY == second / SECONDS_PER_DAY
}

pub const fn is_breathe_block(last_block_time: u64, block_time: u64) -> bool {
    last_block_time != 0 && !is_same_day_in_utc(last_block_time, block_time)
}

pub fn is_system_transaction(tx: &TransactionSigned, sender: Address, header: &Header) -> bool {
    if let Some(to) = tx.to() {
        if sender == header.beneficiary &&
            is_invoke_system_contract(&to) &&
            tx.max_fee_per_gas() == 0
        {
            return true;
        }
    }

    false
}

/// whether the contract is system or not
pub fn is_invoke_system_contract(addr: &Address) -> bool {
    SYSTEM_CONTRACTS.contains(addr)
}

pub fn hash_with_chain_id(header: &Header, chain_id: u64) -> B256 {
    let mut out = BytesMut::new();
    encode_header_with_chain_id(header, &mut out, chain_id);
    keccak256(&out[..])
}

pub fn encode_header_with_chain_id(header: &Header, out: &mut dyn BufMut, chain_id: u64) {
    rlp_header(header, chain_id).encode(out);
    Encodable::encode(&U256::from(chain_id), out);
    Encodable::encode(&header.parent_hash, out);
    Encodable::encode(&header.ommers_hash, out);
    Encodable::encode(&header.beneficiary, out);
    Encodable::encode(&header.state_root, out);
    Encodable::encode(&header.transactions_root, out);
    Encodable::encode(&header.receipts_root, out);
    Encodable::encode(&header.logs_bloom, out);
    Encodable::encode(&header.difficulty, out);
    Encodable::encode(&U256::from(header.number), out);
    Encodable::encode(&header.gas_limit, out);
    Encodable::encode(&header.gas_used, out);
    Encodable::encode(&header.timestamp, out);
    Encodable::encode(&header.extra_data[..header.extra_data.len() - EXTRA_SEAL_LEN], out); // will panic if extra_data is less than EXTRA_SEAL_LEN
    Encodable::encode(&header.mix_hash, out);
    Encodable::encode(&B64::new(header.nonce.to_be_bytes()), out);
}

fn rlp_header(header: &Header, chain_id: u64) -> alloy_rlp::Header {
    let mut rlp_head = alloy_rlp::Header { list: true, payload_length: 0 };

    // add chain_id make more security
    rlp_head.payload_length += U256::from(chain_id).length(); // chain_id
    rlp_head.payload_length += header.parent_hash.length(); // parent_hash
    rlp_head.payload_length += header.ommers_hash.length(); // ommers_hash
    rlp_head.payload_length += header.beneficiary.length(); // beneficiary
    rlp_head.payload_length += header.state_root.length(); // state_root
    rlp_head.payload_length += header.transactions_root.length(); // transactions_root
    rlp_head.payload_length += header.receipts_root.length(); // receipts_root
    rlp_head.payload_length += header.logs_bloom.length(); // logs_bloom
    rlp_head.payload_length += header.difficulty.length(); // difficulty
    rlp_head.payload_length += U256::from(header.number).length(); // block height
    rlp_head.payload_length += header.gas_limit.length(); // gas_limit
    rlp_head.payload_length += header.gas_used.length(); // gas_used
    rlp_head.payload_length += header.timestamp.length(); // timestamp
    rlp_head.payload_length +=
        &header.extra_data[..header.extra_data.len() - EXTRA_SEAL_LEN].length(); // extra_data
    rlp_head.payload_length += header.mix_hash.length(); // mix_hash
    rlp_head.payload_length += &B64::new(header.nonce.to_be_bytes()).length(); // nonce

    rlp_head
}

#[cfg(test)]
mod tests {
    use crate::{encode_header_with_chain_id, hash_with_chain_id};
    use reth_primitives::{address, b256, hex, Bloom, Bytes, Header, U256};

    #[test]
    fn test_encode_header_with_chain_id() {
        // test data from bsc testnet
        let expected_rlp = "f902d361a0b68487ffcf4a419f8f8b77afb31e47eeb05195b0b77fe7a0bbc50ebe2f365992a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d4934794b71b214cb885500844365e95cd9942c7276e7fd8a08fcdaf2f45f782142206517f6c059888db0da8ad7809f2101c19f68a68984499a06765de680f44a688e9eed23adfe732fee37b3376d2888a9d7a3523e3a01bfb10a0c783cd24949a5ab58293ee8d9bcb8638699308e055747d044e3317fe5e638494b90100000000000000000000000040100000000000000000000000000000000080018000001420000000100000000000000000000000060008000000000000002030000200001000100000000000080008000020100000000080004000100000001c00400880200002000000040000000008000804000000000010000000100000040000000008800000000000200400200000081004200080000000000000081000200600400020000020080000002200000000080080001000000000000000000002000000020800000000000020000200000001008020880000001040120000200000100002000000000100000400000102000080000000000040000800000000000284024b8ca68402faf0808316db1a8465f003e3b8d5d88301030a846765746888676f312e32312e37856c696e757800000096d46a82f8b381fbb8608a1933f7b78c4e5fcc87580635f39d8850ec66ed767bce1fafaf582b1b941fc6ab17c6b951e188e845e87f4b77136dd512dcf971b93472e4eb1f54799e5ff90ebc6e8d43339bb0e3009ff488cb87a03b104a7ce9d1ba6c17c54e34c1069c77a8f84c84024b8ca4a00bb45f286d475ad03dfa64215c3d21f9bccd5d199d6da149c2958923666b96f384024b8ca5a0b68487ffcf4a419f8f8b77afb31e47eeb05195b0b77fe7a0bbc50ebe2f36599280a00000000000000000000000000000000000000000000000000000000000000000880000000000000000";
        let expected_hash = "1cc380de1196b5bb088f6b7a0eac87f9634864ee6c3f4a47396155464f6ef8f2";

        let bloom = Bloom::from_slice(&hex::decode("000000000000000000000040100000000000000000000000000000000080018000001420000000100000000000000000000000060008000000000000002030000200001000100000000000080008000020100000000080004000100000001c0040088020000200000004000000000800080400000000001000000010000004000000000880000000000020040020000008100420008000000000000008100020060040002000002008000000220000000008008000100000000000000000000200000002080000000000002000020000000100802088000000104012000020000010000200000000010000040000010200008000000000004000080000000000").unwrap());
        let extra = hex::decode("d88301030a846765746888676f312e32312e37856c696e757800000096d46a82f8b381fbb8608a1933f7b78c4e5fcc87580635f39d8850ec66ed767bce1fafaf582b1b941fc6ab17c6b951e188e845e87f4b77136dd512dcf971b93472e4eb1f54799e5ff90ebc6e8d43339bb0e3009ff488cb87a03b104a7ce9d1ba6c17c54e34c1069c77a8f84c84024b8ca4a00bb45f286d475ad03dfa64215c3d21f9bccd5d199d6da149c2958923666b96f384024b8ca5a0b68487ffcf4a419f8f8b77afb31e47eeb05195b0b77fe7a0bbc50ebe2f3659928094a6203400acc84400f2f1aa658e17180490e2f5b758124097751b9cd0a954dc672530d892214bfc77c35201473e34a5ad44100fc0e2235325ba393d5261c26800").unwrap();

        let header = Header {
            parent_hash: b256!("b68487ffcf4a419f8f8b77afb31e47eeb05195b0b77fe7a0bbc50ebe2f365992"),
            ommers_hash: b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"),
            beneficiary: address!("B71b214Cb885500844365E95CD9942C7276E7fD8"),
            state_root: b256!("8fcdaf2f45f782142206517f6c059888db0da8ad7809f2101c19f68a68984499"),
            transactions_root: b256!(
                "6765de680f44a688e9eed23adfe732fee37b3376d2888a9d7a3523e3a01bfb10"
            ),
            receipts_root: b256!(
                "c783cd24949a5ab58293ee8d9bcb8638699308e055747d044e3317fe5e638494"
            ),
            logs_bloom: bloom,
            difficulty: U256::from(2),
            number: 38505638,
            gas_limit: 50000000,
            gas_used: 1497882,
            timestamp: 1710228451,
            extra_data: Bytes::from(extra),
            mix_hash: b256!("0000000000000000000000000000000000000000000000000000000000000000"),
            nonce: 0,
            ..Default::default()
        };

        let mut data = vec![];
        encode_header_with_chain_id(&header, &mut data, 97);
        println!("rlp output: {:?}", hex::encode(&data));
        assert_eq!(hex::encode(&data), expected_rlp);

        let hash = hash_with_chain_id(&header, 97);
        println!("encode hash: {:?}", hex::encode(hash.as_slice()));
        assert_eq!(hex::encode(hash.as_slice()), expected_hash);
    }
}
