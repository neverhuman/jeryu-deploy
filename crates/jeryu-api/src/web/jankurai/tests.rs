//! Tests for the quality-gate visibility routes and the dispute store.

use std::path::Path;

use axum::http::{Method as HttpMethod, StatusCode};
use chrono::Utc;
use jeryu_core::{
    CreatePullRequestRequest, CreateRepositoryRequest, ForgeCore, MergePullRequestRequest,
    RecordJankuraiScoreRequest, RepoAccessLevel, UserRole,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::web::pipeline::tests::{body_json, request};
use crate::web::{WebState, app};

/// A report shaped like the auditor's: a floor, caps, and findings carrying
/// rule id, path, line and evidence.
fn report(minimum_score: u32, hard_findings: u32, caps: &[&str], rule: &str) -> Value {
    json!({
        "score": 70,
        "raw_score": 64,
        "caps_applied": caps,
        "decision": {"minimum_score": minimum_score, "hard_findings": hard_findings},
        "findings": [{
            "rule_id": rule,
            "check_id": format!("{rule}:shape"),
            "severity": "high",
            "hardness": if hard_findings > 0 { "hard" } else { "soft" },
            "category": "shape",
            "path": "crates/jeryu-api/src/web.rs",
            "line": 42,
            "problem": "the head names a dead marker",
            "evidence": ["matched term in crates/jeryu-api/src/web.rs:42"]
        }]
    })
}

fn score(
    branch: &str,
    sha: &str,
    value: Option<u32>,
    caps: &[&str],
    hard: u32,
    rule: &str,
) -> RecordJankuraiScoreRequest {
    RecordJankuraiScoreRequest {
        branch: branch.to_string(),
        commit_sha: sha.to_string(),
        score: value,
        hard_findings: Some(hard),
        decision: if value.is_some() {
            "scored".to_string()
        } else {
            "tool-failed".to_string()
        },
        caps_applied: caps.iter().map(|cap| (*cap).to_string()).collect(),
        report: Some(report(85, hard, caps, rule)),
        tool_exit: value.is_none().then_some(2),
    }
}

/// A forge with one repository, an admin, an ordinary user granted read on it
/// (repository reads are grant-based here, as everywhere on this surface), and
/// a token for each.
fn forge() -> (ForgeCore, String, String) {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.grant_repo_access("alice", "bob", "alice", "jeryu", RepoAccessLevel::Read)
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, user) = (token("alice"), token("bob"));
    (core, admin, user)
}

fn router(core: ForgeCore) -> axum::Router {
    app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    )
}

async fn get_json(router: &axum::Router, uri: &str, token: &str) -> Value {
    let response = router
        .clone()
        .oneshot(request(HttpMethod::GET, uri, token, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "GET {uri}");
    body_json(response).await
}

fn day_bucket<'a>(overview: &'a Value, date: &str) -> &'a Value {
    overview["daily"]
        .as_array()
        .unwrap()
        .iter()
        .find(|bucket| bucket["date"] == date)
        .unwrap_or_else(|| panic!("daily bucket for {date}"))
}

