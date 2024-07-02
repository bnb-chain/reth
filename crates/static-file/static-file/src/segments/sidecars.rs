use crate::segments::{dataset_for_compression, prepare_jar, Segment};
use alloy_primitives::BlockNumber;
use reth_db::{static_file::create_static_file_T1_T2, tables, RawKey, RawTable};
use reth_db_api::{cursor::DbCursorRO, database::Database, transaction::DbTx};
use reth_provider::{
    providers::{StaticFileProvider, StaticFileWriter},
    DatabaseProviderRO,
};
use reth_static_file_types::{SegmentConfig, SegmentHeader, StaticFileSegment};
use reth_storage_errors::provider::ProviderResult;
use std::{ops::RangeInclusive, path::Path};

/// Static File segment responsible for [`StaticFileSegment::Sidecars`] part of data.
#[derive(Debug, Default)]
pub struct Sidecars;

impl<DB: Database> Segment<DB> for Sidecars {
    fn segment(&self) -> StaticFileSegment {
        StaticFileSegment::Sidecars
    }

    fn copy_to_static_files(
        &self,
        provider: DatabaseProviderRO<DB>,
        static_file_provider: StaticFileProvider,
        block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<()> {
        let mut static_file_writer =
            static_file_provider.get_writer(*block_range.start(), StaticFileSegment::Sidecars)?;

        let mut sidecars_cursor = provider.tx_ref().cursor_read::<tables::Sidecars>()?;
        let sidecars_walker = sidecars_cursor.walk_range(block_range.clone())?;

        let mut canonical_headers_cursor =
            provider.tx_ref().cursor_read::<tables::CanonicalHeaders>()?;
        let canonical_headers_walker = canonical_headers_cursor.walk_range(block_range)?;

        for (sidecar_entry, canonical_header_entry) in sidecars_walker.zip(canonical_headers_walker)
        {
            let (header_block, sidecar) = sidecar_entry?;
            let (canonical_header_block, canonical_header) = canonical_header_entry?;

            debug_assert_eq!(header_block, canonical_header_block);

            let _static_file_block =
                static_file_writer.append_sidecars(sidecar, header_block, canonical_header)?;
            debug_assert_eq!(_static_file_block, header_block);
        }

        Ok(())
    }

    fn create_static_file_file(
        &self,
        provider: &DatabaseProviderRO<DB>,
        directory: &Path,
        config: SegmentConfig,
        block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<()> {
        let range_len = block_range.clone().count();

        let jar = prepare_jar::<DB, 2>(
            provider,
            directory,
            StaticFileSegment::Sidecars,
            config,
            block_range.clone(),
            range_len,
            || {
                Ok([
                    dataset_for_compression::<DB, tables::Sidecars>(
                        provider,
                        &block_range,
                        range_len,
                    )?,
                    dataset_for_compression::<DB, tables::CanonicalHeaders>(
                        provider,
                        &block_range,
                        range_len,
                    )?,
                ])
            },
        )?;

        // Generate list of hashes for filters & PHF
        let mut cursor = provider.tx_ref().cursor_read::<RawTable<tables::CanonicalHeaders>>()?;
        let hashes = if config.filters.has_filters() {
            Some(
                cursor
                    .walk(Some(RawKey::from(*block_range.start())))?
                    .take(range_len)
                    .map(|row| row.map(|(_key, value)| value.into_value()).map_err(|e| e.into())),
            )
        } else {
            None
        };

        create_static_file_T1_T2::<
            tables::Sidecars,
            tables::CanonicalHeaders,
            BlockNumber,
            SegmentHeader,
        >(
            provider.tx_ref(),
            block_range,
            None,
            // We already prepared the dictionary beforehand
            None::<Vec<std::vec::IntoIter<Vec<u8>>>>,
            hashes,
            range_len,
            jar,
        )?;

        Ok(())
    }
}
