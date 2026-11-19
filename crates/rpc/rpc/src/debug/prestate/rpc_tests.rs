use super::*;
use crate::{DebugApi, EthApiBuilder};
use alloy_consensus::{Header, TxLegacy};
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
use reth_chain_state::CanonStateNotification;
use reth_chainspec::{ChainSpecBuilder, ChainSpecProvider};
use reth_ethereum_primitives::{Block, BlockBody, TransactionSigned};
use reth_evm_ethereum::EthEvmConfig;
use reth_execution_types::{Chain, ExecutionOutcome};
use reth_network_api::noop::NoopNetwork;
use reth_primitives_traits::{RecoveredBlock, SignerRecoverable};
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_rpc_api::DebugApiServer;
use reth_rpc_eth_types::cache::cache_new_blocks_task;
use reth_tasks::{pool::BlockingTaskGuard, Runtime};
use reth_transaction_pool::test_utils::testing_pool;
use serde_json::{json, Value};
use std::sync::Arc;

macro_rules! rpc {
    ($code:expr) => {{
        let provider = MockEthProvider::default()
            .with_chain_spec(ChainSpecBuilder::mainnet().cancun_activated().build());
        let target = Address::with_last_byte(0x42);
        provider.add_account(target, ExtendedAccount::new(1, U256::ZERO).with_bytecode($code));
        let tx = TransactionSigned::new_unhashed(
            TxLegacy { gas_limit: 500_000, to: TxKind::Call(target), ..Default::default() }.into(),
            Signature::test_signature(),
        );
        let hash = *tx.hash();
        let sender = tx.recover_signer().unwrap();
        provider.add_account(sender, ExtendedAccount::new(0, U256::from(1_000_000)));
        let parent = Header {
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(0),
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        };
        let parent_hash = parent.hash_slow();
        provider.add_header(parent_hash, parent.clone());
        let block = Block {
            header: Header { parent_hash, number: 1, timestamp: 1, ..parent },
            body: BlockBody { transactions: vec![tx], ..Default::default() },
        };
        provider.add_block(block.header.hash_slow(), block.clone());
        let eth_api = EthApiBuilder::new(
            provider.clone(),
            testing_pool(),
            NoopNetwork::default(),
            EthEvmConfig::new(provider.chain_spec()),
        )
        .build();
        cache_new_blocks_task(
            eth_api.cache().clone(),
            futures::stream::iter([CanonStateNotification::Commit {
                new: Arc::new(Chain::new(
                    [RecoveredBlock::new_unhashed(block, vec![sender])],
                    ExecutionOutcome {
                        receipts: vec![vec![]],
                        first_block: 1,
                        ..Default::default()
                    },
                    Default::default(),
                )),
            }]),
        )
        .await;
        let api = DebugApi::new(
            eth_api,
            BlockingTaskGuard::new(1),
            &Runtime::test(),
            futures::stream::empty(),
        );
        (api.into_rpc(), hash, target)
    }};
}

#[tokio::test]
async fn rejected_creations_retain_empty_candidates_in_prestate_and_mux() {
    // Create with empty init code and a 1 wei endowment, then discard the failure and stop.
    for (code, create2) in [("600060006001f05000", false), ("6000600060006001f55000", true)] {
        let (module, hash, target) = rpc!(code.parse::<Bytes>().unwrap());
        let candidate =
            if create2 { target.create2_from_code(B256::ZERO, []) } else { target.create(1) };
        for mux in [false, true] {
            for include_empty in [false, true] {
                let config = json!({"includeEmpty":include_empty,"disableStorage":true});
                let opts = if mux {
                    json!({"tracer":"muxTracer","tracerConfig":{"prestateTracer":config,"callTracer":{}}})
                } else {
                    json!({"tracer":"prestateTracer","tracerConfig":config})
                };
                for (method, params) in [
                    ("debug_traceCall", json!([{"to":target,"gas":"0x7a120"}, "latest", opts])),
                    ("debug_traceTransaction", json!([hash, opts])),
                ] {
                    let request = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
                    let (response, _) =
                        module.raw_json_request(&request.to_string(), 1).await.unwrap();
                    let response: Value = serde_json::from_str(response.get()).unwrap();
                    assert!(response.get("error").is_none(), "{request}: {response}");
                    let result = &response["result"];
                    let pre = if mux { &result["prestateTracer"] } else { result };
                    if include_empty {
                        assert_eq!(
                            pre[format!("{candidate:#x}")],
                            json!({"balance":"0x0"}),
                            "{request}: {response}"
                        );
                    } else {
                        assert!(pre.get(format!("{candidate:#x}")).is_none(), "{response}");
                    }
                    if mux {
                        assert!(
                            result["callTracer"].get("error").is_none(),
                            "the outer call succeeds: {response}"
                        );
                        assert_eq!(
                            result["callTracer"]["calls"][0]["error"],
                            "insufficient balance for transfer"
                        );
                    }
                }
            }
            let config = json!({"includeEmpty":true});
            let opts = if mux {
                json!({"tracer":"muxTracer","tracerConfig":{"callTracer":{},"prestateTracer":config}})
            } else {
                json!({"tracer":"prestateTracer","tracerConfig":config})
            };
            let untouched = Address::with_last_byte(0x88);
            let request = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceCallMany","params":[[
                {"transactions":[{"to":target,"gas":"0x7a120"},{"to":untouched,"gas":"0x7a120"}]},
                {"transactions":[{"to":untouched,"gas":"0x7a120"}]}
            ], {"blockNumber":"latest"}, opts]});
            let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{request}: {response}");
            let traces = response["result"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|bundle| bundle.as_array().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(traces.len(), 3);
            for (index, trace) in traces.into_iter().enumerate() {
                let pre = if mux { &trace["prestateTracer"] } else { trace };
                assert_eq!(pre.get(format!("{candidate:#x}")).is_some(), index == 0, "creation candidates must not leak into later transactions or bundles: {response}");
            }
        }
    }
}

#[tokio::test]
async fn incompatible_prestate_options_use_geth_rpc_error() {
    let (module, hash, target) = rpc!(Bytes::from_static(&[0]));
    for mux in [false, true] {
        let config = json!({"diffMode":true,"includeEmpty":true});
        let opts = if mux {
            json!({"tracer":"muxTracer","tracerConfig":{"prestateTracer":config}})
        } else {
            json!({"tracer":"prestateTracer","tracerConfig":config})
        };
        for (method, params) in [
            ("debug_traceCall", json!([{"to":target}, "latest", opts])),
            ("debug_traceTransaction", json!([hash, opts])),
        ] {
            let request = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
            let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert_eq!(
                response["error"],
                json!({"code":-32000,"message":"cannot use diffMode with includeEmpty"}),
                "{request}: {response}"
            );
        }
    }
}