/// The owner's question: pass/fail over time, what fails, how scores are
/// spread, and what a required gate would have stopped from merging.
#[tokio::test]
async fn overview_aggregates_pass_rate_failures_and_would_have_blocked() {
    let (core, admin, _user) = forge();
    // A pass, a cap failure, a hard-finding failure and an audit that produced
    // no score at all.
    core.record_jankurai_score(
        "alice",
        "jeryu",
        score("main", "aaa", Some(96), &[], 0, "HLT-001"),
    )
    .unwrap();
    core.record_jankurai_score(
        "alice",
        "jeryu",
        score("feature", "bbb", Some(91), &["dead-language"], 0, "HLT-001"),
    )
    .unwrap();
    core.record_jankurai_score(
        "alice",
        "jeryu",
        score("wip", "ccc", Some(60), &[], 1, "HLT-008"),
    )
    .unwrap();
    core.record_jankurai_score(
        "alice",
        "jeryu",
        score("broken", "ddd", None, &[], 0, "HLT-001"),
    )
    .unwrap();
    // The cap failure merged anyway: exactly the head a required gate stops.
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "capped but merged".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("bbb".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    core.merge_pull_request(
        "alice",
        "jeryu",
        1,
        MergePullRequestRequest {
            merge_method: "merge".to_string(),
            ..MergePullRequestRequest::default()
        },
    )
    .unwrap();

    let router = router(core);
    let overview = get_json(&router, "/api/v1/jankurai/overview?days=7", &admin).await;
    assert_eq!(overview["days"], 7);
    assert_eq!(overview["totals"]["scored"], 4);
    assert_eq!(overview["totals"]["passed"], 1);
    assert_eq!(overview["totals"]["failed"], 3);
    assert_eq!(overview["totals"]["pass_rate"], 0.25);
    assert_eq!(overview["daily"].as_array().unwrap().len(), 7);
    let today = Utc::now().format("%Y-%m-%d").to_string();
    assert_eq!(day_bucket(&overview, &today)["scored"], 4);
    assert_eq!(day_bucket(&overview, &today)["failed"], 3);
    assert_eq!(overview["repos"].as_array().unwrap().len(), 1);
    assert_eq!(overview["repos"][0]["repo"], "alice/jeryu");
    assert_eq!(overview["repos"][0]["failed"], 3);

    let rules: Vec<(String, String, u64, u64)> = overview["failures_by_rule"]
        .as_array()
        .unwrap()
        .iter()
        .map(|rule| {
            (
                rule["rule"].as_str().unwrap().to_string(),
                rule["kind"].as_str().unwrap().to_string(),
                rule["failures"].as_u64().unwrap(),
                rule["repos_affected"].as_u64().unwrap(),
            )
        })
        .collect();
    assert!(
        rules.contains(&("dead-language".to_string(), "cap".to_string(), 1, 1)),
        "caps are counted: {rules:?}"
    );
    assert!(
        rules.contains(&("HLT-008".to_string(), "hard-finding".to_string(), 1, 1)),
        "hard findings are counted by rule: {rules:?}"
    );
    assert!(
        rules.contains(&("tool-failed".to_string(), "tool-failure".to_string(), 1, 1)),
        "an audit with no score is its own failure class: {rules:?}"
    );

    let distribution = overview["score_distribution"].as_array().unwrap();
    assert_eq!(distribution.len(), 11, "0-9 .. 90-99 plus 100");
    let count = |bucket: &str| {
        distribution
            .iter()
            .find(|entry| entry["bucket"] == bucket)
            .unwrap()["count"]
            .clone()
    };
    assert_eq!(count("90-99"), 2);
    assert_eq!(count("60-69"), 1);
    assert_eq!(count("100"), 0);

    let blocked = overview["would_have_blocked"].as_array().unwrap();
    assert_eq!(
        blocked.len(),
        1,
        "only the merged failing head: {blocked:?}"
    );
    assert_eq!(blocked[0]["commit_sha"], "bbb");
    assert_eq!(blocked[0]["floor"], 85);
    assert_eq!(blocked[0]["pull_request"]["number"], 1);
    assert_eq!(blocked[0]["pull_request"]["merged"], true);

    // The window is the one the owner asked for, not a free-form number.
    let rejected = router
        .clone()
        .oneshot(request(
            HttpMethod::GET,
            "/api/v1/jankurai/overview?days=9",
            &admin,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
}

/// `?repo=` narrows the overview, and a user only ever sees repositories they
/// may read.
#[tokio::test]
async fn overview_filters_by_repo_and_by_read_access() {
    let (core, admin, user) = forge();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "secret".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.record_jankurai_score(
        "alice",
        "jeryu",
        score("main", "aaa", Some(96), &[], 0, "HLT-001"),
    )
    .unwrap();
    core.record_jankurai_score(
        "alice",
        "secret",
        score("main", "sss", Some(40), &[], 1, "HLT-008"),
    )
    .unwrap();
    let router = router(core);

    let all = get_json(&router, "/api/v1/jankurai/overview", &admin).await;
    assert_eq!(all["days"], 7, "the default window is a week");
    assert_eq!(all["totals"]["scored"], 2);
    let scoped = get_json(
        &router,
        "/api/v1/jankurai/overview?repo=alice/jeryu",
        &admin,
    )
    .await;
    assert_eq!(scoped["totals"]["scored"], 1);
    assert_eq!(scoped["repos"].as_array().unwrap().len(), 1);

    let as_user = get_json(&router, "/api/v1/jankurai/overview", &user).await;
    assert_eq!(
        as_user["totals"]["scored"], 1,
        "bob holds no grant on alice/secret, so its scores stay out"
    );
    assert_eq!(as_user["repos"][0]["repo"], "alice/jeryu");
}

/// The two drill-downs: what one rule flagged, and everything one score holds.
#[tokio::test]
async fn rule_and_score_detail_expose_findings_caps_and_the_pull_request() {
    let (core, admin, _user) = forge();
    core.record_jankurai_score(
        "alice",
        "jeryu",
        score(
            "feature",
            "bbb",
            Some(70),
            &["dead-language"],
            1,
            "HLT-001-DEAD-MARKER",
        ),
    )
    .unwrap();
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "flagged".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("bbb".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    let router = router(core);

    let by_rule = get_json(
        &router,
        "/api/v1/jankurai/rules/HLT-001-DEAD-MARKER",
        &admin,
    )
    .await;
    assert_eq!(by_rule["days"], 30, "the rule view defaults to a month");
    let head = &by_rule["heads"][0];
    assert_eq!(by_rule["heads"].as_array().unwrap().len(), 1);
    assert_eq!(head["repo"], "alice/jeryu");
    assert_eq!(head["commit_sha"], "bbb");
    assert_eq!(head["score"], 70);
    assert_eq!(head["floor"], 85);
    assert_eq!(head["caps_applied"][0], "dead-language");
    assert_eq!(head["matched_as"], "hard-finding");
    assert_eq!(head["pull_request"]["number"], 1);
    assert_eq!(head["passed"], false);

    // A cap is addressable by its own name too.
    let by_cap = get_json(&router, "/api/v1/jankurai/rules/dead-language", &admin).await;
    assert_eq!(by_cap["heads"][0]["matched_as"], "cap");
    // A known rule outside the filter answers empty rather than 404.
    let filtered = get_json(
        &router,
        "/api/v1/jankurai/rules/HLT-001-DEAD-MARKER?repo=alice/other",
        &admin,
    )
    .await;
    assert!(filtered["heads"].as_array().unwrap().is_empty());
    // A rule id no score ever carried is not a resource.
    let unknown = router
        .clone()
        .oneshot(request(
            HttpMethod::GET,
            "/api/v1/jankurai/rules/HLT-999",
            &admin,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let score_id = head["score_id"].as_str().unwrap().to_string();
    let detail = get_json(
        &router,
        &format!("/api/v1/jankurai/scores/{score_id}"),
        &admin,
    )
    .await;
    assert_eq!(detail["score"], 70);
    assert_eq!(detail["raw_score"], 64);
    assert_eq!(detail["floor"], 85);
    assert_eq!(detail["hard_findings"], 1);
    assert_eq!(detail["caps_applied"][0], "dead-language");
    assert_eq!(detail["report_stored"], true);
    let finding = &detail["findings"][0];
    assert_eq!(finding["rule_id"], "HLT-001-DEAD-MARKER");
    assert_eq!(finding["path"], "crates/jeryu-api/src/web.rs");
    assert_eq!(finding["line"], 42);
    assert_eq!(finding["hardness"], "hard");
    assert_eq!(
        finding["evidence"][0],
        "matched term in crates/jeryu-api/src/web.rs:42"
    );
    assert_eq!(
        detail["pull_request"]["url"],
        "/api/v1/repos/alice/jeryu/pulls/1"
    );

    let missing = router
        .clone()
        .oneshot(request(
            HttpMethod::GET,
            "/api/v1/jankurai/scores/00000000-0000-0000-0000-000000000000",
            &admin,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

/// Disputes: admin-only to file, readable by any login, idempotent per author,
/// and counted per rule in the overview.
#[tokio::test]
async fn disputes_are_admin_only_and_counted_in_the_overview() {
    let (core, admin, user) = forge();
    core.record_jankurai_score(
        "alice",
        "jeryu",
        score("feature", "bbb", Some(70), &[], 1, "HLT-001-DEAD-MARKER"),
    )
    .unwrap();
    let router = router(core);
    let by_rule = get_json(
        &router,
        "/api/v1/jankurai/rules/HLT-001-DEAD-MARKER",
        &admin,
    )
    .await;
    let score_id = by_rule["heads"][0]["score_id"]
        .as_str()
        .unwrap()
        .to_string();
    let body = json!({
        "score_id": score_id,
        "rule_id": "HLT-001-DEAD-MARKER",
        "path": "crates/jeryu-api/src/web.rs",
        "line": 42,
        "reason": "the matched term is a quoted rule name, not product language"
    });

    let denied = router
        .clone()
        .oneshot(request(
            HttpMethod::POST,
            "/api/v1/jankurai/disputes",
            &user,
            Some(body.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(
        get_json(&router, "/api/v1/jankurai/disputes", &admin).await["disputes"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a denied filing must not create dispute state"
    );

    let filed = router
        .clone()
        .oneshot(request(
            HttpMethod::POST,
            "/api/v1/jankurai/disputes",
            &admin,
            Some(body.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(filed.status(), StatusCode::CREATED);
    let filed = body_json(filed).await;
    assert_eq!(filed["rule_id"], "HLT-001-DEAD-MARKER");
    assert_eq!(filed["repo"], "alice/jeryu");
    assert_eq!(filed["commit_sha"], "bbb");
    assert_eq!(filed["line"], 42);
    assert_eq!(filed["author"], "alice");

    // A retried POST returns the first row instead of inflating the rate.
    let retried = router
        .clone()
        .oneshot(request(
            HttpMethod::POST,
            "/api/v1/jankurai/disputes",
            &admin,
            Some(body),
        ))
        .await
        .unwrap();
    assert_eq!(retried.status(), StatusCode::OK);
    assert_eq!(body_json(retried).await["id"], filed["id"]);

    // Bad bodies and unknown scores are rejected cleanly.
    let empty_reason = router
        .clone()
        .oneshot(request(
            HttpMethod::POST,
            "/api/v1/jankurai/disputes",
            &admin,
            Some(json!({"score_id": score_id, "rule_id": "HLT-001", "reason": "  "})),
        ))
        .await
        .unwrap();
    assert_eq!(empty_reason.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let unknown_score = router
        .clone()
        .oneshot(request(
            HttpMethod::POST,
            "/api/v1/jankurai/disputes",
            &admin,
            Some(json!({
                "score_id": "00000000-0000-0000-0000-000000000000",
                "rule_id": "HLT-001",
                "reason": "wrong"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(unknown_score.status(), StatusCode::NOT_FOUND);

    // Any login may read them, and both drill-downs carry them.
    let listed = get_json(&router, "/api/v1/jankurai/disputes", &user).await;
    assert_eq!(listed["disputes"].as_array().unwrap().len(), 1);
    let scoped = get_json(
        &router,
        "/api/v1/jankurai/disputes?rule_id=HLT-001-DEAD-MARKER",
        &user,
    )
    .await;
    assert_eq!(scoped["disputes"].as_array().unwrap().len(), 1);
    let detail = get_json(
        &router,
        &format!("/api/v1/jankurai/scores/{score_id}"),
        &user,
    )
    .await;
    assert_eq!(detail["disputes"][0]["id"], filed["id"]);

    let overview = get_json(&router, "/api/v1/jankurai/overview", &admin).await;
    let rule = overview["failures_by_rule"]
        .as_array()
        .unwrap()
        .iter()
        .find(|rule| rule["rule"] == "HLT-001-DEAD-MARKER")
        .expect("the hard rule is a failure class");
    assert_eq!(rule["disputes"], 1);
    assert_eq!(rule["dispute_rate"], 1.0);
}

/// An empty store answers with a full, zeroed shape — never a 404 or a 500.
#[tokio::test]
async fn empty_store_answers_every_route_with_zeroed_buckets() {
    let (core, admin, _user) = forge();
    let router = router(core);

    let overview = get_json(&router, "/api/v1/jankurai/overview?days=30", &admin).await;
    assert_eq!(overview["totals"]["scored"], 0);
    assert_eq!(overview["totals"]["pass_rate"], 0.0);
    assert_eq!(overview["daily"].as_array().unwrap().len(), 30);
    assert!(overview["repos"].as_array().unwrap().is_empty());
    assert!(overview["failures_by_rule"].as_array().unwrap().is_empty());
    assert!(
        overview["would_have_blocked"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(overview["score_distribution"].as_array().unwrap().len(), 11);

    let rule = router
        .clone()
        .oneshot(request(
            HttpMethod::GET,
            "/api/v1/jankurai/rules/HLT-001",
            &admin,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(rule.status(), StatusCode::NOT_FOUND);
    let disputes = get_json(&router, "/api/v1/jankurai/disputes", &admin).await;
    assert!(disputes["disputes"].as_array().unwrap().is_empty());
}
