//! Cell-surface REST + error-path coverage for the workcell endpoints.
//!
//! These exercise the previously-untested surface handlers — `list` / `status` /
//! `claim` / `heartbeat` — plus the 404 / epoch-fence / malformed-body /
//! id-mismatch error paths, by driving the handlers directly (and the live
//! router once, to prove the route is wired). The whole `web` module is
//! `#[cfg(feature = "web")]`-gated, so this submodule compiles to nothing without
//! `--features web` (an honest skip by cfg). `tests.rs`'s helpers are not visible
//! from a sibling module, so `response_json` is inlined.

use super::*;
use serde_json::{Value, json};

/// Decode an axum response body into JSON. Inlined from `tests.rs`, whose helpers
/// live in a sibling module and so cannot be imported here.
async fn response_json(response: AxumResponse) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body reads");
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("response body is not JSON ({err}): {bytes:?}"))
}

/// Take `r1` a claim epoch from the daemon through the real route, as a runner
/// does before it claims. The ledger's fence fails closed, so a claim carrying
/// an epoch nobody issued takes no cell.
async fn admit_r1(state: &Arc<WebState>) -> u64 {
    let admitted = response_json(
        super::workcells::admit_runner(
            State(state.clone()),
            None,
            axum::body::Bytes::from(json!({ "runner_id": "r1" }).to_string()),
        )
        .await,
    )
    .await;
    assert_eq!(admitted["runner_id"], "r1");
    admitted["runner_epoch"]
        .as_u64()
        .expect("the daemon answers with the epoch it issued")
}

/// A claim body that drives a Ready->Claimed cell via a Rebased startup (no git).
fn claim_body(epoch: u64) -> Value {
    json!({
        "agent_id": "agent-1",
        "workspace_root": "/ws",
        "repo_roots": ["/ws"],
        "branch_budget": 1,
        "runner_id": "r1",
        "runner_epoch": epoch,
        "git_status_summary": "clean",
        "ci_snapshot_age_ms": 0,
        "startup": {
            "state": "rebased",
            "main_ref": "origin/main",
            "base_sha": "aaa",
            "head_sha": "bbb"
        }
    })
}

#[tokio::test]
async fn claim_rewrites_retired_jekko_repo_root() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let epoch = admit_r1(&state).await;
    let mut body = claim_body(epoch);
    body["workspace_root"] = json!("/home/ubuntu/jekko");
    body["repo_roots"] = json!(["/home/ubuntu/jekko/jnoccio-fusion"]);

    let claimed = response_json(
        super::workcells::claim(
            State(state),
            axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()),
        )
        .await,
    )
    .await;

    assert_eq!(claimed["state"], "claimed");
    assert_eq!(
        claimed["workspace_root"],
        json!("/home/ubuntu/jekko-split/jekko")
    );
    assert_eq!(
        claimed["repo_roots"],
        json!(["/home/ubuntu/jekko-split/jekko/jnoccio-fusion"])
    );
}

