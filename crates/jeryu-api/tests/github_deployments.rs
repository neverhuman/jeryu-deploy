//! Conformance tests for the GitHub-shaped deployment routes: deployments,
//! their append-only statuses, and the per-environment summary the release
//! views read.

use jeryu_api::GithubRouter;
use serde_json::Value;

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA_C: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn body(response: &jeryu_api::Response) -> Value {
    serde_json::from_str(&response.body)
        .unwrap_or_else(|err| panic!("response body is not JSON ({err}): {}", response.body))
}

fn router_with_repo() -> GithubRouter {
    let router = GithubRouter::new();
    let response = router.post("/repos", r#"{"owner":"alice","name":"jeryu"}"#);
    assert_eq!(response.status, 201, "create repo: {}", response.body);
    router
}

fn deploy(router: &GithubRouter, sha: &str, environment: &str) -> u64 {
    let response = router.post(
        "/repos/alice/jeryu/deployments",
        &format!(
            r#"{{"sha":"{sha}","ref":"main","environment":"{environment}","payload":{{"release":"rel-{}"}},"actor":"deployer"}}"#,
            &sha[..7]
        ),
    );
    assert_eq!(response.status, 201, "create deployment: {}", response.body);
    body(&response)["id"]
        .as_u64()
        .expect("numeric deployment id")
}

fn post_state(router: &GithubRouter, id: u64, state: &str) -> jeryu_api::Response {
    router.post(
        &format!("/repos/alice/jeryu/deployments/{id}/statuses"),
        &format!(
            r#"{{"state":"{state}","environment_url":"https://git.neverhuman.org","log_url":"https://logs/{id}","actor":"deployer"}}"#
        ),
    )
}

fn states(router: &GithubRouter, id: u64) -> Vec<String> {
    let response = router.get(&format!("/repos/alice/jeryu/deployments/{id}/statuses"));
    assert_eq!(response.status, 200, "{}", response.body);
    body(&response)
        .as_array()
        .expect("status list")
        .iter()
        .map(|status| status["state"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn create_and_get_return_github_shaped_deployments() {
    let router = router_with_repo();
    let id = deploy(&router, SHA_A, "production");

    let fetched = body(&router.get(&format!("/repos/alice/jeryu/deployments/{id}")));
    assert_eq!(fetched["id"], id);
    assert_eq!(fetched["sha"], SHA_A);
    assert_eq!(fetched["ref"], "main");
    assert_eq!(fetched["task"], "deploy");
    assert_eq!(fetched["environment"], "production");
    assert_eq!(fetched["original_environment"], "production");
    assert_eq!(fetched["production_environment"], true);
    assert_eq!(fetched["transient_environment"], false);
    assert_eq!(fetched["payload"]["release"], "rel-aaaaaaa");
    assert_eq!(fetched["creator"]["login"], "deployer");
    assert_eq!(
        fetched["statuses_url"],
        format!("/repos/alice/jeryu/deployments/{id}/statuses")
    );
}

#[test]
fn list_filters_by_query_newest_first() {
    let router = router_with_repo();
    let first = deploy(&router, SHA_A, "production");
    let canary = deploy(&router, SHA_B, "canary");
    let second = deploy(&router, SHA_B, "production");

    let ids = |path: &str| -> Vec<u64> {
        body(&router.get(path))
            .as_array()
            .expect("deployment list")
            .iter()
            .map(|d| d["id"].as_u64().unwrap())
            .collect()
    };
    assert_eq!(
        ids("/repos/alice/jeryu/deployments"),
        vec![second, canary, first]
    );
    assert_eq!(
        ids("/repos/alice/jeryu/deployments?environment=production"),
        vec![second, first]
    );
    assert_eq!(
        ids(&format!("/repos/alice/jeryu/deployments?sha={SHA_B}")),
        vec![second, canary]
    );
    assert_eq!(
        ids("/repos/alice/jeryu/deployments?environment=canary&per_page=1"),
        vec![canary]
    );
}

#[test]
fn statuses_append_and_a_success_retires_the_replaced_deployment() {
    let router = router_with_repo();
    let old = deploy(&router, SHA_A, "production");
    assert_eq!(post_state(&router, old, "success").status, 201);

    let new = deploy(&router, SHA_B, "production");
    let in_progress = post_state(&router, new, "in_progress");
    assert_eq!(in_progress.status, 201);
    let status = body(&in_progress);
    assert_eq!(status["state"], "in_progress");
    assert_eq!(status["environment"], "production");
    assert_eq!(status["log_url"], format!("https://logs/{new}"));
    assert_eq!(status["target_url"], status["log_url"]);
    assert_eq!(
        states(&router, old),
        vec!["success"],
        "an in-progress deploy has not replaced anything yet"
    );

    assert_eq!(post_state(&router, new, "success").status, 201);
    assert_eq!(states(&router, new), vec!["success", "in_progress"]);
    assert_eq!(states(&router, old), vec!["inactive", "success"]);
}

#[test]
fn environments_report_latest_current_and_previous() {
    let router = router_with_repo();
    let first = deploy(&router, SHA_A, "production");
    post_state(&router, first, "success");
    let second = deploy(&router, SHA_B, "production");
    post_state(&router, second, "success");
    let attempt = deploy(&router, SHA_C, "production");
    post_state(&router, attempt, "failure");

    let response = router.get("/repos/alice/jeryu/environments");
    assert_eq!(response.status, 200, "{}", response.body);
    let parsed = body(&response);
    assert_eq!(parsed["total_count"], 1);
    let production = &parsed["environments"][0];
    assert_eq!(production["name"], "production");
    assert_eq!(production["latest"]["deployment"]["id"], attempt);
    assert_eq!(production["latest"]["status"]["state"], "failure");
    assert_eq!(production["current"]["deployment"]["id"], second);
    assert_eq!(production["current"]["deployment"]["sha"], SHA_B);
    assert_eq!(production["previous"]["deployment"]["id"], first);
    assert_eq!(production["previous"]["status"]["state"], "inactive");
    assert_eq!(production["previous"]["succeeded"], true);
}

#[test]
fn invalid_requests_get_github_status_codes() {
    let router = router_with_repo();
    let bad_sha = router.post(
        "/repos/alice/jeryu/deployments",
        r#"{"sha":"main","environment":"production"}"#,
    );
    assert_eq!(
        bad_sha.status, 422,
        "a moving ref is not a deploy record: {}",
        bad_sha.body
    );

    let missing_sha = router.post("/repos/alice/jeryu/deployments", r#"{"environment":"x"}"#);
    assert_eq!(missing_sha.status, 422, "{}", missing_sha.body);

    let id = deploy(&router, SHA_A, "production");
    let bad_state = router.post(
        &format!("/repos/alice/jeryu/deployments/{id}/statuses"),
        r#"{"state":"deployed"}"#,
    );
    assert_eq!(bad_state.status, 422, "{}", bad_state.body);

    for path in [
        "/repos/alice/jeryu/deployments/999",
        "/repos/alice/jeryu/deployments/0",
        "/repos/alice/jeryu/deployments/abc",
        "/repos/alice/jeryu/deployments/999/statuses",
        "/repos/alice/missing/deployments",
        "/repos/alice/missing/environments",
    ] {
        assert_eq!(router.get(path).status, 404, "{path}");
    }
    assert_eq!(post_state(&router, 999, "success").status, 404);
}

#[test]
fn a_repository_with_no_deployments_has_no_environments() {
    let router = router_with_repo();
    let parsed = body(&router.get("/repos/alice/jeryu/environments"));
    assert_eq!(parsed["total_count"], 0);
    assert_eq!(parsed["environments"], serde_json::json!([]));
    assert_eq!(
        body(&router.get("/repos/alice/jeryu/deployments")),
        serde_json::json!([])
    );
}
