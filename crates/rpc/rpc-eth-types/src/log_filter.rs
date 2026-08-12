//! A log filter that remembers how many topic positions the caller actually specified.
//!
//! [`Filter`] stores topics as a fixed `[Topic; 4]` where an empty entry means "wildcard", so the
//! number of positions present in the request is lost during deserialization: `topics: []` and
//! `topics: [[], [], []]` both become four empty sets.
//!
//! That distinction is load-bearing. Per `eth_getLogs`, a filter naming N topic positions only
//! matches logs carrying at least N topics — an empty array makes a position match anything, it
//! does not make the position optional. go-ethereum enforces this with an explicit length check
//! before comparing positions (`filterLogs` in `eth/filters/filter.go`):
//!
//! ```text
//! if len(topics) > len(log.Topics) {
//!     continue Logs
//! }
//! ```
//!
//! Without the count, `Filter::matches` cannot reproduce that rule: its `matches_topics` accepts a
//! filter position that has no counterpart in the log whenever that position is a wildcard, so a
//! two-topic log satisfies a three-position filter. Restoring the count here is what makes the
//! check possible.

use alloy_primitives::B256;
use alloy_rpc_types_eth::Filter;
use serde::{Deserialize, Deserializer};
use std::ops::Deref;

/// Maximum number of topic positions a log can carry, and therefore the most a filter can
/// meaningfully name. Mirrors the width of [`Filter`]'s topics array.
const MAX_TOPICS: usize = 4;

/// A [`Filter`] paired with the number of topic positions named in the request.
///
/// Derefs to the inner [`Filter`], so this can be used anywhere a filter is expected. Use
/// [`LogFilter::matches_topic_count`] to apply the position-count rule that [`Filter`] alone cannot
/// express — see the [module docs](self) for why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilter {
    filter: Filter,
    /// Number of topic positions present in the request, `0` when `topics` was absent or empty.
    topic_positions: usize,
}

impl LogFilter {
    /// Creates a new [`LogFilter`] from a [`Filter`] and an explicit topic-position count.
    ///
    /// The count is clamped to [`MAX_TOPICS`], matching the limit [`Filter`] itself enforces.
    pub fn new(filter: Filter, topic_positions: usize) -> Self {
        Self { filter, topic_positions: topic_positions.min(MAX_TOPICS) }
    }

    /// Returns the number of topic positions the request named.
    pub const fn topic_positions(&self) -> usize {
        self.topic_positions
    }

    /// Returns a reference to the inner [`Filter`].
    pub const fn filter(&self) -> &Filter {
        &self.filter
    }

    /// Consumes this filter and returns the inner [`Filter`].
    pub fn into_filter(self) -> Filter {
        self.filter
    }

    /// Returns `true` if the log carries at least as many topics as the request named.
    ///
    /// This is the check [`Filter::matches`] cannot perform, and must be applied *in addition* to
    /// it. A filter that named no positions imposes no constraint.
    pub const fn matches_topic_count(&self, topics: &[B256]) -> bool {
        topics.len() >= self.topic_positions
    }
}

impl Deref for LogFilter {
    type Target = Filter;

    fn deref(&self) -> &Self::Target {
        &self.filter
    }
}

impl From<Filter> for LogFilter {
    /// Builds a [`LogFilter`] from a [`Filter`] that was not deserialized from a request.
    ///
    /// The position count is unrecoverable at this point, so this imposes no length constraint —
    /// preserving the previous behaviour for internally constructed filters.
    fn from(filter: Filter) -> Self {
        Self { filter, topic_positions: 0 }
    }
}

impl From<LogFilter> for Filter {
    fn from(value: LogFilter) -> Self {
        value.filter
    }
}

impl<'de> Deserialize<'de> for LogFilter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Deserialize once into a generic value so the topics array can be counted, then hand the
        // same value to `Filter`'s own deserializer rather than reimplementing it — it accepts
        // several shapes (`blockHash` vs `fromBlock`/`toBlock`, single address vs array) that are
        // easy to get subtly wrong.
        let value = serde_json::Value::deserialize(deserializer)?;

        let topic_positions = value
            .get("topics")
            .and_then(|topics| topics.as_array())
            .map(|topics| topics.len())
            .unwrap_or(0);

        let filter = Filter::deserialize(&value).map_err(serde::de::Error::custom)?;

        Ok(Self::new(filter, topic_positions))
    }
}