#[tokio::test]
async fn list_is_empty_then_reflects_claim_status_and_heartbeat() {
    let state = Arc::new(WebState::new(ForgeCore::new()));

    // The list starts empty.
    let empty = super::workcells::list(State(state.clone())).await;
    assert!(empty.0.is_empty(), "a fresh manager exposes no workcells");

    // Claim a ready cell at the epoch the daemon issued: state becomes
    // "claimed" and the epoch round-trips.
    let epoch = admit_r1(&state).await;
    let claimed = response_json(
        super::workcells::claim(
            State(state.clone()),
            axum::body::Bytes::from(serde_json::to_vec(&claim_body(epoch)).unwrap()),
        )
        .await,
    )
    .await;
    assert_eq!(claimed["state"], "claimed");
    assert_eq!(claimed["runner_epoch"], epoch);
    let id = claimed["workcell_id"]
        .as_str()
        .expect("claim returns a workcell id")
        .to_string();

    // The list now contains the claimed cell (alongside any warm replacement the
    // manager keeps ready — the exact pool size is an implementation detail).
    let listed = super::workcells::list(State(state.clone())).await;
    assert!(
        listed.0.iter().any(|w| w.workcell_id == id),
        "the claimed cell must appear in the workcell list (got {} cells)",
        listed.0.len()
    );

    // GET :id status returns the same lease.
    let status =
        response_json(super::workcells::status(State(state.clone()), AxumPath(id.clone())).await)
            .await;
    assert_eq!(status["workcell_id"], id.as_str());
    assert_eq!(status["runner_epoch"], epoch);

    // A heartbeat with the matching epoch keeps the cell healthy.
    let hb = response_json(
        super::workcells::heartbeat(
            State(state.clone()),
            AxumPath(id.clone()),
            axum::body::Bytes::from(
                serde_json::to_vec(&json!({ "runner_epoch": epoch, "heartbeat_healthy": true }))
                    .unwrap(),
            ),
        )
        .await,
    )
    .await;
    // The heartbeat returns the refreshed lease: same cell, same epoch, healthy.
    assert_eq!(hb["heartbeat_healthy"], true);
    assert_eq!(hb["runner_epoch"], epoch);
    assert_eq!(hb["workcell_id"], id.as_str());
}

#[tokio::test]
async fn status_of_unknown_cell_is_typed_404() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let resp = super::workcells::status(State(state), AxumPath("wc-nope".into())).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = response_json(resp).await;
    assert_eq!(body["code"], "not_found");
    assert_eq!(body["purpose"], "inspect a workcell");
    for k in ["reason", "common_fixes", "docs_url", "repair_hint"] {
        assert!(body.get(k).is_some(), "typed 404 must carry `{k}`");
    }
}

#[tokio::test]
async fn heartbeat_with_stale_epoch_is_fenced_409() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let epoch = admit_r1(&state).await;
    let claimed = response_json(
        super::workcells::claim(
            State(state.clone()),
            axum::body::Bytes::from(serde_json::to_vec(&claim_body(epoch)).unwrap()),
        )
        .await,
    )
    .await;
    let id = claimed["workcell_id"].as_str().unwrap().to_string();

    // A heartbeat carrying the wrong runner epoch is fenced (409), not accepted.
    let resp = super::workcells::heartbeat(
        State(state.clone()),
        AxumPath(id),
        axum::body::Bytes::from(serde_json::to_vec(&json!({ "runner_epoch": 999 })).unwrap()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(response_json(resp).await["code"], "workcell_epoch_fenced");
}

#[tokio::test]
async fn claim_with_malformed_json_is_typed_422() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let resp =
        super::workcells::claim(State(state), axum::body::Bytes::from_static(b"{ not json")).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response_json(resp).await["code"],
        "workcell_invalid_request"
    );
}

#[tokio::test]
async fn export_pr_with_mismatched_id_is_400_before_any_git() {
    // The path id and the body's workcell_id disagree -> 400, short-circuited
    // before any cell lookup or git diff (so no claim/git setup is needed).
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let body = json!({
        "workcell_id": "wc-other",
        "runner_epoch": 4,
        "branch_suffix": "s",
        "owner": "alice",
        "repo": "jeryu",
        "author": "agent-1"
    });
    let resp = super::workcells::export_pr(
        State(state),
        AxumPath("wc-real".into()),
        axum::http::HeaderMap::new(),
        axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response_json(resp).await["code"], "workcell_id_mismatch");
}

