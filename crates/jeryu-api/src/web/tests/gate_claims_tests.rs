//! `POST /api/v1/gate-claims`: the forge-held lease on a gated head, and the
//! repository capability policy it is matched against.
use super::*;
use tower::ServiceExt;

const HEAD: &str = "abcdef1234567890abcdef1234567890abcdef12";

/// A bare repository whose `main` holds `.jeryu/repo.toml` with `body` (or
/// nothing when it is empty).
fn bare_repo_with_declaration(storage_root: &std::path::Path, owner: &str, repo: &str, body: &str) {
    let bare = storage_root.join(owner).join(format!("{repo}.git"));
    std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("create owner dir");
    let work = storage_root.join(format!("{owner}-{repo}-work"));
    std::fs::create_dir_all(&work).expect("create work dir");
    git_in(&work, &["init", "--quiet", "-b", "main"]);
    if body.is_empty() {
        std::fs::write(work.join("README.md"), "fixture\n").expect("write readme");
        git_in(&work, &["add", "README.md"]);
    } else {
        std::fs::create_dir_all(work.join(".jeryu")).expect("create .jeryu");
        std::fs::write(work.join(".jeryu/repo.toml"), body).expect("write repo.toml");
        git_in(&work, &["add", ".jeryu/repo.toml"]);
    }
    git_in(&work, &["commit", "--quiet", "-m", "declare gate"]);
    git_in(
        &work,
        &[
            "clone",
            "--quiet",
            "--bare",
            ".",
            bare.to_str().expect("bare utf8"),
        ],
    );
    git_in(bare.as_path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
    assert!(bare.join("HEAD").is_file(), "bare repository at {bare:?}");
}

struct Fixture {
    router: AxumRouter,
    runner: String,
    outsider: String,
    _storage: tempfile::TempDir,
}

/// A forge with `veox/jain-deploy` (no capability declared) and
/// `veox/jain` (host-ci), a runner login that may report, and an ordinary
/// account that may not.
async fn fixture(declaration: &str) -> Fixture {
    let storage = tempfile::tempdir().expect("storage root");
    bare_repo_with_declaration(storage.path(), "veox", "jain-deploy", "");
    bare_repo_with_declaration(storage.path(), "veox", "jain", declaration);
    let core = ForgeCore::new();
    for login in ["ci-bot", "outsider"] {
        core.create_account(login, "fixture-password", UserRole::User)
            .expect("create account");
    }
    let token = |login: &str| {
        core.create_personal_access_token(login, "fixture", None)
            .expect("token")
            .secret
    };
    let runner = token("ci-bot");
    let outsider = token("outsider");
    for name in ["jain-deploy", "jain"] {
        core.create_repository(
            "veox",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .expect("create repository");
    }
    let state = WebState::new_with_git_storage(core, storage.path().to_path_buf())
        .with_auth(true, false, false)
        .with_reporters(crate::web::TEST_REPORTERS, "shift-bot");
    Fixture {
        router: app(state, Path::new("/nonexistent-spa")),
        runner,
        outsider,
        _storage: storage,
    }
}

fn ask(token: &str, path: &str, body: Value) -> Request<axum::body::Body> {
    Request::builder()
        .method(HttpMethod::POST)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

fn claim_body(repo: &str, runner_id: &str, host: &str) -> Value {
    json!({"repo": repo, "number": 7, "head_sha": HEAD, "runner_id": runner_id, "host": host})
}

fn beat(runner_id: &str, host: &str, capabilities: Value) -> Value {
    json!({
        "runnerId": runner_id,
        "host": host,
        "slot": 0,
        "labels": ["pr-gate"],
        "capabilities": capabilities,
    })
}

/// Only one host is ever handed a head, the holder renews its own lease, and
/// the lease is given back on publication so the next host does not wait out
/// the TTL.
#[tokio::test]
async fn one_head_is_claimed_by_one_host_and_released_on_publication() {
    let Fixture {
        router,
        runner,
        _storage,
        ..
    } = fixture("").await;
    let send = |request| {
        let router = router.clone();
        async move { router.oneshot(request).await.unwrap() }
    };
    let first = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/jain-deploy", "gate-a/slot0", "gate-a"),
    ))
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let granted = response_json(first).await;
    assert_eq!(granted["state"], "granted");
    assert_eq!(granted["head"], HEAD);
    assert_eq!(granted["required_capabilities"], json!([]));
    // A second host asking for the same head is refused, and told who holds it.
    let refused = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/jain-deploy", "gate-b/slot0", "gate-b"),
    ))
    .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let body = response_json(refused).await;
    assert_eq!(body["code"], "claim_held");
    assert_eq!(body["holder"]["runner_id"], "gate-a/slot0");
    // The holder asking again renews the same lease.
    let renewed = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/jain-deploy", "gate-a/slot0", "gate-a"),
    ))
    .await;
    assert_eq!(renewed.status(), StatusCode::OK);
    let renewed = response_json(renewed).await;
    assert_eq!(renewed["state"], "renewed");
    assert_eq!(renewed["granted_at"], granted["granted_at"]);
    // Another host may not release the lease it does not hold.
    let stolen = send(ask(
        &runner,
        "/api/v1/gate-claims/release",
        claim_body("veox/jain-deploy", "gate-b/slot0", "gate-b"),
    ))
    .await;
    assert_eq!(stolen.status(), StatusCode::CONFLICT);
    // The holder releases, twice: a republishing retry is not an error.
    for expected in ["released", "absent"] {
        let released = send(ask(
            &runner,
            "/api/v1/gate-claims/release",
            claim_body("veox/jain-deploy", "gate-a/slot0", "gate-a"),
        ))
        .await;
        assert_eq!(released.status(), StatusCode::OK);
        assert_eq!(response_json(released).await["state"], expected);
    }
    // And the head is claimable at once.
    let next = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/jain-deploy", "gate-b/slot0", "gate-b"),
    ))
    .await;
    assert_eq!(next.status(), StatusCode::CREATED);
}

