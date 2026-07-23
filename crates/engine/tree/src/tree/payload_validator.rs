        let mut hashed_state_validate_result = debug_span!(
            target: "engine::tree::payload_validator",
            "validate_block_post_execution_with_hashed_state"
        )
        .in_scope(|| {
            self.validator.validate_block_post_execution_with_hashed_state(
                &|| hashed_state.get(),
                &block,
                || provider_builder.build(),
            )
        });

        let root_start = Instant::now();
        let root_outcome = ensure_ok_post_block!(
            state_root_job.finish(&block, output.clone(), &hashed_state),
            block
        );
        let root_elapsed = root_start.elapsed();

        info!(
            target: "engine::tree::payload_validator",
            strategy = state_root_job_name,
            state_root = ?root_outcome.state_root,
            elapsed = ?root_elapsed,
            "State root job finished"
        );

        let state_root = root_outcome.state_root;
        let trie_output = root_outcome.trie_updates;
        let changed_paths = root_outcome.changed_paths;

        // A fallback path recomputed the hashed post state. Replace the streaming-derived one
        // and re-run hashed-state validation against it, since a failed state-root task may
        // have produced an inconsistent byproduct.
        if let Some(refreshed) = root_outcome.hashed_state {
            hashed_state = LazyHandle::ready(refreshed);
            hashed_state_validate_result = debug_span!(
                target: "engine::tree::payload_validator",
                "validate_block_post_execution_with_hashed_state"
            )
            .in_scope(|| {
                self.validator.validate_block_post_execution_with_hashed_state(
                    &|| hashed_state.get(),
                    &block,
                    || provider_builder.build(),
                )
            });
        }

        if let Err(err) = hashed_state_validate_result {
            if err.is_validation_error() {
                self.on_invalid_block(&parent_block, &block, &output, None, ctx.state_mut());
            }
            return Err(InsertBlockError::new(block.into_sealed_block(), err).into())
        }

        self.metrics.block_validation.record_state_root(&trie_output, root_elapsed.as_secs_f64());
        self.metrics
            .record_state_root_gas_bucket(block.header().gas_used(), root_elapsed.as_secs_f64());
        debug!(target: "engine::tree::payload_validator", ?root_elapsed, "Calculated state root");

        // ensure state root matches
        if state_root != block.header().state_root() {
            // call post-block hook
            self.on_invalid_block(
                &parent_block,
                &block,
                &output,
                Some((&trie_output, state_root)),
                ctx.state_mut(),
            );
            let block_state_root = block.header().state_root();
            return Err(InsertBlockError::new(
                block.into_sealed_block(),
                ConsensusError::BodyStateRootDiff(
                    GotExpected { got: state_root, expected: block_state_root }.into(),
                )
                .into(),
            )
            .into())
        }

        let timing_stats = state_provider_stats.filter(|_| slow_block_enabled).map(|stats| {
            self.calculate_timing_stats(
                &block,
                stats,
                cache_stats,
                &output,
                execution_duration,
                root_elapsed,
            )
        });

        if let Some(valid_block_tx) = valid_block_tx {
            let _ = valid_block_tx.send(());
        }

        let executed_block = self.spawn_deferred_trie_task(
            Arc::new(block),
            output,
            hashed_state,
            trie_output,
            changed_paths,
        );
        let raw_bal = decoded_bal.map(|decoded_bal| decoded_bal.as_raw_bal().clone());
        Ok(ValidationOutput::new(executed_block, timing_stats).with_raw_bal(raw_bal))
    }

    /// Spawns a background task to convert a [`BlockOrPayload`] into a [`SealedBlock`] and perform
    /// basic consensus validations on it.
    #[expect(clippy::type_complexity)]
    pub fn spawn_convert_and_validate<T>(
        &self,
        input: &BlockOrPayload<T>,
        parent: SealedHeader<N::BlockHeader>,
    ) -> LazyHandle<Result<SealedBlock<N::Block>, InsertPayloadError<N::Block>>>
    where
        T: PayloadTypes<BuiltPayload: BuiltPayload<Primitives = N>>,
        V: PayloadValidator<T, Block = N::Block> + Clone,
    {
        let input = input.clone();
        let validator = self.validator.clone();
        let consensus = self.consensus.clone();
        let parent_span = Span::current();
        self.runtime.spawn_blocking_named("payload-convert", move || {
            let _span = debug_span!(
                target: "engine::tree::payload_validator",
                parent: parent_span,
                "convert_and_validate",
            )
            .entered();
            let block = match input {
                BlockOrPayload::Block(block) => block,
                BlockOrPayload::Payload(payload) => {
                    validator.convert_payload_to_block(payload)?
                }
            };

            if let Err(e) = consensus.validate_header(block.sealed_header()) {
                error!(target: "engine::tree::payload_validator", ?block, "Failed to validate header {}: {e}", block.hash());
                return Err(InsertBlockError::consensus_error(e, block).into())
            }

            // now validate against the parent
            let _enter = debug_span!(target: "engine::tree::payload_validator", "validate_header_against_parent").entered();
            if let Err(e) = consensus.validate_header_against_parent(block.sealed_header(), &parent)
            {
                warn!(target: "engine::tree::payload_validator", ?block, "Failed to validate header {} against parent: {e}", block.hash());
                return Err(InsertBlockError::consensus_error(e, block).into())
            }
            drop(_enter);

            if let Err(e) =
                consensus.validate_block_pre_execution_with_tx_root(&block, None)
            {
                error!(target: "engine::tree::payload_validator", ?block, "Failed to validate block {}: {e}", block.hash());
                return Err(InsertBlockError::consensus_error(e, block).into())
            }

            Ok(block)
        })
    }

    /// Return sealed block header from database or in-memory state by hash.
    fn sealed_header_by_hash(
        &self,
        hash: B256,
        state: &EngineApiTreeState<N>,
    ) -> ProviderResult<Option<SealedHeader<N::BlockHeader>>> {
        // check memory first
        let header = state.tree_state.sealed_header_by_hash(&hash);

        if header.is_some() {
            Ok(header)
        } else {
            self.provider.sealed_header_by_hash(hash)
        }
    }

    /// Executes a block with the given state provider.
    ///
    /// This method orchestrates block execution:
    /// 1. Sets up the EVM with state database and precompile caching
    /// 2. Spawns a background task for incremental receipt root computation
    /// 3. Executes transactions with metrics collection via state hooks
    /// 4. Merges state transitions and records execution metrics
    #[instrument(level = "debug", target = "engine::tree::payload_validator", skip_all)]
    #[expect(clippy::type_complexity)]
    fn execute_block<S, Err, T>(
        &mut self,
        state_provider: S,
        env: ExecutionEnv<Evm>,
        input: &BlockOrPayload<T>,
        handle: &mut PayloadHandle<impl ExecutableTxFor<Evm>, Err, N::Receipt>,
        state_hook: Option<Box<dyn OnStateHook + 'static>>,
    ) -> Result<
        (
            BlockExecutionOutput<N::Receipt>,
            Vec<Address>,
            ReceiptRootReceiver,
            Option<BlockAccessList>,
        ),
        InsertBlockErrorKind,
    >
    where
        S: StateProvider + Send,
        Err: core::error::Error + Send + Sync + 'static,
        V: PayloadValidator<T, Block = N::Block>,
        T: PayloadTypes<BuiltPayload: BuiltPayload<Primitives = N>>,
        Evm: ConfigureEngineEvm<T::ExecutionData, Primitives = N>,
    {
        debug!(target: "engine::tree::payload_validator", "Executing block");

        let has_bal = env.decoded_bal.is_some();
        let mut db = debug_span!(target: "engine::tree", "build_state_db").in_scope(|| {
            State::builder()
                .with_database(StateProviderDatabase::new(state_provider))
                .with_bundle_update()
                .with_bal_builder_if(has_bal)
                .build()
        });

        let (spec_id, mut executor) = {
            let _span = debug_span!(target: "engine::tree", "create_evm").entered();
            let spec_id = *env.evm_env.spec_id();
            let evm_config = self.evm_config.clone().with_jit_support();
            let evm = evm_config.evm_with_env(&mut db, env.evm_env);
            let ctx = self
                .execution_ctx_for(input)
                .map_err(|e| InsertBlockErrorKind::Other(Box::new(e)))?;
            let executor = self.evm_config.create_executor(evm, ctx);
            (spec_id, executor)
        };

        if !self.config.precompile_cache_disabled() {
            let _span = debug_span!(target: "engine::tree", "setup_precompile_cache").entered();
            executor.evm_mut().precompiles_mut().map_cacheable_precompiles(
                |address, precompile| {
                    let metrics = self
                        .precompile_cache_metrics
                        .entry(*address)
                        .or_insert_with(|| CachedPrecompileMetrics::new_with_address(*address))
                        .clone();
                    CachedPrecompile::wrap(
                        precompile,
                        self.precompile_cache_map.cache_for_address(*address),
                        spec_id,
                        Some(metrics),
                    )
                },
            );
        }

        let transaction_count = input.transaction_count();
        let (receipt_tx, result_rx) = self.spawn_receipt_root_task(transaction_count);
        let executed_tx_index = Arc::clone(handle.executed_tx_index());
        executor.evm_mut().db_mut().set_state_hook(state_hook);

        let execution_start = Instant::now();

        // Execute all transactions and finalize
        let (executor, senders, last_sent_len) = self.execute_transactions(
            executor,
            transaction_count,
            handle.iter_transactions(),
            &receipt_tx,
            &executed_tx_index,
            has_bal,
        )?;

        // Finish execution and get the result. `receipt_tx` is kept alive across this
        // call: some executors (e.g. BSC) append additional receipts here (a system
        // transaction's receipt) that were never streamed by the main loop above.
        let post_exec_start = Instant::now();
        let (_evm, result) = debug_span!(target: "engine::tree", "BlockExecutor::finish")
            .in_scope(|| executor.finish())
            .map(|(evm, result)| (evm.into_db(), result))?;
        self.metrics.record_post_execution(post_exec_start.elapsed());

        // Stream any receipts that were only appended during finalization, so the
        // receipt-root task still receives the complete set instead of always
        // finalizing short.
        if result.receipts.len() > last_sent_len {
            for (tx_index, receipt) in result.receipts.iter().enumerate().skip(last_sent_len) {
                let _ = receipt_tx.send(IndexedReceipt::new(tx_index, receipt.clone()));
            }
        }
        drop(receipt_tx);

        // Merge transitions into bundle state
        debug_span!(target: "engine::tree", "merge_transitions")
            .in_scope(|| db.merge_transitions(BundleRetention::Reverts));

        let built_bal = if has_bal { db.take_built_alloy_bal() } else { None };
        let output = BlockExecutionOutput { result, state: db.take_bundle() };

        let execution_duration = execution_start.elapsed();
        self.metrics.record_block_execution(&output, execution_duration);
        self.metrics.record_block_execution_gas_bucket(output.result.gas_used, execution_duration);
        debug!(target: "engine::tree::payload_validator", elapsed = ?execution_duration, "Executed block");

        Ok((output, senders, result_rx, built_bal))
    }

    /// Returns true when the BAL execute path should be used for this block.
    // TODO: extend with stronger gating before enabling on mainnet:
    //   - Fork check: `Amsterdam.active_at_timestamp(env.evm_env.timestamp)`. Today a BAL only
    //     exists post-Amsterdam, so the BAL-presence check is a sufficient proxy. It is a proxy,
    //     not a guarantee.
    //   - Tx-count threshold (`bal_execute_path_min_tx_count`): below the parallelism break-even
    //     point, provider setup and worker scheduling overhead can exceed the gain. Tune
    //     empirically once workers are parallel; meaningless while the commit loop is sequential.
    fn bal_path_eligible(&self, bal: Option<&DecodedBal>) -> Result<bool, InsertBlockErrorKind> {
        let has_bal = bal.is_some();
        let parallel_execution = has_bal && !self.config.disable_bal_parallel_execution();
        if parallel_execution && self.config.disable_bal_parallel_state_root() {
            return Err(InsertBlockErrorKind::Other(
                "disabling parallel state root is impossible when parallel execution is enabled"
                    .into(),
            ));
        }

        Ok(parallel_execution)
    }

    /// Executes the block on the BAL path. Mirrors the return shape of [`Self::execute_block`]
    /// so the dispatch site stays uniform.
    ///
    /// Inside, this:
    /// 1. Creates a shared parent-state cache handle for provider-backed workers.
    /// 2. Relies on BAL prewarm to stream state-root updates and optional state prefetches.
    /// 3. Spawns the receipt-root task.
    /// 4. Calls [`crate::tree::payload_processor::bal::execute_block`].
    /// 5. Returns the rebuilt BAL for post-execution consensus validation.
    #[instrument(level = "debug", target = "engine::tree::payload_validator", skip_all)]
    #[expect(clippy::type_complexity)]
    fn execute_block_bal<Tx, Err, MakeStateProvider, T>(
        &self,
        env: ExecutionEnv<Evm>,
        input: &BlockOrPayload<T>,
        handle: &PayloadHandle<Tx, Err, N::Receipt>,
        make_state_provider: &MakeStateProvider,
    ) -> Result<
        (
            BlockExecutionOutput<N::Receipt>,
            Vec<Address>,
            ReceiptRootReceiver,
            Option<BlockAccessList>,
        ),
        InsertBlockErrorKind,
    >
    where
        Tx: ExecutableTxFor<Evm> + Send,
        Err: core::error::Error + Send + Sync + 'static,
        MakeStateProvider: Fn(bool) -> ProviderResult<StateProviderBox> + Sync,
        Evm: ConfigureEngineEvm<T::ExecutionData, Primitives = N>,
        T: PayloadTypes<BuiltPayload: BuiltPayload<Primitives = N>>,
        V: PayloadValidator<T, Block = N::Block>,
    {
        debug!(target: "engine::tree::payload_validator", "Executing block via BAL path");

        let (receipt_tx, result_rx) = self.spawn_receipt_root_task(env.transaction_count);
        let input_bal = env.decoded_bal.ok_or_else(|| {
            InsertBlockErrorKind::Other("BAL execute path: no decoded BAL available".into())
        })?;

        let make_db = |fill_on_miss| {
            let provider = make_state_provider(fill_on_miss)
                .map_err(crate::tree::payload_processor::bal::BalExecutionError::Provider)?;
            Ok(StateProviderDatabase::new(provider))
        };
        let execution_start = Instant::now();
        let ctx =
            self.execution_ctx_for(input).map_err(|e| InsertBlockErrorKind::Other(Box::new(e)))?;
        let (output, senders, built_bal) = crate::tree::payload_processor::bal::execute_block(
            &self.runtime,
            &self.evm_config,
            &make_db,
            input_bal,
            env.evm_env,
            ctx,
            env.transaction_count,
            handle.clone_transaction_receiver(),
            receipt_tx,
        )?;
        let execution_duration = execution_start.elapsed();

        self.metrics.record_block_execution(&output, execution_duration);
        self.metrics.record_block_execution_gas_bucket(output.result.gas_used, execution_duration);
        debug!(
            target: "engine::tree::payload_validator",
            elapsed = ?execution_duration,
            "Executed block via BAL path",
        );

        Ok((output, senders, result_rx, Some(built_bal)))
    }

    fn spawn_receipt_root_task(
        &self,
        receipts_len: usize,
    ) -> (ReceiptRootSender<N>, ReceiptRootReceiver) {
        // Unbounded channel is used since tx count bounds capacity anyway.
        let (receipt_tx, receipt_rx) = crossbeam_channel::unbounded();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let task_handle = ReceiptRootTaskHandle::new(receipt_rx, result_tx);
        self.runtime.spawn_blocking_named("receipt-root", move || task_handle.run(receipts_len));

        (receipt_tx, result_rx)
    }

    /// Executes transactions and collects senders, streaming receipts to a background task.
    ///
    /// This method handles:
    /// - Applying pre-execution changes (e.g., beacon root updates)
    /// - Executing each transaction with timing metrics
    /// - Streaming receipts to the receipt root computation task
    /// - Collecting transaction senders for later use
    ///
    /// Returns the executor (for finalization) and the collected senders.
    fn execute_transactions<'a, E, Tx, InnerTx, Err, DB>(
        &self,
        mut executor: E,
        transaction_count: usize,
        transactions: impl Iterator<Item = Result<Tx, Err>>,
        receipt_tx: &crossbeam_channel::Sender<IndexedReceipt<N::Receipt>>,
        executed_tx_index: &AtomicUsize,
        has_bal: bool,
    ) -> Result<(E, Vec<Address>), BlockExecutionError>
    where
        E: BlockExecutor<Receipt = N::Receipt, Evm: alloy_evm::Evm<DB = &'a mut State<DB>>>,
        Tx: alloy_evm::block::ExecutableTx<E> + alloy_evm::RecoveredTx<InnerTx>,
        InnerTx: TxHashRef,
        DB: revm::Database + 'a,
        Err: core::error::Error + Send + Sync + 'static,
    {
        let mut senders = Vec::with_capacity(transaction_count);

        // Apply pre-execution changes (e.g., beacon root update)
        let pre_exec_start = Instant::now();
        debug_span!(target: "engine::tree", "pre_execution")
            .in_scope(|| executor.apply_pre_execution_changes())?;
        self.metrics.record_pre_execution(pre_exec_start.elapsed());

        // Bump BAL index after pre-execution changes (EIP-7928: index 0 is pre-execution)
        if has_bal {
            executor.evm_mut().db_mut().bump_bal_index();
        }

        // Execute transactions
        let exec_span = debug_span!(target: "engine::tree", "execution").entered();
        let mut transactions = transactions.into_iter();
        // Some executors may execute transactions that do not append receipts during the
        // main loop (e.g., system transactions whose receipts are added during finalization).
        // In that case, invoking the callback on every transaction would resend the previous
        // receipt with the same index and can panic the ordered root builder.
        let mut last_sent_len = 0usize;
        loop {
            // Measure time spent waiting for next transaction from iterator
            // (e.g., parallel signature recovery)
            let wait_start = Instant::now();
            let Some(tx_result) = transactions.next() else { break };
            self.metrics.record_transaction_wait(wait_start.elapsed());

            let tx = tx_result.map_err(BlockExecutionError::other)?;
            let tx_signer = *<Tx as alloy_evm::RecoveredTx<InnerTx>>::signer(&tx);

            senders.push(tx_signer);

            let _enter = tracing::enabled!(target: "engine::tree", Level::TRACE).then(|| {
                tracing::trace_span!(
                    target: "engine::tree",
                    "execute tx",
                    tx_index = senders.len() - 1,
                )
                .entered()
            });
            if tracing::enabled!(target: "engine::tree", Level::TRACE) {
                trace!(target: "engine::tree", "Executing transaction");
            }

            let tx_start = Instant::now();
            executor.execute_transaction(tx)?;
            self.metrics.record_transaction_execution(tx_start.elapsed());

            // advance the shared counter so prewarm workers skip already-executed txs
            executed_tx_index.store(senders.len(), Ordering::Relaxed);

            let current_len = executor.receipts().len();
            if current_len > last_sent_len {
                last_sent_len = current_len;
                // Send the latest receipt to the background task for incremental root computation.
                if let Some(receipt) = executor.receipts().last() {
                    let tx_index = current_len - 1;
                    let _ = receipt_tx.send(IndexedReceipt::new(tx_index, receipt.clone()));
                }
            }
            // Bump BAL index after each transaction (EIP-7928)
            if has_bal {
                executor.evm_mut().db_mut().bump_bal_index();
            }
        }

        drop(exec_span);

        Ok((executor, senders, last_sent_len))
    }

    /// Validates the block after execution.
    ///
    /// This performs:
    /// - parent header validation
    /// - post-execution consensus validation
    /// - state-root based post-execution validation
    ///
    /// If `receipt_root_bloom` is provided, it will be used instead of computing the receipt root
    /// and logs bloom from the receipts.
    ///
    /// The `hashed_state` handle wraps the background hashed post state computation.
    #[instrument(level = "debug", target = "engine::tree::payload_validator", skip_all)]
    fn validate_post_execution<T: PayloadTypes<BuiltPayload: BuiltPayload<Primitives = N>>>(
        &self,
        block: &RecoveredBlock<N::Block>,
        parent_block: &SealedHeader<N::BlockHeader>,
        output: &BlockExecutionOutput<N::Receipt>,
        ctx: &mut TreeCtx<'_, N>,
        receipt_root_bloom: Option<ReceiptRootBloom>,
        built_bal: Option<BlockAccessList>,
    ) -> Result<(), InsertBlockErrorKind>
    where
        V: PayloadValidator<T, Block = N::Block>,
    {
        let start = Instant::now();

        trace!(target: "engine::tree::payload_validator", block=?block.num_hash(), "Validating block consensus");

        // Validate block post-execution rules
        let _enter =
            debug_span!(target: "engine::tree::payload_validator", "validate_block_post_execution")
                .entered();
        let block_access_list_hash =
            built_bal.as_ref().map(|bal| compute_block_access_list_hash(bal));

        if let Err(err) = self.consensus.validate_block_post_execution(
            block,
            output,
            receipt_root_bloom,
            block_access_list_hash,
        ) {
            // call post-block hook
            self.on_invalid_block(parent_block, block, output, None, ctx.state_mut());
            return Err(err.into())
        }
        drop(_enter);

        // record post-execution validation duration
        self.metrics
            .block_validation
            .post_execution_validation_duration
            .record(start.elapsed().as_secs_f64());

        Ok(())
    }

    /// Spawns transaction conversion and cache prewarming for payload validation.
    ///
    /// State-root tasks are prepared before this method and can provide capabilities that
    /// prewarm uses for BAL-derived authoritative updates or transaction-derived hints.
    #[instrument(
        level = "debug",
        target = "engine::tree::payload_validator",
        skip_all,
        fields(
            has_hint_stream = hint_stream.is_some(),
            has_hashed_update_stream = hashed_update_stream.is_some(),
            parallel_bal_execution
        )
    )]
    fn spawn_payload_processor<T: ExecutableTxIterator<Evm>>(
        &self,
        env: ExecutionEnv<Evm>,
        txs: T,
        provider_builder: StateProviderBuilder<N, P>,
        hint_stream: Option<StateRootHintStream>,
        hashed_update_stream: Option<StateRootUpdateStream>,
        parallel_bal_execution: bool,
    ) -> Result<
        PayloadHandle<
            impl ExecutableTxFor<Evm> + use<N, P, Evm, V, T>,
            impl core::error::Error + Send + Sync + 'static + use<N, P, Evm, V, T>,
            N::Receipt,
        >,
        InsertBlockErrorKind,
    > {
        let start = Instant::now();
        let handle = self.payload_processor.spawn_with_state_root_streams(
            env,
            txs,
            provider_builder,
            hint_stream,
            hashed_update_stream,
            parallel_bal_execution,
        );

        self.metrics.block_validation.spawn_payload_processor.record(start.elapsed().as_secs_f64());

        Ok(handle)
    }

    /// Creates a `StateProviderBuilder` for the given parent hash.
    ///
    /// This method checks if the parent is in the tree state (in-memory) or persisted to disk,
    /// and creates the appropriate provider builder.
    fn state_provider_builder(
        &self,
        hash: B256,
        state: &EngineApiTreeState<N>,
    ) -> ProviderResult<Option<StateProviderBuilder<N, P>>> {
        if let Some((historical, blocks)) = state.tree_state.blocks_by_hash(hash) {
            debug!(target: "engine::tree::payload_validator", %hash, %historical, "found canonical state for block in memory, creating provider builder");
            // the block leads back to the canonical chain
            return Ok(Some(StateProviderBuilder::new(
                self.provider.clone(),
                historical,
                Some(blocks),
            )))
        }

        // Check if the block is persisted
        if let Some(header) = self.provider.header(hash)? {
            debug!(target: "engine::tree::payload_validator", %hash, number = %header.number(), "found canonical state for block in database, creating provider builder");
            // For persisted blocks, we create a builder that will fetch state directly from the
            // database
            return Ok(Some(StateProviderBuilder::new(self.provider.clone(), hash, None)))
        }

        debug!(target: "engine::tree::payload_validator", %hash, "no canonical state found for block");
        Ok(None)
    }

    /// Called when an invalid block is encountered during validation.
    fn on_invalid_block(
        &self,
        parent_header: &SealedHeader<N::BlockHeader>,
        block: &RecoveredBlock<N::Block>,
        output: &BlockExecutionOutput<N::Receipt>,
        trie_updates: Option<(&TrieUpdates, B256)>,
        state: &mut EngineApiTreeState<N>,
    ) {
        if state.invalid_headers.get(&block.hash()).is_some() {
            // we already marked this block as invalid
            return
        }
        self.invalid_block_hook.on_invalid_block(parent_header, block, output, trie_updates);
    }

    /// Returns an overlay builder configured for a payload parent.
    fn overlay_builder_for_parent(
        parent_hash: B256,
        state: &EngineApiTreeState<N>,
        changeset_cache: ChangesetCache,
    ) -> OverlayBuilder<N> {
        OverlayBuilder::new(parent_hash, changeset_cache)
            .with_state_trie_overlay_manager(state.tree_state.state_trie_overlays.clone())
    }

    /// Spawns a background task to compute and sort trie data for the executed block.
    ///
    /// This function creates a [`LazyTrieData`] handle and spawns a blocking task that:
    /// 1. Sort the block's hashed state and trie updates
    /// 2. Publishes the result so subsequent calls return immediately
    ///
    /// If the background task hasn't completed when `trie_data()` is called, callers wait for the
    /// publishing task instead of computing synchronously.
    ///
    /// The validation hot path can return immediately after state root verification,
    /// while consumers (DB writes, overlay providers, proofs) get trie data from the completed
    /// task.
    fn spawn_deferred_trie_task(
        &self,
        block: Arc<RecoveredBlock<N::Block>>,
        execution_outcome: Arc<BlockExecutionOutput<N::Receipt>>,
        hashed_state: LazyHashedPostState,
        trie_output: Arc<TrieUpdates>,
        changed_paths: Option<Arc<TriePrefixSetsMut>>,
    ) -> ExecutedBlock<N> {
        // Create deferred handle and task that owns the unsorted inputs.
        // Resolve the lazy handle into Arc<HashedPostState>. By this point the hashed state has
        // already been computed and used for state root verification, so .get() returns instantly.
        let hashed_state = match hashed_state.try_into_inner() {
            Ok(state) => state,
            Err(handle) => handle.get().clone(),
        };
        let (deferred_trie_data, deferred_trie_task) =
            LazyTrieData::pending(hashed_state, trie_output, changed_paths);
        let block_validation_metrics = self.metrics.block_validation.clone();

        // Capture block info for tracing.
        let block_number = block.number();

        // Spawn background task to compute trie data.
        let compute_trie_input_task = move || {
            let _span = debug_span!(
                target: "engine::tree::payload_validator",
                "compute_trie_input_task",
                block_number
            )
            .entered();

            let compute_start = Instant::now();
            let computed = deferred_trie_task.compute_and_publish();
            block_validation_metrics
                .deferred_trie_compute_duration
                .record(compute_start.elapsed().as_secs_f64());

            // Record sizes of the computed trie data
            block_validation_metrics
                .hashed_post_state_size
                .record(computed.sorted.hashed_state.total_len() as f64);
            block_validation_metrics
                .trie_updates_sorted_size
                .record(computed.sorted.trie_updates.total_len() as f64);
        };

        // Spawn task that computes trie data asynchronously.
        self.runtime.spawn_blocking_named(DEFERRED_TRIE_WORKER_NAME, compute_trie_input_task);

        ExecutedBlock::with_deferred_trie_data(block, execution_outcome, deferred_trie_data)
    }

    fn calculate_timing_stats(
        &self,
        block: &RecoveredBlock<N::Block>,
        provider_stats: Arc<StateProviderStats>,
        cache_stats: Option<Arc<CacheStats>>,
        output: &BlockExecutionOutput<N::Receipt>,
        execution_duration: Duration,
        state_hash_duration: Duration,
    ) -> Box<ExecutionTimingStats> {
        let accounts_read = provider_stats.total_account_fetches();
        let storage_read = provider_stats.total_storage_fetches();
        let code_read = provider_stats.total_code_fetches();
        let code_bytes_read = provider_stats.total_code_fetched_bytes();

        // Write stats from BundleState (final state changes)
        let accounts_changed = output.state.state.len();
        let accounts_deleted =
            output.state.state.values().filter(|acc| acc.was_destroyed()).count();
        let storage_slots_changed =
            output.state.state.values().map(|account| account.storage.len()).sum::<usize>();
        let storage_slots_deleted = output
            .state
            .state
            .values()
            .flat_map(|account| account.storage.values())
            .filter(|slot| {
                slot.present_value.is_zero() && !slot.previous_or_original_value.is_zero()
            })
            .count();

        // Helper: check if account represents a new contract deployment
        let is_new_deployment = |acc: &BundleAccount| -> bool {
            let has_code_now = acc.info.as_ref().is_some_and(|info| info.code_hash != KECCAK_EMPTY);
            let had_no_code_before = acc
                .original_info
                .as_ref()
                .map(|info| info.code_hash == KECCAK_EMPTY)
                .unwrap_or(true);
            has_code_now && had_no_code_before
        };

        let bytecodes_changed =
            output.state.state.values().filter(|acc| is_new_deployment(acc)).count();

        // Unique new code hashes to count actual bytes persisted (deduplicated)
        let unique_new_code_hashes: B256Set = output
            .state
            .state
            .values()
            .filter(|acc| is_new_deployment(acc))
            .filter_map(|acc| acc.info.as_ref().map(|info| info.code_hash))
            .collect();
        let code_bytes_written: usize = unique_new_code_hashes
            .iter()
            .filter_map(|hash| {
                output.state.contracts.get(hash).map(|bytecode| bytecode.original_bytes().len())
            })
            .sum();

        // Total time spent fetching state during execution
        let state_read_duration = provider_stats.total_account_fetch_latency() +
            provider_stats.total_storage_fetch_latency() +
            provider_stats.total_code_fetch_latency();

        // EIP-7702 delegation tracking from bytecode changes
        // Count new EIP-7702 bytecodes as delegations set
        let eip7702_delegations_set =
            output.state.contracts.values().filter(|bytecode| bytecode.is_eip7702()).count();
        // Delegations cleared: accounts where bytecode changed FROM EIP-7702 TO empty
        // This detects when an EIP-7702 delegation is removed by setting code to empty
        // Note: Clearing a delegation does NOT destroy the account - it just empties the
        // bytecode
        let eip7702_delegations_cleared = output
            .state
            .state
            .values()
            .filter(|acc| {
                // Check if original bytecode was EIP-7702
                let original_was_eip7702 = acc
                    .original_info
                    .as_ref()
                    .and_then(|info| info.code.as_ref())
                    .map(|bytecode| bytecode.is_eip7702())
                    .unwrap_or(false);

                // Check if current code is empty (delegation cleared)
                let code_now_empty =
                    acc.info.as_ref().map(|info| info.code_hash == KECCAK_EMPTY).unwrap_or(false);

                original_was_eip7702 && code_now_empty
            })
            .count();

        // Get cache statistics for detailed block logging
        let (account_cache_hits, account_cache_misses) = cache_stats
            .as_ref()
            .map(|s| (s.account_hits(), s.account_misses()))
            .unwrap_or_default();
        let (storage_cache_hits, storage_cache_misses) = cache_stats
            .as_ref()
            .map(|s| (s.storage_hits(), s.storage_misses()))
            .unwrap_or_default();
        let (code_cache_hits, code_cache_misses) =
            cache_stats.as_ref().map(|s| (s.code_hits(), s.code_misses())).unwrap_or_default();

        // Build execution timing stats for detailed block logging
        Box::new(ExecutionTimingStats {
            block_number: block.number(),
            block_hash: block.hash(),
            gas_used: output.result.gas_used,
            tx_count: block.transaction_count(),
            execution_duration,
            state_read_duration,
            state_hash_duration,
            accounts_read,
            storage_read,
            code_read,
            code_bytes_read,
            accounts_changed,
            accounts_deleted,
            storage_slots_changed,
            storage_slots_deleted,
            bytecodes_changed,
            code_bytes_written,
            eip7702_delegations_set,
            eip7702_delegations_cleared,
            account_cache_hits,
            account_cache_misses,
            storage_cache_hits,
            storage_cache_misses,
            code_cache_hits,
            code_cache_misses,
        })
    }
}

