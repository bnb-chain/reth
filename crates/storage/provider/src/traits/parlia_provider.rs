use crate::{HeaderProvider, ParliaSnapshotReader};

/// Helper trait to combine all the traits we need for the parlia
///
/// This is a temporary solution
pub trait ParliaProvider: HeaderProvider + ParliaSnapshotReader {}

impl<T> ParliaProvider for T where T: HeaderProvider + ParliaSnapshotReader {}
