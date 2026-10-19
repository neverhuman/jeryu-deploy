//! The `families` summary counts what `/needs-you` draws: one row per
//! subject, at its worst severity, in its family.

use super::summary::{FamilyCounts, family_summary, subject_key};
use super::{Counts, Draft, Item, Severity};

fn item(id: &str, severity: Severity, family: Option<&str>) -> Item {
    let mut item = Draft {
        id: id.to_string(),
        kind: "todo_blocked",
        severity,
        title: id.to_string(),
        reason: String::new(),
        href: "/needs-you".to_string(),
        label: "Open",
        api: None,
        command: None,
    }
    .build();
    item.family = family.map(str::to_string);
    item.family_label = item.family.clone();
    item
}

fn counts(critical: usize, action: usize, watch: usize) -> Counts {
    Counts {
        critical,
        action,
        watch,
    }
}

#[test]
fn a_subject_is_the_pull_request_else_the_todo_else_the_shift_else_the_item() {
    let mut pr = item("pr", Severity::Action, Some("acme"));
    pr.repo = Some("acme/web".to_string());
    pr.pr = Some(7);
    pr.todo_id = Some("t-1".to_string());
    assert_eq!(subject_key(&pr), "pr:acme/web#7");

    let mut todo = item("todo", Severity::Action, Some("acme"));
    todo.repo = Some("acme/web".to_string());
    todo.todo_id = Some("t-1".to_string());
    todo.shift = Some("nightshift/2026-09-19".to_string());
    assert_eq!(subject_key(&todo), "todo:acme:t-1");

    let mut shift = item("shift", Severity::Action, Some("acme"));
    shift.shift = Some("nightshift/2026-09-19".to_string());
    assert_eq!(subject_key(&shift), "shift:acme:nightshift/2026-09-19");

    // A blank join key is no key, as on the web.
    let mut blank = item("mirror:acme/web", Severity::Watch, None);
    blank.todo_id = Some("  ".to_string());
    assert_eq!(subject_key(&blank), "item:mirror:acme/web");
}

#[test]
fn every_family_is_counted_by_subject_at_its_worst_severity() {
    let mut checks = item("checks", Severity::Action, Some("acme"));
    checks.repo = Some("acme/web".to_string());
    checks.pr = Some(7);
    // A second item about the same pull request is the same row, and the
    // worse of the two severities is the row's.
    let mut queue = checks.clone();
    queue.id = "queue".to_string();
    queue.severity = Severity::Critical;
    let mut blocked = item("blocked", Severity::Action, Some("acme"));
    blocked.todo_id = Some("t-1".to_string());
    let mut stuck = blocked.clone();
    stuck.id = "stuck".to_string();
    stuck.severity = Severity::Watch;
    let mut other = item("other", Severity::Watch, Some("globex"));
    other.todo_id = Some("t-2".to_string());
    let runner = item("runner", Severity::Critical, None);

    let summary = family_summary(&[runner, other, stuck, blocked, queue, checks]);

    assert_eq!(
        summary,
        vec![
            FamilyCounts {
                family: Some("acme".to_string()),
                family_label: Some("acme".to_string()),
                subjects: counts(1, 1, 0),
            },
            FamilyCounts {
                family: Some("globex".to_string()),
                family_label: Some("globex".to_string()),
                subjects: counts(0, 0, 1),
            },
            // The forge's own rows come last, under no family.
            FamilyCounts {
                family: None,
                family_label: None,
                subjects: counts(1, 0, 0),
            },
        ]
    );
}

#[test]
fn an_empty_inbox_has_no_families() {
    assert!(family_summary(&[]).is_empty());
}