/// Type that validates the payloads processed by the engine.
///
/// This provides the necessary functions for validating/executing payloads/blocks.
pub trait EngineValidator<
    Types: PayloadTypes,
    N: NodePrimitives = <<Types as PayloadTypes>::BuiltPayload as BuiltPayload>::Primitives,
>: Send + Sync + 'static
{
    /// Validates the payload attributes with respect to the header.
    ///
    /// By default, this enforces that the payload attributes timestamp is greater than the
    /// timestamp according to:
    ///   > 7. Client software MUST ensure that payloadAttributes.timestamp is greater than
    ///   > timestamp
    ///   > of a block referenced by forkchoiceState.headBlockHash.
    ///
    /// See also: <https://github.com/ethereum/execution-apis/blob/main/src/engine/common.md#specification-1>
    fn validate_payload_attributes_against_header(
        &self,
        attr: &Types::PayloadAttributes,
        header: &N::BlockHeader,
    ) -> Result<(), InvalidPayloadAttributesError>;

    /// Ensures that the given payload does not violate any consensus rules that concern the block's
    /// layout.
    ///
    /// This function must convert the payload into the executable block and pre-validate its
    /// fields.
    ///
    /// Implementers should ensure that the checks are done in the order that conforms with the
    /// engine-API specification.
    fn convert_payload_to_block(
        &self,
        payload: Types::ExecutionData,
    ) -> Result<SealedBlock<N::Block>, NewPayloadError>;

    /// Validates a payload received from engine API.
    fn validate_payload(
        &mut self,
        payload: Types::ExecutionData,
        ctx: TreeCtx<'_, N>,
    ) -> ValidationOutcome<N>;

    /// Validates a block downloaded from the network.
    fn validate_block(
        &mut self,
        block: SealedBlock<N::Block>,
        ctx: TreeCtx<'_, N>,
    ) -> ValidationOutcome<N>;

    /// Hook called after an executed block is inserted directly into the tree.
    ///
    /// This is invoked when blocks are inserted via `InsertExecutedBlock` (e.g., locally built
    /// blocks by sequencers) to allow implementations to update internal state such as caches.
    fn on_inserted_executed_block(
        &self,
        block: BuiltPayloadExecutedBlock<N>,
    ) -> ProviderResult<ExecutedBlock<N>>;

    /// Returns [`SavedCache`] for the given block hash.
    fn cache_for(&self, _block_hash: B256) -> Option<SavedCache>;

    /// Prepares the optional payload-builder state-root handle through the installed
    /// [`StateRootStrategy`].
    ///
    /// Returns `None` when the strategy declines, in which case the payload builder computes
    /// the state root itself.
    ///
    /// `timestamp` is the timestamp of the payload being built, taken from the payload
    /// attributes.
    fn payload_state_root_handle_for(
        &self,
        parent_hash: B256,
        parent_header: &N::BlockHeader,
        timestamp: u64,
        state: &mut EngineApiTreeState<N>,
    ) -> Option<PayloadStateRootHandle>;
}

