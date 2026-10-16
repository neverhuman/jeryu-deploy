//! The links the forge returns for a repository and a pull request: which one
//! opens in a browser, which one answers JSON, and what the paths already in
//! circulation still do.
//!
//! An agent that builds a browser URL from parts gets it wrong — the shorter
//! `/repos/{owner}/{repo}/pulls/{n}` answers valid JSON, so an HTTP 200 is no
//! proof of a page. These tests pin the way out of that: every native repo and
//! pull request payload carries `html_url` (what to open or paste) and
//! `api_url` (what to fetch), the canonical browser path really is served by
//! the app, and nothing an existing client reads changes.
//!
//! The repositories, accounts and numbers below are invented.

use super::*;

use axum::body::Body;
use axum::http::{Method as HttpMethod, Request, header};
use tower::ServiceExt;

const OWNER: &str = "acme";
const NAME: &str = "widgets";
const PRIVATE_NAME: &str = "ledger";
const HEAD: &str = "9b1c2d3e4f5061728394a5b6c7d8e9f0a1b2c3d4";

struct Fixture {
    _dir: tempfile::TempDir,
    router: axum::Router,
    /// `acme-admin` administers both repositories, `mallory` reads neither
    /// private one.
    admin: String,
    mallory: String,
}

impl Fixture {
    /// `acme/widgets` (public, one open pull request) and `acme/ledger`
    /// (private), with an SPA shell on disk so a browser path can be proven
    /// to serve the app rather than an API answer.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let spa = dir.path().join("spa");
        std::fs::create_dir_all(&spa).expect("spa dir");
        std::fs::write(spa.join("index.html"), SHELL).expect("write the shell");
        let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).expect("open forge");
        core.create_account("acme-admin", "admin-password", UserRole::Admin)
            .expect("create admin");
        core.create_account("mallory", "mallory-password", UserRole::User)
            .expect("create stranger");
        for (name, private) in [(NAME, false), (PRIVATE_NAME, true)] {
            core.create_repository(
                OWNER,
                CreateRepositoryRequest {
                    name: name.to_string(),
                    private,
                    description: None,
                    default_branch: Some("main".to_string()),
                },
            )
            .expect("create repository");
            core.create_pull_request(
                OWNER,
                name,
                "acme-admin",
                CreatePullRequestRequest {
                    title: format!("change in {name}"),
                    head: "acme-admin/feature".to_string(),
                    base: "main".to_string(),
                    head_sha: Some(HEAD.to_string()),
                    ..Default::default()
                },
            )
            .expect("open a pull request");
        }
        let token = |login: &str| {
            core.create_personal_access_token(login, "test", None)
                .expect("mint a token")
                .secret
        };
        let (admin, mallory) = (token("acme-admin"), token("mallory"));
        let state = WebState::new_with_git_storage(core, dir.path().to_path_buf())
            .with_auth(true, false, false);
        Self {
            router: crate::web::app(state, &spa),
            _dir: dir,
            admin,
            mallory,
        }
    }

    async fn response(&self, uri: &str, token: Option<&str>, accept: &str) -> (StatusCode, String) {
        let mut request = Request::builder().method(HttpMethod::GET).uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = self
            .router
            .clone()
            .oneshot(
                request
                    .header(header::ACCEPT, accept)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("the router answers");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads");
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn get(&self, uri: &str, token: &str) -> (StatusCode, Value) {
        let (status, body) = self.response(uri, Some(token), "application/json").await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    /// A browser navigation: what a reader following a link actually sends.
    async fn navigate(&self, uri: &str) -> (StatusCode, String) {
        self.response(uri, None, "text/html,application/xhtml+xml")
            .await
    }
}

/// The shell the fixture serves, distinct enough to tell from any API body.
const SHELL: &str = "<!doctype html><title>jeryu app shell</title>";

#[tokio::test]
async fn a_native_pull_request_returns_the_canonical_browser_link_and_a_separate_api_link() {
    let fx = Fixture::new();
    let (status, detail) = fx
        .get(&format!("/api/v1/repos/{OWNER}/{NAME}/pulls/1"), &fx.admin)
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["html_url"], format!("/{OWNER}/{NAME}/pull/1"));
    assert_eq!(
        detail["api_url"],
        format!("/api/v1/repos/{OWNER}/{NAME}/pulls/1")
    );
    // The embedded summary carries the same two links, so a page that reads
    // either level shares the same URL.
    assert_eq!(detail["summary"]["html_url"], detail["html_url"]);
    assert_eq!(detail["summary"]["api_url"], detail["api_url"]);

    let (status, list) = fx
        .get(
            &format!("/api/v1/repos/{OWNER}/{NAME}/pulls?state=all"),
            &fx.admin,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let row = list["items"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["number"] == 1))
        .unwrap_or_else(|| panic!("no row for #1: {list}"));
    assert_eq!(
        row["html_url"], detail["html_url"],
        "the list row and the detail agree"
    );
    assert_eq!(row["api_url"], detail["api_url"]);
}

#[tokio::test]
async fn a_native_repository_returns_the_canonical_browser_link_and_a_separate_api_link() {
    let fx = Fixture::new();
    let (status, detail) = fx
        .get(&format!("/api/v1/repos/{OWNER}/{NAME}"), &fx.admin)
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["html_url"], format!("/{OWNER}/{NAME}"));
    assert_eq!(detail["api_url"], format!("/api/v1/repos/{OWNER}/{NAME}"));

    let (status, list) = fx.get("/api/v1/repos", &fx.admin).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let row = list["repositories"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["id"]["name"] == NAME && row["id"]["owner"] == OWNER)
        })
        .unwrap_or_else(|| panic!("no row for {OWNER}/{NAME}: {list}"));
    assert_eq!(row["html_url"], detail["html_url"]);
    assert_eq!(row["api_url"], detail["api_url"]);
}

