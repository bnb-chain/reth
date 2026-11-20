//! Preserve empty prestate accounts while the transaction's original database is available.

use alloy_consensus::TxType;
use alloy_primitives::{map::AddressHashSet, Address, B256};
use alloy_rpc_types_trace::geth::{
    AccountState, GethDebugBuiltInTracerType, GethDebugTracerConfig, GethDebugTracerType,
    GethDebugTracingOptions, GethTrace, PreStateFrame,
};
use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};
use reth_rpc_eth_types::EthApiError;
use revm::{
    bytecode::opcode,
    context::JournalTr,
    context_interface::{transaction::AuthorizationTr, Cfg, ContextTr, Transaction},
    inspector::JournalExt,
    interpreter::{
        interpreter_types::{Jumps, LoopControl, RuntimeFlag},
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter,
    },
    primitives::hardfork::SpecId,
    state::EvmState,
    DatabaseRef, Inspector,
};
use revm_inspectors::tracing::DebugInspectorError;
use std::collections::btree_map::Entry;

/// Preserves prestate configuration and account observations alongside `DebugInspector`.
#[derive(Default)]
pub(super) enum PrestateInspector {
    #[default]
    None,
    Prestate {
        include_empty: bool,
        code_enabled: bool,
        storage_enabled: bool,
        observed_accounts: AddressHashSet,
        gas_only_accounts: AddressHashSet,
        pending_access: Option<(u8, Address, bool)>,
    },
    Mux(Vec<(GethDebugTracerType, Self)>),
}

impl PrestateInspector {
    pub(super) fn new(opts: &mut GethDebugTracingOptions) -> Result<Self, EthApiError> {
        match &opts.tracer {
            Some(tracer) => Self::from_config(tracer, &mut opts.tracer_config),
            None => Ok(Self::None),
        }
    }