impl<N, Types, P, Evm, V> EngineValidator<Types> for BasicEngineValidator<P, Evm, V>
where
    P: DatabaseProviderFactory<
            Provider: BlockReader
                          + StageCheckpointReader
                          + PruneCheckpointReader
                          + ChangeSetReader
                          + StorageChangeSetReader
                          + BlockNumReader
                          + StorageSettingsCache,
        > + BlockReader<Header = N::BlockHeader>
        + StateProviderFactory
        + StateReader
        + ChangeSetReader
        + BlockNumReader
        + HashedPostStateProvider
        + Clone
        + 'static,
    OverlayStateProviderFactory<P, N>: DatabaseProviderROFactory<Provider: TrieCursorFactory + HashedCursorFactory>
        + Clone
        + Send
        + Sync
        + 'static,
    N: NodePrimitives,
    V: PayloadValidator<Types, Block = N::Block> + Clone,
    Evm: ConfigureEngineEvm<Types::ExecutionData, Primitives = N> + 'static,
    Types: PayloadTypes<BuiltPayload: BuiltPayload<Primitives = N>>,
{
    fn validate_payload_attributes_against_header(
        &self,
        attr: &Types::PayloadAttributes,
        header: &N::BlockHeader,
    ) -> Result<(), InvalidPayloadAttributesError> {
        self.validator.validate_payload_attributes_against_header(attr, header)
    }

    fn convert_payload_to_block(
        &self,
        payload: Types::ExecutionData,
    ) -> Result<SealedBlock<N::Block>, NewPayloadError> {
        let block = self.validator.convert_payload_to_block(payload)?;
        Ok(block)
    }

    fn validate_payload(
        &mut self,
        payload: Types::ExecutionData,
        ctx: TreeCtx<'_, N>,
    ) -> ValidationOutcome<N> {
        self.validate_block_with_state(BlockOrPayload::Payload(payload), ctx)
    }

    fn validate_block(
        &mut self,
        block: SealedBlock<N::Block>,
        ctx: TreeCtx<'_, N>,
    ) -> ValidationOutcome<N> {
        self.validate_block_with_state(BlockOrPayload::Block(block), ctx)
    }

    fn on_inserted_executed_block(
        &self,
        block: BuiltPayloadExecutedBlock<N>,
    ) -> ProviderResult<ExecutedBlock<N>> {
        self.payload_processor.on_inserted_executed_block(
            block.recovered_block.block_with_parent(),
            &block.execution_output.state,
        );

        Ok(self.spawn_deferred_trie_task(
            block.recovered_block,
            block.execution_output,
            LazyHashedPostState::ready(block.hashed_state),
            block.trie_updates,
            block.changed_paths,
        ))
    }

    fn cache_for(&self, block_hash: B256) -> Option<SavedCache> {
        Some(self.payload_processor.cache_for(block_hash))
    }

    fn payload_state_root_handle_for(
        &self,
        parent_hash: B256,
        parent_header: &N::BlockHeader,
        timestamp: u64,
        state: &mut EngineApiTreeState<N>,
    ) -> Option<PayloadStateRootHandle> {
        let provider_builder = match self.state_provider_builder(parent_hash, state) {
            Ok(Some(provider_builder)) => provider_builder,
            Ok(None) => return None,
            Err(err) => {
                warn!(
                    target: "engine::tree::payload_validator",
                    %err,
                    %parent_hash,
                    "failed to prepare payload-builder state-root provider"
                );
                return None
            }
        };
        let overlay_factory = OverlayStateProviderFactory::new(
            self.provider.clone(),
            Self::overlay_builder_for_parent(parent_hash, state, self.changeset_cache.clone()),
        );

        match self.state_root_strategy.prepare_payload_builder(PayloadStateRootJobContext::new(
            &self.runtime,
            &self.state_trie_overlays,
            parent_hash,
            parent_header,
            timestamp,
            state,
            provider_builder,
            overlay_factory,
            &self.config,
        )) {
            Ok(handle) => handle,
            Err(err) => {
                warn!(
                    target: "engine::tree::payload_validator",
                    %err,
                    %parent_hash,
                    "failed to prepare payload-builder state-root job"
                );
                None
            }
        }
    }
}

