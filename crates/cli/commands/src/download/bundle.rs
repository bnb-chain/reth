//! Planning and selection rules for complete snapshot bundles.

use super::{
    manifest::{
        resolve_archive_url, SnapshotArchive, SnapshotBundle, SnapshotManifest, SnapshotProfile,
    },
    planning::{PlannedArchive, PlannedDownloads},
    DownloadCommand,
};
use eyre::{ensure, Result};
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_cli::chainspec::ChainSpecParser;
use std::{
    collections::BTreeSet,
    path::{Component, Path},
};

impl<C: ChainSpecParser<ChainSpec: EthChainSpec + EthereumHardforks>> DownloadCommand<C> {
    /// Rejects selections that would imply changing the bundle's historical coverage.
    pub(super) fn validate_bundle_selection(&self, bundle: &SnapshotBundle) -> Result<()> {
        ensure!(
            !(self.minimal || self.with_txs || self.with_txs_since.is_some() ||
                self.with_txs_distance.is_some() || self.with_receipts ||
                self.with_receipts_since.is_some() || self.with_receipts_distance.is_some() ||
                self.with_state_history || self.with_state_history_since.is_some() ||
                self.with_state_history_distance.is_some() || self.with_senders ||
                self.with_rocksdb || self.without_rocksdb || self.prune_unlisted),
            "Complete snapshot bundles do not support component selection, ranges, --minimal, --without-rocksdb, or --prune-unlisted"
        );
        ensure!(
            !self.archive || bundle.profile == SnapshotProfile::Archive,
            "--archive requires an archive bundle; a full snapshot cannot restore pruned history"
        );
        ensure!(
            !self.full || bundle.profile == SnapshotProfile::Full,
            "--full requires a full bundle; bundle downloads preserve the producer's profile"
        );
        Ok(())
    }
}

/// Plans one verified archive without synthesizing component selections or pruning settings.
pub(super) fn plan_bundle(
    manifest: &SnapshotManifest,
    bundle: &SnapshotBundle,
) -> Result<PlannedDownloads> {
    ensure!(manifest.components.is_empty(), "A manifest cannot mix a bundle and components");
    ensure!(manifest.storage_version == 2, "Complete snapshot bundles require storage version 2");
    let single = &bundle.archive;
    ensure!(
        safe_relative_path(&single.file) &&
            !single.file.contains([':', '?', '#', '%']) &&
            (single.file.ends_with(".tar.zst") || single.file.ends_with(".tar.lz4")),
        "Bundle archive must be a relative .tar.zst or .tar.lz4 path"
    );
    let mut outputs = BTreeSet::new();
    let mut output_size = 0_u64;
    for output in &single.output_files {
        ensure!(
            safe_relative_path(&output.path) &&
                (output.path == "reth.toml" ||
                    ["db/", "rocksdb/", "static_files/"]
                        .iter()
                        .any(|root| output.path.starts_with(root))),
            "Invalid bundle output path: {}",
            output.path
        );
        ensure!(outputs.insert(output.path.as_str()), "Duplicate bundle output: {}", output.path);
        ensure!(
            blake3::Hash::from_hex(&output.blake3).is_ok(),
            "Invalid bundle output checksum: {}",
            output.path
        );
        output_size = output_size
            .checked_add(output.size)
            .ok_or_else(|| eyre::eyre!("Bundle output size overflow"))?;
    }
    ensure!(outputs.contains("db/mdbx.dat"), "Bundle is missing db/mdbx.dat checksum metadata");
    for path in &outputs {
        let mut parent = Path::new(path).parent();
        while let Some(path) = parent {
            ensure!(
                !outputs.contains(path.to_str().unwrap_or_default()),
                "Bundle output paths overlap"
            );
            parent = path.parent();
        }
    }
    ensure!(
        single.decompressed_size == 0 || single.decompressed_size == output_size,
        "Bundle decompressed_size does not match output files"
    );
    let archive = SnapshotArchive {
        url: resolve_archive_url(manifest.base_url.as_deref().unwrap_or_default(), &single.file),
        file_name: single.file.clone(),
        size: single.size,
        blake3: single.blake3.clone(),
        output_files: single.output_files.clone(),
    };
    Ok(PlannedDownloads {
        total_download_size: archive.size,
        total_output_size: output_size,
        archives: vec![PlannedArchive { ty: None, component: "Complete snapshot".into(), archive }],
    })
}

