//! Preserve empty prestate accounts while the transaction's original database is available.

use alloy_primitives::B256;
use alloy_rpc_types_trace::geth::{
    AccountState, GethDebugBuiltInTracerType, GethDebugTracerConfig, GethDebugTracerType,
    GethDebugTracingOptions, GethTrace, PreStateFrame,
};
use reth_rpc_eth_types::EthApiError;
use revm::{state::EvmState, DatabaseRef};
use revm_inspectors::tracing::DebugInspectorError;
use std::collections::btree_map::Entry;

/// Options not yet represented by Alloy's prestate configuration.
#[derive(Default)]
pub(super) enum PrestateTraceOptions {
    #[default]
    None,
    IncludeEmpty {
        storage_enabled: bool,
    },
    Mux(Vec<(GethDebugTracerType, Self)>),
}

impl PrestateTraceOptions {
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
                    return Err(EthApiError::InvalidParams(
                        "cannot use diffMode with includeEmpty".to_owned(),
                    ))
                }
                Ok(Self::IncludeEmpty { storage_enabled: config.storage_enabled() })
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

    /// Complete the trace before committing the transaction's state changes.
    pub(super) fn apply<DB: DatabaseRef>(
        &self,
        trace: &mut GethTrace,
        state: &EvmState,
        db: &DB,
    ) -> Result<(), DebugInspectorError<DB::Error>> {
        match (self, trace) {
            (
                Self::IncludeEmpty { storage_enabled },
                GethTrace::PreStateTracer(PreStateFrame::Default(pre)),
            ) => {
                // The inspector omits empty accounts even when includeEmpty was requested.
                // Read the original database, never the post-execution account balance.
                for (address, account) in state {
                    if let Entry::Vacant(entry) = pre.0.entry(*address) {
                        let original = db
                            .basic_ref(*address)
                            .map_err(DebugInspectorError::Database)?
                            .unwrap_or_default();
                        if original.is_empty() {
                            let mut account_state = AccountState::from_account_info(
                                original.nonce,
                                original.balance,
                                None,
                            );
                            if *storage_enabled {
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
            assert!(matches!(
                PrestateTraceOptions::new(&opts).unwrap(),
                PrestateTraceOptions::None
            ));
        }
        assert!(matches!(
            PrestateTraceOptions::new(&GethDebugTracingOptions::default()).unwrap(),
            PrestateTraceOptions::None
        ));
        for mux in [false, true] {
            let config = json!({"includeEmpty":true,"diffMode":true});
            let opts = serde_json::from_value(if mux {
                json!({"tracer":"muxTracer","tracerConfig":{"prestateTracer":config}})
            } else {
                json!({"tracer":"prestateTracer","tracerConfig":config})
            })
            .unwrap();
            let err = PrestateTraceOptions::new(&opts).err().unwrap();
            assert!(
                matches!(err, EthApiError::InvalidParams(message) if message == "cannot use diffMode with includeEmpty")
            );
        }
        let opts = serde_json::from_value(json!({
            "tracer":"prestateTracer","tracerConfig":{"includeEmpty":"true"}
        }))
        .unwrap();
        let err = PrestateTraceOptions::new(&opts).err().unwrap();
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
            PrestateTraceOptions::IncludeEmpty { storage_enabled }
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
            PrestateTraceOptions::IncludeEmpty { storage_enabled }
                .apply(&mut trace, &state, &db)
                .unwrap();
            assert_eq!(serde_json::to_value(trace).unwrap(), value);
        }
    }
}