impl<P, Evm, V> WaitForCaches for BasicEngineValidator<P, Evm, V>
where
    Evm: ConfigureEvm,
{
    fn wait_for_caches(&self) -> CacheWaitDurations {
        debug!(target: "engine::tree::payload_validator", "Waiting for execution cache and sparse trie locks");

        let execution_cache = self.payload_processor.execution_cache();
        let state_trie_overlays = self.state_trie_overlays.clone();
        let (execution_tx, execution_rx) = std::sync::mpsc::channel();
        let (sparse_trie_tx, sparse_trie_rx) = std::sync::mpsc::channel();

        self.runtime.spawn_blocking_named("wait-exec-cache", move || {
            let _ = execution_tx.send(execution_cache.wait_for_availability());
        });
        self.runtime.spawn_blocking_named("wait-sparse-tri", move || {
            let _ = sparse_trie_tx.send(state_trie_overlays.wait_for_sparse_trie_availability());
        });

        let execution_cache =
            execution_rx.recv().expect("execution cache wait task failed to send result");
        let sparse_trie =
            sparse_trie_rx.recv().expect("sparse trie wait task failed to send result");
        debug!(
            target: "engine::tree::payload_validator",
            ?execution_cache,
            ?sparse_trie,
            "Execution cache and sparse trie locks acquired"
        );
        CacheWaitDurations { execution_cache, sparse_trie }
    }
}

