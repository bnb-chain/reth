use super::*;
use crate::{DebugApi, EthApiBuilder};
use alloy_consensus::{Header, TxEip7702, TxLegacy};
use alloy_eips::eip7702::{Authorization, SignedAuthorization};
use alloy_evm::{
    eth::EthEvmFactory,
    precompiles::{DynPrecompile, PrecompilesMap},
    Database, Evm, EvmFactory,
};
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
use reth_chain_state::CanonStateNotification;
use reth_chainspec::{ChainSpecBuilder, ChainSpecProvider};
use reth_ethereum_primitives::{Block, BlockBody, TransactionSigned};
use reth_evm_ethereum::EthEvmConfig;
use reth_execution_types::{Chain, ExecutionOutcome};
use reth_network_api::noop::NoopNetwork;
use reth_primitives_traits::{RecoveredBlock, SignerRecoverable};
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_rpc_eth_types::cache::cache_new_blocks_task;
use reth_tasks::{pool::BlockingTaskGuard, Runtime};
use reth_transaction_pool::test_utils::testing_pool;
use revm::{
    database_interface::DBErrorMarker,
    inspector::NoOpInspector,
    precompile::{PrecompileId, PrecompileOutput},
};
use serde_json::{json, Value};
use std::sync::Arc;

macro_rules! rpc {
    ($code:expr) => {
        rpc!($code, None)
    };
    ($code:expr, $authorization:expr) => {
        rpc!($code, $authorization, EthEvmFactory::default(), Vec::new())
    };
    ($code:expr, $authorization:expr, $factory:expr, $preceding:expr) => {{
        let provider = MockEthProvider::default()
            .with_chain_spec(ChainSpecBuilder::mainnet().prague_activated().build());
        let target = Address::with_last_byte(0x42);
        provider.add_account(target, ExtendedAccount::new(1, U256::ZERO).with_bytecode($code));
        let authorization: Option<SignedAuthorization> = $authorization;
        let tx = TransactionSigned::new_unhashed(
            if let Some(auth) = authorization {
                TxEip7702 {
                    chain_id: 1,
                    gas_limit: 500_000,
                    to: target,
                    authorization_list: vec![auth],
                    ..Default::default()
                }
                .into()
            } else {
                TxLegacy { gas_limit: 500_000, to: TxKind::Call(target), ..Default::default() }
                    .into()
            },
            Signature::test_signature(),
        );
        let hash = *tx.hash();
        let parent = Header {
            beneficiary: Address::with_last_byte(0x99),
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(0),
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        };
        let parent_hash = parent.hash_slow();
        provider.add_header(parent_hash, parent.clone());
        let mut transactions: Vec<TransactionSigned> = $preceding;
        transactions.push(tx);
        let signers =
            transactions.iter().map(|tx| tx.recover_signer().unwrap()).collect::<Vec<_>>();
        for signer in &signers {
            provider.add_account(*signer, ExtendedAccount::new(0, U256::from(1_000_000)));
        }
        let block = Block {
            header: Header { parent_hash, number: 1, timestamp: 1, ..parent },
            body: BlockBody { transactions, ..Default::default() },
        };
        provider.add_block(block.header.hash_slow(), block.clone());
        let eth_api = EthApiBuilder::new(
            provider.clone(),
            testing_pool(),
            NoopNetwork::default(),
            EthEvmConfig::new_with_evm_factory(provider.chain_spec(), $factory),
        )
        .build();
        cache_new_blocks_task(
            eth_api.cache().clone(),
            futures::stream::iter([CanonStateNotification::Commit {
                new: Arc::new(Chain::new(
                    [RecoveredBlock::new_unhashed(block, signers)],
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
        (api.into_rpc(), hash, target, provider)
    }};
}

#[tokio::test]
async fn rejected_creations_retain_empty_candidates_in_prestate_and_mux() {
    // Create with empty init code and a 1 wei endowment, then discard the failure and stop.
    for (code, create2) in [("600060006001f05000", false), ("6000600060006001f55000", true)] {
        let (module, hash, target, _) = rpc!(code.parse::<Bytes>().unwrap());
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
    let (module, hash, target, _) = rpc!(Bytes::from_static(&[0]));
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

fn prestate_options(config: Value, mux: bool) -> Value {
    if mux {
        json!({"tracer":"muxTracer","tracerConfig":{"prestateTracer":config,"callTracer":{}}})
    } else {
        json!({"tracer":"prestateTracer","tracerConfig":config})
    }
}

#[tokio::test]
async fn simulated_calls_include_empty_beneficiary() {
    let (module, _, target, _) = rpc!(Bytes::from_static(&[0]));
    for mux in [false, true] {
        for include_empty in [false, true] {
            for indexed in [false, true] {
                let mut opts = prestate_options(json!({"includeEmpty":include_empty}), mux);
                if indexed {
                    opts["txIndex"] = json!(0);
                }
                let request = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{"to":target},"latest",opts]});
                let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
                let response: Value = serde_json::from_str(response.get()).unwrap();
                assert!(response.get("error").is_none(), "{response}");
                let pre =
                    if mux { &response["result"]["prestateTracer"] } else { &response["result"] };
                let beneficiary = pre.get(format!("{:#x}", Address::with_last_byte(0x99)));
                assert_eq!(
                    beneficiary,
                    include_empty.then(|| json!({"balance":"0x0"})).as_ref(),
                    "{response}"
                );
            }
        }
    }
}

#[tokio::test]
async fn static_creation_observes_candidates_but_not_protected_call_or_selfdestruct_targets() {
    let (module, _, target, provider) =
        rpc!("60006000600060006043620f4240fa5000".parse::<Bytes>().unwrap());
    let inner = Address::with_last_byte(0x43);
    let call_target = Address::with_last_byte(0x44);
    for (code, candidate, expected) in [
        ("600060006001f05000", inner.create(1), true),
        ("6000600060006001f55000", inner.create2_from_code(B256::ZERO, []), true),
        ("6000600160006001f55000", inner.create2_from_code(B256::ZERO, [0]), true),
        ("60aa6000536000600260006001f55000", inner.create2_from_code(B256::ZERO, [0xaa, 0]), true),
        // Zero-size EVM memory expansion is free, but the tracer still bounds the raw offset.
        ("60006000621000006000f5", inner.create2_from_code(B256::ZERO, []), true),
        ("60006000621000016000f5", inner.create2_from_code(B256::ZERO, []), false),
        ("600060006780000000000000006000f5", inner.create2_from_code(B256::ZERO, []), false),
        ("60006000680100000000000000006000f5", inner.create2_from_code(B256::ZERO, []), true),
        ("6000621000016000f0", inner.create(1), true),
        ("f0", inner.create(1), false),
        ("60006210000060006001f55000", inner.create2_from_code(B256::ZERO, []), false),
        ("60006000600060006001604461fffff15000", call_target, false),
        ("6044ff", call_target, false),
    ] {
        provider.add_account(
            inner,
            ExtendedAccount::new(1, U256::ZERO).with_bytecode(code.parse().unwrap()),
        );
        for mux in [false, true] {
            let opts = prestate_options(json!({"includeEmpty":true}), mux);
            let request = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{"to":target,"gas":"0x200000"},"latest",opts]});
            let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{response}");
            let pre = if mux { &response["result"]["prestateTracer"] } else { &response["result"] };
            assert_eq!(
                pre.get(format!("{candidate:#x}")),
                expected.then(|| json!({"balance":"0x0"})).as_ref(),
                "{code}: {response}"
            );
            if mux {
                assert!(response["result"]["callTracer"].get("error").is_none(), "{response}");
            }
        }
    }
}

#[tokio::test]
async fn skipped_authorizations_retain_recoverable_authorities() {
    let auth =
        Authorization { chain_id: U256::from(5), address: Address::with_last_byte(0x55), nonce: 0 }
            .into_signed(Signature::test_signature());
    let authority = auth.recover_authority().unwrap();
    let (module, hash, target, _) = rpc!(Bytes::from_static(&[0]), Some(auth.clone()));
    for mux in [false, true] {
        for include_empty in [false, true] {
            let opts = prestate_options(json!({"includeEmpty":include_empty}), mux);
            for (method, params) in [
                ("debug_traceTransaction", json!([hash, opts])),
                (
                    "debug_traceCall",
                    json!([{"to":target,"gas":"0x7a120","authorizationList":[auth]},"latest",opts]),
                ),
            ] {
                let request = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
                let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
                let response: Value = serde_json::from_str(response.get()).unwrap();
                assert!(response.get("error").is_none(), "{response}");
                let pre =
                    if mux { &response["result"]["prestateTracer"] } else { &response["result"] };
                assert_eq!(
                    pre.get(format!("{authority:#x}")),
                    include_empty.then(|| json!({"balance":"0x0"})).as_ref(),
                    "{response}"
                );
                if mux {
                    assert!(response["result"]["callTracer"].get("error").is_none(), "{response}");
                }
            }
        }
    }
}

#[tokio::test]
async fn prestate_config_matches_geth_casing_and_type_errors() {
    let (module, hash, target, _) = rpc!(Bytes::from_static(&[0]));
    for mux in [false, true] {
        for (config, message) in [
            (json!({"IncludeEmpty":true,"diffMode":true}), "cannot use diffMode with includeEmpty"),
            (json!({"includeEmpty":true,"DiffMode":true}), "cannot use diffMode with includeEmpty"),
            (json!({"includeEmpty":"true"}), "json: cannot unmarshal string into Go struct field PrestateTracerConfig.includeEmpty of type bool"),
            (json!({"IncludeEmpty":42}), "json: cannot unmarshal number into Go struct field PrestateTracerConfig.includeEmpty of type bool"),
            (json!({"DisableStorage":[]}), "json: cannot unmarshal array into Go struct field PrestateTracerConfig.disableStorage of type bool"),
            (json!({"DIFFMODE":{}}), "json: cannot unmarshal object into Go struct field PrestateTracerConfig.diffMode of type bool"),
        ] {
            let opts = prestate_options(config, mux);
            for (method, params) in [
                ("debug_traceCall", json!([{"to":target},"latest",opts])),
                ("debug_traceTransaction", json!([hash,opts])),
            ] {
                let request = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
                let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
                let response: Value = serde_json::from_str(response.get()).unwrap();
                assert_eq!(response["error"], json!({"code":-32000,"message":message}), "{request}: {response}");
            }
        }
        for config in [
            json!({"INCLUDEEMPTY":true,"DISABLECODE":true,"DisableStorage":null}),
            json!({"DiffMode":true}),
        ] {
            let opts = prestate_options(config.clone(), mux);
            let request = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{"to":target},"latest",opts]});
            let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{response}");
            let pre = if mux { &response["result"]["prestateTracer"] } else { &response["result"] };
            if config.get("DiffMode").is_some() {
                assert!(pre.get("pre").is_some() && pre.get("post").is_some(), "{response}");
            } else {
                assert!(pre[format!("{target:#x}")].get("code").is_none(), "{response}");
                assert_eq!(
                    pre[format!("{:#x}", Address::with_last_byte(0x99))],
                    json!({"balance":"0x0"})
                );
            }
        }
    }
}

