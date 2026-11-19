//! Preserve empty prestate accounts while the transaction's original database is available.

use alloy_primitives::{map::AddressHashSet, B256};
use alloy_rpc_types_trace::geth::{
    AccountState, GethDebugBuiltInTracerType, GethDebugTracerConfig, GethDebugTracerType,
    GethDebugTracingOptions, GethTrace, PreStateFrame,
};
use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};
use reth_rpc_eth_types::EthApiError;
use revm::{
    context::JournalTr,
    context_interface::ContextTr,
    inspector::JournalExt,
    interpreter::{CreateInputs, CreateOutcome},
    state::EvmState,
    DatabaseRef, Inspector,
};
use revm_inspectors::tracing::DebugInspectorError;
use std::collections::btree_map::Entry;

/// Preserves `includeEmpty` configuration and creation candidates alongside `DebugInspector`.
#[derive(Default)]
pub(super) enum PrestateInspector {
    #[default]
    None,
    IncludeEmpty {
        storage_enabled: bool,
        creation_candidates: AddressHashSet,
    },
    Mux(Vec<(GethDebugTracerType, Self)>),
}

impl PrestateInspector {
    pub(super) fn new(opts: &GethDebugTracingOptions) -> Result<Self, EthApiError> {
        match &opts.tracer {
            Some(tracer) => Self::from_config(tracer, &opts.tracer_config),
            None => Ok(Self::None),
        }
    }