/// Enum representing either block or payload being validated.
#[derive(Debug, Clone)]
pub enum BlockOrPayload<T: PayloadTypes> {
    /// Payload.
    Payload(T::ExecutionData),
    /// Block.
    Block(SealedBlock<BlockTy<<T::BuiltPayload as BuiltPayload>::Primitives>>),
}

impl<T: PayloadTypes> BlockOrPayload<T> {
    /// Returns the hash of the block.
    pub fn hash(&self) -> B256 {
        match self {
            Self::Payload(payload) => payload.block_hash(),
            Self::Block(block) => block.hash(),
        }
    }

    /// Returns the number and hash of the block.
    pub fn num_hash(&self) -> NumHash {
        match self {
            Self::Payload(payload) => payload.num_hash(),
            Self::Block(block) => block.num_hash(),
        }
    }

    /// Returns the parent hash of the block.
    pub fn parent_hash(&self) -> B256 {
        match self {
            Self::Payload(payload) => payload.parent_hash(),
            Self::Block(block) => block.parent_hash(),
        }
    }

    /// Returns [`BlockWithParent`] for the block.
    pub fn block_with_parent(&self) -> BlockWithParent {
        match self {
            Self::Payload(payload) => payload.block_with_parent(),
            Self::Block(block) => block.block_with_parent(),
        }
    }

