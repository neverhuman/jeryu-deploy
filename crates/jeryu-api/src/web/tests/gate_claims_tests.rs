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

/// A bare repository with no commits at all, so no branch of it resolves.
fn empty_bare_repo(storage_root: &std::path::Path, owner: &str, repo: &str) {
    let bare = storage_root.join(owner).join(format!("{repo}.git"));
    std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("create owner dir");
    git_in(
        storage_root,
        &[
            "init",
            "--quiet",
            "--bare",
            "-b",
            "main",
            bare.to_str().expect("bare utf8"),
        ],
    );
}

/// A forge with `veox/jain-deploy` (no capability declared), `veox/jain`
/// (`declaration`), `veox/jain-empty` (a repository whose default branch
/// resolves to nothing), `veox/jain-nobranch` (no default branch recorded)
/// and `veox/jain-ghost` (metadata with no git storage), a runner login that
/// may report, and an ordinary account that may not.
async fn fixture(declaration: &str) -> Fixture {
    let storage = tempfile::tempdir().expect("storage root");
    bare_repo_with_declaration(storage.path(), "veox", "jain-deploy", "");
    bare_repo_with_declaration(storage.path(), "veox", "jain", declaration);
    empty_bare_repo(storage.path(), "veox", "jain-empty");
    bare_repo_with_declaration(storage.path(), "veox", "jain-nobranch", declaration);
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
    for (name, default_branch) in [
        ("jain-deploy", "main"),
        ("jain", "main"),
        ("jain-empty", "main"),
        // Recorded as a branch this repository does not have, which is what
        // a renamed or never-pushed default branch looks like.
        ("jain-nobranch", "trunk"),
        ("jain-ghost", "main"),
    ] {
        core.create_repository(
            "veox",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: false,
                description: None,
                default_branch: Some(default_branch.to_string()),
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

/// The gate-claim routes are not replayed from the `Idempotency-Key` cache:
/// a keyed renewal really extends the lease, and a keyed ask after a release
/// is answered by the store rather than from the first answer.
#[tokio::test]
async fn a_key_never_replays_a_lease_answer() {
    let Fixture {
        router,
        runner,
        _storage,
        ..
    } = fixture("").await;
    let keyed = |path: &str, runner_id: &'static str, host: &'static str, key: &'static str| {
        let mut request = ask(
            &runner,
            path,
            claim_body("veox/jain-deploy", runner_id, host),
        );
        request.headers_mut().insert(
            crate::web::idempotency::IDEMPOTENCY_KEY,
            header::HeaderValue::from_static(key),
        );
        request
    };
    let send = |request| {
        let router = router.clone();
        async move { router.oneshot(request).await.unwrap() }
    };
    let first = send(keyed(
        "/api/v1/gate-claims",
        "gate-a/slot0",
        "gate-a",
        "claim-1",
    ))
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first = response_json(first).await;
    // The same ask with the same key: a renewal that reached the lease, not a
    // replayed answer.
    let renewed = send(keyed(
        "/api/v1/gate-claims",
        "gate-a/slot0",
        "gate-a",
        "claim-1",
    ))
    .await;
    assert_eq!(renewed.status(), StatusCode::OK);
    assert!(
        renewed
            .headers()
            .get(crate::web::idempotency::REPLAYED)
            .is_none(),
        "a lease answer is never replayed"
    );
    let renewed = response_json(renewed).await;
    assert_eq!(renewed["state"], "renewed");
    assert_eq!(renewed["granted_at"], first["granted_at"]);
    assert!(
        renewed["expires_at"].as_str().expect("expires_at")
            >= first["expires_at"].as_str().expect("expires_at"),
        "the renewal moves expires_at: {first} then {renewed}"
    );
    // A release carrying a key is not replayed either, so the ask after it is
    // answered by the store.
    let released = send(keyed(
        "/api/v1/gate-claims/release",
        "gate-a/slot0",
        "gate-a",
        "release-1",
    ))
    .await;
    assert_eq!(released.status(), StatusCode::OK);
    assert_eq!(response_json(released).await["state"], "released");
    // Another host takes the head, and the first host asking again with its
    // first key is told who holds it rather than "granted" from the cache.
    let next = send(keyed(
        "/api/v1/gate-claims",
        "gate-b/slot0",
        "gate-b",
        "claim-2",
    ))
    .await;
    assert_eq!(next.status(), StatusCode::CREATED);
    let refused = send(keyed(
        "/api/v1/gate-claims",
        "gate-a/slot0",
        "gate-a",
        "claim-1",
    ))
    .await;
    assert_eq!(
        refused.status(),
        StatusCode::CONFLICT,
        "a replayed grant would hand one head to two hosts"
    );
    let refused = response_json(refused).await;
    assert_eq!(refused["code"], "claim_held");
    assert_eq!(refused["holder"]["runner_id"], "gate-b/slot0");
}

/// Fail closed: a repository whose gate policy cannot be read at all is not
/// a repository that requires nothing. Every one of these would otherwise
/// have handed a head that needs the host-ci sandbox to a host without it.
#[tokio::test]
async fn a_policy_that_cannot_be_read_refuses_every_claim() {
    let Fixture {
        router,
        runner,
        _storage,
        ..
    } = fixture("[gate]\ncapabilities = [\"host-ci\"]\n").await;
    for (repo, reason) in [
        // A default branch that resolves to no commit.
        ("veox/jain-empty", "does not resolve to a commit"),
        // A default branch the repository does not have.
        ("veox/jain-nobranch", "does not resolve to a commit"),
        // Metadata with no git storage behind it.
        ("veox/jain-ghost", "could not be opened"),
    ] {
        let response = router
            .clone()
            .oneshot(ask(
                &runner,
                "/api/v1/gate-claims",
                claim_body(repo, "gate-a/slot0", "gate-a"),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{repo}"
        );
        let body = response_json(response).await;
        assert_eq!(body["code"], "invalid_repo_config", "{repo}");
        assert!(
            body["message"].as_str().expect("message").contains(reason),
            "{repo}: {body}"
        );
    }
}

/// The lease is on the commit: one sha asked for under two numbers is one
/// lease, so a caller that sends the wrong number cannot get a second host
/// gating the same commit.
#[tokio::test]
async fn one_sha_is_one_lease_whatever_number_is_asked() {
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
    let numbered = |number: u64, runner_id: &str, host: &str| {
        ask(
            &runner,
            "/api/v1/gate-claims",
            json!({"repo": "veox/jain-deploy", "number": number, "head_sha": HEAD,
                   "runner_id": runner_id, "host": host}),
        )
    };
    assert_eq!(
        send(numbered(7, "gate-a/slot0", "gate-a")).await.status(),
        StatusCode::CREATED
    );
    let refused = send(numbered(8, "gate-b/slot0", "gate-b")).await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let refused = response_json(refused).await;
    assert_eq!(refused["code"], "claim_held");
    assert_eq!(refused["holder"]["number"], 7);
    // The holder renews whatever number it sends.
    let renewed = send(numbered(9, "gate-a/slot0", "gate-a")).await;
    assert_eq!(renewed.status(), StatusCode::OK);
    assert_eq!(response_json(renewed).await["state"], "renewed");
    // And only its holder may release it, by any number.
    assert_eq!(
        send(ask(
            &runner,
            "/api/v1/gate-claims/release",
            json!({"repo": "veox/jain-deploy", "number": 8, "head_sha": HEAD,
                   "runner_id": "gate-b/slot0", "host": "gate-b"}),
        ))
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let released = send(ask(
        &runner,
        "/api/v1/gate-claims/release",
        json!({"repo": "veox/jain-deploy", "number": 8, "head_sha": HEAD,
               "runner_id": "gate-a/slot0", "host": "gate-a"}),
    ))
    .await;
    assert_eq!(released.status(), StatusCode::OK);
    assert_eq!(response_json(released).await["state"], "released");
}

/// A slot may only claim as the host it beats from, so it cannot borrow
/// another host's advertised capabilities.
#[tokio::test]
async fn an_ask_names_the_host_its_runner_beats_from() {
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
        let accepted = send(ask(
            &runner,
            "/api/v1/runners/heartbeat",
            beat(runner_id, host, capabilities),
        ))
        .await;
        assert_eq!(accepted.status(), StatusCode::OK);
    }
    let claimed_as_another_host = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/jain", "gate-a/slot0", "gate-b"),
    ))
    .await;
    assert_eq!(
        claimed_as_another_host.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let body = response_json(claimed_as_another_host).await;
    assert_eq!(body["code"], "host_mismatch");
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains("gate-a"),
        "{body}"
    );
    // A runner that has not beaten at all may claim a head that needs
    // nothing, but not one that needs a capability: the match is made on its
    // own live beat.
    let unknown = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/jain", "gate-c/slot0", "gate-b"),
    ))
    .await;
    assert_eq!(unknown.status(), StatusCode::CONFLICT);
    assert_eq!(
        response_json(unknown).await["code"],
        "capability_missing",
        "an unbeaten runner must not borrow its host's capabilities"
    );
    let plain = send(ask(
        &runner,
        "/api/v1/gate-claims",
        claim_body("veox/jain-deploy", "gate-c/slot0", "gate-c"),
    ))
    .await;
    assert_eq!(plain.status(), StatusCode::CREATED);
}
