//! How long a gate or review usually takes, from the passes the event log
//! already holds.
//!
//! Every finished pass leaves a `gate.finished` (or `review.finished`) event
//! with its `seconds`, `outcome` and `detail.recipe`. The estimate for a pass
//! that is running now is read from the newest [`WINDOW`] passes of the same
//! recipe on the same repository that ran to the end: the median is the
//! `typicalSeconds` a reader is told to expect, and the 90th percentile is the
//! `slowSeconds` past which the pass is slower than nearly every recent one.
//! With fewer than [`MIN_SAMPLES`] passes there is no estimate at all, so the
//! page shows elapsed time instead of a guess.

use serde::{Deserialize, Serialize};

use crate::web::WebState;

/// How many recent passes an estimate reads.
pub(crate) const WINDOW: usize = 20;
/// Fewer passes than this give no estimate.
pub(crate) const MIN_SAMPLES: usize = 3;

/// What a running pass is expected to take.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DurationEstimate {
    /// The median of the recent passes, in seconds.
    pub typical_seconds: u64,
    /// The 90th percentile of the recent passes, in seconds.
    pub slow_seconds: u64,
    /// How many passes the estimate was read from.
    pub samples: u32,
}

/// The estimate the given durations support, or `None` below [`MIN_SAMPLES`].
pub(crate) fn from_samples(mut seconds: Vec<u64>) -> Option<DurationEstimate> {
    if seconds.len() < MIN_SAMPLES {
        return None;
    }
    seconds.sort_unstable();
    Some(DurationEstimate {
        typical_seconds: nearest_rank(&seconds, 50),
        slow_seconds: nearest_rank(&seconds, 90),
        samples: u32::try_from(seconds.len()).unwrap_or(u32::MAX),
    })
}

/// The nearest-rank percentile of a sorted, non-empty slice.
fn nearest_rank(sorted: &[u64], percent: usize) -> u64 {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// The outcomes that mean a pass of this kind ran to the end.
fn complete_outcomes(noun: &str) -> &'static [&'static str] {
    if noun == "review" {
        &["approve", "hold"]
    } else {
        &["success"]
    }
}

/// The estimate for a `noun` pass (`gate` or `review`) of `recipe` on `repo`
/// (`owner/name`). A store that cannot be read gives no estimate rather than
/// failing the page that asked.
pub(crate) fn for_pass(
    state: &WebState,
    noun: &str,
    repo: &str,
    recipe: &str,
) -> Option<DurationEstimate> {
    let seconds = state
        .events
        .finished_seconds(
            &format!("{noun}.finished"),
            repo,
            recipe,
            complete_outcomes(noun),
            WINDOW,
        )
        .map_err(|reason| eprintln!("jeryu-api estimate: {repo} {recipe}: {reason}"))
        .ok()?;
    from_samples(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fewer_than_three_passes_give_no_estimate() {
        assert_eq!(from_samples(vec![]), None);
        assert_eq!(from_samples(vec![60, 70]), None);
    }

    #[test]
    fn median_and_ninetieth_percentile_by_nearest_rank() {
        let estimate = from_samples(vec![300, 100, 200]).expect("three samples");
        assert_eq!(estimate.typical_seconds, 200);
        assert_eq!(estimate.slow_seconds, 300);
        assert_eq!(estimate.samples, 3);

        let ten: Vec<u64> = (1..=10).map(|n| n * 60).collect();
        let estimate = from_samples(ten).expect("ten samples");
        assert_eq!(estimate.typical_seconds, 300);
        assert_eq!(estimate.slow_seconds, 540);
    }

    #[test]
    fn one_slow_outlier_moves_only_the_slow_figure() {
        let mut seconds = vec![600; 19];
        seconds.push(7_200);
        let estimate = from_samples(seconds).expect("twenty samples");
        assert_eq!(estimate.typical_seconds, 600);
        assert_eq!(estimate.slow_seconds, 600);
    }
}