    fn from_config(
        tracer: &GethDebugTracerType,
        config: &mut GethDebugTracerConfig,
    ) -> Result<Self, EthApiError> {
        match tracer {
            GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::PreStateTracer) => {
                normalize_prestate_config(config)?;
                let include_empty = config
                    .0
                    .get("includeEmpty")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or_default();
                let config = config
                    .clone()
                    .into_pre_state_config()
                    .map_err(|err| EthApiError::InvalidParams(err.to_string()))?;
                if config.is_diff_mode() && include_empty {
                    return Err(EthApiError::other(ErrorObjectOwned::owned(
                        CALL_EXECUTION_FAILED_CODE,
                        "cannot use diffMode with includeEmpty",
                        None::<()>,
                    )))
                }
                if config.is_diff_mode() {
                    return Ok(Self::None)
                }
                Ok(Self::Prestate {
                    include_empty,
                    code_enabled: config.code_enabled(),
                    storage_enabled: config.storage_enabled(),
                    observed_accounts: Default::default(),
                    gas_only_accounts: Default::default(),
                    pending_access: None,
                })
            }
            GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::MuxTracer) => {
                let mut mux = config
                    .clone()
                    .into_mux_config()
                    .map_err(|err| EthApiError::InvalidParams(err.to_string()))?;
                let mut plans = Vec::with_capacity(mux.0.len());
                for (tracer, options) in &mut mux.0 {
                    let plan = match options {
                        Some(options) => Self::from_config(tracer, options)?,
                        None => Self::from_config(tracer, &mut GethDebugTracerConfig::default())?,
                    };
                    plans.push((tracer.clone(), plan));
                }
                config.0 = serde_json::to_value(mux)
                    .map_err(|err| EthApiError::InvalidParams(err.to_string()))?;
                if plans.iter().all(|(_, plan)| matches!(plan, Self::None)) {
                    return Ok(Self::None)
                }
                Ok(Self::Mux(plans))
            }
            _ => Ok(Self::None),
        }
    }

    /// Complete the trace and clear account observations before committing transaction changes.
    pub(super) fn apply<DB: DatabaseRef>(
        &mut self,
        trace: &mut GethTrace,
        state: &EvmState,
        db: &DB,
        tx: &impl Transaction,
        beneficiary: Address,
    ) -> Result<(), DebugInspectorError<DB::Error>> {
        match (self, trace) {
            (
                Self::Prestate {
                    include_empty,
                    code_enabled,
                    storage_enabled,
                    observed_accounts,
                    gas_only_accounts,
                    pending_access,
                },
                GethTrace::PreStateTracer(PreStateFrame::Default(pre)),
            ) => {
                // The inspector omits empty accounts even when includeEmpty was requested.
                // Read the original database, never the post-execution account balance.
                observed_accounts.insert(beneficiary);
                if tx.tx_type() == TxType::Eip7702 as u8 {
                    observed_accounts
                        .extend(tx.authorization_list().filter_map(|auth| auth.authority()));
                }
                // CALL can load an empty recipient to calculate its new-account gas surcharge,
                // then halt before Go's opcode hook observes that account. Storage accesses from
                // stateful precompiles are still real reads even if a prior CALL failed.
                let accounts = state.iter().filter_map(|(address, account)| {
                    (!gas_only_accounts.contains(address) || !account.storage.is_empty())
                        .then_some(*address)
                });
                for address in accounts.chain(observed_accounts.drain()) {
                    if let Entry::Vacant(entry) = pre.0.entry(address) {
                        let original = db
                            .basic_ref(address)
                            .map_err(DebugInspectorError::Database)?
                            .unwrap_or_default();
                        if *include_empty || !original.is_empty() {
                            let code = if *code_enabled && !original.is_empty_code_hash() {
                                Some(match original.code {
                                    Some(code) => code.original_bytes(),
                                    None => db
                                        .code_by_hash_ref(original.code_hash)
                                        .map_err(DebugInspectorError::Database)?
                                        .original_bytes(),
                                })
                            } else {
                                None
                            };
                            let mut account_state = AccountState::from_account_info(
                                original.nonce,
                                original.balance,
                                code,
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
                gas_only_accounts.clear();
                *pending_access = None;
            }
            (Self::Mux(plans), GethTrace::MuxTracer(frame)) => {
                for (tracer, plan) in plans {
                    if let Some(trace) = frame.0.get_mut(tracer) {
                        plan.apply(trace, state, db, tx, beneficiary)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn record_address(&mut self, address: Address) {
        match self {
            Self::Prestate { observed_accounts, gas_only_accounts, .. } => {
                gas_only_accounts.remove(&address);
                observed_accounts.insert(address);
            }
            Self::Mux(plans) => {
                for (_, plan) in plans {
                    plan.record_address(address);
                }
            }
            Self::None => {}
        }
    }
}

impl<CTX: ContextTr<Journal: JournalExt>> Inspector<CTX> for PrestateInspector {
    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        if let Some(caller) = context.journal_ref().evm_state().get(&inputs.caller()) {
            self.record_address(inputs.created_address(caller.info.nonce));
        }
        None
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.record_address(inputs.bytecode_address);
        None
    }

    fn step(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        if let Self::Mux(plans) = self {
            for (_, plan) in plans {
                plan.step(interp, context);
            }
            return
        }
        if let Self::Prestate { pending_access, gas_only_accounts, .. } = self {
            let op = interp.bytecode.opcode();
            let index = match op {
                opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL | opcode::STATICCALL => {
                    Some(1)
                }
                opcode::BALANCE |
                opcode::EXTCODESIZE |
                opcode::EXTCODEHASH |
                opcode::EXTCODECOPY |
                opcode::SELFDESTRUCT => Some(0),
                _ => None,
            };
            *pending_access =
                index.and_then(|index| {
                    let Ok(word) = interp.stack.peek(index) else { return None };
                    let address = Address::from_word(B256::from(word));
                    let previously_observed =
                        context.journal_ref().evm_state().contains_key(&address) &&
                            !gas_only_accounts.contains(&address);
                    Some((op, address, previously_observed))
                });
        }
        // REVM rejects static creation before invoking create; Go-BSC observes its opcode first.
        if matches!(self, Self::None) || !interp.runtime_flag.is_static() {
            return
        }
        let op = interp.bytecode.opcode();
        let stack_len = match op {
            opcode::CREATE => 3,
            opcode::CREATE2 if interp.runtime_flag.spec_id().is_enabled_in(SpecId::PETERSBURG) => 4,
            _ => return,
        };
        if interp.stack.len() < stack_len {
            return
        }
        let (Ok(offset), Ok(size)) = (interp.stack.peek(1), interp.stack.peek(2)) else { return };
        let Ok(size) = usize::try_from(size) else { return };
        let offset = if size == 0 {
            0
        } else {
            let Ok(offset) = usize::try_from(offset) else { return };
            offset
        };
        let Some(end) = offset.checked_add(size) else { return };
        let shanghai = interp.runtime_flag.spec_id().is_enabled_in(SpecId::SHANGHAI);
        if shanghai && size > context.cfg().max_initcode_size() {
            return
        }
        let gas = context.cfg().gas_params();
        let cost = if op == opcode::CREATE2 { gas.create2_cost(size) } else { gas.create_cost() }
            .saturating_add(
                gas.memory_cost(end.div_ceil(32))
                    .saturating_sub(gas.memory_cost(interp.memory.len().div_ceil(32))),
            )
            .saturating_add(if shanghai { gas.initcode_cost(size) } else { 0 });
        // Go invokes its opcode hook only after stack and dynamic gas checks succeed.
        if cost > interp.gas.remaining() {
            return
        }
        let caller = interp.input.target_address;
        if op == opcode::CREATE {
            if let Some(account) = context.journal_ref().evm_state().get(&caller) {
                self.record_address(caller.create(account.info.nonce));
            }
        } else if let Ok(salt) = interp.stack.peek(3) {
            // Match Go-BSC's GetMemoryCopyPadded limit without expanding EVM memory.
            const MEMORY_PADDING_LIMIT: usize = 1024 * 1024;
            if end.saturating_sub(interp.memory.len()) > MEMORY_PADDING_LIMIT {
                return
            }
            let mut init = vec![0; size];
            let overlap = size.min(interp.memory.len().saturating_sub(offset));
            if overlap > 0 {
                init[..overlap].copy_from_slice(&interp.memory.slice_len(offset, overlap));
            }
            self.record_address(caller.create2_from_code(B256::from(salt), init));
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        match self {
            Self::Prestate { pending_access, gas_only_accounts, observed_accounts, .. } => {
                if let Some((op, address, previously_observed)) = pending_access.take() {
                    let halted = interp
                        .bytecode
                        .action()
                        .as_ref()
                        .and_then(|action| action.instruction_result())
                        .is_some_and(|result| result.is_halt());
                    if !halted {
                        gas_only_accounts.remove(&address);
                        observed_accounts.insert(address);
                    } else if matches!(
                        op,
                        opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL | opcode::STATICCALL
                    ) && !previously_observed &&
                        !observed_accounts.contains(&address) &&
                        context.journal_ref().evm_state().contains_key(&address)
                    {
                        gas_only_accounts.insert(address);
                    }
                }
            }
            Self::Mux(plans) => {
                for (_, plan) in plans {
                    plan.step_end(interp, context);
                }
            }
            Self::None => {}
        }
    }
}

/// Normalize Go's case-insensitive boolean fields before Alloy constructs the actual tracer.
fn normalize_prestate_config(config: &mut GethDebugTracerConfig) -> Result<(), EthApiError> {
    let Some(object) = config.0.as_object_mut() else { return Ok(()) };
    let mut normalized = serde_json::Map::new();
    for (key, value) in std::mem::take(object) {
        let Some(field) = ["includeEmpty", "diffMode", "disableCode", "disableStorage"]
            .into_iter()
            .find(|field| key.eq_ignore_ascii_case(field))
        else {
            normalized.insert(key, value);
            continue
        };
        match value {
            serde_json::Value::Bool(_) => {
                normalized.insert(field.to_owned(), value);
            }
            serde_json::Value::Null => {}
            value => {
                let kind = match value {
                    serde_json::Value::String(_) => "string",
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::Array(_) => "array",
                    _ => "object",
                };
                return Err(EthApiError::other(ErrorObjectOwned::owned(
                    CALL_EXECUTION_FAILED_CODE,
                    format!("json: cannot unmarshal {kind} into Go struct field PrestateTracerConfig.{field} of type bool"),
                    None::<()>,
                )))
            }
        }
    }
    *object = normalized;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use alloy_rpc_types_trace::geth::PreStateMode;
    use revm::{
        context::TxEnv,
        database::{CacheDB, EmptyDB},
        state::{Account, AccountInfo, EvmStorageSlot},
    };
    use serde_json::json;

    #[test]
    fn include_empty_config_validation() {
        for include_empty in [json!(false), json!(null)] {
            let mut opts = serde_json::from_value(json!({
                "tracer":"prestateTracer", "tracerConfig":{"includeEmpty":include_empty}
            }))
            .unwrap();
            assert!(matches!(
                PrestateInspector::new(&mut opts).unwrap(),
                PrestateInspector::Prestate { include_empty: false, .. }
            ));
        }
        assert!(matches!(
            PrestateInspector::new(&mut GethDebugTracingOptions::default()).unwrap(),
            PrestateInspector::None
        ));
        for mux in [false, true] {
            let config = json!({"includeEmpty":true,"diffMode":true});
            let mut opts = serde_json::from_value(if mux {
                json!({"tracer":"muxTracer","tracerConfig":{"prestateTracer":config}})
            } else {
                json!({"tracer":"prestateTracer","tracerConfig":config})
            })
            .unwrap();
            let err = ErrorObjectOwned::from(PrestateInspector::new(&mut opts).err().unwrap());
            assert_eq!(err.code(), CALL_EXECUTION_FAILED_CODE);
            assert_eq!(err.message(), "cannot use diffMode with includeEmpty");
        }
        let mut opts = serde_json::from_value(json!({
            "tracer":"prestateTracer","tracerConfig":{"includeEmpty":"true"}
        }))
        .unwrap();
        let err = ErrorObjectOwned::from(PrestateInspector::new(&mut opts).err().unwrap());
        assert_eq!(err.code(), CALL_EXECUTION_FAILED_CODE);
        assert_eq!(err.message(), "json: cannot unmarshal string into Go struct field PrestateTracerConfig.includeEmpty of type bool");
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
            PrestateInspector::Prestate {
                include_empty: true,
                code_enabled: true,
                storage_enabled,
                observed_accounts: Default::default(),
                gas_only_accounts: Default::default(),
                pending_access: None,
            }
            .apply(&mut trace, &state, &db, &TxEnv::default(), Address::ZERO)
            .unwrap();
            let value = serde_json::to_value(&trace).unwrap();
            let account = &value[format!("{empty:#x}")];
            assert_eq!(account["balance"], "0x0");
            assert_eq!(value[format!("{existing:#x}")]["balance"], "0x7");
            if storage_enabled {
                assert_eq!(
                    account["storage"][format!("{:#066x}", U256::ZERO)],
                    format!("{:#066x}", U256::ZERO)
                );
            } else {
                assert!(account.get("storage").is_none());
            }
            // Existing output must be preserved when filling missing accounts.
            PrestateInspector::Prestate {
                include_empty: true,
                code_enabled: true,
                storage_enabled,
                observed_accounts: Default::default(),
                gas_only_accounts: Default::default(),
                pending_access: None,
            }
            .apply(&mut trace, &state, &db, &TxEnv::default(), Address::ZERO)
            .unwrap();
            assert_eq!(serde_json::to_value(trace).unwrap(), value);
        }
    }
}

#[cfg(test)]
mod rpc_tests;