/// Requires normalized relative paths before output verification or cleanup can touch disk.
fn safe_relative_path(path: &str) -> bool {
    !path.is_empty() &&
        !path.contains('\\') &&
        path.split('/').all(|part| !matches!(part, "" | "." | "..")) &&
        Path::new(path).components().all(|part| matches!(part, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest() -> SnapshotManifest {
        serde_json::from_value(json!({
            "block": 123, "chain_id": 56, "storage_version": 2, "timestamp": 1,
            "base_url": "https://example.com/snapshots/",
            "bundle": {"profile": "full", "archive": {
                "file": "existing.tar.zst", "size": 50, "decompressed_size": 3,
                "output_files": [{"path": "db/mdbx.dat", "size": 3,
                    "blake3": blake3::hash(b"abc").to_hex().to_string()}]
            }}
        }))
        .unwrap()
    }

    #[test]
    fn plans_each_network_and_profile_as_one_archive() {
        for chain_id in [56, 97] {
            for profile in [SnapshotProfile::Full, SnapshotProfile::Archive] {
                let mut manifest = manifest();
                manifest.chain_id = chain_id;
                manifest.bundle.as_mut().unwrap().profile = profile;
                let plan = plan_bundle(&manifest, manifest.bundle.as_ref().unwrap()).unwrap();
                assert_eq!(plan.total_download_size, 50);
                assert_eq!(plan.total_output_size, 3);
                assert_eq!(plan.archives.len(), 1);
                assert_eq!(plan.archives[0].ty, None);
                assert_eq!(
                    plan.archives[0].archive.url,
                    "https://example.com/snapshots/existing.tar.zst"
                );
                let serialized = serde_json::to_value(&manifest).unwrap();
                assert_eq!(serialized.get("components"), None);
                let roundtrip: SnapshotManifest = serde_json::from_value(serialized).unwrap();
                assert_eq!(roundtrip.bundle.unwrap().profile, profile);
            }
        }
    }

    #[test]
    fn rejects_invalid_bundle_metadata_before_download() {
        let cases = [
            ("/outside", "Invalid bundle output path: /outside"),
            ("../outside", "Invalid bundle output path: ../outside"),
            ("db/../outside", "Invalid bundle output path: db/../outside"),
            ("db//mdbx.dat", "Invalid bundle output path: db//mdbx.dat"),
            ("db/./mdbx.dat", "Invalid bundle output path: db/./mdbx.dat"),
            ("db\\mdbx.dat", "Invalid bundle output path: db\\mdbx.dat"),
            ("jwt.hex", "Invalid bundle output path: jwt.hex"),
        ];
        for (path, error) in cases {
            let mut manifest = manifest();
            manifest.bundle.as_mut().unwrap().archive.output_files[0].path = path.into();
            assert_eq!(
                plan_bundle(&manifest, manifest.bundle.as_ref().unwrap()).unwrap_err().to_string(),
                error
            );
        }
        for file in [
            "../data.tar.zst",
            "/data.tar.zst",
            "https://example.com/data.tar.zst",
            "a?b.tar.zst",
            "a%2fb.tar.zst",
            "data.zip",
        ] {
            let mut manifest = manifest();
            manifest.bundle.as_mut().unwrap().archive.file = file.into();
            assert_eq!(
                plan_bundle(&manifest, manifest.bundle.as_ref().unwrap()).unwrap_err().to_string(),
                "Bundle archive must be a relative .tar.zst or .tar.lz4 path"
            );
        }
    }

    #[test]
    fn rejects_incomplete_ambiguous_or_inconsistent_bundles() {
        let base = manifest();
        let mut cases = Vec::new();
        let mut changed = base.clone();
        changed.components.insert(
            "state".into(),
            super::super::manifest::ComponentManifest::Single(
                changed.bundle.as_ref().unwrap().archive.clone(),
            ),
        );
        cases.push((changed, "A manifest cannot mix a bundle and components"));
        let mut changed = base.clone();
        changed.storage_version = 1;
        cases.push((changed, "Complete snapshot bundles require storage version 2"));
        let mut changed = base.clone();
        changed.bundle.as_mut().unwrap().archive.output_files.clear();
        cases.push((changed, "Bundle is missing db/mdbx.dat checksum metadata"));
        let mut changed = base.clone();
        let files = &mut changed.bundle.as_mut().unwrap().archive.output_files;
        files.push(files[0].clone());
        cases.push((changed, "Duplicate bundle output: db/mdbx.dat"));
        let mut changed = base.clone();
        changed.bundle.as_mut().unwrap().archive.output_files[0].blake3 = "bad".into();
        cases.push((changed, "Invalid bundle output checksum: db/mdbx.dat"));
        let mut changed = base.clone();
        changed.bundle.as_mut().unwrap().archive.decompressed_size = 4;
        cases.push((changed, "Bundle decompressed_size does not match output files"));
        let mut changed = base.clone();
        let files = &mut changed.bundle.as_mut().unwrap().archive.output_files;
        let mut extra = files[0].clone();
        extra.path = "db/mdbx.dat/child".into();
        files.push(extra);
        cases.push((changed, "Bundle output paths overlap"));
        let mut changed = base;
        let files = &mut changed.bundle.as_mut().unwrap().archive.output_files;
        let mut extra = files[0].clone();
        extra.path = "db/overflow".into();
        extra.size = u64::MAX;
        files.push(extra);
        cases.push((changed, "Bundle output size overflow"));
        for (manifest, error) in cases {
            assert_eq!(
                plan_bundle(&manifest, manifest.bundle.as_ref().unwrap()).unwrap_err().to_string(),
                error
            );
        }
    }
}