#[tokio::test]
async fn rejected_creations_retain_prefunded_candidates_in_default_mode() {
    for (code, create2) in [("600060006001f05000", false), ("6000600060006001f55000", true)] {
        let (module, _, target, provider) = rpc!(code.parse::<Bytes>().unwrap());
        let candidate =
            if create2 { target.create2_from_code(B256::ZERO, []) } else { target.create(1) };
        provider.add_account(candidate, ExtendedAccount::new(0, U256::from(7)));
        for mux in [false, true] {
            for config in [json!({}), json!({"includeEmpty":false}), json!({"includeEmpty":true})] {
                let opts = prestate_options(config, mux);
                let request = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{"to":target,"gas":"0x7a120"},"latest",opts]});
                let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
                let response: Value = serde_json::from_str(response.get()).unwrap();
                assert!(response.get("error").is_none(), "{response}");
                let pre =
                    if mux { &response["result"]["prestateTracer"] } else { &response["result"] };
                assert_eq!(pre[format!("{candidate:#x}")], json!({"balance":"0x7"}), "{response}");
            }
        }
    }
}

#[tokio::test]
async fn call_gas_checks_do_not_add_unobserved_recipients() {
    let (module, _, target, _) =
        rpc!("60006000600060006001604461fffff15000".parse::<Bytes>().unwrap());
    let recipient = format!("{:#x}", Address::with_last_byte(0x44));
    for mux in [false, true] {
        for (gas, expected) in [("0x9c40", false), ("0x186a0", true)] {
            let opts = prestate_options(json!({"includeEmpty":true}), mux);
            let request = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{"to":target,"gas":gas},"latest",opts]});
            let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{response}");
            let pre = if mux { &response["result"]["prestateTracer"] } else { &response["result"] };
            assert_eq!(
                pre.get(&recipient),
                expected.then(|| json!({"balance":"0x0"})).as_ref(),
                "gas={gas}: {response}"
            );
            if mux {
                if expected {
                    assert!(response["result"]["callTracer"].get("error").is_none(), "{response}");
                    assert_eq!(
                        response["result"]["callTracer"]["calls"][0]["error"],
                        "insufficient balance for transfer"
                    );
                } else {
                    assert_eq!(response["result"]["callTracer"]["error"], "out of gas");
                }
            }
        }
    }
}

