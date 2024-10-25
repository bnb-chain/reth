/// Trie stats.
#[derive(Clone, Copy, Debug)]
pub struct TriePrefetchStats {
    branches_prefetched: u64,
    leaves_prefetched: u64,
}

impl TriePrefetchStats {
    /// The number of added branch nodes for which we prefetched.
    pub const fn branches_prefetched(&self) -> u64 {
        self.branches_prefetched
    }

    /// The number of added leaf nodes for which we prefetched.
    pub const fn leaves_prefetched(&self) -> u64 {
        self.leaves_prefetched
    }
}

/// Trie metrics tracker.
#[derive(Default, Debug, Clone, Copy)]
pub struct TriePrefetchTracker {
    branches_prefetched: u64,
    leaves_prefetched: u64,
}

impl TriePrefetchTracker {
    /// Increment the number of branches prefetched.
    pub fn inc_branches(&mut self, num: u64) {
        self.branches_prefetched += num;
    }

    /// Increment the number of leaves prefetched.
    pub fn inc_leaves(&mut self, num: u64) {
        self.leaves_prefetched += num;
    }

    /// Called when prefetch is finished to return trie prefetch statistics.
    pub const fn finish(self) -> TriePrefetchStats {
        TriePrefetchStats {
            branches_prefetched: self.branches_prefetched,
            leaves_prefetched: self.leaves_prefetched,
        }
    }
}
