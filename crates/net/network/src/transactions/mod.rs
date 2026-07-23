                    // Only include the transaction if the peer hasn't seen it yet
                    if peer.seen_transactions.insert(*tx.tx_hash()) {
                        builder.push(tx);
                    }
                }
            }

            if builder.is_empty() {
                trace!(target: "net::tx", ?peer_id, "Nothing to propagate to peer; has seen all transactions");
                continue
            }

            let PropagateTransactions { pooled, full } = builder.build();

            // send hashes if any
            if let Some(mut new_pooled_hashes) = pooled {
                // Unhappy path: too many hashes for a single message. This should not happen
                // during regular propagation, which is capped at the soft limit per batch, and
                // is only reachable via manual propagation commands with oversized batches.
                if new_pooled_hashes.len() >
                    SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE
                {
                    // hashes that exceed the limit are not sent, so they must not be tracked as
                    // seen by the peer
                    for hash in new_pooled_hashes
                        .iter_hashes()
                        .skip(SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE)
                    {
                        peer.seen_transactions.remove(hash);
                    }
                    new_pooled_hashes.truncate(
                        SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE,
                    );
                }

                for hash in new_pooled_hashes.iter_hashes().copied() {
                    propagated.record(hash, PropagateKind::Hash(*peer_id));
                }

                trace!(target: "net::tx", ?peer_id, num_txs=?new_pooled_hashes.len(), "Propagating tx hashes to peer");

                // send hashes of transactions
                self.network.send_transactions_hashes(*peer_id, new_pooled_hashes);
            }

            // send full transactions, if any
            if let Some(new_full_transactions) = full {
                for hash in new_full_transactions.iter_hashes() {
                    propagated.record(*hash, PropagateKind::Full(*peer_id));
                }

                trace!(target: "net::tx", ?peer_id, num_txs=?new_full_transactions.len(), "Propagating full transactions to peer");

                // send full transactions
                self.network.send_broadcast_pool_transactions(*peer_id, new_full_transactions);
            }
        }

        // Update propagated transactions metrics
        self.metrics.propagated_transactions.increment(propagated.len() as u64);

        propagated
    }

    /// Propagates the given transactions to the peers
    ///
    /// This fetches all transaction from the pool, including the 4844 blob transactions but
    /// __without__ their sidecar, because 4844 transactions are only ever announced as hashes.
    fn propagate_all(&mut self, hashes: Vec<TxHash>) {
        if self.peers.is_empty() {
            // nothing to propagate
            return
        }
        let propagated = self.propagate_transactions(
            self.pool.get_all(hashes).into_iter().map(PropagateTransaction::pool_tx).collect(),
            PropagationMode::Basic,
        );

        // notify pool so events get fired
        self.pool.on_propagated(propagated);
    }

    /// Reannounces local pending transactions as hashes to a square root subset of peers.
    fn reannounce_local_pending_transactions(&mut self, now: Instant) {
        let hashes = transaction_hashes_to_reannounce(
            self.pool.get_local_pending_transactions(),
            now,
            self.config.reannounce_time,
        );
        let propagated = self.reannounce_transaction_hashes(hashes);
        if !propagated.0.is_empty() {
            self.pool.on_propagated(propagated);
        }
    }

    /// Reannounces the provided transaction hashes as hash-only gossip to a square root subset of
    /// eligible peers.
    fn reannounce_transaction_hashes(&mut self, hashes: Vec<TxHash>) -> PropagatedTransactions {
        let mut propagated = PropagatedTransactions::default();

        if hashes.is_empty() ||
            self.peers.is_empty() ||
            self.network.is_initially_syncing() ||
            self.network.tx_gossip_disabled()
        {
            return propagated
        }

        let mut peers = self
            .peers
            .iter_mut()
            .filter_map(|(peer_id, peer)| {
                self.policies.propagation_policy().can_propagate(peer).then_some(*peer_id)
            })
            .collect::<Vec<_>>();
        peers.truncate((peers.len() as f64).sqrt() as usize);
        if peers.is_empty() {
            return propagated
        }

        debug!(
            target: "net::tx",
            txs = hashes.len(),
            peers = peers.len(),
            "Reannouncing local pending transactions"
        );

        for peer_id in peers {
            if let Some(peer_propagated) =
                self.propagate_hashes_to_peer(hashes.clone(), peer_id, PropagationMode::Forced)
            {
                for (hash, kinds) in peer_propagated.0 {
                    propagated.0.entry(hash).or_default().extend(kinds);
                }
            }
        }

        propagated
    }

    /// Request handler for an incoming request for transactions
    fn on_get_pooled_transactions(
        &mut self,
        peer_id: PeerId,
        request: GetPooledTransactions,
        response: oneshot::Sender<RequestResult<PooledTransactions<N::PooledTransaction>>>,
    ) {
        // fast exit if gossip is disabled
        if self.network.tx_gossip_disabled() {
            let _ = response.send(Ok(PooledTransactions::default()));
            return
        }
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            let transactions = self.pool.get_pooled_transaction_elements(
                request.0,
                GetPooledTransactionLimit::ResponseSizeSoftLimit(
                    self.transaction_fetcher.info.soft_limit_byte_size_pooled_transactions_response,
                ),
            );
            trace!(target: "net::tx::propagation", sent_txs=?transactions.iter().map(|tx| tx.tx_hash()), "Sending requested transactions to peer");

            // we sent a response at which point we assume that the peer is aware of the
            // transactions
            peer.seen_transactions.extend(transactions.iter().map(|tx| *tx.tx_hash()));

            let resp = PooledTransactions(transactions);
            let _ = response.send(Ok(resp));
        }
    }

    /// Handles a command received from a detached [`TransactionsHandle`]
    fn on_command(&mut self, cmd: TransactionsCommand<N>) {
        match cmd {
            TransactionsCommand::PropagateHash(hash) => {
                self.on_new_pending_transactions(vec![hash])
            }
            TransactionsCommand::PropagateHashesTo(hashes, peer) => {
                self.propagate_hashes_to(hashes, peer, PropagationMode::Forced)
            }
            TransactionsCommand::GetActivePeers(tx) => {
                let peers = self.peers.keys().copied().collect::<HashSet<_>>();
                tx.send(peers).ok();
            }
            TransactionsCommand::PropagateTransactionsTo(txs, peer) => {
                if let Some(propagated) =
                    self.propagate_full_transactions_to_peer(txs, peer, PropagationMode::Forced)
                {
                    self.pool.on_propagated(propagated);
                }
            }
            TransactionsCommand::PropagateTransactions(txs) => self.propagate_all(txs),
            TransactionsCommand::BroadcastTransactions(txs) => {
                let propagated = self.propagate_transactions(txs, PropagationMode::Forced);
                self.pool.on_propagated(propagated);
            }
            TransactionsCommand::GetTransactionHashes { peers, tx } => {
                let mut res = HashMap::with_capacity_and_hasher(peers.len(), Default::default());
                for peer_id in peers {
                    let hashes = self
                        .peers
                        .get(&peer_id)
                        .map(|peer| peer.seen_transactions.iter().copied().collect::<B256Set>())
                        .unwrap_or_default();
                    res.insert(peer_id, hashes);
                }
                tx.send(res).ok();
            }
            TransactionsCommand::GetPeerSender { peer_id, peer_request_sender } => {
                let sender = self.peers.get(&peer_id).map(|peer| peer.request_tx.clone());
                peer_request_sender.send(sender).ok();
            }
        }
    }

    /// Handles session establishment and peer transactions initialization.
    ///
    /// This is invoked when a new session is established.
    fn handle_peer_session(
        &mut self,
        info: SessionInfo,
        messages: PeerRequestSender<PeerRequest<N>>,
    ) {
        let SessionInfo { peer_id, client_version, version, .. } = info;

        // Insert a new peer into the peerset.
        let peer = PeerMetadata::<N>::new(
            messages,
            version,
            client_version,
            self.config.max_transactions_seen_by_peer_history,
            info.peer_kind,
        );
        let peer = match self.peers.entry(peer_id) {
            Entry::Occupied(mut entry) => {
                entry.insert(peer);
                entry.into_mut()
            }
            Entry::Vacant(entry) => entry.insert(peer),
        };

        self.policies.propagation_policy_mut().on_session_established(peer);

        // Send a `NewPooledTransactionHashes` to the peer with up to
        // `SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE`
        // transactions in the pool.
        if self.network.is_initially_syncing() || self.network.tx_gossip_disabled() {
            trace!(target: "net::tx", ?peer_id, "Skipping transaction broadcast: node syncing or gossip disabled");
            return
        }

        // Get transactions to broadcast
        let pooled_txs = self.pool.pooled_transactions_max(
            SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE,
        );
        if pooled_txs.is_empty() {
            trace!(target: "net::tx", ?peer_id, "No transactions in the pool to broadcast");
            return;
        }

        // Build and send transaction hashes message
        let mut msg_builder = PooledTransactionsHashesBuilder::new(version);
        for pooled_tx in pooled_txs {
            peer.seen_transactions.insert(*pooled_tx.hash());
            msg_builder.push_pooled(pooled_tx);
        }

        debug!(target: "net::tx", ?peer_id, tx_count = msg_builder.len(), "Broadcasting transaction hashes");
        let msg = msg_builder.build();
        self.network.send_transactions_hashes(peer_id, msg);
    }

    /// Handles a received event related to common network events.
    fn on_network_event(&mut self, event_result: NetworkEvent<PeerRequest<N>>) {
        match event_result {
            NetworkEvent::Peer(PeerEvent::SessionClosed { peer_id, .. }) => {
                self.on_peer_session_closed(&peer_id);
            }
            NetworkEvent::ActivePeerSession { info, messages } => {
                // process active peer session and broadcast available transaction from the pool
                self.handle_peer_session(info, messages);
            }
            NetworkEvent::Peer(PeerEvent::SessionEstablished(info)) => {
                let peer_id = info.peer_id;
                // get messages from existing peer
                let messages = match self.peers.get(&peer_id) {
                    Some(p) => p.request_tx.clone(),
                    None => {
                        debug!(target: "net::tx", ?peer_id, "No peer request sender found");
                        return;
                    }
                };
                self.handle_peer_session(info, messages);
            }
            _ => {}
        }
    }

    /// Returns true if the ingress policy allows processing messages from the given peer.
    fn accepts_incoming_from(&self, peer_id: &PeerId) -> bool {
        if self.config.ingress_policy.allows_all() {
            return true;
        }
        let Some(peer) = self.peers.get(peer_id) else {
            return false;
        };
        self.config.ingress_policy.allows(peer.peer_kind())
    }

    /// Handles dedicated transaction events related to the `eth` protocol.
    fn on_network_tx_event(&mut self, event: NetworkTransactionEvent<N>) {
        match event {
            NetworkTransactionEvent::IncomingTransactions { peer_id, msg } => {
                if !self.accepts_incoming_from(&peer_id) {
                    trace!(target: "net::tx", peer_id=format!("{peer_id:#}"), policy=?self.config.ingress_policy, "Ignoring full transactions from peer blocked by ingress policy");
                    return;
                }

                // ensure we didn't receive any blob transactions as these are disallowed to be
                // broadcasted in full

                let has_blob_txs = msg.has_eip4844();

                let non_blob_txs = msg
                    .into_iter()
                    .map(N::PooledTransaction::try_from)
                    .filter_map(Result::ok)
                    .collect();

                self.import_transactions(peer_id, non_blob_txs, TransactionSource::Broadcast);

                if has_blob_txs {
                    debug!(target: "net::tx", ?peer_id, "received bad full blob transaction broadcast");
                    self.report_peer_bad_transactions(peer_id);
                }
            }
            NetworkTransactionEvent::IncomingPooledTransactionHashes { peer_id, msg } => {
                if !self.accepts_incoming_from(&peer_id) {
                    trace!(target: "net::tx", peer_id=format!("{peer_id:#}"), policy=?self.config.ingress_policy, "Ignoring transaction hashes from peer blocked by ingress policy");
                    return;
                }
                self.on_new_pooled_transaction_hashes(peer_id, msg)
            }
            NetworkTransactionEvent::GetPooledTransactions { peer_id, request, response } => {
                self.on_get_pooled_transactions(peer_id, request, response)
            }
            NetworkTransactionEvent::GetTransactionsHandle(response) => {
                let _ = response.send(Some(self.handle()));
            }
        }
    }

    /// Starts the import process for the given transactions.
    fn import_transactions(
        &mut self,
        peer_id: PeerId,
        transactions: PooledTransactions<N::PooledTransaction>,
        source: TransactionSource,
    ) {
        // If the node is pipeline syncing, ignore transactions
        if self.network.is_initially_syncing() {
            return
        }
        if self.network.tx_gossip_disabled() {
            return
        }

        // Early return if we don't have capacity for any imports
        if !self.has_capacity_for_pending_pool_imports() {
            return
        }

        let mut transactions = transactions.0;

        // Truncate to remaining capacity early to bound work on all subsequent processing.
        // Well-behaved peers follow the 4096 soft limit, so oversized payloads are likely
        // malicious and we avoid wasting CPU on them.
        let capacity = self.remaining_pool_import_capacity();
        if transactions.len() > capacity {
            let skipped = transactions.len() - capacity;
            transactions.truncate(capacity);
            self.metrics
                .skipped_transactions_pending_pool_imports_at_capacity
                .increment(skipped as u64);
            trace!(target: "net::tx", skipped, capacity, "Truncated transactions batch to capacity");
        }

        let Some(peer) = self.peers.get_mut(&peer_id) else { return };
        let client_version = peer.client_version.clone();

        let start = Instant::now();

        // mark the transactions as received
        self.transaction_fetcher
            .remove_hashes_from_transaction_fetcher(transactions.iter().map(|tx| tx.tx_hash()));

        // track that the peer knows these transaction, but only if this is a new broadcast.
        // If we received the transactions as the response to our `GetPooledTransactions``
        // requests (based on received `NewPooledTransactionHashes`) then we already
        // recorded the hashes as seen by this peer in `Self::on_new_pooled_transaction_hashes`.
        let mut num_already_seen_by_peer = 0;
        for tx in &transactions {
            if source.is_broadcast() && !peer.seen_transactions.insert(*tx.tx_hash()) {
                num_already_seen_by_peer += 1;
            }
        }

        // tracks the quality of the given transactions
        let mut has_bad_transactions = false;

        // 1. Remove known, already-tracked, and invalid transactions first since these are
        // cheap in-memory checks against local maps
        transactions.retain(|tx| {
            if let Entry::Occupied(mut entry) = self.transactions_by_peers.entry(*tx.tx_hash()) {
                let peers = entry.get_mut();
                if !peers.contains(&peer_id) {
                    peers.push(peer_id);
                }
                return false
            }
            if self.bad_imports.contains(tx.tx_hash()) {
                trace!(target: "net::tx",
                    peer_id=format!("{peer_id:#}"),
                    hash=%tx.tx_hash(),
                    %client_version,
                    "received a known bad transaction from peer"
                );
                has_bad_transactions = true;
                return false;
            }
            true
        });

        // 2. filter out txns already inserted into pool
        let txns_count_pre_pool_filter = transactions.len();
        self.pool.retain_unknown(&mut transactions);
        if txns_count_pre_pool_filter > transactions.len() {
            let already_known_txns_count = txns_count_pre_pool_filter - transactions.len();
            self.metrics
                .occurrences_transactions_already_in_pool
                .increment(already_known_txns_count as u64);
        }

        let txs_len = transactions.len();

        let recover = |tx| match Pool::Transaction::try_recover(tx) {
            Ok(tx) => Some(tx),
            Err(badtx) => {
                trace!(target: "net::tx",
                    peer_id=format!("{peer_id:#}"),
                    hash=%badtx.tx_hash(),
                    client_version=%client_version,
                    "failed ecrecovery for transaction"
                );
                None
            }
        };

        let new_txs = transactions.into_par_iter().filter_map(recover).collect::<Vec<_>>();

        has_bad_transactions |= new_txs.len() != txs_len;

        // Record the transactions as seen by the peer
        for tx in &new_txs {
            self.transactions_by_peers.insert(*tx.hash(), smallvec::smallvec![peer_id]);
        }

        // 3. import new transactions as a batch to minimize lock contention on the underlying
        // pool
        if !new_txs.is_empty() {
            let pool = self.pool.clone();
            // update metrics
            let metric_pending_pool_imports = self.metrics.pending_pool_imports.clone();
            metric_pending_pool_imports.increment(new_txs.len() as f64);

            // update self-monitoring info
            self.pending_pool_imports_info
                .pending_pool_imports
                .fetch_add(new_txs.len(), Ordering::Relaxed);
            let tx_manager_info_pending_pool_imports =
                self.pending_pool_imports_info.pending_pool_imports.clone();

            trace!(target: "net::tx::propagation", new_txs_len=?new_txs.len(), "Importing new transactions");
            let import = Box::pin(async move {
                let added = new_txs.len();
                let res = pool.add_external_transactions(new_txs).await;

                // update metrics
                metric_pending_pool_imports.decrement(added as f64);
                // update self-monitoring info
                tx_manager_info_pending_pool_imports.fetch_sub(added, Ordering::Relaxed);

                res
            });

            self.pool_imports.push(import);
        }

        if num_already_seen_by_peer > 0 {
            self.metrics.messages_with_transactions_already_seen_by_peer.increment(1);
            self.metrics
                .occurrences_of_transaction_already_seen_by_peer
                .increment(num_already_seen_by_peer);
            trace!(target: "net::tx", num_txs=%num_already_seen_by_peer, ?peer_id, client=%client_version, "Peer sent already seen transactions");
        }

        if has_bad_transactions {
            // peer sent us invalid transactions
            self.report_peer_bad_transactions(peer_id)
        }

        if num_already_seen_by_peer > 0 {
            self.report_already_seen(peer_id);
        }

        self.metrics.pool_import_prepare_duration.record(start.elapsed());
    }

    /// Processes a [`FetchEvent`].
    fn on_fetch_event(&mut self, fetch_event: FetchEvent<N::PooledTransaction>) {
        match fetch_event {
            FetchEvent::TransactionsFetched { peer_id, transactions, report_peer } => {
                self.import_transactions(peer_id, transactions, TransactionSource::Response);
                if report_peer {
                    self.report_peer(peer_id, ReputationChangeKind::BadTransactions);
                }
            }
            FetchEvent::FetchError { peer_id, error } => {
                trace!(target: "net::tx", ?peer_id, %error, "requesting transactions from peer failed");
                self.on_request_error(peer_id, error);
            }
            FetchEvent::EmptyResponse { peer_id } => {
                trace!(target: "net::tx", ?peer_id, "peer returned empty response");
            }
        }
    }
}