#[tokio::test]
async fn failed_call_gas_checks_preserve_real_reads_across_reverts() {
    let inner = Address::with_last_byte(0x43);
    let recipient = Address::with_last_byte(0x44);
    for (before, after) in [(false, false), (true, false), (false, true)] {
        // The outer call catches the inner out-of-gas failure and optionally reads the recipient.
        let code =
            format!("600060006000600060006043614a38f150{}00", if after { "60443150" } else { "" });
        let (module, _, target, provider) = rpc!(code.parse::<Bytes>().unwrap());
        let inner_code =
            format!("{}60006000600060006001604461fffff15000", if before { "60443150" } else { "" });
        provider.add_account(
            inner,
            ExtendedAccount::new(1, U256::ZERO).with_bytecode(inner_code.parse().unwrap()),
        );
        for mux in [false, true] {
            for access_list in [false, true] {
                let mut call = json!({"to":target,"gas":"0x7a120"});
                if access_list {
                    call["accessList"] = json!([{"address":recipient,"storageKeys":[]}]);
                }
                let opts = prestate_options(json!({"includeEmpty":true}), mux);
                let request = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[call,"latest",opts]});
                let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
                let response: Value = serde_json::from_str(response.get()).unwrap();
                assert!(response.get("error").is_none(), "{response}");
                let pre =
                    if mux { &response["result"]["prestateTracer"] } else { &response["result"] };
                assert_eq!(
                    pre.get(format!("{recipient:#x}")),
                    (before || after).then(|| json!({"balance":"0x0"})).as_ref(),
                    "before={before}, after={after}, access_list={access_list}: {response}"
                );
                if mux {
                    assert!(response["result"]["callTracer"].get("error").is_none(), "{response}");
                    assert_eq!(response["result"]["callTracer"]["calls"][0]["error"], "out of gas");
                }
            }
        }
    }
}

