use crate::{HeaderProvider, ParliaSnapshotReader, ParliaSnapshotWriter};

/// Helper trait to combine all the traits we need for the parlia
///
/// This is a temporary solution
pub trait ParliaProvider: HeaderProvider + ParliaSnapshotWriter + ParliaSnapshotReader {}

impl<T> ParliaProvider for T where T: HeaderProvider + ParliaSnapshotWriter + ParliaSnapshotReader {}