/// An endless future. Preemption ensure that future is non-blocking, nonetheless. See
/// [`crate::NetworkManager`] for more context on the design pattern.
///
/// This should be spawned or used as part of `tokio::select!`.
//
// spawned in `NodeConfig::start_network`(reth_node_core::NodeConfig) and
// `NetworkConfig::start_network`(reth_network::NetworkConfig)
impl<
        Pool: TransactionPool + Unpin + 'static,
        N: NetworkPrimitives<
                BroadcastedTransaction: SignedTransaction,
                PooledTransaction: SignedTransaction,
            > + Unpin,
    > Future for TransactionsManager<Pool, N>
where
    Pool::Transaction:
        PoolTransaction<Consensus = N::BroadcastedTransaction, Pooled = N::PooledTransaction>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let start = Instant::now();
        let mut poll_durations = TxManagerPollDurations::default();

        let this = self.get_mut();

        // All streams are polled until their corresponding budget is exhausted, then we manually
        // yield back control to tokio. See `NetworkManager` for more context on the design
        // pattern.

        // Advance network/peer related events (update peers map).
        let maybe_more_network_events = metered_poll_nested_stream_with_budget!(
            poll_durations.acc_network_events,
            "net::tx",
            "Network events stream",
            DEFAULT_BUDGET_TRY_DRAIN_STREAM,
            this.network_events.poll_next_unpin(cx),
            |event| this.on_network_event(event)
        );

        // Advance incoming transaction events (stream new txns/announcements from
        // network manager and queue for import to pool/fetch txns).
        //
        // This will potentially remove hashes from hashes pending fetch, it the event
        // is an announcement (if same hashes are announced that didn't fit into a
        // previous request).
        //
        // The smallest decodable transaction is an empty legacy transaction, 10 bytes
        // (128 KiB / 10 bytes > 13k transactions).
        //
        // If this is an event with `Transactions` message, since transactions aren't
        // validated until they are inserted into the pool, this can potentially queue
        // >13k transactions for insertion to pool. More if the message size is bigger
        // than the soft limit on a `Transactions` broadcast message, which is 128 KiB.
        let maybe_more_tx_events = metered_poll_nested_stream_with_budget!(
            poll_durations.acc_tx_events,
            "net::tx",
            "Network transaction events stream",
            DEFAULT_BUDGET_TRY_DRAIN_NETWORK_TRANSACTION_EVENTS,
            this.transaction_events.poll_next_unpin(cx),
            |event: NetworkTransactionEvent<N>| this.on_network_tx_event(event),
        );

        // Advance inflight fetch requests (flush transaction fetcher and queue for
        // import to pool).
        //
        // The smallest decodable transaction is an empty legacy transaction, 10 bytes
        // (2 MiB / 10 bytes > 200k transactions).
        //
        // Since transactions aren't validated until they are inserted into the pool,
        // this can potentially queue >200k transactions for insertion to pool. More
        // if the message size is bigger than the soft limit on a `PooledTransactions`
        // response which is 2 MiB.
        let mut maybe_more_tx_fetch_events = metered_poll_nested_stream_with_budget!(
            poll_durations.acc_fetch_events,
            "net::tx",
            "Transaction fetch events stream",
            DEFAULT_BUDGET_TRY_DRAIN_STREAM,
            this.transaction_fetcher.poll_next_unpin(cx),
            |event| this.on_fetch_event(event),
        );

        // Advance pool imports (flush txns to pool).
        //
        // Note, this is done in batches. A batch is filled from one `Transactions`
        // broadcast messages or one `PooledTransactions` response at a time. The
        // minimum batch size is 1 transaction (and might often be the case with blob
        // transactions).
        //
        // The smallest decodable transaction is an empty legacy transaction, 10 bytes
        // (2 MiB / 10 bytes > 200k transactions).
        //
        // Since transactions aren't validated until they are inserted into the pool,
        // this can potentially validate >200k transactions. More if the message size
        // is bigger than the soft limit on a `PooledTransactions` response which is
        // 2 MiB (`Transactions` broadcast messages is smaller, 128 KiB).
        let maybe_more_pool_imports = metered_poll_nested_stream_with_budget!(
            poll_durations.acc_pending_imports,
            "net::tx",
            "Batched pool imports stream",
            DEFAULT_BUDGET_TRY_DRAIN_PENDING_POOL_IMPORTS,
            this.pool_imports.poll_next_unpin(cx),
            |batch_results| this.on_batch_import_result(batch_results)
        );

        // Advances new __pending__ transactions, transactions that were successfully inserted into
        // pending set in pool (are valid), and propagates them (inform peers which
        // transactions we have seen).
        //
        // This is polled after pool imports so transactions that became pending in this poll
        // iteration are propagated immediately, instead of waiting for the task to be woken
        // again.
        //
        // We try to drain this to batch the transactions in a single message.
        //
        // We don't expect this buffer to be large, since only pending transactions are
        // emitted here.
        let mut new_txs = Vec::new();
        let maybe_more_pending_txns = match this.pending_transactions.poll_recv_many(
            cx,
            &mut new_txs,
            SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE,
        ) {
            Poll::Ready(count) => {
                if count == SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE {
                    // we filled the entire buffer capacity and need to try again on the next poll
                    // immediately
                    true
                } else {
                    // try once more, because mostlikely the channel is now empty and the waker is
                    // registered if this is pending, if we filled additional hashes, we poll again
                    // on the next iteration
                    let limit =
                        SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE -
                            new_txs.len();
                    this.pending_transactions.poll_recv_many(cx, &mut new_txs, limit).is_ready()
                }
            }
            Poll::Pending => false,
        };
        if !new_txs.is_empty() {
            this.on_new_pending_transactions(new_txs);
        }

        // Tries to drain hashes pending fetch cache if the tx manager currently has
        // capacity for this (fetch txns).
        //
        // Sends at most one request.
        duration_metered_exec!(
            {
                if this.has_capacity_for_fetching_pending_hashes() &&
                    this.on_fetch_hashes_pending_fetch()
                {
                    maybe_more_tx_fetch_events = true;
                }
            },
            poll_durations.acc_pending_fetch
        );

        // Advance commands (propagate/fetch/serve txns).
        let maybe_more_commands = metered_poll_nested_stream_with_budget!(
            poll_durations.acc_cmds,
            "net::tx",
            "Commands channel",
            DEFAULT_BUDGET_TRY_DRAIN_STREAM,
            this.command_rx.poll_next_unpin(cx),
            |cmd| this.on_command(cmd)
        );

        this.transaction_fetcher.update_metrics();

        // all channels are fully drained and import futures pending
        if maybe_more_network_events ||
            maybe_more_commands ||
            maybe_more_tx_events ||
            maybe_more_tx_fetch_events ||
            maybe_more_pool_imports ||
            maybe_more_pending_txns
        {
            // make sure we're woken up again
            cx.waker().wake_by_ref();
            return Poll::Pending
        }

        this.update_poll_metrics(start, poll_durations);

        Poll::Pending
    }
}

