                    payload = built_payloads.select_next_some(), if !built_payloads.is_terminated() => {
                        if let Some(executed_block) = payload.executed_block() {
                            debug!(target: "reth::cli", block=?executed_block.recovered_block.num_hash(),  "inserting built payload");
                            orchestrator.handler_mut().handler_mut().on_event(EngineApiRequest::InsertExecutedBlock(executed_block).into());
                        }
                    }
                    shutdown_req = &mut shutdown_rx => {
                        if let Ok(req) = shutdown_req {
                            debug!(target: "reth::cli", "received engine shutdown request");
                            orchestrator.handler_mut().handler_mut().on_event(
                                FromOrchestrator::Terminate { tx: req.done_tx }.into()
                            );
                        }
                    }
                    _guard = &mut on_graceful_shutdown => {
                        // Shutdown signal received.
                        // Send Terminate so the engine OS thread can exit cleanly before we
                        // drop the orchestrator.
                        debug!(target: "reth::cli", "shutdown signal received, terminating engine");
                        let (done_tx, done_rx) = oneshot::channel();
                        orchestrator.handler_mut().handler_mut().on_event(
                            FromOrchestrator::Terminate { tx: done_tx }.into()
                        );
                        let _ = done_rx.await;
                        break;
                    }
                }
            }

            let _ = exit.send(res);
        };
        ctx.task_executor()
            .spawn_critical_with_graceful_shutdown_signal("consensus engine", consensus_engine);

        let engine_events_for_ethstats = engine_events.new_listener();

        let full_node = FullNode {
            evm_config: ctx.components().evm_config().clone(),
            pool: ctx.components().pool().clone(),
            network: ctx.components().network().clone(),
            provider: ctx.node_adapter().provider.clone(),
            payload_builder_handle: ctx.components().payload_builder_handle().clone(),
            task_executor: ctx.task_executor().clone(),
            config: ctx.node_config().clone(),
            data_dir: ctx.data_dir().clone(),
            add_ons_handle: RpcHandle {
                rpc_server_handles,
                rpc_registry,
                engine_events,
                beacon_engine_handle,
                engine_shutdown,
                engine_api_tx: Some(engine_api_tx),
            },
        };
        // Notify on node started
        on_node_started.on_event(FullNode::clone(&full_node))?;

        ctx.spawn_ethstats(engine_events_for_ethstats).await?;

        let handle = NodeHandle {
            node_exit_future: NodeExitFuture::new(async { rx.await? }),
            node: full_node,
        };

        Ok(handle)
    }
}

impl<N, DB, T, CB, AO> LaunchNode<NodeBuilderWithComponents<T, CB, AO>> for EngineNodeLauncher
where
    T: FullNodeTypes<
        Types = N,
        DB = DB,
        Provider = BlockchainProvider<NodeTypesWithDBAdapter<N, DB>>,
    >,
    N: Node<RethFullAdapter<DB, N>> + NodeTypesForProvider,
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    CB: NodeComponentsBuilder<T> + 'static,
    AO: RethRpcAddOns<NodeAdapter<T, CB::Components>>
        + EngineValidatorAddOn<NodeAdapter<T, CB::Components>>
        + 'static,
{
    type Node = NodeHandle<NodeAdapter<T, CB::Components>, AO>;
    type Future = Pin<Box<dyn Future<Output = eyre::Result<Self::Node>> + Send>>;

    fn launch_node(self, target: NodeBuilderWithComponents<T, CB, AO>) -> Self::Future {
        Box::pin(self.launch_node(target))
    }
}