/// The link an API client is handed opens the app, and the API path beside it
/// answers JSON. Mixing them up is the failure this pair rules out.
#[tokio::test]
async fn the_returned_browser_link_is_served_by_the_app_and_the_api_link_is_not() {
    let fx = Fixture::new();
    let (_, detail) = fx
        .get(&format!("/api/v1/repos/{OWNER}/{NAME}/pulls/1"), &fx.admin)
        .await;
    let html_url = detail["html_url"].as_str().expect("html_url").to_string();
    let api_url = detail["api_url"].as_str().expect("api_url").to_string();

    let (status, body) = fx.navigate(&html_url).await;
    assert_eq!(status, StatusCode::OK, "{html_url} serves the app");
    assert_eq!(body, SHELL, "{html_url} serves the app shell");

    // Tabs, a query string and a fragment are the app's own; the shell
    // answers for each of them too.
    for suffix in ["/files", "/checks", "/commits"] {
        let (status, body) = fx.navigate(&format!("{html_url}{suffix}")).await;
        assert_eq!(status, StatusCode::OK, "{html_url}{suffix}");
        assert_eq!(body, SHELL, "{html_url}{suffix}");
    }
    let (status, body) = fx
        .navigate(&format!("{html_url}/files?path=src/lib.rs"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, SHELL);

    // The repository's own canonical link, too.
    let (_, repo) = fx
        .get(&format!("/api/v1/repos/{OWNER}/{NAME}"), &fx.admin)
        .await;
    let (status, body) = fx
        .navigate(repo["html_url"].as_str().expect("html_url"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, SHELL);

    // The API link is JSON, not a page: an agent that pastes it into a
    // browser can see the difference.
    let (status, json) = fx.get(&api_url, &fx.admin).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["summary"]["number"], 1, "{json}");
}

/// Nothing above is a redirect: a canonical link is answered where it is
/// asked, so there is no hop to loop on.
#[tokio::test]
async fn canonical_links_answer_without_a_redirect() {
    let fx = Fixture::new();
    for uri in [
        format!("/{OWNER}/{NAME}"),
        format!("/{OWNER}/{NAME}/pull/1"),
        format!("/repos/jeryu/{OWNER}/{NAME}/pulls/1"),
    ] {
        let (status, _) = fx
            .response(&uri, None, "text/html,application/xhtml+xml")
            .await;
        assert!(
            !status.is_redirection(),
            "{uri} answered {status}, a redirect to follow"
        );
    }
}

/// The paths clients already hold keep their meaning: the compatibility edge
/// still answers its provider-qualified `html_url`, and the provider-qualified
/// browser path still serves the app.
#[tokio::test]
async fn existing_links_and_api_semantics_are_unchanged() {
    let fx = Fixture::new();
    let (status, edge) = fx
        .get(&format!("/api/v3/repos/{OWNER}/{NAME}/pulls/1"), &fx.admin)
        .await;
    assert_eq!(status, StatusCode::OK, "{edge}");
    assert_eq!(
        edge["html_url"],
        format!("/repos/jeryu/{OWNER}/{NAME}/pulls/1"),
        "the compatibility edge keeps the html_url its clients read"
    );

    // The same path a browser asks for serves the app, and the JSON client
    // asking for it still gets JSON.
    let (status, body) = fx
        .navigate(&format!("/repos/jeryu/{OWNER}/{NAME}/pulls/1"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, SHELL);
    let (status, json) = fx
        .get(&format!("/repos/{OWNER}/{NAME}/pulls/1"), &fx.admin)
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["number"], 1, "the REST route still answers JSON");
}

/// A reserved top-level path is the forge's own, never an `owner/repo`.
#[tokio::test]
async fn reserved_top_level_routes_are_not_taken_for_a_repository() {
    let fx = Fixture::new();
    let (status, manifest) = fx.get(crate::discovery::CAPABILITIES_PATH, &fx.admin).await;
    assert_eq!(status, StatusCode::OK, "{manifest}");
    assert_eq!(manifest["server"], "jeryu");
    let (status, repos) = fx.get("/api/v1/repos", &fx.admin).await;
    assert_eq!(status, StatusCode::OK, "{repos}");
    // `/api/...` is never the web app, whatever an owner is called.
    let (status, body) = fx
        .response(
            "/api/v1/no-such-route",
            Some(&fx.admin),
            "text/html,application/xhtml+xml",
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_ne!(body, SHELL);
}

/// The links are a field of a payload, so who may read them is who may read
/// the object: a private repository's links are not handed to a stranger.
#[tokio::test]
async fn a_stranger_gets_no_links_to_a_private_repository() {
    let fx = Fixture::new();
    let (status, body) = fx
        .get(
            &format!("/api/v1/repos/{OWNER}/{PRIVATE_NAME}"),
            &fx.mallory,
        )
        .await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
        "a stranger is refused, not served: {status} {body}"
    );
    assert!(body.get("html_url").is_none(), "{body}");

    let (status, list) = fx.get("/api/v1/repos", &fx.mallory).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert!(
        !list.to_string().contains(PRIVATE_NAME),
        "a private repository is absent from a stranger's listing: {list}"
    );

    // An owner who may read it does get them.
    let (status, mine) = fx
        .get(&format!("/api/v1/repos/{OWNER}/{PRIVATE_NAME}"), &fx.admin)
        .await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    assert_eq!(mine["html_url"], format!("/{OWNER}/{PRIVATE_NAME}"));
}

/// A repository nobody hosts has no links to hand out, and says so as JSON.
#[tokio::test]
async fn an_unknown_repository_answers_a_typed_not_found() {
    let fx = Fixture::new();
    let (status, body) = fx
        .get("/api/v1/repos/nobody/nothing/pulls/1", &fx.admin)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.get("html_url").is_none(), "{body}");
}

