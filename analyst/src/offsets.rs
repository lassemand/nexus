use std::collections::{BTreeSet, HashMap};

/// Which Kafka offsets may be committed.
///
/// Each release is processed in its own task, so offsets finish out of order.
/// Committing one that finished while an earlier one is still in flight would
/// lose that earlier release if the process died, so a partition's committable
/// offset is the lowest offset still in flight, or one past the highest seen
/// when none is.
#[derive(Debug, Default)]
pub struct OffsetTracker {
    partitions: HashMap<(String, i32), Partition>,
}

#[derive(Debug, Default)]
struct Partition {
    in_flight: BTreeSet<i64>,
    next_after_seen: i64,
    committed: Option<i64>,
}

impl OffsetTracker {
    /// An empty tracker.
    pub fn new() -> OffsetTracker {
        OffsetTracker::default()
    }

    /// A message has been taken off the topic and is being processed.
    pub fn begin(&mut self, topic: &str, partition: i32, offset: i64) {
        let state = self
            .partitions
            .entry((topic.to_owned(), partition))
            .or_default();
        state.in_flight.insert(offset);
        state.next_after_seen = state.next_after_seen.max(offset + 1);
    }

    /// A message is done. Returns the offset to commit (the next one to read)
    /// when this lets the partition's commit move forward.
    pub fn done(&mut self, topic: &str, partition: i32, offset: i64) -> Option<i64> {
        let state = self.partitions.get_mut(&(topic.to_owned(), partition))?;
        state.in_flight.remove(&offset);
        let candidate = state
            .in_flight
            .iter()
            .next()
            .copied()
            .unwrap_or(state.next_after_seen);
        if state
            .committed
            .is_none_or(|committed| candidate > committed)
        {
            state.committed = Some(candidate);
            Some(candidate)
        } else {
            None
        }
    }

    /// How many messages are in flight on all partitions.
    pub fn in_flight(&self) -> usize {
        self.partitions.values().map(|p| p.in_flight.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_completion_commits_each_offset() {
        let mut tracker = OffsetTracker::new();
        tracker.begin("t", 0, 10);
        assert_eq!(tracker.done("t", 0, 10), Some(11));
        tracker.begin("t", 0, 11);
        assert_eq!(tracker.done("t", 0, 11), Some(12));
        assert_eq!(tracker.in_flight(), 0);
    }

    #[test]
    fn an_offset_finishing_early_waits_for_the_one_before_it() {
        let mut tracker = OffsetTracker::new();
        tracker.begin("t", 0, 10);
        tracker.begin("t", 0, 11);
        tracker.begin("t", 0, 12);
        assert_eq!(
            tracker.done("t", 0, 12),
            Some(10),
            "10 and 11 are still in flight"
        );
        assert_eq!(tracker.done("t", 0, 11), None, "10 is still in flight");
        assert_eq!(
            tracker.done("t", 0, 10),
            Some(13),
            "now everything up to 12 is done"
        );
    }

    #[test]
    fn partitions_and_topics_are_independent() {
        let mut tracker = OffsetTracker::new();
        tracker.begin("t", 0, 5);
        tracker.begin("t", 1, 100);
        tracker.begin("u", 0, 7);
        assert_eq!(tracker.done("t", 1, 100), Some(101));
        assert_eq!(tracker.done("u", 0, 7), Some(8));
        assert_eq!(tracker.in_flight(), 1);
        assert_eq!(tracker.done("t", 0, 5), Some(6));
    }

    #[test]
    fn the_commit_never_moves_back() {
        let mut tracker = OffsetTracker::new();
        tracker.begin("t", 0, 1);
        tracker.begin("t", 0, 2);
        assert_eq!(tracker.done("t", 0, 2), Some(1));
        assert_eq!(tracker.done("t", 0, 1), Some(3));
        assert_eq!(
            tracker.done("t", 0, 1),
            None,
            "finishing twice changes nothing"
        );
        assert_eq!(
            tracker.done("t", 9, 1),
            None,
            "an unknown partition is ignored"
        );
    }
}
