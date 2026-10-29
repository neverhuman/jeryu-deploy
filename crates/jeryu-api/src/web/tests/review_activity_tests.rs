//! The pull request page's live review fact: which head the automated
//! reviewer is reading, so a hold on a head that has been replaced reads as
//! history rather than as the verdict. The accounts and repository below are
//! invented.

use super::*;
use crate::web::control_plane::{GateRunnerHeartbeat, GateRunnerTask};

const HELD_HEAD: &str = "91beebd9f2f4b1c3d5e7a9b1c3d5e7a9b1c3d5e7";
const FIXED_HEAD: &str = "edd08c5a1f2e4b6c8d0a2b4c6d8e0f1a2b3c4d5e";

fn forge_with_pull() -> (Arc<WebState>, String, u64) {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "jeryu",
            CreateRepositoryRequest {
                name: "jeryu-ci-runner".to_string(),
                default_branch: Some("main".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    let pr = core
        .create_pull_request(
            "jeryu",
            "jeryu-ci-runner",
            "worker",
            CreatePullRequestRequest {
                title: "fix the runner".to_string(),
                head: "work/fix".to_string(),
                base: "main".to_string(),
                head_sha: Some(FIXED_HEAD.to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    (
        Arc::new(WebState::new(core)),
        repo.id.to_string(),
        pr.number,
    )
}

/// A reviewer last heard from `seconds_ago`, with a review of `sha` running
/// on `pr`.
fn reviewing(state: &WebState, pr: u64, sha: &str, seconds_ago: i64) {
    let beat = GateRunnerHeartbeat {
        runner_id: "node-0/pr-redteam".to_string(),
        host: "node-0".to_string(),
        slot: 0,
        labels: vec!["redteam".to_string()],
        capabilities: Vec::new(),
        interval_seconds: None,
        current: Some(GateRunnerTask {
            repo: "jeryu/jeryu-ci-runner".to_string(),
            pr: Some(pr),
            sha: sha.to_string(),
            recipe: "redteam-review".to_string(),
            target: None,
            kind: Some("pr".to_string()),
            started_at: "2026-10-22T18:44:12Z".parse().unwrap(),
        }),
        last: None,
        code: None,
        tools: Vec::new(),
    };
    state
        .gate_runners
        .record(
            beat,
            "ci-bot",
            chrono::Utc::now() - chrono::Duration::seconds(seconds_ago),
        )
        .expect("heartbeat is accepted");
}

/// The page is told which head the reviewer is on, by name for a signed-in
/// reader. Without it the page cannot tell "held, and nobody has looked
/// since" from "held, and the reviewer is already reading the fix" (live:
/// jeryu/jeryu-ci-runner#109, held at 91beebd and re-read at edd08c5).
#[tokio::test]
async fn the_pull_request_payload_names_the_head_the_reviewer_is_reading() {
    let (state, repo_id, number) = forge_with_pull();
    let path = || AxumPath((repo_id.clone(), number));

    // Nothing reviewing: the key is present and null, so a reader does not
    // have to tell "no pass" from "an older forge that never says".
    let quiet = response_json(
        crate::web::pulls::detail(State(state.clone()), authenticated_account("owner"), path())
            .await,
    )
    .await;
    assert_eq!(quiet["review_activity"], Value::Null);
    assert_eq!(quiet["summary"]["head_sha"], FIXED_HEAD);

    reviewing(&state, number, FIXED_HEAD, 30);
    let live = response_json(
        crate::web::pulls::detail(State(state.clone()), authenticated_account("owner"), path())
            .await,
    )
    .await;
    assert_eq!(live["review_activity"]["sha"], FIXED_HEAD);
    assert_eq!(
        live["review_activity"]["started_at"],
        "2026-10-22T18:44:12+00:00"
    );
    assert_eq!(live["review_activity"]["runner_id"], "node-0/pr-redteam");
}

/// A pass on a head that is itself no longer current is reported as the head
/// it is on, never as the pull request's head: the page compares the two and
/// says which. A reviewer that has stopped beating claims nothing.
#[tokio::test]
async fn a_review_of_an_older_head_is_reported_as_that_head() {
    let (state, repo_id, number) = forge_with_pull();
    let path = || AxumPath((repo_id.clone(), number));
    reviewing(&state, number, HELD_HEAD, 30);
    let detail = response_json(
        crate::web::pulls::detail(State(state.clone()), authenticated_account("owner"), path())
            .await,
    )
    .await;
    assert_eq!(detail["review_activity"]["sha"], HELD_HEAD);
    assert_eq!(detail["summary"]["head_sha"], FIXED_HEAD);

    // The same claim on the current head, from a reviewer that stopped
    // beating two hours ago: nothing to wait for, so nothing is claimed.
    let (stale, stale_repo, stale_number) = forge_with_pull();
    reviewing(&stale, stale_number, FIXED_HEAD, 2 * 60 * 60);
    let gone = response_json(
        crate::web::pulls::detail(
            State(stale.clone()),
            authenticated_account("owner"),
            AxumPath((stale_repo, stale_number)),
        )
        .await,
    )
    .await;
    assert_eq!(
        gone["review_activity"],
        Value::Null,
        "a reviewer that stopped beating is not reviewing anything"
    );
}
