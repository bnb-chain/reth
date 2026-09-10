use crate::{
    segments::{PruneInput, Segment},
    PrunerError,
};
use reth_provider::StaticFileProviderFactory;
use reth_prune_types::{
    PruneMode, PruneProgress, PrunePurpose, PruneSegment, SegmentOutput, SegmentOutputCheckpoint,
};
use reth_stages_types::StageId;
use reth_static_file_types::StaticFileSegment;
use std::time::Instant;
use tracing::info;

/// Prunes historical header static files.
#[derive(Debug)]
pub struct HeaderHistory {
    mode: PruneMode,
}

impl HeaderHistory {
    /// Creates a new header history segment.
    pub const fn new(mode: PruneMode) -> Self {
        Self { mode }
    }
}

impl<Provider> Segment<Provider> for HeaderHistory
where
    Provider: StaticFileProviderFactory,
{
    fn segment(&self) -> PruneSegment {
        PruneSegment::HeaderHistory
    }

    fn mode(&self) -> Option<PruneMode> {
        Some(self.mode)
    }

    fn purpose(&self) -> PrunePurpose {
        PrunePurpose::User
    }

    fn prune(&self, provider: &Provider, input: PruneInput) -> Result<SegmentOutput, PrunerError> {
        let started_at = Instant::now();
        let static_files = provider.static_file_provider();
        let deleted = static_files.delete_segment_below_block(
            StaticFileSegment::Headers,
            input.to_block.saturating_add(1),
        )?;

        let pruned = deleted
            .iter()
            .filter_map(|header| header.block_range())
            .map(|range| range.len() as usize)
            .sum();
        // Recover if files were deleted before their checkpoint was committed.
        let checkpoint = deleted
            .iter()
            .filter_map(|header| header.block_range().map(|range| range.end()))
            .chain(
                static_files
                    .get_lowest_range_start(StaticFileSegment::Headers)
                    .and_then(|block| block.checked_sub(1)),
            )
            .chain(input.previous_checkpoint.and_then(|checkpoint| checkpoint.block_number))
            .max()
            .map(|block_number| SegmentOutputCheckpoint {
                block_number: Some(block_number),
                tx_number: None,
            });

        if !deleted.is_empty() {
            info!(
                target: "pruner",
                deleted_files = deleted.len(),
                pruned_blocks = pruned,
                highest_pruned_block = ?checkpoint.and_then(|checkpoint| checkpoint.block_number),
                elapsed = ?started_at.elapsed(),
                "Pruned header history"
            );
        }

        Ok(SegmentOutput { progress: PruneProgress::Finished, pruned, checkpoint })
    }

    fn required_stage(&self) -> Option<StageId> {
        Some(StageId::Bodies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PruneLimiter;
    use reth_provider::{
        test_utils::create_test_provider_factory, DatabaseProviderFactory, HeaderProvider,
        ProviderError, StaticFileProviderFactory, StaticFileWriter,
    };
    use reth_static_file_types::{
        SegmentHeader, SegmentRangeInclusive, DEFAULT_BLOCKS_PER_STATIC_FILE,
    };

    fn setup_header_jars(provider: &impl StaticFileProviderFactory, jars: u64) {
        let static_files = provider.static_file_provider();
        let mut writer = static_files.latest_writer(StaticFileSegment::Headers).unwrap();

        for jar in 0..jars {
            let start = jar * DEFAULT_BLOCKS_PER_STATIC_FILE;
            let end = start + DEFAULT_BLOCKS_PER_STATIC_FILE - 1;
            *writer.user_header_mut() = SegmentHeader::new(
                SegmentRangeInclusive::new(start, end),
                Some(SegmentRangeInclusive::new(start, end)),
                None,
                StaticFileSegment::Headers,
            );
            writer.inner().set_dirty();
            writer.commit().unwrap();

            if jar + 1 < jars {
                writer.increment_block(end + 1).unwrap();
            }
        }

        static_files.initialize_index().unwrap();
    }

    #[test]
    fn prunes_complete_header_jars() {
        let factory = create_test_provider_factory();
        setup_header_jars(&factory, 3);

        let segment = HeaderHistory::new(PruneMode::Distance(10_064));
        let output = segment
            .prune(
                &factory.database_provider_rw().unwrap(),
                PruneInput {
                    previous_checkpoint: None,
                    to_block: 899_999,
                    limiter: PruneLimiter::default(),
                },
            )
            .unwrap();

        assert_eq!(output.pruned, DEFAULT_BLOCKS_PER_STATIC_FILE as usize);
        assert_eq!(output.checkpoint.unwrap().block_number, Some(499_999));
        assert_eq!(
            factory.static_file_provider().get_lowest_range_start(StaticFileSegment::Headers),
            Some(500_000)
        );
        assert_eq!(factory.static_file_provider().earliest_history_height(), 500_000);
        assert!(matches!(
            factory.static_file_provider().headers_range(0..1),
            Err(ProviderError::BlockExpired { requested: 0, earliest_available: 500_000 })
        ));
    }

    #[test]
    fn restores_checkpoint_from_retained_floor() {
        let factory = create_test_provider_factory();
        setup_header_jars(&factory, 3);
        let provider = factory.database_provider_rw().unwrap();
        let segment = HeaderHistory::new(PruneMode::Distance(10_064));
        let input = PruneInput {
            previous_checkpoint: None,
            to_block: 899_999,
            limiter: PruneLimiter::default(),
        };

        segment.prune(&provider, input.clone()).unwrap();
        let output = segment.prune(&provider, input).unwrap();

        assert_eq!(output.pruned, 0);
        assert_eq!(output.checkpoint.unwrap().block_number, Some(499_999));
    }
}