/// Represents the different modes of transaction propagation.
///
/// This enum is used to determine how transactions are propagated to peers in the network.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum PropagationMode {
    /// Default propagation mode.
    ///
    /// Transactions are only sent to peers that haven't seen them yet.
    Basic,
    /// Forced propagation mode.
    ///
    /// Transactions are sent to all peers regardless of whether they have been sent or received
    /// before.
    Forced,
}

impl PropagationMode {
    /// Returns `true` if the propagation kind is `Forced`.
    const fn is_forced(self) -> bool {
        matches!(self, Self::Forced)
    }
}

/// A transaction that's about to be propagated to multiple peers.
#[derive(Debug, Clone)]
struct PropagateTransaction {
    is_broadcastable_in_full: bool,
    /// Size advertised in `NewPooledTransactionHashes` metadata and used for full broadcast
    /// soft-limit accounting.
    ///
    /// This is the network encoded transaction size. For pool-backed blob transactions, this is
    /// the pool's cached encoded length, which includes the sidecar returned by
    /// `PooledTransactions`.
    propagation_size: usize,
    transaction: LazyEncodedTransaction,
}

impl PropagateTransaction {
    /// Create a new instance from a transaction supplied directly for propagation.
    ///
    /// Direct transactions use their EIP-2718 encoded length so eth/68+ hash announcements carry
    /// the same size metadata as [`NewPooledTransactionHashes68::push`] and
    /// [`NewPooledTransactionHashes72::push`].
    fn new<T: SignedTransaction>(transaction: T) -> Self {
        let is_broadcastable_in_full = transaction.is_broadcastable_in_full();
        let propagation_size = transaction.encode_2718_len();

        Self {
            is_broadcastable_in_full,
            propagation_size,
            transaction: LazyEncoded::new(transaction),
        }
    }

    /// Create a new instance from a pooled transaction.
    ///
    /// Pool transactions already cache the network encoded size used by txpool admission and
    /// pooled hash announcements. For blob transactions, this includes the sidecar size expected in
    /// a `PooledTransactions` response.
    fn pool_tx<P: PoolTransaction>(tx: Arc<ValidPoolTransaction<P>>) -> Self {
        let is_broadcastable_in_full = tx.transaction.consensus_ref().is_broadcastable_in_full();
        let propagation_size = tx.encoded_length();
        Self {
            is_broadcastable_in_full,
            propagation_size,
            transaction: LazyEncoded::new(PropagatePooledTransactionEncoder::new(tx)),
        }
    }

    fn tx_hash(&self) -> &TxHash {
        self.transaction.tx_hash()
    }

    /// Returns the network encoded size used for propagation limits and hash metadata.
    const fn propagation_size(&self) -> usize {
        self.propagation_size
    }

    fn tx_type(&self) -> u8 {
        self.transaction.ty()
    }

    const fn is_broadcastable_in_full(&self) -> bool {
        self.is_broadcastable_in_full
    }

    fn shared(&self) -> LazyEncodedTransaction {
        self.transaction.clone()
    }
}

/// A pooled transaction encoder that avoids cloning into the consensus transaction for propagation.
#[derive(Debug)]
struct PropagatePooledTransactionEncoder<P: PoolTransaction> {
    transaction: Arc<ValidPoolTransaction<P>>,
}

impl<P: PoolTransaction> PropagatePooledTransactionEncoder<P> {
    const fn new(transaction: Arc<ValidPoolTransaction<P>>) -> Self {
        Self { transaction }
    }

    fn encode_uncached(&self, out: &mut dyn BufMut) {
        (*self.transaction.transaction.consensus_ref().inner()).encode(out);
    }
}

impl<P: PoolTransaction> Encodable for PropagatePooledTransactionEncoder<P> {
    fn encode(&self, out: &mut dyn BufMut) {
        self.encode_uncached(out);
    }

    fn length(&self) -> usize {
        (*self.transaction.transaction.consensus_ref().inner()).length()
    }
}

impl<P: PoolTransaction> TxHashRef for PropagatePooledTransactionEncoder<P> {
    fn tx_hash(&self) -> &TxHash {
        self.transaction.hash()
    }
}

impl<P: PoolTransaction> Typed2718 for PropagatePooledTransactionEncoder<P> {
    fn ty(&self) -> u8 {
        self.transaction.transaction.ty()
    }
}

fn transaction_hashes_to_reannounce<T: PoolTransaction>(
    pending: impl IntoIterator<Item = Arc<ValidPoolTransaction<T>>>,
    now: Instant,
    reannounce_time: Duration,
) -> Vec<TxHash> {
    pending
        .into_iter()
        .filter(|tx| {
            tx.propagate &&
                tx.is_local() &&
                now.saturating_duration_since(tx.timestamp) >= reannounce_time
        })
        .map(|tx| *tx.hash())
        .take(DEFAULT_MAX_COUNT_REANNOUNCED_LOCAL_TRANSACTIONS)
        .collect()
}

/// Helper type to construct the appropriate message to send to the peer based on whether the peer
/// should receive them in full or as pooled
#[derive(Debug, Clone)]
enum PropagateTransactionsBuilder {
    Pooled(PooledTransactionsHashesBuilder),
    Full(FullTransactionsBuilder),
}

impl PropagateTransactionsBuilder {
    /// Create a builder for pooled transactions with capacity for the expected number of
    /// transactions.
    fn pooled(version: EthVersion, capacity: usize) -> Self {
        Self::Pooled(PooledTransactionsHashesBuilder::with_capacity(version, capacity))
    }

    /// Create a builder that sends transactions in full and records transactions that don't fit,
    /// with capacity for the expected number of transactions.
    fn full(version: EthVersion, capacity: usize) -> Self {
        Self::Full(FullTransactionsBuilder::with_capacity(version, capacity))
    }

    /// Returns true if no transactions are recorded.
    fn is_empty(&self) -> bool {
        match self {
            Self::Pooled(builder) => builder.is_empty(),
            Self::Full(builder) => builder.is_empty(),
        }
    }

    /// Consumes the type and returns the built messages that should be sent to the peer.
    fn build(self) -> PropagateTransactions {
        match self {
            Self::Pooled(pooled) => {
                PropagateTransactions { pooled: Some(pooled.build()), full: None }
            }
            Self::Full(full) => full.build(),
        }
    }
}

impl PropagateTransactionsBuilder {
    /// Appends a transaction to the list.
    fn push(&mut self, transaction: &PropagateTransaction) {
        match self {
            Self::Pooled(builder) => builder.push(transaction),
            Self::Full(builder) => builder.push(transaction),
        }
    }

    /// Appends a transaction as a hash-only announcement, regardless of whether this builder
    /// would otherwise send full transactions.
    fn push_pooled(&mut self, transaction: &PropagateTransaction<T>) {
        match self {
            Self::Pooled(builder) => builder.push(transaction),
            Self::Full(builder) => builder.pooled.push(transaction),
        }
    }
}

/// Represents how the transactions should be sent to a peer if any.
struct PropagateTransactions {
    /// The pooled transaction hashes to send.
    pooled: Option<NewPooledTransactionHashes>,
    /// The transactions to send in full.
    full: Option<BroadcastPoolTransactions>,
}

/// Helper type for constructing the full transaction message that enforces the
/// [`DEFAULT_SOFT_LIMIT_BYTE_SIZE_TRANSACTIONS_BROADCAST_MESSAGE`] for full transaction broadcast
/// and enforces other propagation rules for EIP-4844 and tracks those transactions that can't be
/// broadcasted in full.
#[derive(Debug, Clone)]
struct FullTransactionsBuilder {
    /// The soft limit to enforce for a single broadcast message of full transactions.
    total_size: usize,
    /// All transactions to be broadcasted.
    transactions: Vec<LazyEncodedTransaction>,
    /// Transactions that didn't fit into the broadcast message
    pooled: PooledTransactionsHashesBuilder,
}

impl FullTransactionsBuilder {
    /// Create a builder for the negotiated version of the peer's session
    fn new(version: EthVersion) -> Self {
        Self {
            total_size: 0,
            pooled: PooledTransactionsHashesBuilder::new(version),
            transactions: vec![],
        }
    }

    /// Create a builder with capacity for the expected number of full transactions.
    ///
    /// The overflow hashes builder remains lazily allocated since most transactions are expected
    /// to be broadcast in full.
    fn with_capacity(version: EthVersion, capacity: usize) -> Self {
        Self {
            total_size: 0,
            pooled: PooledTransactionsHashesBuilder::new(version),
            transactions: Vec::with_capacity(capacity),
        }
    }

    /// Returns whether or not any transactions are in the [`FullTransactionsBuilder`].
    fn is_empty(&self) -> bool {
        self.transactions.is_empty() && self.pooled.is_empty()
    }

    /// Returns the messages that should be propagated to the peer.
    fn build(self) -> PropagateTransactions {
        let pooled = Some(self.pooled.build()).filter(|pooled| !pooled.is_empty());
        let full =
            (!self.transactions.is_empty()).then_some(BroadcastPoolTransactions(self.transactions));
        PropagateTransactions { pooled, full }
    }

    /// Appends all transactions.
    fn extend(&mut self, txs: impl IntoIterator<Item = PropagateTransaction>) {
        for tx in txs {
            self.push(&tx)
        }
    }