/// The manifest explains the two kinds of link and gives a discovery path an
/// agent can follow, at both capability paths.
#[tokio::test]
async fn the_capability_manifest_explains_browser_links_and_api_links() {
    let fx = Fixture::new();
    for path in [
        crate::discovery::CAPABILITIES_PATH,
        crate::discovery::FIRST_CAPABILITIES_PATH,
    ] {
        let (status, manifest) = fx.get(path, &fx.admin).await;
        assert_eq!(status, StatusCode::OK, "{path}: {manifest}");
        let links = &manifest["links"];
        assert_eq!(
            links["html_url"]["pull_request"], "/{owner}/{repo}/pull/{number}",
            "{path}: {links}"
        );
        assert_eq!(links["html_url"]["repository"], "/{owner}/{repo}");
        assert_eq!(
            links["api_url"]["pull_request"],
            "/api/v1/repos/{owner}/{repo}/pulls/{number}"
        );
        let steps = links["discovery_example"]
            .as_array()
            .unwrap_or_else(|| panic!("{path}: no discovery example in {links}"));
        assert!(
            steps
                .first()
                .and_then(Value::as_str)
                .is_some_and(|step| step.contains(crate::discovery::CAPABILITIES_PATH)),
            "{path}: the example starts at the manifest: {links}"
        );
        assert!(
            links["warning"]
                .as_str()
                .is_some_and(|warning| warning.contains("200")),
            "{path}: the manifest warns that a 200 is not a browser link: {links}"
        );
    }
}

/// The control-plane snapshot MCP clients read carries the canonical browser
/// link for each pull request, beside the in-app path its own pages use.
#[tokio::test]
async fn the_control_plane_snapshot_carries_the_canonical_pull_request_link() {
    let fx = Fixture::new();
    let (status, snapshot) = fx.get("/api/v1/control-plane/status", &fx.admin).await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    let pulls = snapshot["pullRequests"]
        .as_array()
        .unwrap_or_else(|| panic!("no pull requests in {snapshot}"));
    let row = pulls
        .iter()
        .find(|row| row["repo"] == format!("{OWNER}/{NAME}") && row["number"] == 1)
        .unwrap_or_else(|| panic!("no row for {OWNER}/{NAME}#1: {snapshot}"));
    assert_eq!(row["htmlUrl"], format!("/{OWNER}/{NAME}/pull/1"), "{row}");
    assert_eq!(
        row["sourceLinks"][0]["url"],
        format!("/repos/jeryu/{OWNER}/{NAME}/pulls/1"),
        "the in-app path its own pages use is unchanged: {row}"
    );
}
