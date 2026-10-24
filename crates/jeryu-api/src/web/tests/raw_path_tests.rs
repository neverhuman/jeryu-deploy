//! Path-form raw URLs, `/api/v1/repos/{id}/raw/{ref}/{path}`, through the
//! whole router: auth, ref splitting, traversal, and the inert-content
//! headers both raw forms carry.

use super::*;
use crate::web::repositories::RAW_CONTENT_SECURITY_POLICY as RAW_CSP;
use crate::web::shift::tests::run_git;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

struct Forge {
    router: axum::Router,
    open: String,
    closed: String,
    reader: String,
    outsider: String,
    _dir: tempfile::TempDir,
}

/// Push one history into `alice/<name>.git`: `main` holds a small site and a
/// `v1.2/notes/r.txt`; branch `release/v1.2` holds its own `notes/r.txt`; tag
/// `release` points at `main`, so `release/v1.2/notes/r.txt` is ambiguous
/// and the longest ref has to win.
fn seed(storage: &Path, name: &str) {
    let work = storage.join(format!("{name}-work"));
    std::fs::create_dir_all(work.join("site/nested/deep")).unwrap();
    std::fs::create_dir_all(work.join("v1.2/notes")).unwrap();
    run_git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(
        work.join("site/index.html"),
        "<!doctype html><img src=\"img%20one.png\"><script src=\"app.js\"></script>",
    )
    .unwrap();
    std::fs::write(work.join("site/img one.png"), b"\x89PNG\r\n\x1a\nfake").unwrap();
    std::fs::write(work.join("site/app.js"), "alert(1)").unwrap();
    std::fs::write(work.join("site/nested/deep/style.css"), "p{}").unwrap();
    std::fs::write(work.join("v1.2/notes/r.txt"), "main copy").unwrap();
    run_git(&work, &["add", "."]);
    run_git(&work, &["commit", "-q", "-m", "base"]);
    run_git(&work, &["tag", "release"]);
    run_git(&work, &["checkout", "-q", "-b", "release/v1.2"]);
    std::fs::create_dir_all(work.join("notes")).unwrap();
    std::fs::write(work.join("notes/r.txt"), "release copy").unwrap();
    run_git(&work, &["add", "."]);
    run_git(&work, &["commit", "-q", "-m", "release"]);
    let owner = storage.join("alice");
    std::fs::create_dir_all(&owner).unwrap();
    let bare = format!("{name}.git");
    run_git(&owner, &["init", "-q", "--bare", &bare]);
    run_git(
        &work,
        &[
            "push",
            "-q",
            owner.join(&bare).to_str().unwrap(),
            "main",
            "release/v1.2",
            "refs/tags/release",
        ],
    );
}

fn forge() -> Forge {
    let dir = tempdir().unwrap();
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    core.create_account("carol", "carol-password", UserRole::User)
        .unwrap();
    let mut ids = Vec::new();
    for (name, private) in [("open", false), ("closed", true)] {
        seed(dir.path(), name);
        let repo = core
            .create_repository(
                "alice",
                CreateRepositoryRequest {
                    name: name.to_string(),
                    private,
                    description: None,
                    default_branch: Some("main".to_string()),
                },
            )
            .unwrap();
        ids.push(repo.id.to_string());
    }
    core.grant_repo_access("alice", "bob", "alice", "closed", RepoAccessLevel::Read)
        .unwrap();
    let reader = core
        .create_personal_access_token("bob", "t", None)
        .unwrap()
        .secret;
    let outsider = core
        .create_personal_access_token("carol", "t", None)
        .unwrap()
        .secret;
    let state = WebState::new_with_git_storage(core, dir.path().to_path_buf())
        .with_auth(true, false, false);
    let router = app(state, Path::new("/nonexistent-jeryu-spa"));
    let closed = ids.pop().unwrap();
    let open = ids.pop().unwrap();
    Forge {
        router,
        open,
        closed,
        reader,
        outsider,
        _dir: dir,
    }
}

async fn get(router: &axum::Router, uri: &str, token: Option<&str>) -> AxumResponse {
    let mut request = Request::builder().uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body_text(response: AxumResponse) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).to_string()
}