    /// Append a transaction to the list of full transaction if the total message bytes size doesn't
    /// exceed the soft maximum target byte size. The limit is soft, meaning if one single
    /// transaction goes over the limit, it will be broadcasted in its own [`Transactions`]
    /// message. The same pattern is followed in filling a [`GetPooledTransactions`] request in
    /// [`TransactionFetcher::fill_request_from_hashes_pending_fetch`].
    ///
    /// If the transaction is unsuitable for broadcast or would exceed the softlimit, it is appended
    /// to list of pooled transactions, (e.g. 4844 transactions).
    /// See also [`SignedTransaction::is_broadcastable_in_full`].
    fn push(&mut self, transaction: &PropagateTransaction) {
        // Do not send full 4844 transaction hashes to peers.
        //
        //  Nodes MUST NOT automatically broadcast blob transactions to their peers.
        //  Instead, those transactions are only announced using
        //  `NewPooledTransactionHashes` messages, and can then be manually requested
        //  via `GetPooledTransactions`.
        //
        // From: <https://eips.ethereum.org/EIPS/eip-4844#networking>
        if !transaction.is_broadcastable_in_full() {
            self.pooled.push(transaction);
            return
        }

        let new_size = self.total_size + transaction.propagation_size();
        if new_size > DEFAULT_SOFT_LIMIT_BYTE_SIZE_TRANSACTIONS_BROADCAST_MESSAGE &&
            self.total_size > 0
        {
            // transaction does not fit into the message
            self.pooled.push(transaction);
            return
        }

        self.total_size = new_size;
        self.transactions.push(transaction.shared());
    }
}

/// A helper type to create the pooled transactions message based on the negotiated version of the
/// session with the peer
#[derive(Debug, Clone)]
enum PooledTransactionsHashesBuilder {
    Eth66(NewPooledTransactionHashes66),
    Eth68(NewPooledTransactionHashes68),
    Eth72(NewPooledTransactionHashes72),
}

// === impl PooledTransactionsHashesBuilder ===

impl PooledTransactionsHashesBuilder {
    /// Push a transaction from the pool to the list.
    fn push_pooled<T: PoolTransaction>(&mut self, pooled_tx: Arc<ValidPoolTransaction<T>>) {
        match self {
            Self::Eth66(msg) => msg.push(*pooled_tx.hash()),
            Self::Eth68(msg) => {
                msg.hashes.push(*pooled_tx.hash());
                msg.sizes.push(pooled_tx.encoded_length());
                msg.types.push(pooled_tx.transaction.ty());
            }
            Self::Eth72(msg) => {
                msg.hashes.push(*pooled_tx.hash());
                msg.sizes.push(pooled_tx.encoded_length());
                msg.types.push(pooled_tx.transaction.ty());
            }
        }
    }

    /// Returns whether or not any transactions are in the [`PooledTransactionsHashesBuilder`].
    fn is_empty(&self) -> bool {
        match self {
            Self::Eth66(hashes) => hashes.is_empty(),
            Self::Eth68(hashes) => hashes.is_empty(),
            Self::Eth72(hashes) => hashes.is_empty(),
        }
    }

    /// Returns the number of transactions in the builder.
    fn len(&self) -> usize {
        match self {
            Self::Eth66(hashes) => hashes.len(),
            Self::Eth68(hashes) => hashes.len(),
            Self::Eth72(hashes) => hashes.len(),
        }
    }

    /// Appends all hashes
    fn extend(&mut self, txs: impl IntoIterator<Item = PropagateTransaction>) {
        for tx in txs {
            self.push(&tx);
        }
    }

    fn push(&mut self, tx: &PropagateTransaction) {
        match self {
            Self::Eth66(msg) => msg.push(*tx.tx_hash()),
            Self::Eth68(msg) => {
                msg.hashes.push(*tx.tx_hash());
                msg.sizes.push(tx.propagation_size());
                msg.types.push(tx.tx_type());
            }
            Self::Eth72(msg) => {
                msg.hashes.push(*tx.tx_hash());
                msg.sizes.push(tx.propagation_size());
                msg.types.push(tx.tx_type());
            }
        }
    }

    /// Create a builder for the negotiated version of the peer's session
    fn new(version: EthVersion) -> Self {
        match version {
            EthVersion::Eth66 | EthVersion::Eth67 => Self::Eth66(Default::default()),
            EthVersion::Eth68 | EthVersion::Eth69 | EthVersion::Eth70 | EthVersion::Eth71 => {
                Self::Eth68(Default::default())
            }
            EthVersion::Eth72 => Self::Eth72(Default::default()),
        }
    }

    /// Create a builder for the negotiated version of the peer's session with capacity for the
    /// expected number of hashes.
    fn with_capacity(version: EthVersion, capacity: usize) -> Self {
        match version {
            EthVersion::Eth66 | EthVersion::Eth67 => {
                Self::Eth66(NewPooledTransactionHashes66::with_capacity(capacity))
            }
            EthVersion::Eth68 | EthVersion::Eth69 | EthVersion::Eth70 | EthVersion::Eth71 => {
                Self::Eth68(NewPooledTransactionHashes68::with_capacity(capacity))
            }
            EthVersion::Eth72 => Self::Eth72(NewPooledTransactionHashes72::with_capacity(capacity)),
        }
    }

    fn build(self) -> NewPooledTransactionHashes {
        match self {
            Self::Eth66(mut msg) => {
                msg.shrink_to_fit();
                msg.into()
            }
            Self::Eth68(mut msg) => {
                msg.shrink_to_fit();
                msg.into()
            }
            Self::Eth72(mut msg) => {
                msg.shrink_to_fit();
                msg.into()
            }
        }
    }
}

/// How we received the transactions.
enum TransactionSource {
    /// Transactions were broadcast to us via [`Transactions`] message.
    Broadcast,
    /// Transactions were sent as the response of [`fetcher::GetPooledTxRequest`] issued by us.
    Response,
}

// === impl TransactionSource ===

impl TransactionSource {
    /// Whether the transaction were sent as broadcast.
    const fn is_broadcast(&self) -> bool {
        matches!(self, Self::Broadcast)
    }
}

/// Tracks a single peer in the context of [`TransactionsManager`].
#[derive(Debug)]
pub struct PeerMetadata<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// Optimistically keeps track of transactions that we know the peer has seen. Optimistic, in
    /// the sense that transactions are preemptively marked as seen by peer when they are sent to
    /// the peer.
    seen_transactions: LruCache<TxHash, FbBuildHasher<32>>,
    /// A communication channel directly to the peer's session task.
    request_tx: PeerRequestSender<PeerRequest<N>>,
    /// negotiated version of the session.
    version: EthVersion,
    /// The peer's client version.
    client_version: Arc<str>,
    /// The kind of peer.
    peer_kind: PeerKind,
}

impl<N: NetworkPrimitives> PeerMetadata<N> {
    /// Returns a new instance of [`PeerMetadata`].
    pub fn new(
        request_tx: PeerRequestSender<PeerRequest<N>>,
        version: EthVersion,
        client_version: Arc<str>,
        max_transactions_seen_by_peer: u32,
        peer_kind: PeerKind,
    ) -> Self {
        Self {
            seen_transactions: LruCache::with_hasher(
                max_transactions_seen_by_peer,
                Default::default(),
            ),
            request_tx,
            version,
            client_version,
            peer_kind,
        }
    }

    /// Returns a reference to the peer's request sender channel.
    pub const fn request_tx(&self) -> &PeerRequestSender<PeerRequest<N>> {
        &self.request_tx
    }

    /// Returns a mutable reference to the seen transactions LRU cache.
    pub const fn seen_transactions_mut(&mut self) -> &mut LruCache<TxHash, FbBuildHasher<32>> {
        &mut self.seen_transactions
    }

    /// Returns the negotiated `EthVersion` of the session.
    pub const fn version(&self) -> EthVersion {
        self.version
    }

    /// Returns a reference to the peer's client version string.
    pub fn client_version(&self) -> &str {
        &self.client_version
    }

    /// Returns the peer's kind.
    pub const fn peer_kind(&self) -> PeerKind {
        self.peer_kind
    }
}

/// Commands to send to the [`TransactionsManager`]
#[derive(Debug)]
enum TransactionsCommand<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// Propagate a transaction hash to the network.
    PropagateHash(B256),
    /// Propagate transaction hashes to a specific peer.
    PropagateHashesTo(Vec<B256>, PeerId),
    /// Request the list of active peer IDs from the [`TransactionsManager`].
    GetActivePeers(oneshot::Sender<HashSet<PeerId>>),
    /// Propagate a collection of full transactions to a specific peer.
    PropagateTransactionsTo(Vec<TxHash>, PeerId),
    /// Propagate a collection of hashes to all peers.
    PropagateTransactions(Vec<TxHash>),
    /// Propagate a collection of broadcastable transactions in full to all peers.
    BroadcastTransactions(Vec<PropagateTransaction>),
    /// Request transaction hashes known by specific peers from the [`TransactionsManager`].
    GetTransactionHashes { peers: Vec<PeerId>, tx: oneshot::Sender<HashMap<PeerId, B256Set>> },
    /// Requests a clone of the sender channel to the peer.
    GetPeerSender {
        peer_id: PeerId,
        peer_request_sender: oneshot::Sender<Option<PeerRequestSender<PeerRequest<N>>>>,
    },
}

/// All events related to transactions emitted by the network.
#[derive(Debug)]
pub enum NetworkTransactionEvent<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// Represents the event of receiving a list of transactions from a peer.
    ///
    /// This indicates transactions that were broadcasted to us from the peer.
    IncomingTransactions {
        /// The ID of the peer from which the transactions were received.
        peer_id: PeerId,
        /// The received transactions.
        msg: Transactions<N::BroadcastedTransaction>,
    },
    /// Represents the event of receiving a list of transaction hashes from a peer.
    IncomingPooledTransactionHashes {
        /// The ID of the peer from which the transaction hashes were received.
        peer_id: PeerId,
        /// The received new pooled transaction hashes.
        msg: NewPooledTransactionHashes,
    },
    /// Represents the event of receiving a `GetPooledTransactions` request from a peer.
    GetPooledTransactions {
        /// The ID of the peer from which the request was received.
        peer_id: PeerId,
        /// The received `GetPooledTransactions` request.
        request: GetPooledTransactions,
        /// The sender for responding to the request with a result of `PooledTransactions`.
        response: oneshot::Sender<RequestResult<PooledTransactions<N::PooledTransaction>>>,
    },
    /// Represents the event of receiving a `GetTransactionsHandle` request.
    GetTransactionsHandle(oneshot::Sender<Option<TransactionsHandle<N>>>),
}

/// Tracks stats about the [`TransactionsManager`].
#[derive(Debug)]
pub struct PendingPoolImportsInfo {
    /// Number of transactions about to be inserted into the pool.
    pending_pool_imports: Arc<AtomicUsize>,
    /// Max number of transactions allowed to be imported concurrently.
    max_pending_pool_imports: usize,
}

impl PendingPoolImportsInfo {
    /// Returns a new [`PendingPoolImportsInfo`].
    pub fn new(max_pending_pool_imports: usize) -> Self {
        Self { pending_pool_imports: Arc::new(AtomicUsize::default()), max_pending_pool_imports }
    }

    /// Returns `true` if the number of pool imports is under a given tolerated max.
    pub fn has_capacity(&self, max_pending_pool_imports: usize) -> bool {
        self.pending_pool_imports.load(Ordering::Relaxed) < max_pending_pool_imports
    }
}

impl Default for PendingPoolImportsInfo {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_COUNT_PENDING_POOL_IMPORTS)
    }
}

#[derive(Debug, Default)]
struct TxManagerPollDurations {
    acc_network_events: Duration,
    acc_pending_imports: Duration,
    acc_tx_events: Duration,
    acc_imported_txns: Duration,
    acc_fetch_events: Duration,
    acc_pending_fetch: Duration,
    acc_cmds: Duration,
}

