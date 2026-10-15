//! Tests for the forge's own emit points: each helper's kind, summary,
//! outcome, attention flag and detail, read back from the event store.

use jeryu_core::{ForgeCore, PullRequest, UserRole};
use serde_json::{Value, json};

use super::emit;
use super::types::{Event, EventsQuery};
use crate::web::WebState;

const SHA: &str = "81dc3310aa5eadc15694dd1434d9f8f99c44a0d3";

fn forge() -> (WebState, PullRequest) {
    let core = ForgeCore::new();
    core.create_account("rel-bot", "rel-bot-password", UserRole::Admin)
        .unwrap();
    core.create_repository(
        "jeryu",
        jeryu_core::CreateRepositoryRequest {
            name: "jeryu-deploy".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let pr = core
        .create_pull_request(
            "jeryu",
            "jeryu-deploy",
            "rel-bot",
            jeryu_core::CreatePullRequestRequest {
                title: "Fix the header".to_string(),
                head: "fix-header".to_string(),
                base: "main".to_string(),
                head_sha: Some(SHA.to_string()),
                ..jeryu_core::CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    (WebState::new(core), pr)
}

fn events(state: &WebState) -> Vec<Event> {
    // The store reads newest first; oldest first reads like the timeline.
    let mut all = state.events.query(&EventsQuery::default()).unwrap();
    all.reverse();
    all
}

fn only(state: &WebState) -> Event {
    let mut all = events(state);
    assert_eq!(all.len(), 1, "{all:?}");
    all.remove(0)
}

fn create_deployment(state: &WebState, request: Value) -> jeryu_core::Deployment {
    let request: jeryu_core::CreateDeploymentRequest = serde_json::from_value(request).unwrap();
    state
        .core
        .create_deployment("jeryu", "jeryu-deploy", "rel-bot", request)
        .unwrap()
}

#[test]
fn pull_events_name_the_pr_its_head_and_who_acted() {
    let (state, pr) = forge();
    let label = format!("jeryu/jeryu-deploy#{}", pr.number);

    emit::pull_opened(&state, &pr, "rel-bot");
    emit::pull_approved(&state, &pr, "review-bot");
    emit::pull_reviewed(
        &state,
        &pr,
        "review-bot",
        "request_changes",
        Some("  fix it  "),
    );
    emit::pull_reviewed(&state, &pr, "review-bot", "comment", Some("   "));

    let all = events(&state);
    let lines: Vec<(&str, &str, Option<&str>, bool)> = all
        .iter()
        .map(|e| {
            (
                e.kind.as_str(),
                e.summary.as_str(),
                e.outcome.as_deref(),
                e.needs_human,
            )
        })
        .collect();
    assert_eq!(
        lines,
        [
            (
                "pr.opened",
                &*format!("opened {label}: Fix the header"),
                None,
                false
            ),
            (
                "pr.approved",
                &*format!("review-bot approved {label}"),
                Some("approve"),
                false
            ),
            (
                "pr.review",
                &*format!("review-bot reviewed {label}: request_changes"),
                Some("request_changes"),
                true
            ),
            (
                "pr.review",
                &*format!("review-bot reviewed {label}: comment"),
                Some("comment"),
                false
            ),
        ]
    );
    for event in &all {
        assert_eq!(event.source, "forge");
        assert_eq!(event.reporter, "forge");
        assert_eq!(event.repo.as_deref(), Some("jeryu/jeryu-deploy"));
        assert_eq!(event.pr, i64::try_from(pr.number).ok());
        assert_eq!(event.sha.as_deref(), Some(SHA), "defaults to the head");
        // Not a shift branch: no shift trace.
        assert_eq!(event.shift, None);
    }
    assert_eq!(all[0].actor.as_deref(), Some("rel-bot"));
    assert_eq!(
        all[0].detail,
        Some(json!({"head": "fix-header", "base": "main", "author": "rel-bot"}))
    );
    assert_eq!(all[2].reason.as_deref(), Some("fix it"), "trimmed");
    assert_eq!(all[3].reason, None, "a blank body is no reason");
    assert_eq!(
        all[3].detail,
        // review-bot is the configured review identity, so its verdict is an agent's.
        Some(json!({"title": "Fix the header", "author": "rel-bot", "reviewer_kind": "agent"}))
    );
}

#[test]
fn a_merge_carries_the_merge_commit_and_the_head_it_came_from() {
    let (state, mut pr) = forge();
    let merge_sha = "0123456789abcdef0123456789abcdef01234567";
    pr.merge_commit_sha = Some(merge_sha.to_string());

    emit::pull_merged(&state, &pr, "mergebot", "queue");

    let event = only(&state);
    assert_eq!(event.kind, "pr.merged");
    assert_eq!(event.outcome.as_deref(), Some("success"));
    assert_eq!(event.sha.as_deref(), Some(merge_sha));
    assert_eq!(
        event.summary,
        format!("merged jeryu/jeryu-deploy#{}: Fix the header", pr.number)
    );
    assert_eq!(
        event.detail,
        Some(json!({"via": "queue", "head_sha": SHA, "base": "main", "author": "rel-bot"}))
    );
}

#[test]
fn archive_and_moves_are_plain_successes_whose_kind_says_which() {
    let (state, _) = forge();
    let mut repo = state.core.get_repository("jeryu", "jeryu-deploy").unwrap();

    repo.archived = true;
    emit::repository_archived(&state, &repo, "rel-bot");
    repo.archived = false;
    emit::repository_archived(&state, &repo, "rel-bot");
    emit::repository_moved(&state, "jeryu/old-deploy", &repo, "rel-bot");
    emit::repository_moved(&state, "elsewhere/jeryu-deploy", &repo, "rel-bot");

    let all = events(&state);
    let kinds: Vec<&str> = all.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "repo.archived",
            "repo.unarchived",
            "repo.renamed",
            "repo.transferred"
        ]
    );
    assert_eq!(
        all[0].summary,
        "jeryu/jeryu-deploy is archived: read-only until it is unarchived"
    );
    assert_eq!(all[0].detail, Some(json!({"archived": true})));
    assert_eq!(
        all[1].summary,
        "jeryu/jeryu-deploy is unarchived: writable again"
    );
    assert_eq!(
        all[2].summary,
        "jeryu/old-deploy moved to jeryu/jeryu-deploy"
    );
    assert_eq!(
        all[3].detail,
        Some(json!({"from": "elsewhere/jeryu-deploy", "to": "jeryu/jeryu-deploy"}))
    );
    for event in &all {
        assert_eq!(event.outcome.as_deref(), Some("success"));
        assert!(!event.needs_human);
        assert_eq!(event.repo.as_deref(), Some("jeryu/jeryu-deploy"));
    }
}

#[test]
fn the_github_edge_emits_only_for_successful_writes() {
    let (state, pr) = forge();
    let path = format!("/repos/jeryu/jeryu-deploy/pulls/{}/merge", pr.number);
    emit::github_edge(&state, false, &path, "rel-bot", "", 200, "{}");
    emit::github_edge(&state, true, &path, "rel-bot", "", 409, "{}");
    emit::github_edge(
        &state,
        true,
        "/repos/jeryu/jeryu-deploy/issues",
        "rel-bot",
        "",
        201,
        "{}",
    );
    // A PR number that does not exist is skipped, not an error.
    emit::github_edge(
        &state,
        true,
        "/repos/jeryu/jeryu-deploy/pulls/999/merge",
        "rel-bot",
        "",
        200,
        "{}",
    );
    // A PATCH that did not change the name moves nothing.
    emit::github_edge(
        &state,
        true,
        "/repos/jeryu/jeryu-deploy",
        "rel-bot",
        "",
        200,
        r#"{"full_name": "jeryu/jeryu-deploy"}"#,
    );
    assert!(events(&state).is_empty());

    emit::github_edge(&state, true, &path, "rel-bot", "", 200, "{}");
    let merged = only(&state);
    assert_eq!(merged.kind, "pr.merged");
    assert_eq!(merged.detail.unwrap()["via"], "merge");
}

#[test]
fn github_edge_pull_writes_map_to_pull_events_and_review_verdicts() {
    let (state, pr) = forge();
    emit::github_edge(
        &state,
        true,
        "/repos/jeryu/jeryu-deploy/pulls",
        "rel-bot",
        "{}",
        201,
        &json!({"number": pr.number}).to_string(),
    );
    let reviews = format!("/repos/jeryu/jeryu-deploy/pulls/{}/reviews", pr.number);
    for (event, body) in [
        ("APPROVE", "ok"),
        ("CHANGES_REQUESTED", "no"),
        ("COMMENT", "hm"),
    ] {
        let comments = if event == "COMMENT" {
            json!([{"path": "src/lib.rs", "line": 4, "body": "rename"}])
        } else {
            json!([])
        };
        let request = json!({"event": event, "body": body, "comments": comments}).to_string();
        // A person reviews here: only a human reviewer's verdicts become
        // review.finding events (automation posts its own findings).
        emit::github_edge(&state, true, &reviews, "reviewer-ann", &request, 200, "{}");
    }

    let all = events(&state);
    let lines: Vec<(&str, Option<&str>, bool)> = all
        .iter()
        .map(|e| (e.kind.as_str(), e.outcome.as_deref(), e.needs_human))
        .collect();
    assert_eq!(
        lines,
        [
            ("pr.opened", None, false),
            ("pr.review", Some("approve"), false),
            ("pr.review", Some("request_changes"), true),
            // A request-changes with no line comment is one finding on the change.
            ("review.finding", Some("blocking"), false),
            ("pr.review", Some("comment"), false),
            ("review.finding", Some("comment"), false),
        ]
    );
    assert_eq!(all[2].reason.as_deref(), Some("no"));
    let finding = all[5].detail.as_ref().unwrap();
    assert_eq!(finding["reviewer_kind"], "human");
    assert_eq!(finding["path"], "src/lib.rs");
    assert_eq!(finding["line"], 4);
}

#[test]
fn deployments_become_created_then_status_events_with_the_log_tail() {
    let (state, _) = forge();
    let payload = json!({"release": "v1.2.3", "previous_release": "v1.2.2"});
    let deployment = create_deployment(
        &state,
        json!({"sha": SHA, "environment": "production", "payload": payload}),
    );

    // The create response is what the edge reads for `deploy.created`.
    let created = json!({
        "id": deployment.id,
        "sha": SHA,
        "environment": "production",
        "payload": payload,
    });
    emit::github_edge(
        &state,
        true,
        "/repos/jeryu/jeryu-deploy/deployments",
        "deployer",
        "{}",
        201,
        &created.to_string(),
    );
    let statuses = format!(
        "/repos/jeryu/jeryu-deploy/deployments/{}/statuses",
        deployment.id
    );
    let tail: String = (1..=30).map(|n| format!("line {n}\n")).collect();
    emit::github_edge(
        &state,
        true,
        &statuses,
        "deployer",
        &json!({"state": "failure", "log_path": "/var/log/release.log", "log_tail": tail})
            .to_string(),
        201,
        &json!({"state": "failure", "description": "the release step exited 1",
                "log_url": "https://forge/logs/1"})
        .to_string(),
    );
    emit::github_edge(
        &state,
        true,
        &statuses,
        "deployer",
        r#"{"state": "success", "log_tail": "  "}"#,
        201,
        r#"{"state": "success"}"#,
    );
    // An unknown deployment id emits nothing.
    emit::github_edge(
        &state,
        true,
        "/repos/jeryu/jeryu-deploy/deployments/9999/statuses",
        "deployer",
        "{}",
        201,
        r#"{"state": "success"}"#,
    );

    let all = events(&state);
    let lines: Vec<(&str, &str, Option<&str>, bool)> = all
        .iter()
        .map(|e| {
            (
                e.kind.as_str(),
                e.summary.as_str(),
                e.outcome.as_deref(),
                e.needs_human,
            )
        })
        .collect();
    assert_eq!(
        lines,
        [
            (
                "deploy.created",
                "production deploy of v1.2.3 started",
                None,
                false
            ),
            (
                "deploy.status",
                "production deploy of v1.2.3: failure",
                Some("failure"),
                true
            ),
            (
                "deploy.status",
                "production deploy of v1.2.3: success",
                Some("success"),
                false
            ),
        ]
    );
    for event in &all {
        assert_eq!(event.actor.as_deref(), Some("deployer"));
        assert_eq!(event.sha.as_deref(), Some(SHA));
    }
    let failed = &all[1];
    assert_eq!(failed.reason.as_deref(), Some("the release step exited 1"));
    assert_eq!(failed.log_url.as_deref(), Some("https://forge/logs/1"));
    let detail = failed.detail.clone().unwrap();
    assert_eq!(detail["deployment_id"], deployment.id);
    assert_eq!(detail["previous_release"], "v1.2.2");
    assert_eq!(detail["log_path"], "/var/log/release.log");
    let kept: Vec<&str> = detail["log_tail"].as_str().unwrap().lines().collect();
    assert_eq!(kept.len(), 20, "only the last 20 lines");
    assert_eq!(kept[0], "line 11");
    assert_eq!(kept[19], "line 30");
    let quiet = all[2].detail.clone().unwrap();
    assert_eq!(quiet["log_tail"], Value::Null, "a blank tail is dropped");
    assert_eq!(quiet["log_path"], Value::Null);
}

#[test]
fn a_deploy_without_a_release_names_the_short_sha() {
    let (state, _) = forge();
    emit::github_edge(
        &state,
        true,
        "/repos/jeryu/jeryu-deploy/deployments",
        "deployer",
        "{}",
        201,
        &json!({"id": 7, "sha": SHA}).to_string(),
    );
    let event = only(&state);
    assert_eq!(
        event.summary,
        "production deploy of jeryu/jeryu-deploy@81dc3310aa started"
    );
    assert_eq!(event.detail.unwrap()["environment"], "production");
}

#[test]
fn a_long_log_tail_is_cut_to_fit_the_detail_and_never_drops_the_event() {
    let (state, _) = forge();
    let deployment = create_deployment(&state, json!({"sha": SHA}));
    let statuses = format!(
        "/repos/jeryu/jeryu-deploy/deployments/{}/statuses",
        deployment.id
    );
    // Three lines of 5000 two-byte characters, then lines of control
    // characters whose JSON escapes are six times their size: each is far
    // over the detail limit before the cut.
    let wide = "é".repeat(5000);
    let control = "\u{1}".repeat(2000);
    for tail in [
        format!("{wide}\n{wide}\n{wide}"),
        format!("{control}\n{control}"),
    ] {
        emit::github_edge(
            &state,
            true,
            &statuses,
            "deployer",
            &json!({"log_tail": tail, "log_path": "/var/log/release.log"}).to_string(),
            201,
            r#"{"state": "failure", "description": "the release step exited 1"}"#,
        );
    }

    let all = events(&state);
    assert_eq!(all.len(), 2, "both failures are recorded: {all:?}");
    let wide_tail = all[0].detail.as_ref().unwrap()["log_tail"].clone();
    let kept = wide_tail.as_str().unwrap();
    assert!(kept.len() <= 6_000 && kept.len() >= 5_998, "{}", kept.len());
    assert!(kept.chars().all(|c| c == 'é'), "cut on a char boundary");
    let control_tail = all[1].detail.as_ref().unwrap()["log_tail"].clone();
    assert!(control_tail.to_string().len() <= 6_002);
    assert!(control_tail.as_str().unwrap().ends_with('\u{1}'));
    for event in &all {
        assert!(event.needs_human);
        assert!(event.detail.as_ref().unwrap().to_string().len() <= 8 * 1024);
    }
}

#[test]
fn the_gh_compatible_patch_lands_on_the_same_draft_events_as_the_named_routes() {
    let (state, pr) = forge();
    let path = format!("/repos/jeryu/jeryu-deploy/pulls/{}", pr.number);

    // A PATCH that changes only the title says nothing about drafts.
    emit::github_edge(
        &state,
        true,
        &path,
        "rel-bot",
        &json!({"title": "Fix the header again"}).to_string(),
        200,
        "{}",
    );
    assert!(events(&state).is_empty());

    for draft in [true, false] {
        state
            .core
            .update_pull_request(
                "jeryu",
                "jeryu-deploy",
                pr.number,
                jeryu_core::UpdatePullRequestRequest {
                    draft: Some(draft),
                    ..Default::default()
                },
            )
            .unwrap();
        emit::github_edge(
            &state,
            true,
            &path,
            "rel-bot",
            &json!({"draft": draft}).to_string(),
            200,
            "{}",
        );
    }

    let all = events(&state);
    let lines: Vec<(&str, Option<&str>, &str)> = all
        .iter()
        .map(|e| (e.kind.as_str(), e.outcome.as_deref(), e.summary.as_str()))
        .collect();
    assert_eq!(
        lines,
        [
            (
                "pr.draft",
                Some("draft"),
                "rel-bot converted jeryu/jeryu-deploy#1 back to a draft"
            ),
            (
                "pr.ready_for_review",
                Some("ready"),
                "jeryu/jeryu-deploy#1 marked ready by rel-bot"
            ),
        ]
    );
}

/// A finished gate pass on a head of the default branch posts `main.status`
/// with the verdict, and the first red head also charges the merged change
/// that carried it with a `main.escaped` defect. A head the gate already
/// judged stays a main head even after main moves on, which is what lets the
/// rerun of a red commit be recognised.
#[test]
fn a_gate_verdict_on_a_main_head_posts_the_status_and_the_escaped_defect() {
    let (state, mut pr) = forge();
    let merge_sha = "0123456789abcdef0123456789abcdef01234567";
    pr.merge_commit_sha = Some(merge_sha.to_string());
    emit::pull_merged(&state, &pr, "mergebot", "merge_queue");
    // The gate announced this head, which is how the test says "main is here"
    // without a git fixture; a live pass posts it from main's resolved head.
    crate::web::pipeline::record(
        &state,
        "forge",
        super::types::NewEvent {
            repo: Some("jeryu/jeryu-deploy".to_string()),
            sha: Some(merge_sha.to_string()),
            outcome: Some("started".to_string()),
            ..super::types::NewEvent::forge("main.status", "gate started on main")
        },
    )
    .unwrap();

    let beat = |last: Value| -> crate::web::control_plane::GateRunnerHeartbeat {
        serde_json::from_value(json!({
            "runnerId": "build-1/slot0", "host": "build-1", "slot": 0, "last": last
        }))
        .expect("heartbeat")
    };
    let pass = |sha: &str, conclusion: &str, finished: &str| {
        json!({
            "repo": "jeryu/jeryu-deploy", "sha": sha, "recipe": "ops/ci/check.sh",
            "conclusion": conclusion, "seconds": 61, "finishedAt": finished
        })
    };
    let red = beat(pass(merge_sha, "failure", "2026-10-06T12:00:00Z"));
    emit::runner_heartbeat(&state, Some(&beat(json!(null))), &red);

    let kinds = |kind: &str| {
        events(&state)
            .into_iter()
            .filter(|event| event.kind == kind)
            .collect::<Vec<_>>()
    };
    let status = kinds("main.status");
    assert_eq!(status.len(), 2, "{status:?}");
    assert_eq!(status[1].outcome.as_deref(), Some("red"));
    assert!(status[1].needs_human);
    assert_eq!(status[1].sha.as_deref(), Some(merge_sha));
    assert_eq!(
        status[1].summary,
        "jeryu/jeryu-deploy main at 0123456789ab is red"
    );
    let escaped = kinds("main.escaped");
    assert_eq!(escaped.len(), 1, "{escaped:?}");
    assert_eq!(escaped[0].pr, Some(pr.number as i64));
    assert_eq!(escaped[0].sha.as_deref(), Some(merge_sha));
    assert!(escaped[0].needs_human);

    // A rerun of the same commit that goes green closes the interval; it is
    // one more verdict and charges nothing new.
    let green = beat(pass(merge_sha, "success", "2026-10-06T12:30:00Z"));
    emit::runner_heartbeat(&state, Some(&red), &green);
    let status = kinds("main.status");
    assert_eq!(status.len(), 3, "{status:?}");
    assert_eq!(status[2].outcome.as_deref(), Some("green"));
    assert!(!status[2].needs_human);
    assert_eq!(kinds("main.escaped").len(), 1, "no second defect");

    let health =
        super::main_health::for_repo(&state, Some("jeryu/jeryu-deploy"), chrono::Utc::now())
            .unwrap();
    assert_eq!(health.len(), 1, "{health:?}");
    assert!(!health[0].red_now);
    assert_eq!(health[0].gate_runs, 2);
    assert_eq!(health[0].flaky_runs, 1, "the same commit later passed");
    assert_eq!(health[0].escaped_defects, 0, "a flake is no defect");
    assert_eq!(health[0].intervals[0].culprit_pr, Some(pr.number as i64));
}