/// A repository that declares `host-ci` is claimable only by a host whose
/// heartbeat advertises it. The policy is read at the base branch, so the
/// claim of a head whose own commit says otherwise is judged by `main`.
#[tokio::test]
async fn a_head_that_needs_a_capability_is_refused_to_a_host_without_it() {
    let Fixture {
        router,
        runner,
        _storage,
        ..
    } = fixture("[gate]\ncapabilities = [\"host-ci\"]\n").await;
    let send = |request| {
        let router = router.clone();
        async move { router.oneshot(request).await.unwrap() }
    };
    for (runner_id, host, capabilities) in [
        ("gate-a/slot0", "gate-a", json!([])),
        ("gate-b/slot0", "gate-b", json!(["host-ci"])),
    ] {
        let beat = send(ask(
            &runner,
            "/api/v1/runners/heartbeat",
            beat(runner_id, host, capabilities),
        ))
        .await;
        assert_eq!(beat.status(), StatusCode::OK);
    }
    let refused = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/jain", "gate-a/slot0", "gate-a"),
    ))
    .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let body = response_json(refused).await;
    assert_eq!(body["code"], "capability_missing");
    assert_eq!(body["missing_capabilities"], json!(["host-ci"]));
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains(".jeryu/repo.toml"),
        "{body}"
    );
    // The capable host takes it, and the lease records what it had to have.
    let granted = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/jain", "gate-b/slot0", "gate-b"),
    ))
    .await;
    assert_eq!(granted.status(), StatusCode::CREATED);
    assert_eq!(
        response_json(granted).await["required_capabilities"],
        json!(["host-ci"])
    );
    // A repository that declares nothing is claimable by either host.
    let plain = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/jain-deploy", "gate-a/slot0", "gate-a"),
    ))
    .await;
    assert_eq!(plain.status(), StatusCode::CREATED);
}