impl serde::Serialize for LogFilter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // The count is derived from `topics`, which the inner filter already round-trips.
        self.filter.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};

    fn topics(n: usize) -> Vec<B256> {
        (0..n).map(|i| B256::with_last_byte(i as u8)).collect()
    }

    fn parse(json: &str) -> LogFilter {
        serde_json::from_str(json).expect("valid filter")
    }

    #[test]
    fn counts_specified_wildcard_positions() {
        // The case from the report: three wildcard positions must still require three topics.
        let filter = parse(r#"{"topics":[[],[],[]]}"#);
        assert_eq!(filter.topic_positions(), 3);
        assert!(!filter.matches_topic_count(&topics(2)));
        assert!(filter.matches_topic_count(&topics(3)));
        assert!(filter.matches_topic_count(&topics(4)));
    }

    #[test]
    fn absent_and_empty_topics_impose_no_constraint() {
        // These must stay distinguishable from `[[],[],[]]`, otherwise every ordinary query would
        // start demanding topics.
        for json in [r#"{}"#, r#"{"topics":[]}"#, r#"{"fromBlock":"0x1"}"#] {
            let filter = parse(json);
            assert_eq!(filter.topic_positions(), 0, "{json}");
            assert!(filter.matches_topic_count(&topics(0)), "{json}");
        }
    }

    #[test]
    fn null_positions_count_as_specified() {
        // `null` is the other spelling of "wildcard at this position", so it counts too.
        let filter = parse(r#"{"topics":[null,null]}"#);
        assert_eq!(filter.topic_positions(), 2);
        assert!(!filter.matches_topic_count(&topics(1)));
        assert!(filter.matches_topic_count(&topics(2)));
    }

    #[test]
    fn concrete_topics_are_counted_and_still_parsed() {
        let topic = B256::with_last_byte(9);
        let filter = parse(&format!(r#"{{"topics":["{topic}",[]]}}"#));
        assert_eq!(filter.topic_positions(), 2);
        // The inner filter must still carry the concrete topic, i.e. counting did not disturb
        // `Filter`'s own deserialization.
        assert!(filter.filter().topics[0].matches(&topic));
        assert!(filter.filter().topics[1].is_empty());
    }

    #[test]
    fn inner_filter_fields_survive_the_two_step_deserialize() {
        let filter = parse(
            r#"{"address":"0x0000000000000000000000000000000000001002",
                "fromBlock":"0x2b1fda","toBlock":"0x2b203e","topics":[[],[],[]]}"#,
        );
        assert_eq!(filter.topic_positions(), 3);
        assert!(filter
            .filter()
            .address
            .matches(&"0x0000000000000000000000000000000000001002".parse::<Address>().unwrap()));
        assert_eq!(filter.filter().get_from_block(), Some(0x2b1fda));
        assert_eq!(filter.filter().get_to_block(), Some(0x2b203e));
    }

    #[test]
    fn internally_constructed_filters_are_unconstrained() {
        // `From<Filter>` is used where no request was parsed; it must not start rejecting logs.
        let filter = LogFilter::from(Filter::default());
        assert_eq!(filter.topic_positions(), 0);
        assert!(filter.matches_topic_count(&topics(0)));
    }

    #[test]
    fn position_count_is_clamped_to_max_topics() {
        assert_eq!(LogFilter::new(Filter::default(), 9).topic_positions(), MAX_TOPICS);
    }

    #[test]
    fn serialize_drops_trailing_wildcards_so_a_round_trip_only_loosens() {
        let filter = parse(r#"{"topics":[[],[],[]]}"#);
        let json = serde_json::to_string(&filter).unwrap();
        let round_tripped: LogFilter = serde_json::from_str(&json).unwrap();

        assert_eq!(round_tripped.filter(), filter.filter());
        // `Filter::serialize` truncates trailing empty positions, so all-wildcard topics come back
        // as `[]`. The count is therefore lost across a serialize/deserialize hop, which means the
        // constraint can only ever relax, never wrongly reject. Requests are parsed from the
        // caller's JSON directly, so this does not affect the server path; it does mean the
        // generated Rust client cannot express the constraint, which matches today's behaviour.
        assert!(json.contains(r#""topics":[]"#), "{json}");
        assert_eq!(round_tripped.topic_positions(), 0);

        // A concrete topic pins the length, so that part does survive.
        let pinned = parse(
            r#"{"topics":[[],[],["0x0000000000000000000000000000000000000000000000000000000000000009"]]}"#,
        );
        let pinned_json = serde_json::to_string(&pinned).unwrap();
        assert_eq!(serde_json::from_str::<LogFilter>(&pinned_json).unwrap().topic_positions(), 3);
    }
}
