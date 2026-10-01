//! Dimension-floor results: findings the auditor emits for a scoring
//! dimension, not for a rule.
//!
//! For each scoring dimension below the floor, the auditor adds one soft
//! finding titled "`<dimension>` scored <n> below the standard floor of <m>"
//! and files it under a rule id from a fixed table (build speed under the
//! concurrency rule, code shape under HLT-001, and so on). The
//! rule did not detect anything there, so counting those findings as rule
//! failures inflates every rule the table names. They are separated here and
//! reported per dimension instead.

use super::{FindingDetail, ScoredHead};

/// One dimension the auditor scored below the floor on one head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DimensionResult {
    pub(super) dimension: String,
    pub(super) score: u32,
    pub(super) floor: u32,
}

/// Parses "`<dimension>` scored <n> below the standard floor of <m>".
fn parse_title(problem: &str) -> Option<DimensionResult> {
    let rest = problem.trim().strip_prefix('`')?;
    let (dimension, rest) = rest.split_once('`')?;
    let rest = rest.strip_prefix(" scored ")?;
    let (score, floor) = rest.split_once(" below the standard floor of ")?;
    let dimension = dimension.trim();
    if dimension.is_empty() {
        return None;
    }
    Some(DimensionResult {
        dimension: dimension.to_string(),
        score: score.trim().parse().ok()?,
        floor: floor.trim().trim_end_matches('.').parse().ok()?,
    })
}

/// The dimension result this finding is, if it is one.
///
/// The stored finding carries no field of its own that marks it (its check
/// id is `<rule>:<category>` like any other), so the title decides, checked
/// against the report: such a finding is never hard, and when the report
/// lists its `dimensions` the named one must be among them.
pub(super) fn dimension_result(
    head: &ScoredHead,
    finding: &FindingDetail,
) -> Option<DimensionResult> {
    if finding.hardness.as_deref() == Some("hard") {
        return None;
    }
    let result = parse_title(finding.problem.as_deref()?)?;
    let listed = head
        .report
        .as_ref()
        .and_then(|report| report.get("dimensions"))
        .and_then(serde_json::Value::as_array);
    match listed {
        Some(dimensions) => dimensions
            .iter()
            .any(|entry| {
                entry.get("name").and_then(serde_json::Value::as_str)
                    == Some(result.dimension.as_str())
            })
            .then_some(result),
        None => Some(result),
    }
}

/// The middle score, or the mean of the two middle ones; 0 when empty.
pub(super) fn median(scores: &mut [u32]) -> f64 {
    scores.sort_unstable();
    let len = scores.len();
    if len == 0 {
        return 0.0;
    }
    let mid = len / 2;
    if len % 2 == 1 {
        f64::from(scores[mid])
    } else {
        (f64::from(scores[mid - 1]) + f64::from(scores[mid])) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::{DimensionResult, median, parse_title};

    #[test]
    fn parses_the_auditor_title() {
        assert_eq!(
            parse_title("`Build speed signals` scored 70 below the standard floor of 85"),
            Some(DimensionResult {
                dimension: "Build speed signals".to_string(),
                score: 70,
                floor: 85,
            })
        );
        assert_eq!(
            parse_title("`Code shape` scored 0 below the standard floor of 85."),
            Some(DimensionResult {
                dimension: "Code shape".to_string(),
                score: 0,
                floor: 85,
            })
        );
    }

    #[test]
    fn other_titles_are_not_dimension_results() {
        for title in [
            "a marker left in product code",
            "`AGENTS.md` is missing",
            "`` scored 70 below the standard floor of 85",
            "`Build speed` scored high below the standard floor of 85",
            "`Build speed` scored 70 below the floor of 85",
        ] {
            assert_eq!(parse_title(title), None, "{title}");
        }
    }

    #[test]
    fn median_of_odd_even_and_empty() {
        assert_eq!(median(&mut [70, 10, 40]), 40.0);
        assert_eq!(median(&mut [80, 50]), 65.0);
        assert_eq!(median(&mut []), 0.0);
    }
}