    fn from_config(
        tracer: &GethDebugTracerType,
        config: &GethDebugTracerConfig,
    ) -> Result<Self, EthApiError> {
        match tracer {
            GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::PreStateTracer) => {
                let include_empty = config
                    .0
                    .get("includeEmpty")
                    .map(|value| serde_json::from_value::<Option<bool>>(value.clone()))
                    .transpose()
                    .map_err(|err| EthApiError::InvalidParams(err.to_string()))?
                    .flatten()
                    .unwrap_or_default();
                if !include_empty {
                    return Ok(Self::None)
                }
                let config = config
                    .clone()
                    .into_pre_state_config()
                    .map_err(|err| EthApiError::InvalidParams(err.to_string()))?;
                if config.is_diff_mode() {
                    return Err(EthApiError::other(ErrorObjectOwned::owned(
                        CALL_EXECUTION_FAILED_CODE,
                        "cannot use diffMode with includeEmpty",
                        None::<()>,
                    )))
                }
                Ok(Self::IncludeEmpty {
                    storage_enabled: config.storage_enabled(),
                    creation_candidates: Default::default(),
                })
            }
            GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::MuxTracer) => {
                let config = config
                    .clone()
                    .into_mux_config()
                    .map_err(|err| EthApiError::InvalidParams(err.to_string()))?;
                let plans = config
                    .0
                    .into_iter()
                    .map(|(tracer, config)| {
                        let plan = Self::from_config(&tracer, &config.unwrap_or_default())?;
                        Ok((tracer, plan))
                    })
                    .collect::<Result<_, EthApiError>>()?;
                Ok(Self::Mux(plans))
            }
            _ => Ok(Self::None),
        }
    }

    /// Complete the trace and drain creation candidates before committing transaction changes.
    pub(super) fn apply<DB: DatabaseRef>(
        &mut self,
        trace: &mut GethTrace,
        state: &EvmState,
        db: &DB,
    ) -> Result<(), DebugInspectorError<DB::Error>> {
        match (self, trace) {
            (
                Self::IncludeEmpty { storage_enabled, creation_candidates },
                GethTrace::PreStateTracer(PreStateFrame::Default(pre)),
            ) => {
                // The inspector omits empty accounts even when includeEmpty was requested.
                // Read the original database, never the post-execution account balance.
                for address in state.keys().copied().chain(creation_candidates.drain()) {
                    if let Entry::Vacant(entry) = pre.0.entry(address) {
                        let original = db
                            .basic_ref(address)
                            .map_err(DebugInspectorError::Database)?
                            .unwrap_or_default();
                        if original.is_empty() {
                            let mut account_state = AccountState::from_account_info(
                                original.nonce,
                                original.balance,
                                None,
                            );
                            if *storage_enabled && let Some(account) = state.get(&address) {
                                account_state.storage.extend(account.storage.iter().map(
                                    |(key, slot)| {
                                        (B256::from(*key), B256::from(slot.original_value))
                                    },
                                ));
                            }
                            entry.insert(account_state);
                        }
                    }
                }
            }
            (Self::Mux(plans), GethTrace::MuxTracer(frame)) => {
                for (tracer, plan) in plans {
                    if let Some(trace) = frame.0.get_mut(tracer) {
                        plan.apply(trace, state, db)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn record_create<CTX: ContextTr<Journal: JournalExt>>(
        &mut self,
        context: &CTX,
        inputs: &CreateInputs,
    ) {
        match self {
            Self::IncludeEmpty { creation_candidates, .. } => {
                // The paired DebugInspector loads the caller before this hook. REVM can reject
                // the endowment before loading the candidate into the transaction's state.
                if let Some(caller) = context.journal_ref().evm_state().get(&inputs.caller()) {
                    creation_candidates.insert(inputs.created_address(caller.info.nonce));
                }
            }
            Self::Mux(plans) => {
                for (_, plan) in plans {
                    plan.record_create(context, inputs);
                }
            }
            Self::None => {}
        }
    }
}

impl<CTX: ContextTr<Journal: JournalExt>> Inspector<CTX> for PrestateInspector {
    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.record_create(context, inputs);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use alloy_rpc_types_trace::geth::PreStateMode;
    use revm::{
        database::{CacheDB, EmptyDB},
        state::{Account, AccountInfo, EvmStorageSlot},
    };
    use serde_json::json;

    #[test]
    fn include_empty_config_validation() {
        for include_empty in [json!(false), json!(null)] {
            let opts = serde_json::from_value(json!({
                "tracer":"prestateTracer", "tracerConfig":{"includeEmpty":include_empty}
            }))
            .unwrap();
            assert!(matches!(PrestateInspector::new(&opts).unwrap(), PrestateInspector::None));
        }
        assert!(matches!(
            PrestateInspector::new(&GethDebugTracingOptions::default()).unwrap(),
            PrestateInspector::None
        ));
        for mux in [false, true] {
            let config = json!({"includeEmpty":true,"diffMode":true});
            let opts = serde_json::from_value(if mux {
                json!({"tracer":"muxTracer","tracerConfig":{"prestateTracer":config}})
            } else {
                json!({"tracer":"prestateTracer","tracerConfig":config})
            })
            .unwrap();
            let err = ErrorObjectOwned::from(PrestateInspector::new(&opts).err().unwrap());
            assert_eq!(err.code(), CALL_EXECUTION_FAILED_CODE);
            assert_eq!(err.message(), "cannot use diffMode with includeEmpty");
        }
        let opts = serde_json::from_value(json!({
            "tracer":"prestateTracer","tracerConfig":{"includeEmpty":"true"}
        }))
        .unwrap();
        let err = PrestateInspector::new(&opts).err().unwrap();
        assert!(
            matches!(err, EthApiError::InvalidParams(message) if message == "invalid type: string \"true\", expected a boolean")
        );
    }

    #[test]
    fn empty_accounts_use_pre_transaction_balance_and_storage() {
        let empty = Address::repeat_byte(1);
        let existing = Address::repeat_byte(2);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            existing,
            AccountInfo { balance: U256::from(7), ..Default::default() },
        );
        let mut changed =
            Account::from(AccountInfo { balance: U256::from(1), ..Default::default() });
        changed.storage.insert(
            U256::ZERO,
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(9), Default::default()),
        );
        let state = [(empty, changed), (existing, Account::default())].into_iter().collect();
        for storage_enabled in [false, true] {
            let mut trace =
                GethTrace::PreStateTracer(PreStateFrame::Default(PreStateMode::default()));
            PrestateInspector::IncludeEmpty {
                storage_enabled,
                creation_candidates: Default::default(),
            }
            .apply(&mut trace, &state, &db)
            .unwrap();
            let value = serde_json::to_value(&trace).unwrap();
            let account = &value[format!("{empty:#x}")];
            assert_eq!(account["balance"], "0x0");
            assert!(value.get(format!("{existing:#x}")).is_none());
            if storage_enabled {
                assert_eq!(
                    account["storage"][format!("{:#066x}", U256::ZERO)],
                    format!("{:#066x}", U256::ZERO)
                );
            } else {
                assert!(account.get("storage").is_none());
            }
            // Existing output must be preserved when filling missing accounts.
            PrestateInspector::IncludeEmpty {
                storage_enabled,
                creation_candidates: Default::default(),
            }
            .apply(&mut trace, &state, &db)
            .unwrap();
            assert_eq!(serde_json::to_value(trace).unwrap(), value);
        }
    }
}

#[cfg(test)]
mod rpc_tests;