fn header_of(response: &AxumResponse, name: header::HeaderName) -> &str {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

fn assert_inert(response: &AxumResponse, uri: &str) {
    let csp = header_of(response, header::CONTENT_SECURITY_POLICY);
    assert_eq!(csp, RAW_CSP, "{uri}");
    assert!(csp.contains("default-src 'none'"), "{uri}");
    assert!(csp.contains("sandbox allow-same-origin"), "{uri}");
    assert!(!csp.contains("allow-scripts"), "{uri}");
    assert!(!csp.contains("script-src"), "{uri}");
    assert!(csp.contains("form-action 'none'"), "{uri}");
    assert_eq!(
        header_of(response, header::X_CONTENT_TYPE_OPTIONS),
        "nosniff",
        "{uri}"
    );
}

#[tokio::test]
async fn path_form_serves_nested_files_and_resolves_siblings() {
    let forge = forge();
    let base = format!("/api/v1/repos/{}/raw/main/site/", forge.open);
    let page = get(&forge.router, &format!("{base}index.html"), None).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(
        header_of(&page, header::CONTENT_TYPE),
        "text/html; charset=utf-8"
    );
    assert_inert(&page, "index.html");
    assert_eq!(
        header_of(&page, header::CONTENT_DISPOSITION),
        "inline; filename=\"index.html\"; filename*=UTF-8''index.html"
    );
    assert!(body_text(page).await.contains("img%20one.png"));

    // The page's relative `img%20one.png` resolves against `{base}`: a
    // URL-encoded name decodes to the file with a space in it.
    let image = get(&forge.router, &format!("{base}img%20one.png"), None).await;
    assert_eq!(image.status(), StatusCode::OK);
    assert_eq!(header_of(&image, header::CONTENT_TYPE), "image/png");
    assert_inert(&image, "img one.png");

    let nested = get(&forge.router, &format!("{base}nested/deep/style.css"), None).await;
    assert_eq!(nested.status(), StatusCode::OK);
    assert_eq!(
        header_of(&nested, header::CONTENT_TYPE),
        "text/css; charset=utf-8"
    );
    assert_eq!(body_text(nested).await, "p{}");
}

/// A repository's `.js` must never run on the forge origin: it is plain text
/// under `nosniff` (a `<script src>` refuses it), and the page that names it
/// has no script source at all.
#[tokio::test]
async fn script_files_are_served_non_executable_in_both_forms() {
    let forge = forge();
    for uri in [
        format!("/api/v1/repos/{}/raw/main/site/app.js", forge.open),
        format!(
            "/api/v1/repos/{}/raw?ref=main&path=site%2Fapp.js",
            forge.open
        ),
    ] {
        let script = get(&forge.router, &uri, None).await;
        assert_eq!(script.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            header_of(&script, header::CONTENT_TYPE),
            "text/plain; charset=utf-8",
            "{uri}"
        );
        assert_inert(&script, &uri);
    }
    let query_page = get(
        &forge.router,
        &format!(
            "/api/v1/repos/{}/raw?ref=main&path=site%2Findex.html",
            forge.open
        ),
        None,
    )
    .await;
    assert_eq!(query_page.status(), StatusCode::OK);
    assert_inert(&query_page, "query index.html");
}

#[tokio::test]
async fn a_ref_with_slashes_resolves_longest_first_or_percent_encoded() {
    let forge = forge();
    for uri in [
        format!("/api/v1/repos/{}/raw/release/v1.2/notes/r.txt", forge.open),
        format!(
            "/api/v1/repos/{}/raw/release%2Fv1.2/notes/r.txt",
            forge.open
        ),
    ] {
        let response = get(&forge.router, &uri, None).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert_eq!(body_text(response).await, "release copy", "{uri}");
    }
    // The shorter tag `release` names `main`, where the URL above would be
    // `v1.2/notes/r.txt` ("main copy"): the longer ref won. The tag is still
    // reachable where no longer ref matches, and a commit id pins `main`.
    let tag = get(
        &forge.router,
        &format!(
            "/api/v1/repos/{}/raw/release/site/nested/deep/style.css",
            forge.open
        ),
        None,
    )
    .await;
    assert_eq!(tag.status(), StatusCode::OK);
    assert_eq!(body_text(tag).await, "p{}");
    let sha = bare_ref(forge._dir.path(), "alice", "open", "refs/heads/main");
    let pinned = get(
        &forge.router,
        &format!("/api/v1/repos/{}/raw/{sha}/v1.2/notes/r.txt", forge.open),
        None,
    )
    .await;
    assert_eq!(pinned.status(), StatusCode::OK);
    assert_eq!(body_text(pinned).await, "main copy");
}

#[tokio::test]
async fn traversal_revision_syntax_and_misses_are_refused() {
    let forge = forge();
    let base = format!("/api/v1/repos/{}/raw", forge.open);
    for rest in [
        "main/site/../v1.2/notes/r.txt",
        "main/site/%2E%2E/v1.2/notes/r.txt",
        "main/./site/index.html",
        "main//site/index.html",
        "main",
    ] {
        let response = get(&forge.router, &format!("{base}/{rest}"), None).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{rest}"
        );
        assert_inert(&response, rest);
    }
    for rest in [
        "main/site/missing.png",
        "no-such-branch/site/index.html",
        "main~1/site/index.html",
        "main:site/index.html",
        "main/site/nested",
    ] {
        let response = get(&forge.router, &format!("{base}/{rest}"), None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{rest}");
    }
}

/// The path form is under the same per-repository gate as every other read:
/// anonymous is refused a private repository, another user without a grant
/// is forbidden, a reader with a grant gets the bytes.
#[tokio::test]
async fn private_repository_needs_a_credential_and_a_grant() {
    let forge = forge();
    let uri = format!("/api/v1/repos/{}/raw/main/site/index.html", forge.closed);
    let anonymous = get(&forge.router, &uri, None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let other_user = get(&forge.router, &uri, Some(&forge.outsider)).await;
    assert_eq!(other_user.status(), StatusCode::FORBIDDEN);
    let reader = get(&forge.router, &uri, Some(&forge.reader)).await;
    assert_eq!(reader.status(), StatusCode::OK);
    assert_inert(&reader, &uri);
}
