//! How stranded-branch facts become rows once the backlog groups old ones.

use chrono::{DateTime, Duration, Utc};

use super::*;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-20T13:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn old_branch(branch: &str) -> StrandedBranchFacts {
    StrandedBranchFacts {
        repo: "acme/widget-shop".to_string(),
        branch: branch.to_string(),
        default_branch: "main".to_string(),
        commit_count: 1,
        newest_sha: "f2bed08".to_string(),
        newest_subject: "finish invoice export".to_string(),
        newest_at: Some(now() - Duration::days(30)),
        closed_pr: Some(115),
        scanned_at: None,
    }
}

fn pushed(branch: &str) -> PushedAfterMergeFacts {
    PushedAfterMergeFacts {
        repo: "acme/widget-shop".to_string(),
        branch: branch.to_string(),
        number: 115,
        title: "Widget tax".to_string(),
        base_ref: "main".to_string(),
        merged: true,
        commits: vec!["f2bed08".to_string()],
        commit_count: 1,
        closed_at: Some(now() - Duration::days(4)),
    }
}

/// A branch a recently merged pull request left commits on keeps its own
/// row, however old the commits are: the backlog row only counts the rest.
#[test]
fn an_old_branch_left_behind_by_a_merge_keeps_its_pushed_after_merge_row() {
    let facts = vec![
        old_branch("old/a"),
        old_branch("old/b"),
        old_branch("old/c"),
        old_branch("dev/widget-tax"),
    ];
    let items = stranded_rows(facts, &[pushed("dev/widget-tax")], now());
    let kinds: Vec<_> = items.iter().map(|item| item.kind).collect();
    assert_eq!(
        kinds,
        ["pushed_after_merge", "stranded_branch"],
        "{items:?}"
    );
    assert!(
        items[0].title.contains("dev/widget-tax after #115 merged"),
        "{}",
        items[0].title
    );
    assert!(
        items[1].title.contains("3 old branches"),
        "{}",
        items[1].title
    );
    assert!(
        !items[1].reason.contains("dev/widget-tax"),
        "{}",
        items[1].reason
    );
}