    /// Returns a string showing whether or not this is a block or payload.
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Payload(_) => "payload",
            Self::Block(_) => "block",
        }
    }

    /// Returns true if this is a payload.
    pub const fn is_payload(&self) -> bool {
        matches!(self, Self::Payload(_))
    }

    /// Returns true if this is a block.
    pub const fn is_block(&self) -> bool {
        matches!(self, Self::Block(_))
    }

    /// Returns the decoded block access list, if present and successfully decoded.
    pub fn try_decoded_access_list(&self) -> Result<Option<DecodedBal>, alloy_rlp::Error> {
        match self {
            Self::Payload(payload) => payload
                .block_access_list()
                .map(|block_access_list| DecodedBal::from_rlp_bytes(block_access_list.clone()))
                .transpose(),
            Self::Block(_) => Ok(None),
        }
    }

    /// Returns the number of transactions in the payload or block.
    pub fn transaction_count(&self) -> usize
    where
        T::ExecutionData: ExecutionPayload,
    {
        match self {
            Self::Payload(payload) => payload.transaction_count(),
            Self::Block(block) => block.transaction_count(),
        }
    }

    /// Returns the withdrawals from the payload or block.
    pub fn withdrawals(&self) -> Option<&[Withdrawal]>
    where
        T::ExecutionData: ExecutionPayload,
    {
        match self {
            Self::Payload(payload) => payload.withdrawals().map(|w| w.as_slice()),
            Self::Block(block) => block.body().withdrawals().map(|w| w.as_slice()),
        }
    }

    /// Returns the total gas used by the block.
    pub fn gas_used(&self) -> u64
    where
        T::ExecutionData: ExecutionPayload,
    {
        match self {
            Self::Payload(payload) => payload.gas_used(),
            Self::Block(block) => block.gas_used(),
        }
    }

    /// Returns the gas limit used by the block.
    pub fn gas_limit(&self) -> u64
    where
        T::ExecutionData: ExecutionPayload,
    {
        match self {
            Self::Payload(payload) => payload.gas_limit(),
            Self::Block(block) => block.gas_limit(),
        }
    }
}