impl<N: NetworkPrimitives> InMemorySize for NetworkTransactionEvent<N> {
    // `N::BroadcastedTransaction` and `N::PooledTransaction` already implement
    // `InMemorySize` via `SignedTransaction: InMemorySize`, so no extra bound is needed.
    fn size(&self) -> usize {
        match self {
            Self::IncomingTransactions { peer_id, msg } => {
                core::mem::size_of_val(peer_id) +
                    msg.0.iter().map(InMemorySize::size).sum::<usize>()
            }
            Self::IncomingPooledTransactionHashes { peer_id, msg } => {
                core::mem::size_of_val(peer_id) + msg.size()
            }
            Self::GetPooledTransactions { peer_id, request, response } => {
                core::mem::size_of_val(peer_id) +
                    request.0.len() * core::mem::size_of::<TxHash>() +
                    core::mem::size_of_val(response)
            }
            Self::GetTransactionsHandle(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_utils::{
            transactions::{buffer_hash_to_tx_fetcher, new_mock_session, new_tx_manager},
            Testnet,
        },
        transactions::config::RelaxedEthAnnouncementFilter,
        NetworkConfigBuilder, NetworkManager,
    };
    use alloy_consensus::{Transaction as _, TxEip1559, TxLegacy};
    use alloy_eips::{eip2718::Encodable2718, eip4844::BlobTransactionValidationError};
    use alloy_primitives::{hex, Signature, TxKind, B256, U256};
    use alloy_rlp::Decodable;
    use futures::FutureExt;
    use reth_chainspec::MIN_TRANSACTION_GAS;
    use reth_ethereum_primitives::{PooledTransactionVariant, Transaction, TransactionSigned};
    use reth_network_api::{NetworkInfo, PeerKind};
    use reth_network_p2p::{
        error::{RequestError, RequestResult},
        sync::{NetworkSyncUpdater, SyncState},
    };
    use reth_storage_api::noop::NoopProvider;
    use reth_tasks::Runtime;
    use reth_transaction_pool::{
        blobstore::InMemoryBlobStore,
        error::{Eip4844PoolTransactionError, InvalidPoolTransactionError, PoolError},
        identifier::SenderIdentifiers,
        test_utils::{
            testing_pool, MockTransaction, MockTransactionFactory, OkValidator, TestPool,
            TransactionGenerator,
        },
        CoinbaseTipOrdering, EthPooledTransaction, Pool, TransactionOrigin, ValidPoolTransaction,
    };
    use secp256k1::SecretKey;
    use std::{
        future::poll_fn,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        str::FromStr,
        time::Instant,
    };
    use tracing::error;

    type EthTestPool = Pool<
        OkValidator<EthPooledTransaction>,
        CoinbaseTipOrdering<EthPooledTransaction>,
        InMemoryBlobStore,
    >;

    async fn new_eth_tx_manager() -> (
        TransactionsManager<EthTestPool, EthNetworkPrimitives>,
        NetworkManager<EthNetworkPrimitives>,
    ) {
        let secret_key = SecretKey::new(&mut rand_08::thread_rng());
        let client = NoopProvider::default();

        let config = NetworkConfigBuilder::new(secret_key, Runtime::test())
            .listener_port(0)
            .disable_discovery()
            .build(client);

        let pool = Pool::new(
            OkValidator::default(),
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        );

        let transactions_manager_config = config.transactions_manager_config.clone();
        let (_network_handle, network, transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();

        (transactions, network)
    }

    fn valid_eth_pool_transaction(
        transaction: EthPooledTransaction,
    ) -> Arc<ValidPoolTransaction<EthPooledTransaction>> {
        let mut ids = SenderIdentifiers::default();
        let transaction_id =
            ids.sender_id_or_create(transaction.sender()).into_transaction_id(transaction.nonce());

        Arc::new(ValidPoolTransaction {
            propagate: false,
            transaction_id,
            transaction,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    fn gen_eip1559_pooled_with_nonce<R: rand::RngCore>(
        tx_gen: &mut TransactionGenerator<R>,
        nonce: u64,
    ) -> EthPooledTransaction {
        EthPooledTransaction::try_from_consensus(
            tx_gen.transaction().nonce(nonce).into_eip1559().try_into_recovered().unwrap(),
        )
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_ignored_tx_broadcasts_while_initially_syncing() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(3).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();
        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand_08::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::eth(secret_key, Runtime::test())
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let transactions_manager_config = config.transactions_manager_config.clone();
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();

        tokio::task::spawn(network);

        // go to syncing (pipeline sync)
        network_handle.update_sync_state(SyncState::Syncing);
        assert!(NetworkInfo::is_syncing(&network_handle));
        assert!(NetworkInfo::is_initially_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::Peer(PeerEvent::SessionEstablished(info)) => {
                    // to insert a new peer in transactions peerset
                    transactions
                        .on_network_event(NetworkEvent::Peer(PeerEvent::SessionEstablished(info)))
                }
                NetworkEvent::Peer(PeerEvent::PeerAdded(_peer_id)) => {}
                ev => {
                    error!("unexpected event {ev:?}")
                }
            }
        }
        // random tx: <https://etherscan.io/getRawTx?tx=0x9448608d36e721ef403c53b00546068a6474d6cbab6816c3926de449898e7bce>
        let input = hex!(
            "02f871018302a90f808504890aef60826b6c94ddf4c5025d1a5742cf12f74eec246d4432c295e487e09c3bbcc12b2b80c080a0f21a4eacd0bf8fea9c5105c543be5a1d8c796516875710fafafdf16d16d8ee23a001280915021bb446d1973501a67f93d2b38894a514b976e7b46dc2fe54598d76"
        );
        let signed_tx = TransactionSigned::decode(&mut &input[..]).unwrap();
        transactions.on_network_tx_event(NetworkTransactionEvent::IncomingTransactions {
            peer_id: *handle1.peer_id(),
            msg: Transactions(vec![signed_tx.clone()]),
        });
        poll_fn(|cx| {
            let _ = transactions.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;
        assert!(pool.is_empty());
        handle.terminate().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_tx_broadcasts_through_two_syncs() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(3).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();
        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand_08::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::new(secret_key, Runtime::test())
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let transactions_manager_config = config.transactions_manager_config.clone();
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();

        tokio::task::spawn(network);

        // go to syncing (pipeline sync) to idle and then to syncing (live)
        network_handle.update_sync_state(SyncState::Syncing);
        assert!(NetworkInfo::is_syncing(&network_handle));
        network_handle.update_sync_state(SyncState::Idle);
        assert!(!NetworkInfo::is_syncing(&network_handle));
        network_handle.update_sync_state(SyncState::Syncing);
        assert!(NetworkInfo::is_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::ActivePeerSession { .. } |
                NetworkEvent::Peer(PeerEvent::SessionEstablished(_)) => {
                    // to insert a new peer in transactions peerset
                    transactions.on_network_event(ev);
                }
                NetworkEvent::Peer(PeerEvent::PeerAdded(_peer_id)) => {}
                _ => {
                    error!("unexpected event {ev:?}")
                }
            }
        }
        // random tx: <https://etherscan.io/getRawTx?tx=0x9448608d36e721ef403c53b00546068a6474d6cbab6816c3926de449898e7bce>
        let input = hex!(
            "02f871018302a90f808504890aef60826b6c94ddf4c5025d1a5742cf12f74eec246d4432c295e487e09c3bbcc12b2b80c080a0f21a4eacd0bf8fea9c5105c543be5a1d8c796516875710fafafdf16d16d8ee23a001280915021bb446d1973501a67f93d2b38894a514b976e7b46dc2fe54598d76"
        );
        let signed_tx = TransactionSigned::decode(&mut &input[..]).unwrap();
        transactions.on_network_tx_event(NetworkTransactionEvent::IncomingTransactions {
            peer_id: *handle1.peer_id(),
            msg: Transactions(vec![signed_tx.clone()]),
        });
        poll_fn(|cx| {
            let _ = transactions.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;
        assert!(!NetworkInfo::is_initially_syncing(&network_handle));
        assert!(NetworkInfo::is_syncing(&network_handle));
        assert!(!pool.is_empty());
        handle.terminate().await;
    }

    // Ensure that the transaction manager correctly handles the `IncomingPooledTransactionHashes`
    // event and is able to retrieve the corresponding transactions.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_handle_incoming_transactions_hashes() {
        reth_tracing::init_test_tracing();

        let secret_key = SecretKey::new(&mut rand_08::thread_rng());
        let client = NoopProvider::default();

        let config = NetworkConfigBuilder::new(secret_key, Runtime::test())
            // let OS choose port
            .listener_port(0)
            .disable_discovery()
            .build(client);

        let pool = testing_pool();

        let transactions_manager_config = config.transactions_manager_config.clone();
        let (_network_handle, _network, mut tx_manager, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();

        let peer_id_1 = PeerId::new([1; 64]);
        let eth_version = EthVersion::Eth66;

        let txs = vec![TransactionSigned::new_unhashed(
            Transaction::Legacy(TxLegacy {
                chain_id: Some(4),
                nonce: 15u64,
                gas_price: 2200000000,
                gas_limit: 34811,
                to: TxKind::Call(hex!("cf7f9e66af820a19257a2108375b180b0ec49167").into()),
                value: U256::from(1234u64),
                input: Default::default(),
            }),
            Signature::new(
                U256::from_str(
                    "0x35b7bfeb9ad9ece2cbafaaf8e202e706b4cfaeb233f46198f00b44d4a566a981",
                )
                .unwrap(),
                U256::from_str(
                    "0x612638fb29427ca33b9a3be2a0a561beecfe0269655be160d35e72d366a6a860",
                )
                .unwrap(),
                true,
            ),
        )];

        let txs_hashes: Vec<B256> = txs.iter().map(|tx| *tx.hash()).collect();

        let (peer_1, mut to_mock_session_rx) = new_mock_session(peer_id_1, eth_version);
        tx_manager.peers.insert(peer_id_1, peer_1);

        assert!(pool.is_empty());

        tx_manager.on_network_tx_event(NetworkTransactionEvent::IncomingPooledTransactionHashes {
            peer_id: peer_id_1,
            msg: NewPooledTransactionHashes::from(NewPooledTransactionHashes66::from(
                txs_hashes.clone(),
            )),
        });

        // mock session of peer_1 receives request
        let req = to_mock_session_rx
            .recv()
            .await
            .expect("peer_1 session should receive request with buffered hashes");
        let PeerRequest::GetPooledTransactions { request, response } = req else { unreachable!() };
        assert_eq!(request, GetPooledTransactions::from(txs_hashes.clone()));

        let message: Vec<PooledTransactionVariant> = txs
            .into_iter()
            .map(|tx| {
                PooledTransactionVariant::try_from(tx)
                    .expect("Failed to convert MockTransaction to PooledTransaction")
            })
            .collect();

        // return the transactions corresponding to the transaction hashes.
        response
            .send(Ok(PooledTransactions(message)))
            .expect("should send peer_1 response to tx manager");

        // adance the transaction manager future
        poll_fn(|cx| {
            let _ = tx_manager.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;

        // ensure that the transactions corresponding to the transaction hashes have been
        // successfully retrieved and stored in the Pool.
        assert_eq!(pool.get_all(txs_hashes.clone()).len(), txs_hashes.len());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_handle_incoming_transactions() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(3).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();

        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand_08::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::new(secret_key, Runtime::test())
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let transactions_manager_config = config.transactions_manager_config.clone();
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();
        tokio::task::spawn(network);

        network_handle.update_sync_state(SyncState::Idle);

        assert!(!NetworkInfo::is_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::ActivePeerSession { .. } |
                NetworkEvent::Peer(PeerEvent::SessionEstablished(_)) => {
                    // to insert a new peer in transactions peerset
                    transactions.on_network_event(ev);
                }
                NetworkEvent::Peer(PeerEvent::PeerAdded(_peer_id)) => {}
                ev => {
                    error!("unexpected event {ev:?}")
                }
            }
        }
        // random tx: <https://etherscan.io/getRawTx?tx=0x9448608d36e721ef403c53b00546068a6474d6cbab6816c3926de449898e7bce>
        let input = hex!(
            "02f871018302a90f808504890aef60826b6c94ddf4c5025d1a5742cf12f74eec246d4432c295e487e09c3bbcc12b2b80c080a0f21a4eacd0bf8fea9c5105c543be5a1d8c796516875710fafafdf16d16d8ee23a001280915021bb446d1973501a67f93d2b38894a514b976e7b46dc2fe54598d76"
        );
        let signed_tx = TransactionSigned::decode(&mut &input[..]).unwrap();
        transactions.on_network_tx_event(NetworkTransactionEvent::IncomingTransactions {
            peer_id: *handle1.peer_id(),
            msg: Transactions(vec![signed_tx.clone()]),
        });
        assert!(transactions
            .transactions_by_peers
            .get(signed_tx.tx_hash())
            .unwrap()
            .contains(handle1.peer_id()));

        // advance the transaction manager future
        poll_fn(|cx| {
            let _ = transactions.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;

        assert!(!pool.is_empty());
        assert!(pool.get(signed_tx.tx_hash()).is_some());
        handle.terminate().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_session_closed_cleans_transaction_peer_state() {
        let (mut tx_manager, _network) = new_tx_manager().await;
        let peer_id = PeerId::new([1; 64]);
        let fallback_peer = PeerId::new([2; 64]);
        let (peer, _) = new_mock_session(peer_id, EthVersion::Eth66);
        let hash_shared = B256::from_slice(&[1; 32]);

        tx_manager.peers.insert(peer_id, peer);
        buffer_hash_to_tx_fetcher(
            &mut tx_manager.transaction_fetcher,
            hash_shared,
            peer_id,
            0,
            None,
        );
        buffer_hash_to_tx_fetcher(
            &mut tx_manager.transaction_fetcher,
            hash_shared,
            fallback_peer,
            0,
            None,
        );
        tx_manager.transaction_fetcher.active_peers.insert(peer_id, 1);

        tx_manager.on_network_event(NetworkEvent::Peer(PeerEvent::SessionClosed {
            peer_id,
            reason: None,
        }));

        // peer removed from peers map and active_peers
        assert!(!tx_manager.peers.contains_key(&peer_id));
        assert!(tx_manager.transaction_fetcher.active_peers.peek(&peer_id).is_none());
        // fallback peer is still available for the hash
        assert_eq!(
            tx_manager.transaction_fetcher.get_idle_peer_for(hash_shared),
            Some(&fallback_peer)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bad_blob_sidecar_not_cached_as_bad_import() {
        let (mut tx_manager, _network) = new_tx_manager().await;
        let peer_id = PeerId::new([1; 64]);
        let hash = B256::from_slice(&[1; 32]);

        tx_manager.network.update_sync_state(SyncState::Idle);
        tx_manager.transactions_by_peers.insert(hash, smallvec::smallvec![peer_id]);

        let err = PoolError::new(
            hash,
            InvalidPoolTransactionError::Eip4844(Eip4844PoolTransactionError::InvalidEip4844Blob(
                BlobTransactionValidationError::InvalidProof,
            )),
        );

        tx_manager.on_bad_import(err);

        assert!(!tx_manager.bad_imports.contains(&hash));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_missing_blob_sidecar_not_cached_as_bad_import() {
        let (mut tx_manager, _network) = new_tx_manager().await;
        let peer_id = PeerId::new([1; 64]);
        let hash = B256::from_slice(&[3; 32]);

        tx_manager.network.update_sync_state(SyncState::Idle);
        tx_manager.transactions_by_peers.insert(hash, smallvec::smallvec![peer_id]);

        let err = PoolError::new(
            hash,
            InvalidPoolTransactionError::Eip4844(
                Eip4844PoolTransactionError::MissingEip4844BlobSidecar,
            ),
        );

        tx_manager.on_bad_import(err);

        assert!(!tx_manager.bad_imports.contains(&hash));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_non_blob_sidecar_error_still_cached_as_bad_import() {
        let (mut tx_manager, _network) = new_tx_manager().await;
        let peer_id = PeerId::new([1; 64]);
        let hash = B256::from_slice(&[2; 32]);

        tx_manager.network.update_sync_state(SyncState::Idle);
        tx_manager.transactions_by_peers.insert(hash, smallvec::smallvec![peer_id]);

        let err = PoolError::new(
            hash,
            InvalidPoolTransactionError::Eip4844(Eip4844PoolTransactionError::NoEip4844Blobs),
        );

        tx_manager.on_bad_import(err);

        assert!(tx_manager.bad_imports.contains(&hash));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_on_get_pooled_transactions_network() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(2).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();

        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand_08::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::new(secret_key, Runtime::test())
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let transactions_manager_config = config.transactions_manager_config.clone();
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();
        tokio::task::spawn(network);

        network_handle.update_sync_state(SyncState::Idle);

        assert!(!NetworkInfo::is_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::ActivePeerSession { .. } |
                NetworkEvent::Peer(PeerEvent::SessionEstablished(_)) => {
                    transactions.on_network_event(ev);
                }
                NetworkEvent::Peer(PeerEvent::PeerAdded(_peer_id)) => {}
                ev => {
                    error!("unexpected event {ev:?}")
                }
            }
        }
        handle.terminate().await;

        let tx = MockTransaction::eip1559();
        let _ = transactions
            .pool
            .add_transaction(reth_transaction_pool::TransactionOrigin::External, tx.clone())
            .await;

        let request = GetPooledTransactions(vec![*tx.get_hash()]);

        let (send, receive) =
            oneshot::channel::<RequestResult<PooledTransactions<PooledTransactionVariant>>>();

        transactions.on_network_tx_event(NetworkTransactionEvent::GetPooledTransactions {
            peer_id: *handle1.peer_id(),
            request,
            response: send,
        });

        match receive.await.unwrap() {
            Ok(PooledTransactions(transactions)) => {
                assert_eq!(transactions.len(), 1);
            }
            Err(e) => {
                panic!("error: {e:?}");
            }
        }
    }

    // Ensure that when the remote peer only returns part of the requested transactions, the
    // replied transactions are removed from the `tx_fetcher`, while the unresponsive ones are
    // re-buffered.
    #[tokio::test]
    async fn test_partially_tx_response() {
        reth_tracing::init_test_tracing();

        let mut tx_manager = new_tx_manager().await.0;
        let tx_fetcher = &mut tx_manager.transaction_fetcher;

        let peer_id_1 = PeerId::new([1; 64]);
        let eth_version = EthVersion::Eth66;

        let txs = vec![
            TransactionSigned::new_unhashed(
                Transaction::Legacy(TxLegacy {
                    chain_id: Some(4),
                    nonce: 15u64,
                    gas_price: 2200000000,
                    gas_limit: 34811,
                    to: TxKind::Call(hex!("cf7f9e66af820a19257a2108375b180b0ec49167").into()),
                    value: U256::from(1234u64),
                    input: Default::default(),
                }),
                Signature::new(
                    U256::from_str(
                        "0x35b7bfeb9ad9ece2cbafaaf8e202e706b4cfaeb233f46198f00b44d4a566a981",
                    )
                    .unwrap(),
                    U256::from_str(
                        "0x612638fb29427ca33b9a3be2a0a561beecfe0269655be160d35e72d366a6a860",
                    )
                    .unwrap(),
                    true,
                ),
            ),
            TransactionSigned::new_unhashed(
                Transaction::Eip1559(TxEip1559 {
                    chain_id: 4,
                    nonce: 26u64,
                    max_priority_fee_per_gas: 1500000000,
                    max_fee_per_gas: 1500000013,
                    gas_limit: MIN_TRANSACTION_GAS,
                    to: TxKind::Call(hex!("61815774383099e24810ab832a5b2a5425c154d5").into()),
                    value: U256::from(3000000000000000000u64),
                    input: Default::default(),
                    access_list: Default::default(),
                }),
                Signature::new(
                    U256::from_str(
                        "0x59e6b67f48fb32e7e570dfb11e042b5ad2e55e3ce3ce9cd989c7e06e07feeafd",
                    )
                    .unwrap(),
                    U256::from_str(
                        "0x016b83f4f980694ed2eee4d10667242b1f40dc406901b34125b008d334d47469",
                    )
                    .unwrap(),
                    true,
                ),
            ),
        ];

        let txs_hashes: Vec<B256> = txs.iter().map(|tx| *tx.hash()).collect();

        let (mut peer_1, mut to_mock_session_rx) = new_mock_session(peer_id_1, eth_version);
        // mark hashes as seen by peer so it can fish them out from the cache for hashes pending
        // fetch
        peer_1.seen_transactions.insert(txs_hashes[0]);
        peer_1.seen_transactions.insert(txs_hashes[1]);
        tx_manager.peers.insert(peer_id_1, peer_1);

        buffer_hash_to_tx_fetcher(tx_fetcher, txs_hashes[0], peer_id_1, 0, None);
        buffer_hash_to_tx_fetcher(tx_fetcher, txs_hashes[1], peer_id_1, 0, None);

        // peer_1 is idle
        assert!(tx_fetcher.is_idle(&peer_id_1));
        assert_eq!(tx_fetcher.active_peers.len(), 0);

        // sends requests for buffered hashes to peer_1
        tx_fetcher.on_fetch_pending_hashes(&tx_manager.peers, |_| true);

        assert_eq!(tx_fetcher.num_pending_hashes(), 0);
        // as long as request is in flight peer_1 is not idle
        assert!(!tx_fetcher.is_idle(&peer_id_1));
        assert_eq!(tx_fetcher.active_peers.len(), 1);

        // mock session of peer_1 receives request
        let req = to_mock_session_rx
            .recv()
            .await
            .expect("peer_1 session should receive request with buffered hashes");
        let PeerRequest::GetPooledTransactions { response, .. } = req else { unreachable!() };

        let message: Vec<PooledTransactionVariant> = txs
            .into_iter()
            .take(1)
            .map(|tx| {
                PooledTransactionVariant::try_from(tx)
                    .expect("Failed to convert MockTransaction to PooledTransaction")
            })
            .collect();
        // response partial request
        response
            .send(Ok(PooledTransactions(message)))
            .expect("should send peer_1 response to tx manager");
        let Some(FetchEvent::TransactionsFetched { peer_id, .. }) = tx_fetcher.next().await else {
            unreachable!()
        };

        // request has resolved, peer_1 is idle again
        assert!(tx_fetcher.is_idle(&peer_id));
        assert_eq!(tx_fetcher.active_peers.len(), 0);
        // failing peer_1's request buffers requested hashes for retry.
        assert_eq!(tx_fetcher.num_pending_hashes(), 1);
    }

    #[tokio::test]
    async fn test_max_retries_tx_request() {
        reth_tracing::init_test_tracing();

        let mut tx_manager = new_tx_manager().await.0;
        let tx_fetcher = &mut tx_manager.transaction_fetcher;

        let peer_id_1 = PeerId::new([1; 64]);
        let peer_id_2 = PeerId::new([2; 64]);
        let eth_version = EthVersion::Eth66;
        let seen_hashes = [B256::from_slice(&[1; 32]), B256::from_slice(&[2; 32])];

        let (mut peer_1, mut to_mock_session_rx) = new_mock_session(peer_id_1, eth_version);
        // mark hashes as seen by peer so it can fish them out from the cache for hashes pending
        // fetch
        peer_1.seen_transactions.insert(seen_hashes[0]);
        peer_1.seen_transactions.insert(seen_hashes[1]);
        tx_manager.peers.insert(peer_id_1, peer_1);

        // hashes are seen and currently not inflight, with one fallback peer, and are buffered
        // for first retry in reverse order to make index 0 lru
        let retries = 1;
        buffer_hash_to_tx_fetcher(tx_fetcher, seen_hashes[1], peer_id_1, retries, None);
        buffer_hash_to_tx_fetcher(tx_fetcher, seen_hashes[0], peer_id_1, retries, None);

        // peer_1 is idle
        assert!(tx_fetcher.is_idle(&peer_id_1));
        assert_eq!(tx_fetcher.active_peers.len(), 0);

        // sends request for buffered hashes to peer_1
        tx_fetcher.on_fetch_pending_hashes(&tx_manager.peers, |_| true);

        let tx_fetcher = &mut tx_manager.transaction_fetcher;

        assert_eq!(tx_fetcher.num_pending_hashes(), 0);
        // as long as request is in inflight peer_1 is not idle
        assert!(!tx_fetcher.is_idle(&peer_id_1));
        assert_eq!(tx_fetcher.active_peers.len(), 1);

        // mock session of peer_1 receives request
        let req = to_mock_session_rx
            .recv()
            .await
            .expect("peer_1 session should receive request with buffered hashes");
        let PeerRequest::GetPooledTransactions { request, response } = req else { unreachable!() };
        let GetPooledTransactions(hashes) = request;

        let hashes = hashes.into_iter().collect::<B256Set>();

        assert_eq!(hashes, seen_hashes.into_iter().collect::<B256Set>());

        // fail request to peer_1
        response
            .send(Err(RequestError::BadResponse))
            .expect("should send peer_1 response to tx manager");
        let Some(FetchEvent::FetchError { peer_id, .. }) = tx_fetcher.next().await else {
            unreachable!()
        };

        // request has resolved, peer_1 is idle again
        assert!(tx_fetcher.is_idle(&peer_id));
        assert_eq!(tx_fetcher.active_peers.len(), 0);
        // failing peer_1's request buffers requested hashes for retry
        assert_eq!(tx_fetcher.num_pending_hashes(), 2);

        let (peer_2, mut to_mock_session_rx) = new_mock_session(peer_id_2, eth_version);
        tx_manager.peers.insert(peer_id_2, peer_2);

        // peer_2 announces same hashes as peer_1
        let msg =
            NewPooledTransactionHashes::Eth66(NewPooledTransactionHashes66(seen_hashes.to_vec()));
        tx_manager.on_new_pooled_transaction_hashes(peer_id_2, msg);

        let tx_fetcher = &mut tx_manager.transaction_fetcher;

        // peer_2 should be in active_peers.
        assert_eq!(tx_fetcher.active_peers.len(), 1);

        // since hashes are already seen, no changes to length of unknown hashes
        assert_eq!(tx_fetcher.num_all_hashes(), 2);
        // but hashes are taken out of buffer and packed into request to peer_2
        assert_eq!(tx_fetcher.num_pending_hashes(), 0);

        // mock session of peer_2 receives request
        let req = to_mock_session_rx
            .recv()
            .await
            .expect("peer_2 session should receive request with buffered hashes");
        let PeerRequest::GetPooledTransactions { response, .. } = req else { unreachable!() };

        // report failed request to tx manager
        response
            .send(Err(RequestError::BadResponse))
            .expect("should send peer_2 response to tx manager");
        let Some(FetchEvent::FetchError { .. }) = tx_fetcher.next().await else { unreachable!() };

        // `MAX_REQUEST_RETRIES_PER_TX_HASH`, 2, for hashes reached so this time won't be buffered
        // for retry
        assert_eq!(tx_fetcher.num_pending_hashes(), 0);
        assert_eq!(tx_fetcher.active_peers.len(), 0);
    }

    #[test]
    fn test_direct_propagation_transaction_uses_2718_size() {
        let mut tx_gen = TransactionGenerator::new(rand::rng());
        let tx = tx_gen.gen_eip1559();
        let expected_size = tx.encode_2718_len();

        let tx = PropagateTransaction::new(tx);

        assert_eq!(tx.propagation_size(), expected_size);
    }

    #[test]
    fn test_transaction_builder_empty() {
        let mut builder = PropagateTransactionsBuilder::pooled(EthVersion::Eth68, 0);
        assert!(builder.is_empty());

        let mut tx_gen = TransactionGenerator::new(rand::rng());
        let tx =
            PropagateTransaction::pool_tx(valid_eth_pool_transaction(tx_gen.gen_eip1559_pooled()));
        builder.push(&tx);
        assert!(!builder.is_empty());

        let txs = builder.build();
        assert!(txs.full.is_none());
        let txs = txs.pooled.unwrap();
        assert_eq!(txs.len(), 1);
    }

    #[test]
    fn test_pooled_propagation_transaction_encoder_length_matches_network_encoding() {
        let mut tx_gen = TransactionGenerator::new(rand::rng());
        let tx = valid_eth_pool_transaction(tx_gen.gen_eip1559_pooled());
        let pooled = PropagatePooledTransactionEncoder::new(tx);

        let mut pooled_encoded = Vec::new();
        pooled.encode(&mut pooled_encoded);
        assert_eq!(pooled.length(), pooled_encoded.len());

        let broadcast = BroadcastPoolTransactions(vec![LazyEncoded::new(pooled)]);
        let mut first_encoded = Vec::new();
        broadcast.encode(&mut first_encoded);
        let mut second_encoded = Vec::new();
        broadcast.encode(&mut second_encoded);
        assert_eq!(first_encoded, second_encoded);

        let mut encoded = first_encoded.as_slice();
        let decoded = Transactions::<TransactionSigned>::decode(&mut encoded).unwrap();
        assert_eq!(decoded.len(), 1);
        assert!(encoded.is_empty());
    }

    #[test]
    fn test_transaction_builder_large() {
        let mut builder = PropagateTransactionsBuilder::full(EthVersion::Eth68, 0);
        assert!(builder.is_empty());

        let mut tx_gen = TransactionGenerator::new(rand::rng());
        let mut tx = tx_gen.gen_eip1559_pooled();
        // create a transaction that still fits
        tx.encoded_length = DEFAULT_SOFT_LIMIT_BYTE_SIZE_TRANSACTIONS_BROADCAST_MESSAGE + 1;
        let tx = PropagateTransaction::pool_tx(valid_eth_pool_transaction(tx));
        builder.push(&tx);
        assert!(!builder.is_empty());

        let txs = builder.clone().build();
        assert!(txs.pooled.is_none());
        let txs = txs.full.unwrap();
        assert_eq!(txs.len(), 1);

        builder.push(&tx);

        let txs = builder.clone().build();
        let pooled = txs.pooled.unwrap();
        assert_eq!(pooled.len(), 1);
        let txs = txs.full.unwrap();
        assert_eq!(txs.len(), 1);
    }

    #[test]
    fn test_transaction_builder_eip4844() {
        let mut builder = PropagateTransactionsBuilder::full(EthVersion::Eth68, 0);
        assert!(builder.is_empty());

        let mut tx_gen = TransactionGenerator::new(rand::rng());
        let tx =
            PropagateTransaction::pool_tx(valid_eth_pool_transaction(tx_gen.gen_eip4844_pooled()));
        builder.push(&tx);
        assert!(!builder.is_empty());

        let txs = builder.clone().build();
        assert!(txs.full.is_none());
        let txs = txs.pooled.unwrap();
        assert_eq!(txs.len(), 1);

        let tx =
            PropagateTransaction::pool_tx(valid_eth_pool_transaction(tx_gen.gen_eip1559_pooled()));
        builder.push(&tx);

        let txs = builder.clone().build();
        let pooled = txs.pooled.unwrap();
        assert_eq!(pooled.len(), 1);
        let txs = txs.full.unwrap();
        assert_eq!(txs.len(), 1);
    }

    #[tokio::test]
    async fn test_large_tx_broadcast_threshold() {
        reth_tracing::init_test_tracing();

        let (mut tx_manager, network) = new_tx_manager().await;

        network.handle().update_sync_state(SyncState::Idle);

        // Register two peers so we can test Basic on one and Forced on the other
        let peer_id_1 = PeerId::random();
        let (tx1, _rx1) = mpsc::channel::<PeerRequest>(1);
        let session_info_1 = SessionInfo {
            peer_id: peer_id_1,
            remote_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            client_version: Arc::from(""),
            capabilities: Arc::new(vec![].into()),
            status: Arc::new(Default::default()),
            version: EthVersion::Eth68,
            peer_kind: PeerKind::Basic,
        };
        let messages_1: PeerRequestSender<PeerRequest> = PeerRequestSender::new(peer_id_1, tx1);
        tx_manager.on_network_event(NetworkEvent::ActivePeerSession {
            info: session_info_1,
            messages: messages_1,
        });

        let mut factory = MockTransactionFactory::default();

        // A small transaction (within TX_MAX_BROADCAST_SIZE) should be sent in full via Basic mode
        let small_tx = Arc::new(factory.create_eip1559());
        let small_propagate = vec![PropagateTransaction::pool_tx(small_tx.clone())];
        let propagated = tx_manager.propagate_transactions(small_propagate, PropagationMode::Basic);
        let prop_txs = propagated.0.get(small_tx.transaction.hash()).unwrap();
        assert_eq!(prop_txs.len(), 1);
        assert!(prop_txs[0].is_full(), "small tx should be broadcast in full");

        // A large transaction (exceeding TX_MAX_BROADCAST_SIZE) should be hash-only in Basic mode
        let mut large_valid_tx = factory.create_eip1559();
        large_valid_tx.transaction.set_size(TX_MAX_BROADCAST_SIZE + 1);
        let large_tx = Arc::new(large_valid_tx);
        let large_propagate = vec![PropagateTransaction::pool_tx(large_tx.clone())];
        let propagated = tx_manager.propagate_transactions(large_propagate, PropagationMode::Basic);
        let prop_txs = propagated.0.get(large_tx.transaction.hash()).unwrap();
        assert_eq!(prop_txs.len(), 1);
        assert!(prop_txs[0].is_hash(), "large tx should be hash-only in Basic mode");

        // Register a second peer to test Forced mode with a fresh seen set
        let peer_id_2 = PeerId::random();
        let (tx2, _rx2) = mpsc::channel::<PeerRequest>(1);
        let session_info_2 = SessionInfo {
            peer_id: peer_id_2,
            remote_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
            client_version: Arc::from(""),
            capabilities: Arc::new(vec![].into()),
            status: Arc::new(Default::default()),
            version: EthVersion::Eth68,
            peer_kind: PeerKind::Basic,
        };
        let messages_2: PeerRequestSender<PeerRequest> = PeerRequestSender::new(peer_id_2, tx2);
        tx_manager.on_network_event(NetworkEvent::ActivePeerSession {
            info: session_info_2,
            messages: messages_2,
        });

        // The same large transaction should be sent in full via Forced mode (e.g.
        // broadcast_transactions before pool insertion)
        let mut large_valid_tx_2 = factory.create_eip1559();
        large_valid_tx_2.transaction.set_size(TX_MAX_BROADCAST_SIZE + 1);
        let large_tx_2 = Arc::new(large_valid_tx_2);
        let large_propagate_2 = vec![PropagateTransaction::pool_tx(large_tx_2.clone())];
        let propagated =
            tx_manager.propagate_transactions(large_propagate_2, PropagationMode::Forced);
        let prop_txs = propagated.0.get(large_tx_2.transaction.hash()).unwrap();
        // Forced mode should deliver to both peers in full
        assert!(
            prop_txs.iter().all(|p| p.is_full()),
            "large tx should be broadcast in full in Forced mode"
        );
    }

    #[tokio::test]
    async fn test_propagate_full() {
        reth_tracing::init_test_tracing();

        let (mut tx_manager, network) = new_eth_tx_manager().await;
        let peer_id = PeerId::random();

        // ensure not syncing
        network.handle().update_sync_state(SyncState::Idle);

        // mock a peer
        let (tx, _rx) = mpsc::channel::<PeerRequest>(1);

        let session_info = SessionInfo {
            peer_id,
            remote_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            client_version: Arc::from(""),
            capabilities: Arc::new(vec![].into()),
            status: Arc::new(Default::default()),
            version: EthVersion::Eth68,
            peer_kind: PeerKind::Basic,
        };
        let messages: PeerRequestSender<PeerRequest> = PeerRequestSender::new(peer_id, tx);
        tx_manager
            .on_network_event(NetworkEvent::ActivePeerSession { info: session_info, messages });
        let mut propagate = vec![];
        let mut tx_gen = TransactionGenerator::new(rand::rng());
        let eip1559_tx = valid_eth_pool_transaction(tx_gen.gen_eip1559_pooled());
        propagate.push(eip1559_tx.clone());
        let eip4844_tx = valid_eth_pool_transaction(tx_gen.gen_eip4844_pooled());
        propagate.push(eip4844_tx.clone());

        let propagated = tx_manager.propagate_transactions(
            propagate.clone().into_iter().map(PropagateTransaction::pool_tx).collect(),
            PropagationMode::Basic,
        );
        assert_eq!(propagated.len(), 2);
        let prop_txs = propagated.get(eip1559_tx.transaction.hash()).unwrap();
        assert_eq!(prop_txs.len(), 1);
        assert!(prop_txs[0].is_full());

        let prop_txs = propagated.get(eip4844_tx.transaction.hash()).unwrap();
        assert_eq!(prop_txs.len(), 1);
        assert!(prop_txs[0].is_hash());

        let peer = tx_manager.peers.get(&peer_id).unwrap();
        assert!(peer.seen_transactions.contains(eip1559_tx.transaction.hash()));
        assert!(peer.seen_transactions.contains(eip1559_tx.transaction.hash()));
        peer.seen_transactions.contains(eip4844_tx.transaction.hash());

        // propagate again
        let propagated = tx_manager.propagate_transactions(
            propagate.into_iter().map(PropagateTransaction::pool_tx).collect(),
            PropagationMode::Basic,
        );
        assert!(propagated.is_empty());
    }

    #[tokio::test]
    async fn test_truncated_hash_announcement_not_marked_seen() {
        reth_tracing::init_test_tracing();

        let (mut tx_manager, network) = new_eth_tx_manager().await;
        // all peers receive hash announcements only
        tx_manager.config.propagation_mode = TransactionPropagationMode::Max(0);

        // ensure not syncing
        network.handle().update_sync_state(SyncState::Idle);

        let peer_id = PeerId::random();
        let (peer, _rx) = new_mock_session(peer_id, EthVersion::Eth68);
        tx_manager.peers.insert(peer_id, peer);

        // one more transaction than fits into a single hashes broadcast message
        let mut tx_gen = TransactionGenerator::new(rand::rng());
        let txs = (0..=SOFT_LIMIT_COUNT_HASHES_IN_NEW_POOLED_TRANSACTIONS_BROADCAST_MESSAGE)
            .map(|nonce| {
                valid_eth_pool_transaction(gen_eip1559_pooled_with_nonce(&mut tx_gen, nonce as u64))
            })
            .collect::<Vec<_>>();
        let last_sent = *txs[txs.len() - 2].hash();
        let truncated = *txs[txs.len() - 1].hash();

        let propagated = tx_manager.propagate_transactions(
            txs.into_iter().map(PropagateTransaction::pool_tx).collect(),
            PropagationMode::Basic,
        );

        // the truncated hash was not sent, so it must not be tracked as seen by the peer
        assert!(propagated.get(&truncated).is_none());
        let peer = tx_manager.peers.get(&peer_id).unwrap();
        assert!(!peer.seen_transactions.contains(&truncated));
        assert!(peer.seen_transactions.contains(&last_sent));
    }

    #[tokio::test]
    async fn test_propagate_pending_txs_while_initially_syncing() {
        reth_tracing::init_test_tracing();

        let (mut tx_manager, network) = new_eth_tx_manager().await;
        let peer_id = PeerId::random();

        // Keep the node in initial sync mode.
        network.handle().update_sync_state(SyncState::Syncing);
        assert!(NetworkInfo::is_initially_syncing(&network.handle()));

        // Add a peer so propagation has a destination.
        let (peer, _rx) = new_mock_session(peer_id, EthVersion::Eth68);
        tx_manager.peers.insert(peer_id, peer);

        let mut tx_gen = TransactionGenerator::new(rand::rng());
        let tx = gen_eip1559_pooled_with_nonce(&mut tx_gen, 0);
        let tx_hash = *tx.hash();
        tx_manager
            .pool
            .add_transaction(reth_transaction_pool::TransactionOrigin::External, tx.clone())
            .await
            .expect("transaction should be accepted into the pool");

        tx_manager.on_new_pending_transactions(vec![tx_hash]);

        let peer = tx_manager.peers.get(&peer_id).expect("peer should exist");
        assert!(peer.seen_transactions.contains(&tx_hash));
    }

    #[test]
    fn test_transaction_hashes_to_reannounce_filters_local_age_and_propagation() {
        let mut factory = MockTransactionFactory::default();
        let now = Instant::now();

        let mut old_local =
            factory.validated_with_origin(TransactionOrigin::Local, MockTransaction::eip1559());
        old_local.propagate = true;
        old_local.timestamp = now - Duration::from_secs(60);
        let old_local_hash = *old_local.hash();

        let mut fresh_local =
            factory.validated_with_origin(TransactionOrigin::Local, MockTransaction::eip1559());
        fresh_local.propagate = true;
        fresh_local.timestamp = now - Duration::from_secs(59);

        let mut old_external =
            factory.validated_with_origin(TransactionOrigin::External, MockTransaction::eip1559());
        old_external.propagate = true;
        old_external.timestamp = now - Duration::from_secs(60);

        let mut old_local_no_propagation =
            factory.validated_with_origin(TransactionOrigin::Local, MockTransaction::eip1559());
        old_local_no_propagation.propagate = false;
        old_local_no_propagation.timestamp = now - Duration::from_secs(60);

        let hashes = transaction_hashes_to_reannounce(
            vec![
                Arc::new(old_local),
                Arc::new(fresh_local),
                Arc::new(old_external),
                Arc::new(old_local_no_propagation),
            ],
            now,
            Duration::from_secs(60),
        );

        assert_eq!(hashes, vec![old_local_hash]);
    }

    #[test]
    fn test_transaction_hashes_to_reannounce_respects_max_per_interval() {
        let mut factory = MockTransactionFactory::default();
        let now = Instant::now();
        let pending = (0..(DEFAULT_MAX_COUNT_REANNOUNCED_LOCAL_TRANSACTIONS + 1))
            .map(|_| {
                let mut tx = factory
                    .validated_with_origin(TransactionOrigin::Local, MockTransaction::eip1559());
                tx.propagate = true;
                tx.timestamp = now - Duration::from_secs(60);
                Arc::new(tx)
            })
            .collect::<Vec<_>>();

        let hashes = transaction_hashes_to_reannounce(pending, now, Duration::from_secs(60));

        assert_eq!(hashes.len(), DEFAULT_MAX_COUNT_REANNOUNCED_LOCAL_TRANSACTIONS);
    }

    #[tokio::test]
    async fn test_reannounce_transaction_hashes_force_hashes_to_sqrt_peers() {
        reth_tracing::init_test_tracing();

        let (mut tx_manager, network) = new_tx_manager().await;
        network.handle().update_sync_state(SyncState::Idle);

        let mut factory = MockTransactionFactory::default();
        let tx = factory.create_eip1559();
        let hash = *tx.hash();

        tx_manager
            .pool
            .add_transaction(TransactionOrigin::Local, tx.transaction.clone())
            .await
            .unwrap();

        for _ in 0..4 {
            let peer_id = PeerId::random();
            let (mut peer, _rx) = new_mock_session(peer_id, EthVersion::Eth68);
            peer.seen_transactions.insert(hash);
            tx_manager.peers.insert(peer_id, peer);
        }

        let propagated = tx_manager.reannounce_transaction_hashes(vec![hash]);
        let kinds = propagated.0.get(&hash).unwrap();

        assert_eq!(kinds.len(), 2);
        assert!(kinds.iter().all(PropagateKind::is_hash));
    }

    #[tokio::test]
    async fn test_relaxed_filter_ignores_unknown_tx_types() {
        reth_tracing::init_test_tracing();

        let transactions_manager_config = TransactionsManagerConfig::default();

        let propagation_policy = TransactionPropagationKind::default();
        let announcement_policy = RelaxedEthAnnouncementFilter::default();

        let policy_bundle = NetworkPolicies::new(propagation_policy, announcement_policy);

        let pool = testing_pool();
        let secret_key = SecretKey::new(&mut rand_08::thread_rng());
        let client = NoopProvider::default();

        let network_config = NetworkConfigBuilder::new(secret_key, Runtime::test())
            .listener_port(0)
            .disable_discovery()
            .build(client.clone());

        let mut network_manager = NetworkManager::new(network_config).await.unwrap();
        let (to_tx_manager_tx, from_network_rx) =
            reth_metrics::common::mpsc::memory_bounded_channel::<
                NetworkTransactionEvent<EthNetworkPrimitives>,
            >(
                crate::transactions::constants::tx_manager::DEFAULT_TX_MANAGER_CHANNEL_MEMORY_LIMIT_BYTES,
                "test_tx_channel",
            );
        network_manager.set_transactions(to_tx_manager_tx);
        let network_handle = network_manager.handle().clone();
        let network_service_handle = tokio::spawn(network_manager);

        let mut tx_manager = TransactionsManager::<TestPool, EthNetworkPrimitives>::with_policy(
            network_handle.clone(),
            pool.clone(),
            from_network_rx,
            transactions_manager_config,
            policy_bundle,
        );

        let peer_id = PeerId::random();
        let eth_version = EthVersion::Eth68;
        let (mock_peer_metadata, mut mock_session_rx) = new_mock_session(peer_id, eth_version);
        tx_manager.peers.insert(peer_id, mock_peer_metadata);

        let mut tx_factory = MockTransactionFactory::default();

        let valid_known_tx = tx_factory.create_eip1559();
        let known_tx_signed: Arc<ValidPoolTransaction<MockTransaction>> = Arc::new(valid_known_tx);

        let known_tx_hash = *known_tx_signed.hash();
        let known_tx_type_byte = known_tx_signed.transaction.tx_type();
        let known_tx_size = known_tx_signed.encoded_length();

        let unknown_tx_hash = B256::random();
        let unknown_tx_type_byte = 0xff_u8;
        let unknown_tx_size = 150;

        let announcement_msg = NewPooledTransactionHashes::Eth68(NewPooledTransactionHashes68 {
            types: vec![known_tx_type_byte, unknown_tx_type_byte],
            sizes: vec![known_tx_size, unknown_tx_size],
            hashes: vec![known_tx_hash, unknown_tx_hash],
        });

        tx_manager.on_new_pooled_transaction_hashes(peer_id, announcement_msg);

        poll_fn(|cx| {
            let _ = tx_manager.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;

        let mut requested_hashes_in_getpooled = B256Set::default();
        let mut unexpected_request_received = false;

        match tokio::time::timeout(std::time::Duration::from_millis(200), mock_session_rx.recv())
            .await
        {
            Ok(Some(PeerRequest::GetPooledTransactions { request, response: tx_response_ch })) => {
                let GetPooledTransactions(hashes) = request;
                for hash in hashes {
                    requested_hashes_in_getpooled.insert(hash);
                }
                let _ = tx_response_ch.send(Ok(PooledTransactions(vec![])));
            }
            Ok(Some(other_request)) => {
                tracing::error!(?other_request, "Received unexpected PeerRequest type");
                unexpected_request_received = true;
            }
            Ok(None) => tracing::info!("Mock session channel closed or no request received."),
            Err(_timeout_err) => {
                tracing::info!("Timeout: No GetPooledTransactions request received.")
            }
        }

        assert!(
            requested_hashes_in_getpooled.contains(&known_tx_hash),
            "Should have requested the known EIP-1559 transaction. Requested: {requested_hashes_in_getpooled:?}"
        );
        assert!(
            !requested_hashes_in_getpooled.contains(&unknown_tx_hash),
            "Should NOT have requested the unknown transaction type. Requested: {requested_hashes_in_getpooled:?}"
        );
        assert!(
            !unexpected_request_received,
            "An unexpected P2P request was received by the mock peer."
        );

        network_service_handle.abort();
    }
}
