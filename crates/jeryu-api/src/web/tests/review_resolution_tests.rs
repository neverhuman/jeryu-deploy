//! The merge-time resolution of a pull request's findings
//! (`crate::web::review_catch::resolve_at_merge`): every finding the log
//! holds is resolved, not just the newest page of them, and each resolution
//! names itself so a merge emitted twice records one. The repository, logins
//! and finding ids below are invented.

use super::*;
use crate::web::pipeline::{EventsQuery, NewEvent, record};
use crate::web::review_catch::{self, FINDING_KIND, RESOLVED_KIND};

const REPO: &str = "acme/widgets";
/// More findings than the event store's default page of 100 rows: a single
/// unpaged read would leave the older ones open forever.
const FINDINGS: usize = 150;

fn resolutions(state: &WebState) -> Vec<crate::web::pipeline::Event> {
    state
        .events
        .query(&EventsQuery {
            kind: Some(RESOLVED_KIND.to_string()),
            limit: Some(500),
            ..Default::default()
        })
        .unwrap()
}

#[test]
fn every_finding_is_resolved_at_merge_not_only_the_newest_page() {
    let core = ForgeCore::new();
    core.create_repository(
        "acme",
        CreateRepositoryRequest {
            name: "widgets".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let pr = core
        .create_pull_request(
            "acme",
            "widgets",
            "dana",
            CreatePullRequestRequest {
                title: "the feature".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some("a".repeat(40)),
                base_sha: Some("b".repeat(40)),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    let state = WebState::new(core);

    // The agent reviewer posted its findings on the head that merges, so the
    // verdict needs no commit range: each finding is `not_acted`.
    for number in 0..FINDINGS {
        record(
            &state,
            "pr-redteam",
            NewEvent {
                event_id: Some(format!("pr-redteam:finding:{number}")),
                source: "pr-redteam".to_string(),
                repo: Some(REPO.to_string()),
                pr: Some(pr.number as i64),
                sha: Some(pr.head.sha.clone()),
                outcome: Some("medium".to_string()),
                detail: Some(json!({"reviewer_kind": "agent", "severity": "medium"})),
                ..NewEvent::forge(FINDING_KIND, format!("finding {number} on acme/widgets#1"))
            },
        )
        .unwrap();
    }

    review_catch::resolve_at_merge(&state, &pr);
    let resolved = resolutions(&state);
    assert_eq!(
        resolved.len(),
        FINDINGS,
        "a finding past the first page stayed open"
    );
    assert!(
        resolved.iter().all(|event| event
            .event_id
            .as_deref()
            .is_some_and(|id| id.starts_with("resolved-"))),
        "every resolution names itself"
    );
    assert_eq!(
        resolved
            .iter()
            .filter_map(|event| event.event_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        FINDINGS,
        "one id per finding"
    );

    // The merge emitted again (a replay, a restart) records no second
    // resolution for the same finding.
    review_catch::resolve_at_merge(&state, &pr);
    assert_eq!(resolutions(&state).len(), FINDINGS);

    // And the catch report sees every finding resolved, none left open.
    let events = review_catch::read_events(&state, None).unwrap();
    let report = review_catch::report(&events, None);
    assert_eq!(report.agent.findings, FINDINGS as u64);
    assert_eq!(report.agent.not_acted, FINDINGS as u64);
    assert_eq!(report.agent.open, 0, "no finding is left open at merge");
}
