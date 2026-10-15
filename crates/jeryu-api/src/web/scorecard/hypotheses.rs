//! Monthly checks use the scheduled check month, with missing hypotheses visible.
use chrono::{Datelike, TimeZone, Utc};
use serde_json::{Value, json};

use super::compute::Metric;
use crate::web::outcomes::project::Change;
use crate::web::shift::ShiftTodo;
use crate::web::shift::hypothesis::{Outcome, deployments};

pub(crate) fn metric(todos: &[ShiftTodo], changes: &[Change], now_ms: i64) -> Metric {
    let now = Utc
        .timestamp_millis_opt(now_ms)
        .single()
        .expect("valid report clock");
    let start = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .expect("valid month")
        .timestamp_millis();
    let deployments = deployments(changes);
    let mut shipped = 0;
    let mut with_hypothesis = 0;
    let mut without_hypothesis = 0;
    let mut unscheduled = 0;
    let mut eligible = 0;
    let mut checked = 0;
    let mut met = 0;
    let mut rows = Vec::new();
    let mut total_rows = 0;
    for todo in todos {
        let Some(at) = deployments
            .get(&(todo.family.as_str(), todo.id.as_str()))
            .copied()
            .filter(|at| *at <= now_ms)
        else {
            continue;
        };
        let has_hypothesis = !todo.hypothesis.hypothesis.trim().is_empty();
        let in_ship_month = at >= start;
        if in_ship_month {
            shipped += 1;
            if has_hypothesis {
                with_hypothesis += 1;
            } else {
                without_hypothesis += 1;
            }
            if has_hypothesis && todo.hypothesis.check_after.is_none() {
                unscheduled += 1;
            }
        }
        let due = todo.hypothesis.due_ms(at);
        let due_this_month = has_hypothesis && due.is_some_and(|due| due >= start && due <= now_ms);
        if due_this_month {
            eligible += 1;
            if todo.hypothesis.outcome != Outcome::NotChecked {
                checked += 1;
            }
            if todo.hypothesis.outcome == Outcome::Met {
                met += 1;
            }
        }
        if in_ship_month || due_this_month {
            total_rows += 1;
        }
        if (in_ship_month || due_this_month) && rows.len() < 500 {
            rows.push(
                json!({"id": todo.id, "family": todo.family, "title": todo.title,
                "hypothesis": todo.hypothesis.hypothesis, "outcome": todo.hypothesis.outcome,
                "outcome_note": todo.hypothesis.outcome_note,
                "shipped_at": crate::web::shift::rfc3339_ms(at),
                "check_due_at": due.map(crate::web::shift::rfc3339_ms),
                "eligible": due_this_month,
                "href": format!("/work/{}?family={}", todo.id, todo.family)}),
            );
        }
    }
    rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let share = |n: usize, d: usize| {
        if d == 0 {
            Value::Null
        } else {
            json!(100.0 * n as f64 / d as f64)
        }
    };
    Metric {
        value: Some(
            json!({"month": now.format("%Y-%m").to_string(), "shipped": shipped,
            "with_hypothesis": with_hypothesis, "without_hypothesis": without_hypothesis,
            "without_check_date": unscheduled, "eligible": eligible, "checked": checked, "met": met,
            "checked_share_pct": share(checked, eligible), "met_share_pct": share(met, checked)}),
        ),
        previous: None,
        daily: Vec::new(),
        sample_size: eligible,
        source: "todo hypothesis records and durable production deployment events",
        detail: Some(json!({"rows": rows, "total_rows": total_rows})),
        unknown: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::outcomes::project::Leg;
    use crate::web::shift::hypothesis::{CheckAfter, Hypothesis};
    use crate::web::shift::todo_file::TodoFile;

    #[test]
    fn monthly_checks_count_missing_hypotheses_and_use_all_repository_deploys() {
        let now = Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap();
        let shipped = Utc
            .with_ymd_and_hms(2026, 10, 1, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        let mut todos = Vec::new();
        let mut changes = Vec::new();
        for (id, outcome, days) in [
            ("met", Outcome::Met, 7),
            ("miss", Outcome::NotMet, 7),
            ("unclear", Outcome::Unclear, 7),
            ("waiting", Outcome::NotChecked, 7),
            ("future", Outcome::Met, 20),
            ("missing", Outcome::NotChecked, 7),
            ("unstaged", Outcome::Met, 7),
            ("old", Outcome::Met, 7),
        ] {
            let mut todo = TodoFile::new(id.into(), "acme".into(), id.into()).to_api(now);
            todo.hypothesis = Hypothesis {
                hypothesis: if id == "missing" {
                    ""
                } else {
                    "Users finish checkout faster."
                }
                .into(),
                check_after: Some(CheckAfter::Days(days)),
                outcome,
                ..Default::default()
            };
            let deployed = if id == "old" {
                shipped - 30 * 86_400_000
            } else {
                shipped
            };
            changes.push(Change {
                todo_id: Some(id.into()),
                family: Some("acme".into()),
                legs: vec![
                    Leg {
                        merged_ms: Some(deployed - 1000),
                        deployed_ms: Some(deployed),
                        ..Default::default()
                    },
                    Leg {
                        merged_ms: Some(deployed - 500),
                        deployed_ms: (id != "unstaged").then_some(deployed + 1000),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            });
            todos.push(todo);
        }
        let result = metric(&todos, &changes, now.timestamp_millis());
        let value = result.value.unwrap();
        assert_eq!(value["shipped"], 6);
        assert_eq!(value["with_hypothesis"], 5);
        assert_eq!(value["without_hypothesis"], 1);
        assert_eq!(value["eligible"], 4);
        assert_eq!(value["checked"], 3);
        assert_eq!(value["met"], 1);
        assert_eq!(value["checked_share_pct"], 75.0);
        assert!((value["met_share_pct"].as_f64().unwrap() - 100.0 / 3.0).abs() < 0.001);
        let empty = metric(&[], &[], now.timestamp_millis()).value.unwrap();
        assert!(empty["met_share_pct"].is_null());
        assert!(empty["checked_share_pct"].is_null());
        // An older shipped idea belongs to the month its absolute check is due.
        todos.last_mut().unwrap().hypothesis.check_after =
            Some(CheckAfter::Date("2026-10-15".into()));
        let value = metric(&todos, &changes, now.timestamp_millis())
            .value
            .unwrap();
        assert_eq!(value["eligible"], 5);
        assert_eq!(value["met"], 2);
    }
    #[test]
    fn monthly_rows_are_bounded_without_truncating_counts() {
        let now = Utc::now();
        let mut todos = Vec::new();
        let mut changes = Vec::new();
        for n in 0..501 {
            let id = format!("idea-{n}");
            todos.push(TodoFile::new(id.clone(), "acme".into(), id.clone()).to_api(now));
            changes.push(Change {
                todo_id: Some(id),
                family: Some("acme".into()),
                legs: vec![Leg {
                    merged_ms: Some(now.timestamp_millis()),
                    deployed_ms: Some(now.timestamp_millis()),
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
        let report = metric(&todos, &changes, now.timestamp_millis());
        assert_eq!(report.value.unwrap()["without_hypothesis"], 501);
        let detail = report.detail.unwrap();
        assert_eq!(detail["total_rows"], 501);
        assert_eq!(detail["rows"].as_array().unwrap().len(), 500);
    }
}