/// A declaration nobody can read grants nothing: a typo must not turn a
/// capability requirement into "no requirement".
#[tokio::test]
async fn an_unreadable_capability_policy_refuses_every_claim() {
    for (declaration, reason) in [
        (
            "[gate]\ncapabilities = [\"host_ci\"]\n",
            "unknown capability",
        ),
        (
            "[gate]\ncapabilities = [\"host-ci\", \"host-ci\"]\n",
            "duplicate capability",
        ),
        ("[gate]\ncapabilities = \"host-ci\"\n", "must be an array"),
    ] {
        let Fixture {
            router,
            runner,
            _storage,
            ..
        } = fixture(declaration).await;
        let response = router
            .clone()
            .oneshot(ask(
                &runner,
                "/api/v1/gate-claims",
                claim_body("veox/jain", "gate-a/slot0", "gate-a"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response_json(response).await;
        assert_eq!(body["code"], "invalid_repo_config");
        assert!(
            body["message"].as_str().expect("message").contains(reason),
            "{declaration}: {body}"
        );
    }
}

/// An account that may not report heartbeats may not park a lease on a head
/// either, and an ask that names no head of a hosted repository is refused.
#[tokio::test]
async fn claiming_needs_a_runner_identity_and_a_well_formed_head() {
    let Fixture {
        router,
        runner,
        outsider,
        _storage,
    } = fixture("").await;
    let send = |request| {
        let router = router.clone();
        async move { router.oneshot(request).await.unwrap() }
    };
    for path in ["/api/v1/gate-claims", "/api/v1/gate-claims/release"] {
        let response = send(ask(
            &outsider,
            path,
            claim_body("veox/jain-deploy", "gate-a/slot0", "gate-a"),
        ))
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        assert_eq!(
            response_json(response).await["code"],
            "permission_denied",
            "{path}"
        );
    }
    let anonymous = Request::builder()
        .method(HttpMethod::POST)
        .uri("/api/v1/gate-claims")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            claim_body("veox/jain-deploy", "gate-a/slot0", "gate-a").to_string(),
        ))
        .unwrap();
    assert_eq!(
        send(anonymous).await.status(),
        StatusCode::UNAUTHORIZED,
        "a gate claim is never anonymous"
    );
    // Nothing was claimed by any of that: the head is still free.
    let granted = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/jain-deploy", "gate-a/slot0", "gate-a"),
    ))
    .await;
    assert_eq!(granted.status(), StatusCode::CREATED);
    let unknown = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/absent", "gate-a/slot0", "gate-a"),
    ))
    .await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    for body in [
        json!({"repo": "veox/jain-deploy", "number": 7, "head_sha": "main",
               "runner_id": "gate-a/slot0", "host": "gate-a"}),
        json!({"repo": "jain-deploy", "number": 7, "head_sha": HEAD,
               "runner_id": "gate-a/slot0", "host": "gate-a"}),
        json!({"repo": "veox/jain-deploy", "number": 7, "head_sha": HEAD, "queue_sha": HEAD,
               "runner_id": "gate-a/slot0", "host": "gate-a"}),
    ] {
        let response = send(ask(&runner, "/api/v1/gate-claims", body.clone())).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}"
        );
        assert_eq!(response_json(response).await["code"], "invalid_input");
    }
}

/// A retry that carries the same `Idempotency-Key` replays the first answer
/// instead of granting a second time, so a runner that lost the answer to a
/// timeout can ask again safely.
#[tokio::test]
async fn a_repeated_ask_with_a_key_replays_its_first_answer() {
    let Fixture {
        router,
        runner,
        _storage,
        ..
    } = fixture("").await;
    let keyed = || {
        let mut request = ask(
            &runner,
            "/api/v1/gate-claims",
            claim_body("veox/jain-deploy", "gate-a/slot0", "gate-a"),
        );
        request.headers_mut().insert(
            crate::web::idempotency::IDEMPOTENCY_KEY,
            header::HeaderValue::from_static("claim-retry-1"),
        );
        request
    };
    let first = router.clone().oneshot(keyed()).await.unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    let first = response_json(first).await;
    let replay = router.clone().oneshot(keyed()).await.unwrap();
    assert_eq!(replay.status(), StatusCode::CREATED);
    assert_eq!(
        replay
            .headers()
            .get(crate::web::idempotency::REPLAYED)
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    let replay = response_json(replay).await;
    assert_eq!(replay["renewed_at"], first["renewed_at"]);
    assert_eq!(replay["expires_at"], first["expires_at"]);
}
