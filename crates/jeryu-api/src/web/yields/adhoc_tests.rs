//! Yield's `adhoc` block: spend of agent sessions run by hand, on invented
//! ledger rows, then through the event and yield routes.

use std::path::Path;

use axum::http::{Method as HttpMethod, StatusCode};
use chrono::{NaiveDate, TimeZone, Utc};
use jeryu_core::{ForgeCore, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::adhoc::{Adhoc, summarise};
use super::report::{By, Request, Window, build};
use super::tests::{close, scenario};
use super::window_ms;
use crate::web::pipeline::SpendRow;
use crate::web::pipeline::tests::{body_json, request};
use crate::web::{WebState, app};

fn day(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
}

fn window() -> Window {
    Window {
        since: day("2026-10-01"),
        until: day("2026-10-14"),
    }
}

/// Milliseconds of an RFC 3339 instant.
fn at(instant: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(instant)
        .unwrap()
        .timestamp_millis()
}

fn spend(
    instant: &str,
    family: Option<&str>,
    repo: Option<&str>,
    usd: f64,
    session: Option<&str>,
    agent: &str,
    model: &str,
) -> SpendRow {
    let mut detail = json!({"agent": agent, "model": model});
    if let Some(session) = session {
        detail["session"] = json!(session);
    }
    SpendRow {
        ts_ms: at(instant),
        family: family.map(str::to_string),
        repo: repo.map(str::to_string),
        cost_usd: usd,
        detail: Some(detail),
    }
}

fn near(got: f64, want: f64) {
    assert!((got - want).abs() < 1e-6, "{got} != {want}");
}

#[test]
fn the_window_covers_whole_utc_days_at_both_ends() {
    let rows = [
        spend(
            "2026-09-30T23:59:59.999Z",
            Some("acme"),
            None,
            100.0,
            Some("s-0"),
            "codex",
            "model-x",
        ),
        spend(
            "2026-10-01T00:00:00Z",
            Some("acme"),
            None,
            1.0,
            Some("s-1"),
            "codex",
            "model-x",
        ),
        spend(
            "2026-10-14T23:59:59.999Z",
            Some("acme"),
            None,
            2.0,
            Some("s-2"),
            "codex",
            "model-x",
        ),
        spend(
            "2026-10-15T00:00:00Z",
            Some("acme"),
            None,
            100.0,
            Some("s-3"),
            "codex",
            "model-x",
        ),
    ];
    let adhoc = summarise(window(), None, &rows);
    near(adhoc.total_usd, 3.0);
    assert_eq!(adhoc.sessions, 2);
    // The route reads the ledger over the same span.
    assert_eq!(
        window_ms(window()),
        (at("2026-10-01T00:00:00Z"), at("2026-10-15T00:00:00Z"))
    );
}

#[test]
fn a_family_filter_keeps_only_that_family_in_either_spelling() {
    let rows = [
        spend(
            "2026-10-02T10:00:00Z",
            Some("acme"),
            Some("acme/widgets"),
            1.0,
            Some("s-1"),
            "claude-code",
            "model-x",
        ),
        spend(
            "2026-10-02T11:00:00Z",
            Some("acme-split"),
            None,
            0.5,
            Some("s-1"),
            "claude-code",
            "model-x",
        ),
        spend(
            "2026-10-02T12:00:00Z",
            Some("globex"),
            Some("globex/gadgets"),
            4.0,
            Some("s-2"),
            "codex",
            "model-y",
        ),
        spend(
            "2026-10-02T13:00:00Z",
            None,
            None,
            8.0,
            Some("s-3"),
            "codex",
            "model-y",
        ),
    ];
    for asked in ["acme", "ACME-split"] {
        let adhoc = summarise(window(), Some(asked), &rows);
        near(adhoc.total_usd, 1.5);
        assert_eq!(adhoc.sessions, 1, "{asked}");
        near(adhoc.unattributed_usd, 0.0);
        assert_eq!(adhoc.by_family.len(), 1);
        assert_eq!(adhoc.by_family[0].group, "acme", "canonical key back");
        assert_eq!(adhoc.by_repo.len(), 1);
        assert_eq!(adhoc.by_repo[0].group, "acme/widgets");
    }
    let everyone = summarise(window(), None, &rows);
    near(everyone.total_usd, 13.5);
    near(everyone.unattributed_usd, 8.0);
}

#[test]
fn spend_with_no_repo_and_no_family_is_unattributed_and_in_no_place_group() {
    let rows = [
        spend(
            "2026-10-03T10:00:00Z",
            None,
            None,
            0.75,
            Some("s-1"),
            "codex",
            "model-y",
        ),
        // A repo the forge put in no family: attributed, but to no family.
        spend(
            "2026-10-03T11:00:00Z",
            None,
            Some("initech/tools"),
            0.25,
            Some("s-1"),
            "codex",
            "model-y",
        ),
    ];
    let adhoc = summarise(window(), None, &rows);
    near(adhoc.total_usd, 1.0);
    near(adhoc.unattributed_usd, 0.75);
    assert!(adhoc.by_family.is_empty(), "no null family group");
    assert_eq!(adhoc.by_repo.len(), 1);
    assert_eq!(adhoc.by_repo[0].group, "initech/tools");
    assert_eq!(adhoc.by_repo[0].family, None);
    // Agent and model still count every row.
    near(adhoc.by_agent[0].usd, 1.0);
    near(adhoc.by_model[0].usd, 1.0);
}

#[test]
fn sessions_are_distinct_overall_and_within_each_group() {
    let rows = [
        spend(
            "2026-10-04T10:00:00Z",
            Some("acme"),
            Some("acme/widgets"),
            1.0,
            Some("claude:s-1"),
            "claude-code",
            "model-x",
        ),
        spend(
            "2026-10-04T11:00:00Z",
            Some("acme"),
            Some("acme/widgets"),
            1.0,
            Some("claude:s-1"),
            "claude-code",
            "model-x",
        ),
        spend(
            "2026-10-04T12:00:00Z",
            Some("acme"),
            Some("acme/gears"),
            1.0,
            Some("claude:s-1"),
            "claude-code",
            "model-z",
        ),
        spend(
            "2026-10-04T13:00:00Z",
            Some("acme"),
            Some("acme/widgets"),
            1.0,
            Some("codex:s-2"),
            "codex",
            "model-x",
        ),
        // A row that names no session spends but counts no session.
        spend(
            "2026-10-04T14:00:00Z",
            Some("acme"),
            Some("acme/widgets"),
            1.0,
            None,
            "codex",
            "model-x",
        ),
    ];
    let adhoc = summarise(window(), None, &rows);
    assert_eq!(adhoc.sessions, 2);
    assert_eq!(adhoc.by_family[0].sessions, 2);
    let widgets = &adhoc.by_repo[0];
    assert_eq!(widgets.group, "acme/widgets");
    near(widgets.usd, 4.0);
    assert_eq!(widgets.sessions, 2);
    assert_eq!(widgets.family.as_deref(), Some("acme"));
    assert_eq!(adhoc.by_repo[1].sessions, 1);
    let model_x = adhoc
        .by_model
        .iter()
        .find(|group| group.group == "model-x")
        .unwrap();
    assert_eq!(model_x.sessions, 2);
    near(model_x.usd, 4.0);
}

#[test]
fn groups_are_ranked_by_spend_then_name_and_missing_detail_is_unknown() {
    let mut bare = spend(
        "2026-10-05T10:00:00Z",
        Some("initech"),
        None,
        2.0,
        None,
        "x",
        "x",
    );
    bare.detail = None;
    let rows = [
        spend(
            "2026-10-05T10:00:00Z",
            Some("globex"),
            None,
            2.0,
            Some("s-1"),
            "codex",
            "model-y",
        ),
        spend(
            "2026-10-05T10:00:00Z",
            Some("acme"),
            None,
            2.0,
            Some("s-2"),
            "claude-code",
            "model-x",
        ),
        spend(
            "2026-10-05T10:00:00Z",
            Some("acme"),
            None,
            3.0,
            Some("s-3"),
            "claude-code",
            "model-x",
        ),
        bare,
    ];
    let adhoc = summarise(window(), None, &rows);
    let names = |groups: &[super::adhoc::AdhocGroup]| -> Vec<String> {
        groups.iter().map(|group| group.group.clone()).collect()
    };
    assert_eq!(names(&adhoc.by_family), ["acme", "globex", "initech"]);
    assert_eq!(
        names(&adhoc.by_agent),
        ["claude-code", "(unknown)", "codex"]
    );
    assert_eq!(names(&adhoc.by_model), ["model-x", "(unknown)", "model-y"]);
}

#[test]
fn weekly_keys_are_the_report_s_mondays() {
    let rows = [
        // Sunday: the week before.
        spend(
            "2026-10-04T23:30:00Z",
            None,
            None,
            1.0,
            Some("s-1"),
            "codex",
            "model-y",
        ),
        spend(
            "2026-10-05T00:00:00Z",
            None,
            None,
            2.0,
            Some("s-1"),
            "codex",
            "model-y",
        ),
        spend(
            "2026-10-11T23:59:59Z",
            None,
            None,
            0.5,
            Some("s-1"),
            "codex",
            "model-y",
        ),
    ];
    let adhoc = summarise(window(), None, &rows);
    let weeks: Vec<(&str, f64)> = adhoc
        .weekly
        .iter()
        .map(|week| (week.week.as_str(), week.usd))
        .collect();
    assert_eq!(weeks, [("2026-09-28", 1.0), ("2026-10-05", 2.5)]);
}

#[test]
fn adhoc_spend_never_changes_the_todo_economics() {
    let (todos, reviews) = scenario();
    let request = Request {
        window: window(),
        family: None,
        by: By::Model,
        now: Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap(),
    };
    let rows = [spend(
        "2026-10-06T10:00:00Z",
        Some("acme"),
        Some("acme/widget"),
        50.0,
        Some("s-1"),
        "claude-code",
        "model-a",
    )];
    let mut without = serde_json::to_value(build(&request, &todos, &reviews, &[])).unwrap();
    let mut with = serde_json::to_value(build(&request, &todos, &reviews, &rows)).unwrap();
    close(&with["adhoc"]["total_usd"], 50.0);
    assert_eq!(
        without["adhoc"],
        serde_json::to_value(Adhoc::default()).unwrap(),
        "always present, zeros and empty lists"
    );
    without.as_object_mut().unwrap().remove("adhoc");
    with.as_object_mut().unwrap().remove("adhoc");
    assert_eq!(with, without);
}

fn costed(id: &str, actor: &str, repo: Option<&str>, family: Option<&str>, usd: f64) -> Value {
    let mut event = json!({
        "event_id": format!("sc:{id}"),
        "source": "todoq-agents",
        "kind": "session.costed",
        "actor": actor,
        "cost_usd": usd,
        "summary": format!("session {id}: +${usd}"),
        "detail": {"session": format!("claude:{id}"), "agent": "claude-code",
                   "model": "model-x", "seq": 0, "cumulative_usd": usd,
                   "tokens": {"model-x": {"input": 10, "output": 2, "cost_usd": usd}}},
    });
    if let Some(repo) = repo {
        event["repo"] = json!(repo);
    }
    if let Some(family) = family {
        event["family"] = json!(family);
    }
    event
}

#[tokio::test]
async fn posted_session_spend_reaches_the_yield_adhoc_block() {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    // A JERYU_EVENT_REPORTERS login without admin (see `TEST_REPORTERS`).
    core.create_account("ci-bot", "ci-bot-password", UserRole::User)
        .unwrap();
    for (owner, name) in [("acme", "widgets"), ("globex", "gadgets")] {
        core.create_repository(
            owner,
            jeryu_core::CreateRepositoryRequest {
                name: name.to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }
    core.set_repository_family("acme", "widgets", Some("acme".to_string()))
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, reporter) = (token("alice"), token("ci-bot"));
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let call = |method, uri: &str, token: &str, body| {
        router.clone().oneshot(request(method, uri, token, body))
    };

    let batch = json!({"events": [
        costed("s-1", "ada@node-a", Some("acme/widgets"), None, 0.5),
        costed("s-1b", "ada@node-a", None, Some("globex"), 0.75),
        costed("s-2", "ada@node-a", None, None, 0.25),
    ]});
    let posted = call(HttpMethod::POST, "/api/v1/events", &admin, Some(batch))
        .await
        .unwrap();
    assert_eq!(posted.status(), StatusCode::CREATED);

    // A reporter charges its own operator, never another one.
    let own = call(
        HttpMethod::POST,
        "/api/v1/events",
        &reporter,
        Some(costed(
            "s-3",
            "ci-bot@node-b",
            Some("globex/gadgets"),
            None,
            1.0,
        )),
    )
    .await
    .unwrap();
    assert_eq!(own.status(), StatusCode::CREATED);
    let other = call(
        HttpMethod::POST,
        "/api/v1/events",
        &reporter,
        Some(costed("s-4", "grace@node-b", None, None, 9.0)),
    )
    .await
    .unwrap();
    assert_eq!(other.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(other).await["code"], "events_reporter_required");
    // A re-post of the same delta is dropped.
    let again = call(
        HttpMethod::POST,
        "/api/v1/events",
        &admin,
        Some(costed("s-1", "ada@node-a", Some("acme/widgets"), None, 0.5)),
    )
    .await
    .unwrap();
    assert_eq!(again.status(), StatusCode::OK);

    // globex/gadgets joins its family after the spend was posted: the report
    // reads the family from the repository.
    core.set_repository_family("globex", "gadgets", Some("globex".to_string()))
        .unwrap();

    let body = body_json(
        call(HttpMethod::GET, "/api/v1/yield", &admin, None)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["totals"]["todos"], 0);
    close(&body["totals"]["spend_usd"], 0.0);
    let adhoc = &body["adhoc"];
    close(&adhoc["total_usd"], 2.5);
    assert_eq!(adhoc["sessions"], 4);
    close(&adhoc["unattributed_usd"], 0.25);
    assert_eq!(adhoc["by_family"][0]["group"], "globex");
    close(&adhoc["by_family"][0]["usd"], 1.75);
    assert_eq!(adhoc["by_family"][0]["sessions"], 2);
    assert_eq!(adhoc["by_family"][1]["group"], "acme");
    assert_eq!(
        adhoc["by_repo"],
        json!([
            {"group": "globex/gadgets", "family": "globex", "usd": 1.0, "sessions": 1},
            {"group": "acme/widgets", "family": "acme", "usd": 0.5, "sessions": 1},
        ])
    );
    assert_eq!(adhoc["by_agent"][0]["group"], "claude-code");
    assert_eq!(adhoc["by_model"][0]["group"], "model-x");
    close(&adhoc["weekly"][0]["usd"], 2.5);

    let acme = body_json(
        call(HttpMethod::GET, "/api/v1/yield?family=acme", &admin, None)
            .await
            .unwrap(),
    )
    .await;
    close(&acme["adhoc"]["total_usd"], 0.5);
    assert_eq!(acme["adhoc"]["sessions"], 1);
    close(&acme["adhoc"]["unattributed_usd"], 0.0);

    let past = body_json(
        call(
            HttpMethod::GET,
            "/api/v1/yield?since=2020-01-01&until=2020-01-31",
            &admin,
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        past["adhoc"],
        json!({"total_usd": 0.0, "sessions": 0, "unattributed_usd": 0.0, "by_family": [],
               "by_repo": [], "by_agent": [], "by_model": [], "weekly": []})
    );
}
