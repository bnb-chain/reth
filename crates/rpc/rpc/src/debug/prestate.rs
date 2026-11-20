//! Preserve empty prestate accounts while the transaction's original database is available.

use alloy_consensus::TxType;
use alloy_primitives::{map::AddressMap, Address, TxKind, B256, U256};
use alloy_rpc_types_trace::geth::{
    AccountState, GethDebugBuiltInTracerType, GethDebugTracerConfig, GethDebugTracerType,
    GethDebugTracingOptions, GethTrace, PreStateFrame,
};
use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};
use reth_revm::native_reads::NativeReads;
use reth_rpc_api::prestate::normalize_prestate_config;
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
    DatabaseRef, Inspector,
};
use revm_inspectors::tracing::DebugInspectorError;
use std::collections::BTreeSet;

/// Preserves prestate configuration and account observations alongside `DebugInspector`.
#[derive(Default)]
pub(super) enum PrestateInspector {
    #[default]
    None,
    Prestate {
        include_empty: bool,
        code_enabled: bool,
        storage_enabled: bool,
        observed_accounts: AddressMap<BTreeSet<U256>>,
        pending_access: Option<(u8, Address, Option<U256>)>,
        prague: bool,
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
                normalize_prestate_config(&mut config.0).map_err(EthApiError::other)?;
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
                    prague: false,
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

    /// Whether this tracer needs explicit native observations.
    pub(super) fn is_active(&self) -> bool {
        match self {
            Self::None => false,
            Self::Prestate { .. } => true,
            Self::Mux(plans) => plans.iter().any(|(_, plan)| plan.is_active()),
        }
    }

    /// Build prestate from observed reads and the original database, before committing changes.
    pub(super) fn apply<DB: DatabaseRef>(
        &mut self,
        trace: &mut GethTrace,
        native: &NativeReads,
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
                    pending_access,
                    prague,
                },
                GethTrace::PreStateTracer(PreStateFrame::Default(pre)),
            ) => {
                observed_accounts.entry(tx.caller()).or_default();
                observed_accounts.entry(beneficiary).or_default();
                let to = match tx.kind() {
                    TxKind::Call(to) => to,
                    TxKind::Create => {
                        let caller = db
                            .basic_ref(tx.caller())
                            .map_err(DebugInspectorError::Database)?
                            .unwrap_or_default();
                        tx.caller().create(caller.nonce)
                    }
                };
                observed_accounts.entry(to).or_default();
                if *prague && tx.kind().is_call() {
                    let original = db
                        .basic_ref(to)
                        .map_err(DebugInspectorError::Database)?
                        .unwrap_or_default();
                    let code = match original.code {
                        Some(code) => code,
                        None => db
                            .code_by_hash_ref(original.code_hash)
                            .map_err(DebugInspectorError::Database)?,
                    };
                    if let Some(delegate) = code.eip7702_address() {
                        observed_accounts.entry(delegate).or_default();
                    }
                }
                if tx.tx_type() == TxType::Eip7702 as u8 {
                    for authority in tx.authorization_list().filter_map(|auth| auth.authority()) {
                        observed_accounts.entry(authority).or_default();
                    }
                }
                for (address, slots) in &native.accounts {
                    observed_accounts.entry(*address).or_default().extend(slots);
                }
                // The upstream builder uses final journal membership. Replace its default frame:
                // gas-only loads are not observations, while reads in reverted frames still are.
                pre.0.clear();
                for (address, slots) in std::mem::take(observed_accounts) {
                    let original = db
                        .basic_ref(address)
                        .map_err(DebugInspectorError::Database)?
                        .unwrap_or_default();
                    if !*include_empty && original.is_empty() {
                        continue
                    }
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
                    let mut account =
                        AccountState::from_account_info(original.nonce, original.balance, code);
                    if *storage_enabled {
                        for key in slots {
                            let value = db
                                .storage_ref(address, key)
                                .map_err(DebugInspectorError::Database)?;
                            account.storage.insert(B256::from(key), B256::from(value));
                        }
                    }
                    pre.0.insert(address, account);
                }
                *pending_access = None;
            }
            (Self::Mux(plans), GethTrace::MuxTracer(frame)) => {
                for (tracer, plan) in plans {
                    if let Some(trace) = frame.0.get_mut(tracer) {
                        plan.apply(trace, native, db, tx, beneficiary)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn record_address(&mut self, address: Address) {
        match self {
            Self::Prestate { observed_accounts, .. } => {
                observed_accounts.entry(address).or_default();
            }
            Self::Mux(plans) => {
                for (_, plan) in plans {
                    plan.record_address(address);
                }
            }
            Self::None => {}
        }
    }

    fn set_prague(&mut self, enabled: bool) {
        match self {
            Self::Prestate { prague, .. } => *prague = enabled,
            Self::Mux(plans) => {
                for (_, plan) in plans {
                    plan.set_prague(enabled);
                }
            }
            Self::None => {}
        }
    }
}

impl<CTX: ContextTr<Journal: JournalExt>> Inspector<CTX> for PrestateInspector {
    fn create(&mut self, context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.set_prague(context.cfg().spec().into().is_enabled_in(SpecId::PRAGUE));
        None
    }

    fn call(&mut self, context: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.set_prague(context.cfg().spec().into().is_enabled_in(SpecId::PRAGUE));
        None
    }

    fn step(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        if let Self::Mux(plans) = self {
            for (_, plan) in plans {
                plan.step(interp, context);
            }
            return
        }
        if let Self::Prestate { pending_access, .. } = self {
            let op = interp.bytecode.opcode();
            let index = match op {
                opcode::SLOAD |
                opcode::SSTORE |
                opcode::BALANCE |
                opcode::EXTCODESIZE |
                opcode::EXTCODEHASH |
                opcode::EXTCODECOPY |
                opcode::SELFDESTRUCT => Some(0),
                opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL | opcode::STATICCALL => {
                    Some(1)
                }
                _ => None,
            };
            *pending_access = index.and_then(|index| {
                let Ok(word) = interp.stack.peek(index) else { return None };
                Some(if matches!(op, opcode::SLOAD | opcode::SSTORE) {
                    (op, interp.input.target_address, Some(word))
                } else {
                    (op, Address::from_word(B256::from(word)), None)
                })
            });
        }
        if matches!(self, Self::None) {
            return
        }
        // Go observes CREATE before execution-time failures, but after stack and gas checks.
        // Derive both static and non-static candidates here, where the original memory offset is
        // still available (CreateInputs has already discarded it).
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
        let memory_offset = if size == 0 {
            0
        } else {
            let Ok(offset) = usize::try_from(offset) else { return };
            offset
        };
        let Some(end) = memory_offset.checked_add(size) else { return };
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
            // Go's tracer uses the original offset's low 64 bits even for zero-size initcode.
            let Ok(offset) = usize::try_from(offset.wrapping_to::<u64>()) else { return };
            let Some(end) = offset.checked_add(size) else { return };
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
            Self::Prestate { pending_access, observed_accounts, .. } => {
                if let Some((op, address, slot)) = pending_access.take() {
                    let halted = interp
                        .bytecode
                        .action()
                        .as_ref()
                        .and_then(|action| action.instruction_result())
                        .is_some_and(|result| result.is_halt());
                    if halted {
                        return
                    }
                    let slots = observed_accounts.entry(address).or_default();
                    if let Some(slot) = slot {
                        slots.insert(slot);
                    }
                    if matches!(
                        op,
                        opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL | opcode::STATICCALL
                    ) && interp.runtime_flag.spec_id().is_enabled_in(SpecId::PRAGUE) &&
                        let Some(delegate) = context
                            .journal_ref()
                            .evm_state()
                            .get(&address)
                            .and_then(|account| account.info.code.as_ref())
                            .and_then(|code| code.eip7702_address())
                    {
                        observed_accounts.entry(delegate).or_default();
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use alloy_rpc_types_trace::geth::PreStateMode;
    use revm::{
        context::TxEnv,
        database::{CacheDB, EmptyDB},
        state::AccountInfo,
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
        let reads = NativeReads {
            accounts: [(empty, [U256::ZERO].into()), (existing, Default::default())]
                .into_iter()
                .collect(),
        };
        for storage_enabled in [false, true] {
            let mut trace =
                GethTrace::PreStateTracer(PreStateFrame::Default(PreStateMode::default()));
            PrestateInspector::Prestate {
                include_empty: true,
                code_enabled: true,
                storage_enabled,
                observed_accounts: Default::default(),
                prague: false,
                pending_access: None,
            }
            .apply(&mut trace, &reads, &db, &TxEnv::default(), Address::ZERO)
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
            // Rebuilding from the same observations must produce the same original state.
            PrestateInspector::Prestate {
                include_empty: true,
                code_enabled: true,
                storage_enabled,
                observed_accounts: Default::default(),
                prague: false,
                pending_access: None,
            }
            .apply(&mut trace, &reads, &db, &TxEnv::default(), Address::ZERO)
            .unwrap();
            assert_eq!(serde_json::to_value(trace).unwrap(), value);
        }
    }
}

#[cfg(test)]
mod rpc_tests;
