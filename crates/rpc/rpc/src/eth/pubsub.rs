
impl<Eth> EthPubSubInner<Eth>
where
    Eth: EthApiTypes<RpcConvert: RpcConvert<Primitives = Eth::Primitives>> + RpcNodeCore,
{
    /// Returns a stream that yields all new RPC blocks.
    fn new_headers_stream(&self) -> impl Stream<Item = RpcHeader<Eth::NetworkTypes>> {
        let converter = self.eth_api.converter();
        self.eth_api.provider().canonical_state_stream().flat_map(|new_chain| {
            let headers = new_chain
                .committed()
                .blocks_iter()
                .filter_map(|block| {
                    match converter.convert_header(
                        block.clone_sealed_header(),
                        block.rlp_length(),
                        None,
                    ) {
                        Ok(header) => Some(header),
                        Err(err) => {
                            error!(target = "rpc", %err, "Failed to convert header");
                            None
                        }
                    }
                })
                .collect::<Vec<_>>();
            futures::stream::iter(headers)
        })
    }

    /// Returns a stream that yields all logs that match the given filter.
    fn log_stream(&self, filter: Filter) -> impl Stream<Item = Log> {
        self.eth_api
            .provider()
            .canonical_state_stream()
            .map(move |canon_state| canon_state.block_receipts())
            .flat_map(futures::stream::iter)
            .flat_map(move |(block_receipts, removed)| {
                let all_logs = logs_utils::matching_block_logs_with_tx_hashes(
                    &filter,
                    block_receipts.block,
                    block_receipts.timestamp,
                    block_receipts.tx_receipts.iter().map(|(tx, receipt)| (*tx, receipt)),
                    removed,
                );
                futures::stream::iter(all_logs)
            })
    }
}
