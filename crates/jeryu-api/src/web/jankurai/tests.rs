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
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
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

/// The web console's contract: the Quality gate pages read these routes, so
/// the overview, rule, head and dispute shapes are pinned here.
#[tokio::test]
async fn quality_gate_routes_serve_the_console_contract() {
    let (core, admin, user) = forge();
    core.record_jankurai_score(
        "alice",
        "jeryu",
        score("main", "aaa", Some(96), &[], 0, "HLT-001"),
    )
    .unwrap();
    core.record_jankurai_score(
        "alice",
        "jeryu",
        score("wip", "ccc", Some(60), &[], 1, "HLT-008"),
    )
    .unwrap();
    let router = router(core);

    let overview = get_json(&router, "/api/v1/quality-gate/overview?days=30", &user).await;
    assert_eq!(overview["schema_version"], 1);
    assert_eq!(overview["window_days"], 30);
    assert_eq!(overview["heads_scored"], 2);
    assert_eq!(overview["heads_failed"], 1);
    assert_eq!(overview["fail_rate"], 0.5);
    assert_eq!(overview["disputes"], 0);
    assert_eq!(overview["daily"].as_array().unwrap().len(), 30);
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let bucket = overview["daily"]
        .as_array()
        .unwrap()
        .iter()
        .find(|day| day["day"] == today.as_str())
        .unwrap();
    assert_eq!(
        (bucket["passed"].clone(), bucket["failed"].clone()),
        (json!(1), json!(1))
    );
    assert_eq!(overview["repos"][0]["repo"], "alice/jeryu");
    assert_eq!(overview["repos"][0]["top_rule"], "HLT-008");
    let rules = overview["rules"].as_array().unwrap();
    assert!(
        rules
            .iter()
            .any(|rule| rule["rule"] == "HLT-008" && rule["failures"] == 1)
    );
    assert_eq!(overview["repos_scored"], 1);
    assert!(
        rules.iter().any(|rule| rule["rule"] == "HLT-008"
            && rule["findings_all_heads"] == 1
            && rule["latest_findings"] == 1
            && rule["latest_repos"] == 1),
        "the latest head of alice/jeryu is ccc: {overview}"
    );
    assert!(
        overview["dimensions_below_floor"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let rule = get_json(&router, "/api/v1/quality-gate/rules/HLT-008?days=30", &user).await;
    assert_eq!(rule["rule"], "HLT-008");
    assert_eq!(rule["heads"][0]["sha"], "ccc");
    assert_eq!(rule["heads"][0]["score"], 60);
    assert_eq!(rule["heads"][0]["threshold"], 85);
    assert_eq!(rule["heads"][0]["findings"], 1);

    let head = get_json(&router, "/api/v1/quality-gate/heads/alice/jeryu/ccc", &user).await;
    assert_eq!(head["passed"], false);
    let finding = &head["findings"][0];
    assert_eq!(finding["rule"], "HLT-008");
    assert_eq!(finding["path"], "crates/jeryu-api/src/web.rs");
    assert_eq!(finding["line"], 42);
    assert_eq!(finding["disputed"], false);
    let finding_id = finding["id"].as_str().unwrap().to_string();
    let uri = format!("/api/v1/quality-gate/findings/{finding_id}/dispute");
    let body = json!({ "reason": "quoted rule name" });

    let denied = router
        .clone()
        .oneshot(request(HttpMethod::POST, &uri, &user, Some(body.clone())))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let filed = router
        .clone()
        .oneshot(request(HttpMethod::POST, &uri, &admin, Some(body)))
        .await
        .unwrap();
    assert_eq!(filed.status(), StatusCode::CREATED);
    let filed = body_json(filed).await;
    assert_eq!(filed["finding"]["id"], finding_id.as_str());
    assert_eq!(filed["finding"]["disputed_by"], "alice");

    let head = get_json(&router, "/api/v1/quality-gate/heads/alice/jeryu/ccc", &user).await;
    assert_eq!(head["findings"][0]["dispute_reason"], "quoted rule name");
    let overview = get_json(&router, "/api/v1/quality-gate/overview?days=30", &user).await;
    assert_eq!(overview["disputes"], 1);

    for missing in [
        "/api/v1/quality-gate/heads/alice/jeryu/zzz",
        "/api/v1/quality-gate/rules/NOT-A-RULE",
    ] {
        let response = router
            .clone()
            .oneshot(request(HttpMethod::GET, missing, &user, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "GET {missing}");
    }
}

#[tokio::test]
async fn quality_gate_head_explains_each_applied_cap() {
    let (core, _admin, user) = forge();
    core.record_jankurai_score(
        "alice",
        "jeryu",
        score(
            "wip",
            "ccc",
            Some(60),
            &["dead-language", "HLT-008"],
            1,
            "HLT-008",
        ),
    )
    .unwrap();
    let router = router(core);

    let head = get_json(&router, "/api/v1/quality-gate/heads/alice/jeryu/ccc", &user).await;
    let caps = head["caps"].as_array().unwrap();
    assert_eq!(caps.len(), 2, "{head}");
    assert_eq!(caps[0]["id"], "dead-language");
    assert_eq!(caps[0]["findings"], 0);
    assert!(
        caps[0]["meaning"]
            .as_str()
            .unwrap()
            .contains("dead or superseded code")
    );
    assert!(caps[0]["how_to_clear"].as_str().unwrap().contains("push"));
    assert_eq!(caps[1]["id"], "HLT-008");
    assert_eq!(caps[1]["findings"], 1);
    assert!(
        caps[1]["meaning"]
            .as_str()
            .unwrap()
            .contains("the head names a dead marker")
    );
    assert!(
        caps[1]["how_to_clear"]
            .as_str()
            .unwrap()
            .contains("1 `HLT-008` finding")
    );
}

/// The ingest will not take a diff report for a head that has no commit base:
/// such a ticket asks for a whole-tree audit, and a diff against the "no base"
/// marker is the empty change set that scored nothing for 16 repositories.
#[test]
fn a_head_with_no_commit_base_is_only_scored_by_a_full_audit() {
    use crate::ci_bridge::audit_queue::{
        self, AUDIT_MODE_DIFF, AUDIT_MODE_FULL, AuditTicket, NO_COMMIT_BASE_OID,
    };
    use crate::web::jankurai::audits::{RunnerAuditSubmission, authorize_runner_submission};
    use jeryu_core::{AccountStatus, AccountSummary};

    let head = "c".repeat(40);
    let scorer = AccountSummary {
        login: "ci-bot".to_string(),
        display_name: "ci-bot".to_string(),
        role: UserRole::User,
        status: AccountStatus::Active,
        auth_epoch: 0,
        must_change_password: false,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let submission = |mode: &str| RunnerAuditSubmission {
        branch: "main".to_string(),
        commit_sha: head.clone(),
        base_sha: NO_COMMIT_BASE_OID.to_string(),
        runner_id: "runner0".to_string(),
        jankurai_version: "jankurai 1.6.11".to_string(),
        jankurai_sha256: crate::ci_bridge::GOVERNED_JANKURAI_SHA256.to_string(),
        audit_mode: mode.to_string(),
        jankurai_receipt_sha256: None,
        report: None,
        tool_exit: Some(0),
    };

    audit_queue::queue()
        .lock()
        .unwrap()
        .enqueue(AuditTicket::whole_tree("alice", "jeryu", "main", &head));
    let Err(refused) =
        authorize_runner_submission(&scorer, "alice", "jeryu", &submission(AUDIT_MODE_DIFF))
    else {
        panic!("a diff report cannot score a head with no commit base");
    };
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert!(refused.reason.contains("full audit"), "{}", refused.reason);

    // Refusing put the job back, so the audit that can score this head still
    // has one to submit against.
    let Ok(ticket) =
        authorize_runner_submission(&scorer, "alice", "jeryu", &submission(AUDIT_MODE_FULL))
    else {
        panic!("a whole-tree report scores a head with no commit base");
    };
    assert_eq!(ticket.audit_mode, AUDIT_MODE_FULL);
}

/// One auditor report with explicit findings and, like the auditor's, the
/// `dimensions` it scored.
fn report_with(findings: Value, dimensions: &[(&str, u32)]) -> RecordJankuraiScoreRequest {
    RecordJankuraiScoreRequest {
        branch: "main".to_string(),
        commit_sha: String::new(),
        score: Some(70),
        hard_findings: Some(0),
        decision: "scored".to_string(),
        caps_applied: Vec::new(),
        report: Some(json!({
            "score": 70,
            "decision": {"minimum_score": 85, "hard_findings": 0},
            "dimensions": dimensions
                .iter()
                .map(|(name, score)| json!({"name": name, "score": score}))
                .collect::<Vec<_>>(),
            "findings": findings,
        })),
        tool_exit: None,
    }
}

fn rule_finding(rule: &str, path: &str, line: i64) -> Value {
    json!({
        "rule_id": rule,
        "check_id": format!("{rule}:shape"),
        "hardness": "soft",
        "path": path,
        "line": line,
        "problem": "a marker left in product code",
        "evidence": [],
    })
}

fn dimension_finding(rule: Option<&str>, dimension: &str, score: u32) -> Value {
    let mut finding = json!({
        "check_id": format!("{}:proof", rule.unwrap_or("HLT-000-SCORE-DIMENSION")),
        "severity": "medium",
        "hardness": "soft",
        "path": "Justfile",
        "problem": format!("`{dimension}` scored {score} below the standard floor of 85"),
        "evidence": [],
    });
    if let Some(rule) = rule {
        finding["rule_id"] = json!(rule);
    }
    finding
}

/// Rule rows count what is open on each repository's latest head, not every
/// push that carried it; dimension-floor results leave the rule rows and are
/// reported per dimension; rule-less checks keep their own `unknown` row.
#[tokio::test]
async fn quality_gate_overview_counts_latest_heads_and_separates_dimensions() {
    let (core, admin, _user) = forge();
    for owner in ["acme", "globex"] {
        core.create_account(owner, "owner-password", UserRole::User)
            .unwrap();
        core.create_repository(
            owner,
            CreateRepositoryRequest {
                name: "app".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }
    let dims = [("Build speed signals", 50), ("Code shape", 70)];
    let rule_less = json!({
        "check_id": "HLT-000-SCORE-DIMENSION:docs",
        "hardness": "soft",
        "path": "docs/",
        "problem": "agent-readable documentation is incomplete",
        "evidence": [],
    });
    // acme/app: the same finding on three pushes, then a fourth that also
    // carries a second, distinct one plus the same one twice in the report.
    for sha in ["a1", "a2", "a3"] {
        let mut request = report_with(
            json!([
                rule_finding("HLT-001", "src/lib.rs", 3),
                dimension_finding(Some("HLT-018"), "Build speed signals", 40),
            ]),
            &dims,
        );
        request.commit_sha = sha.to_string();
        core.record_jankurai_score("acme", "app", request).unwrap();
    }
    let mut latest = report_with(
        json!([
            rule_finding("HLT-001", "src/lib.rs", 3),
            rule_finding("HLT-001", "src/lib.rs", 3),
            rule_finding("HLT-001", "src/main.rs", 9),
            dimension_finding(Some("HLT-018"), "Build speed signals", 50),
            dimension_finding(Some("HLT-001"), "Code shape", 70),
            rule_less.clone(),
        ]),
        &dims,
    );
    latest.commit_sha = "a4".to_string();
    core.record_jankurai_score("acme", "app", latest).unwrap();
    // globex/app: one head, one dimension result, and a title that only looks
    // like one because its dimension is not in the report.
    let mut globex = report_with(
        json!([
            dimension_finding(Some("HLT-018"), "Build speed signals", 70),
            dimension_finding(Some("HLT-007"), "Not a scored dimension", 10),
        ]),
        &dims,
    );
    globex.commit_sha = "g1".to_string();
    core.record_jankurai_score("globex", "app", globex).unwrap();
    let router = router(core);

    let overview = get_json(&router, "/api/v1/quality-gate/overview?days=30", &admin).await;
    assert_eq!(overview["repos_scored"], 2, "{overview}");
    let rule = |id: &str| {
        overview["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| rule["rule"] == id)
            .cloned()
    };
    let hlt001 = rule("HLT-001").expect("HLT-001 row");
    assert_eq!(
        hlt001["failures"], 6,
        "three pushes once, the fourth thrice"
    );
    assert_eq!(hlt001["findings_all_heads"], 6);
    assert_eq!(hlt001["repos"], 1);
    assert_eq!(hlt001["latest_findings"], 2, "two distinct findings open");
    assert_eq!(hlt001["latest_repos"], 1);
    assert!(
        rule("HLT-018").is_none(),
        "only dimension results named HLT-018: {overview}"
    );
    let hlt007 = rule("HLT-007").expect("an unlisted dimension stays a finding");
    assert_eq!(hlt007["latest_findings"], 1);
    let unknown = rule("unknown").expect("rule-less checks keep their own row");
    assert_eq!(unknown["latest_findings"], 1);
    assert_eq!(unknown["latest_repos"], 1);

    let dimensions = overview["dimensions_below_floor"].as_array().unwrap();
    assert_eq!(dimensions.len(), 2, "{overview}");
    assert_eq!(dimensions[0]["dimension"], "Build speed signals");
    assert_eq!(dimensions[0]["repos"], 2);
    assert_eq!(dimensions[0]["median_score"], 60.0, "latest 50 and 70");
    assert_eq!(dimensions[0]["floor"], 85);
    assert_eq!(dimensions[0]["attributed_rule"], "HLT-018");
    assert_eq!(dimensions[1]["dimension"], "Code shape");
    assert_eq!(dimensions[1]["repos"], 1);
    assert_eq!(dimensions[1]["median_score"], 70.0);
}