#[tokio::test]
async fn list_route_is_wired_and_returns_empty_array() {
    use axum::body::Body;
    use tower::ServiceExt;
    let resp = app(
        WebState::new(ForgeCore::new()),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    )
    .oneshot(
        Request::builder()
            .uri("/api/v1/workcells")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(response_json(resp).await.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_claim_at_an_epoch_the_daemon_never_issued_is_fenced() {
    // No runner has been admitted, so the ledger's fence holds every claim:
    // a runner cannot mint itself a lifetime by naming an epoch.
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let resp = super::workcells::claim(
        State(state.clone()),
        axum::body::Bytes::from(serde_json::to_vec(&claim_body(1)).unwrap()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = response_json(resp).await;
    assert_eq!(body["code"], "workcell_epoch_fenced");
    // After a deploy or restart this is what every runner sees first, so the
    // refusal names the route that admits it.
    assert!(
        body["common_fixes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fix| fix
                .as_str()
                .unwrap()
                .contains("POST /api/v1/workcells/runners")),
        "a fenced claim names how to take an epoch: {body}"
    );
    assert!(
        !super::workcells::list(State(state))
            .await
            .0
            .iter()
            .any(|cell| cell.runner_id == "r1"),
        "a fenced claim takes no cell"
    );
}

#[tokio::test]
async fn a_claim_from_a_previous_runner_lifetime_is_fenced() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let first = admit_r1(&state).await;
    // The runner restarted and took a fresh lifetime, so its earlier epoch is
    // no longer the one the daemon issues.
    let second = admit_r1(&state).await;
    assert!(second > first, "a new lifetime raises the epoch");

    let stale = super::workcells::claim(
        State(state.clone()),
        axum::body::Bytes::from(serde_json::to_vec(&claim_body(first)).unwrap()),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(response_json(stale).await["code"], "workcell_epoch_fenced");

    let claimed = response_json(
        super::workcells::claim(
            State(state),
            axum::body::Bytes::from(serde_json::to_vec(&claim_body(second)).unwrap()),
        )
        .await,
    )
    .await;
    assert_eq!(claimed["state"], "claimed");
    assert_eq!(claimed["runner_epoch"], second);
}

#[tokio::test]
async fn a_retired_runner_is_fenced_and_its_held_cell_still_releases() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let epoch = admit_r1(&state).await;
    let claimed = response_json(
        super::workcells::claim(
            State(state.clone()),
            axum::body::Bytes::from(serde_json::to_vec(&claim_body(epoch)).unwrap()),
        )
        .await,
    )
    .await;
    let id = claimed["workcell_id"].as_str().unwrap().to_string();

    let retired = response_json(
        super::workcells::retire_runner(State(state.clone()), None, AxumPath("r1".into())).await,
    )
    .await;
    assert_eq!(retired["retired"], true);
    let again = response_json(
        super::workcells::retire_runner(State(state.clone()), None, AxumPath("r1".into())).await,
    )
    .await;
    assert_eq!(
        again["retired"], false,
        "the lifetime is gone, not held twice"
    );

    // No new cell for a runner the daemon no longer admits.
    let resp = super::workcells::claim(
        State(state.clone()),
        axum::body::Bytes::from(serde_json::to_vec(&claim_body(epoch)).unwrap()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(response_json(resp).await["code"], "workcell_epoch_fenced");

    // The cell it already held is fenced per cell, not per lifetime, so the
    // work it holds is still released instead of stranded.
    let release = super::workcells::release(
        State(state),
        AxumPath(id),
        axum::body::Bytes::from(serde_json::to_vec(&json!({ "runner_epoch": epoch })).unwrap()),
    )
    .await;
    assert_eq!(release.status(), StatusCode::OK);
}

#[tokio::test]
async fn admit_runner_refuses_an_id_that_may_not_key_the_fence() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    for runner_id in [
        "",
        "   ",
        "node 2",
        "../etc/passwd",
        "/node-2",
        "..",
        "node-2//slot0",
    ] {
        let resp = super::workcells::admit_runner(
            State(state.clone()),
            None,
            axum::body::Bytes::from(json!({ "runner_id": runner_id }).to_string()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{runner_id:?}");
        assert_eq!(
            response_json(resp).await["code"],
            "workcell_runner_id_invalid",
            "{runner_id:?}"
        );
    }
}

#[tokio::test]
async fn taking_a_runner_epoch_is_admin_only() {
    use axum::body::Body;
    use axum::http::{Method, header};
    use jeryu_core::UserRole;
    use tower::ServiceExt;

    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    core.create_account("reader", "reader-password", UserRole::User)
        .unwrap();
    let reader = core
        .create_personal_access_token("reader", "test", None)
        .unwrap()
        .secret;
    let state = WebState::new(core).with_auth(true, false, false);
    let registry = Arc::clone(&state.runner_epochs);
    let router = app(state, std::path::Path::new("/tmp/jeryu-no-spa"));

    for (method, uri) in [
        (Method::POST, "/api/v1/workcells/runners"),
        (Method::DELETE, "/api/v1/workcells/runners/r1"),
    ] {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(uri)
                    .header(header::AUTHORIZATION, format!("Bearer {reader}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({ "runner_id": "r1" }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
    }
    assert_eq!(
        registry.lock().unwrap().epoch("r1"),
        None,
        "a refused request issues no lifetime"
    );
}

/// Runner ids may contain `/` (`node-2/slot0`), so the drop route takes the
/// rest of the path: a one-segment parameter would never match such an id and
/// its lifetime could not be dropped.
#[tokio::test]
async fn a_runner_named_with_a_slash_is_listed_and_dropped_through_the_router() {
    use axum::body::Body;
    use axum::http::Method;
    use tower::ServiceExt;

    let state = WebState::new(ForgeCore::new());
    let registry = Arc::clone(&state.runner_epochs);
    let router = app(state, std::path::Path::new("/tmp/jeryu-no-spa"));
    let send = |method: Method, uri: &str, body: Option<Value>| {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap();
        router.clone().oneshot(request)
    };

    let admitted = send(
        Method::POST,
        "/api/v1/workcells/runners",
        Some(json!({ "runner_id": "node-2/slot0" })),
    )
    .await
    .unwrap();
    assert_eq!(admitted.status(), StatusCode::OK);
    let admitted = response_json(admitted).await;
    assert_eq!(admitted["replaced_epoch"], Value::Null);
    let epoch = admitted["runner_epoch"].as_u64().unwrap();

    let again = response_json(
        send(
            Method::POST,
            "/api/v1/workcells/runners",
            Some(json!({ "runner_id": "node-2/slot0" })),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        again["replaced_epoch"], epoch,
        "a reissue says which lifetime it fenced"
    );

    let listed = send(Method::GET, "/api/v1/workcells/runners", None)
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed = response_json(listed).await;
    let row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["runner_id"] == "node-2/slot0")
        .expect("the admitted runner is listed");
    assert_eq!(row["runner_epoch"], again["runner_epoch"]);
    assert_eq!(row["live_cells"], 0);

    let dropped = send(
        Method::DELETE,
        "/api/v1/workcells/runners/node-2/slot0",
        None,
    )
    .await
    .unwrap();
    assert_eq!(dropped.status(), StatusCode::OK);
    let dropped = response_json(dropped).await;
    assert_eq!(dropped["runner_id"], "node-2/slot0");
    assert_eq!(dropped["retired"], true);
    assert_eq!(registry.lock().unwrap().epoch("node-2/slot0"), None);
}

/// Admitting and dropping a runner lifetime fences whoever held it, so both are
/// written to the forge audit trail with the admin who did it and, for a
/// reissue, the epoch it replaced.
#[tokio::test]
async fn admitting_and_dropping_a_runner_are_audited_with_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    core.create_account(
        "ops-admin",
        "ops-admin-password",
        jeryu_core::UserRole::Admin,
    )
    .unwrap();
    let account = core.get_account("ops-admin").unwrap();
    let state = Arc::new(WebState::new(core));
    for _ in 0..2 {
        super::workcells::admit_runner(
            State(state.clone()),
            Some(axum::Extension(account.clone())),
            axum::body::Bytes::from(json!({ "runner_id": "r1" }).to_string()),
        )
        .await;
    }
    super::workcells::retire_runner(
        State(state.clone()),
        Some(axum::Extension(account)),
        AxumPath("r1".into()),
    )
    .await;
    let entries = state
        .core
        .list_audit(&super::workcells::runner_audit_subject("r1"))
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.action.as_str())
            .collect::<Vec<_>>(),
        [
            "workcell.runner.admit",
            "workcell.runner.admit",
            "workcell.runner.retire"
        ]
    );
    assert!(entries.iter().all(|entry| entry.actor == "ops-admin"));
    assert_eq!(entries[0].detail["replaced_epoch"], Value::Null);
    assert_eq!(
        entries[1].detail["replaced_epoch"], entries[0].detail["runner_epoch"],
        "a reissue records the lifetime it fenced"
    );
}

/// This host's own lifetime fences only its warm session pool. It never
/// enters the work-cell ledger's fence, so a claim through `/api/v1/workcells`
/// cannot borrow it, and its id can be neither admitted nor dropped.
#[tokio::test]
async fn the_host_lifetime_cannot_be_used_admitted_or_dropped_through_workcells() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let host_epoch = state.runner_epochs.lock().unwrap().host_epoch();
    let mut body = claim_body(host_epoch);
    body["runner_id"] = json!(super::runner_epochs::HOST_RUNNER_ID);
    let resp = super::workcells::claim(
        State(state.clone()),
        axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(response_json(resp).await["code"], "workcell_epoch_fenced");

    let admit = super::workcells::admit_runner(
        State(state.clone()),
        None,
        axum::body::Bytes::from(
            json!({ "runner_id": super::runner_epochs::HOST_RUNNER_ID }).to_string(),
        ),
    )
    .await;
    assert_eq!(admit.status(), StatusCode::BAD_REQUEST);
    for id in [
        super::runner_epochs::HOST_RUNNER_ID,
        "node-2//slot0",
        "..",
        // The wildcard's own leading `/` is taken off once; a second is an
        // empty segment.
        "//r1",
    ] {
        let drop =
            super::workcells::retire_runner(State(state.clone()), None, AxumPath(id.into())).await;
        assert_eq!(drop.status(), StatusCode::BAD_REQUEST, "{id:?}");
    }
    assert_eq!(
        state.runner_epochs.lock().unwrap().host_epoch(),
        host_epoch,
        "the host lifetime is untouched"
    );
}

/// An eviction fences the evicted runner, so it is audited under that
/// runner's own subject as well as under the newcomer's admission.
#[tokio::test]
async fn an_eviction_is_audited_under_the_evicted_runner() {
    use super::runner_epochs::{EVICTION_GRACE, MAX_ADMITTED_RUNNERS};

    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    let state = Arc::new(WebState::new(core));
    {
        let mut registry = state.runner_epochs.lock().unwrap();
        for slot in 0..MAX_ADMITTED_RUNNERS {
            registry.issue(&format!("r{slot}"), |_| false).unwrap();
        }
        registry.backdate_all(EVICTION_GRACE + std::time::Duration::from_secs(1));
    }
    let admitted = response_json(
        super::workcells::admit_runner(
            State(state.clone()),
            None,
            axum::body::Bytes::from(json!({ "runner_id": "newcomer" }).to_string()),
        )
        .await,
    )
    .await;
    let evicted = admitted["evicted_runner_id"]
        .as_str()
        .expect("a full registry of idle runners makes room")
        .to_string();
    let entries = state
        .core
        .list_audit(&super::workcells::runner_audit_subject(&evicted))
        .unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].action, "workcell.runner.evict");
    assert_eq!(entries[0].detail["evicted_for"], "newcomer");
}