#[tokio::test]
async fn prestate_aliases_follow_json_input_order() {
    let (module, hash, _, _) = rpc!(Bytes::from_static(&[0]));
    for mux in [false, true] {
        for (config, expected) in [
            (r#"{"includeEmpty":false,"IncludeEmpty":true}"#, true),
            (r#"{"IncludeEmpty":true,"includeEmpty":false}"#, false),
            (r#"{"includeEmpty":true,"IncludeEmpty":null}"#, true),
        ] {
            // Construct raw JSON so the test preserves wire order before RPC deserialization.
            let opts = if mux {
                format!(r#"{{"tracer":"muxTracer","tracerConfig":{{"prestateTracer":{config}}}}}"#)
            } else {
                format!(r#"{{"tracer":"prestateTracer","tracerConfig":{config}}}"#)
            };
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"debug_traceTransaction","params":["{hash:#x}",{opts}]}}"#
            );
            let (response, _) = module.raw_json_request(&request, 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{response}");
            let pre = if mux { &response["result"]["prestateTracer"] } else { &response["result"] };
            assert_eq!(
                pre.get(format!("{:#x}", Address::with_last_byte(0x99))),
                expected.then(|| json!({"balance":"0x0"})).as_ref(),
                "{config}: {response}"
            );
        }
        for config in [
            r#"{"diffMode":true,"includeEmpty":false,"IncludeEmpty":true}"#,
            r#"{"includeEmpty":true,"diffMode":false,"DiffMode":true}"#,
        ] {
            let opts = if mux {
                format!(r#"{{"tracer":"muxTracer","tracerConfig":{{"prestateTracer":{config}}}}}"#)
            } else {
                format!(r#"{{"tracer":"prestateTracer","tracerConfig":{config}}}"#)
            };
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"debug_traceTransaction","params":["{hash:#x}",{opts}]}}"#
            );
            let (response, _) = module.raw_json_request(&request, 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert_eq!(
                response["error"],
                json!({"code":-32000,"message":"cannot use diffMode with includeEmpty"}),
                "{config}: {response}"
            );
        }
    }
}

#[tokio::test]
async fn selfdestruct_gas_checks_preserve_only_observed_beneficiaries() {
    let child = Address::with_last_byte(0x43);
    let beneficiary = Address::with_last_byte(0x44);
    for (gas, completes) in [(19_000, false), (65_535, true)] {
        for (before, after) in [(false, false), (true, false), (false, true)] {
            let code = format!(
                "60006000600060006000604361{gas:04x}f150{}00",
                if after { "60443150" } else { "" }
            );
            let (module, hash, target, provider) = rpc!(code.parse::<Bytes>().unwrap());
            let code = format!("{}6044ff", if before { "60443150" } else { "" });
            provider.add_account(
                child,
                ExtendedAccount::new(1, U256::from(1)).with_bytecode(code.parse().unwrap()),
            );
            for mux in [false, true] {
                let opts = prestate_options(json!({"includeEmpty":true}), mux);
                for (method, params) in [
                    ("debug_traceCall", json!([{"to":target,"gas":"0x7a120"},"latest",opts])),
                    ("debug_traceTransaction", json!([hash, opts])),
                ] {
                    let request = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
                    let (response, _) =
                        module.raw_json_request(&request.to_string(), 1).await.unwrap();
                    let response: Value = serde_json::from_str(response.get()).unwrap();
                    assert!(response.get("error").is_none(), "{response}");
                    let pre = if mux {
                        &response["result"]["prestateTracer"]
                    } else {
                        &response["result"]
                    };
                    assert_eq!(
                        pre.get(format!("{beneficiary:#x}")),
                        (completes || before || after).then(|| json!({"balance":"0x0"})).as_ref(),
                        "gas={gas}, before={before}, after={after}: {response}"
                    );
                    if mux {
                        assert!(
                            response["result"]["callTracer"].get("error").is_none(),
                            "{response}"
                        );
                        assert_eq!(
                            response["result"]["callTracer"]["calls"][0].get("error"),
                            (!completes).then(|| json!("out of gas")).as_ref(),
                            "{response}"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn prestate_config_rejects_non_objects_at_rpc_boundary() {
    let (module, hash, target, _) = rpc!(Bytes::from_static(&[0]));
    for mux in [false, true] {
        for (config, kind) in [
            (json!([]), "array"),
            (json!([true]), "array"),
            (json!(true), "bool"),
            (json!(42), "number"),
            (json!("invalid"), "string"),
        ] {
            let opts = prestate_options(config, mux);
            for (method, params) in [
                ("debug_traceCall", json!([{"to":target},"latest",opts])),
                ("debug_traceTransaction", json!([hash, opts])),
            ] {
                let request = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
                let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
                let response: Value = serde_json::from_str(response.get()).unwrap();
                assert_eq!(
                    response["error"],
                    json!({"code":-32000,"message":format!("json: cannot unmarshal {kind} into Go value of type native.PrestateTracerConfig")}),
                    "{response}"
                );
            }
        }
        if !mux {
            let opts = prestate_options(Value::Null, mux);
            let request = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{"to":target},"latest",opts]});
            let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{response}");
            assert!(response["result"].get(format!("{target:#x}")).is_some(), "{response}");
        }
    }
}

#[tokio::test]
async fn delegation_after_failed_call() {
    // Catch an out-of-gas call to 0x43, then call a 7702 authority delegating to 0x44.
    let code = "600060006000600060006043614a38f15060006000600060006000604561fffff15000";
    let (module, _, target, provider) = rpc!(code.parse::<Bytes>().unwrap());
    provider.add_account(
        Address::with_last_byte(0x43),
        ExtendedAccount::new(1, U256::ZERO)
            .with_bytecode("60006000600060006001604461fffff15000".parse().unwrap()),
    );
    let delegate = Address::with_last_byte(0x44);
    provider.add_account(
        Address::with_last_byte(0x45),
        ExtendedAccount::new(1, U256::ZERO).with_bytecode(
            format!("ef0100{}", alloy_primitives::hex::encode(delegate)).parse().unwrap(),
        ),
    );
    let mut mismatches = Vec::new();
    for mux in [false, true] {
        let opts = prestate_options(json!({"includeEmpty":true}), mux);
        let req = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{"to":target,"gas":"0x7a120"},"latest",opts]});
        let (res, _) = module.raw_json_request(&req.to_string(), 1).await.unwrap();
        let res: Value = serde_json::from_str(res.get()).unwrap();
        assert!(res.get("error").is_none(), "{res}");
        let pre = if mux { &res["result"]["prestateTracer"] } else { &res["result"] };
        if pre.get(format!("{delegate:#x}")) != Some(&json!({"balance":"0x0"})) {
            mismatches.push(mux);
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:?}");
}

#[tokio::test]
async fn nonstatic_create2_padding() {
    let mut mismatches = Vec::new();
    for (offset, expected) in [(0x100000, true), (0x100001, false)] {
        let code = format!("6000600062{offset:06x}6000f55000");
        let (module, _, target, _) = rpc!(code.parse::<Bytes>().unwrap());
        let candidate = target.create2_from_code(B256::ZERO, []);
        for mux in [false, true] {
            let opts = prestate_options(json!({"includeEmpty":true}), mux);
            let req = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{"to":target,"gas":"0x7a120"},"latest",opts]});
            let (res, _) = module.raw_json_request(&req.to_string(), 1).await.unwrap();
            let res: Value = serde_json::from_str(res.get()).unwrap();
            assert!(res.get("error").is_none(), "{res}");
            let pre = if mux { &res["result"]["prestateTracer"] } else { &res["result"] };
            if pre.get(format!("{candidate:#x}")).is_some() != expected {
                mismatches.push((offset, mux));
            }
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:?}");
}

#[tokio::test]
async fn repeated_alias_order() {
    let (module, hash, _, _) = rpc!(Bytes::from_static(&[0]));
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"debug_traceTransaction","params":["{hash:#x}",{{"tracer":"prestateTracer","tracerConfig":{{"includeEmpty":false,"IncludeEmpty":false,"includeEmpty":true}}}}]}}"#
    );
    let (res, _) = module.raw_json_request(&req, 1).await.unwrap();
    let res: Value = serde_json::from_str(res.get()).unwrap();
    assert!(res["result"].get(format!("{:#x}", Address::with_last_byte(0x99))).is_some(), "{res}");
}

/// The same JSON cases are executed by testdata/prestate/generate.go in Go-BSC.
#[tokio::test]
async fn go_bsc_differential_prestate_matrix() {
    let cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../../../testdata/prestate/cases.json")).unwrap();
    let expected: Value =
        serde_json::from_str(include_str!("../../../testdata/prestate/expected.json")).unwrap();
    let mut mismatches = Vec::new();
    for case in cases {
        let (module, _, target, provider) = rpc!(Bytes::from_static(&[0]));
        for (address, account) in case["accounts"].as_object().unwrap() {
            let balance = account
                .get("balance")
                .map(|v| serde_json::from_value::<U256>(v.clone()).unwrap())
                .unwrap_or_default();
            let mut state = ExtendedAccount::new(account["nonce"].as_u64().unwrap(), balance)
                .with_bytecode(account["code"].as_str().unwrap().parse().unwrap());
            if let Some(storage) = account.get("storage") {
                state = state.extend_storage(storage.as_object().unwrap().iter().map(
                    |(key, value)| {
                        (
                            key.parse::<B256>().unwrap(),
                            value.as_str().unwrap().parse::<U256>().unwrap(),
                        )
                    },
                ));
            }
            provider.add_account(address.parse().unwrap(), state);
        }
        for mux in [false, true] {
            let name = format!("{}/mux={mux}", case["name"].as_str().unwrap());
            let config = case["config"].as_str().unwrap();
            let options = if mux {
                format!(r#"{{"tracer":"muxTracer","tracerConfig":{{"prestateTracer":{config}}}}}"#)
            } else {
                format!(r#"{{"tracer":"prestateTracer","tracerConfig":{config}}}"#)
            };
            let call = json!({"to":target,"gas":format!("0x{:x}",case["gas"].as_u64().unwrap()),"input":case["input"]});
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{call},"latest",{options}]}}"#
            );
            let (response, _) = module.raw_json_request(&request, 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            let actual = if expected[&name].get("error").is_some() {
                assert_eq!(response["error"]["code"], -32000, "{name}: {response}");
                json!({"error":response["error"]["message"]})
            } else if mux {
                response["result"]["prestateTracer"].clone()
            } else {
                response["result"].clone()
            };
            if actual != expected[&name] {
                mismatches.push(format!("{name}: expected {}, got {actual}", expected[&name]));
            }
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[derive(Debug, Clone, Copy)]
struct NativeReadEvmFactory;

impl EvmFactory for NativeReadEvmFactory {
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> =
        <EthEvmFactory as EvmFactory>::Evm<DB, I>;
    type Context<DB: Database> = <EthEvmFactory as EvmFactory>::Context<DB>;
    type Tx = <EthEvmFactory as EvmFactory>::Tx;
    type Error<E: DBErrorMarker> = <EthEvmFactory as EvmFactory>::Error<E>;
    type HaltReason = <EthEvmFactory as EvmFactory>::HaltReason;
    type Spec = <EthEvmFactory as EvmFactory>::Spec;
    type BlockEnv = <EthEvmFactory as EvmFactory>::BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        env: alloy_evm::EvmEnv,
    ) -> Self::Evm<DB, NoOpInspector> {
        let mut evm = EthEvmFactory::default().create_evm(db, env);
        install_native_reader(evm.precompiles_mut());
        evm
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        env: alloy_evm::EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let mut evm = EthEvmFactory::default().create_evm_with_inspector(db, env, inspector);
        install_native_reader(evm.precompiles_mut());
        evm
    }
}

fn install_native_reader(precompiles: &mut PrecompilesMap) {
    precompiles.extend_precompiles([(
        Address::with_last_byte(0x20),
        DynPrecompile::new_stateful(PrecompileId::custom("native-reader"), |_input| {
            reth_revm::native_reads::account(Address::with_last_byte(0x44));
            Ok(PrecompileOutput::new(10, Bytes::new(), 0))
        }),
    )]);
}

#[tokio::test]
async fn native_reads_from_replay_do_not_leak_into_the_target_transaction() {
    let preceding = TransactionSigned::new_unhashed(
        TxLegacy {
            gas_limit: 100_000,
            to: Address::with_last_byte(0x20).into(),
            ..Default::default()
        }
        .into(),
        Signature::test_signature(),
    );
    let preceding_hash = *preceding.hash();
    let (module, hash, _, _) =
        rpc!(Bytes::from_static(&[0]), None, NativeReadEvmFactory, vec![preceding]);
    for mux in [false, true] {
        let options = prestate_options(json!({"includeEmpty":true}), mux);
        for (tx, observed) in [(preceding_hash, true), (hash, false)] {
            let request = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceTransaction","params":[tx,options]});
            let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{response}");
            let pre = if mux { &response["result"]["prestateTracer"] } else { &response["result"] };
            assert_eq!(
                pre.get(format!("{:#x}", Address::with_last_byte(0x44))).is_some(),
                observed,
                "{response}"
            );
        }
        let request = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceBlockByNumber","params":["0x1",options]});
        let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
        let response: Value = serde_json::from_str(response.get()).unwrap();
        for (index, trace) in response["result"].as_array().unwrap().iter().enumerate() {
            let pre = if mux { &trace["result"]["prestateTracer"] } else { &trace["result"] };
            assert_eq!(
                pre.get(format!("{:#x}", Address::with_last_byte(0x44))).is_some(),
                index == 0,
                "{response}"
            );
        }
    }
}
