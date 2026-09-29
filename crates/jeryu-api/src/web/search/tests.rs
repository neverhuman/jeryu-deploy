//! `/api/v1/search` through the real authentication middleware: what each
//! reader finds, what ranks first, and what the endpoint refuses.

use super::*;
use crate::web::app;

use axum::body::Body;
use axum::http::{Method as HttpMethod, Request, header};
use jeryu_core::{
    CreateIssueRequest, CreatePullRequestRequest, CreateRepositoryRequest, ForgeCore,
    RepoAccessLevel, UserRole,
};
use serde_json::Value;
use std::collections::BTreeMap;
use tempfile::{TempDir, tempdir};
use tower::ServiceExt;

/// A forge with one public and one private repository, each carrying an issue
/// and a pull request, and accounts that can and cannot read the private one.
struct Fixture {
    _root: TempDir,
    state: WebState,
    app: axum::Router,
    tokens: BTreeMap<String, String>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempdir().expect("search fixture root");
        let core = ForgeCore::new();
        let state = WebState::new_with_git_storage(core, root.path().join("git"))
            .with_auth(true, false, false);
        let mut tokens = BTreeMap::new();
        for (login, role) in [
            ("admin", UserRole::Admin),
            ("reader", UserRole::User),
            ("outsider", UserRole::User),
        ] {
            state
                .core
                .create_account(login, "search-fixture-password", role)
                .unwrap();
            tokens.insert(
                login.to_string(),
                state
                    .core
                    .create_personal_access_token(login, "search fixture", None)
                    .unwrap()
                    .secret,
            );
        }
        for (name, private, description) in [
            ("gatekeeper", false, "Merge gate service."),
            ("vault", true, "Secrets for the gatekeeper."),
        ] {
            state
                .core
                .create_repository(
                    "neverhuman",
                    CreateRepositoryRequest {
                        name: name.into(),
                        private,
                        description: Some(description.into()),
                        default_branch: Some("main".into()),
                    },
                )
                .unwrap();
            state
                .core
                .create_issue(
                    "neverhuman",
                    name,
                    "admin",
                    CreateIssueRequest {
                        title: format!("{name} loses the gate receipt"),
                        body: Some("The receipt is written before the gate finishes.".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            state
                .core
                .create_pull_request(
                    "neverhuman",
                    name,
                    "admin",
                    CreatePullRequestRequest {
                        title: format!("Write the {name} receipt after the gate"),
                        body: Some("Moves the write to the end of the run.".into()),
                        head: "receipt-order".into(),
                        base: "main".into(),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        state
            .core
            .grant_repo_access(
                "admin",
                "reader",
                "neverhuman",
                "vault",
                RepoAccessLevel::Read,
            )
            .unwrap();
        let app = app(state.clone(), &root.path().join("absent-spa"));
        Self {
            _root: root,
            state,
            app,
            tokens,
        }
    }

    async fn get(&self, uri: &str, actor: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder().method(HttpMethod::GET).uri(uri);
        if let Some(actor) = actor {
            request = request.header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.tokens[actor]),
            );
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

fn titles(answer: &Value, kind: &str) -> Vec<String> {
    answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|hit| hit["kind"] == kind)
        .map(|hit| hit["title"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_search_finds_every_kind_the_reader_may_read() {
    let f = Fixture::new();
    let (status, answer) = f.get("/api/v1/search?q=gatekeeper", Some("admin")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["query"], "gatekeeper");
    // An admin searches every kind, activity included.
    assert_eq!(
        answer["kinds"],
        serde_json::json!(["repository", "pull_request", "issue", "todo", "activity"])
    );
    // The repository named `gatekeeper` first, then the one that merely
    // mentions it in its description.
    assert_eq!(
        titles(&answer, "repository"),
        ["neverhuman/gatekeeper", "neverhuman/vault"]
    );
    assert_eq!(
        titles(&answer, "issue"),
        ["#1 gatekeeper loses the gate receipt"]
    );
    assert_eq!(
        titles(&answer, "pull_request"),
        ["#2 Write the gatekeeper receipt after the gate"]
    );
    let repository = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["kind"] == "repository")
        .unwrap();
    assert_eq!(repository["path"], "/repos/jeryu/neverhuman/gatekeeper");
    assert_eq!(repository["repo"]["name"], "gatekeeper");
    let pull = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["kind"] == "pull_request")
        .unwrap();
    assert_eq!(pull["path"], "/repos/jeryu/neverhuman/gatekeeper/pulls/2");
}

#[tokio::test]
async fn results_are_scoped_to_what_the_reader_may_read() {
    let f = Fixture::new();
    for (actor, private_repos) in [("admin", 1), ("reader", 1), ("outsider", 0)] {
        let (status, answer) = f.get("/api/v1/search?q=vault", Some(actor)).await;
        assert_eq!(status, StatusCode::OK, "{actor}: {answer}");
        assert_eq!(
            titles(&answer, "repository").len(),
            private_repos,
            "{actor} saw the wrong private repositories: {answer}"
        );
        // The private repository's issue and pull request follow the same rule.
        assert_eq!(titles(&answer, "issue").len(), private_repos, "{actor}");
        assert_eq!(
            titles(&answer, "pull_request").len(),
            private_repos,
            "{actor}"
        );
    }
    // A description mentioning the private repository is still a public hit.
    let (_, answer) = f.get("/api/v1/search?q=gatekeeper", Some("outsider")).await;
    assert_eq!(titles(&answer, "repository"), ["neverhuman/gatekeeper"]);
}

#[tokio::test]
async fn a_name_outranks_a_body_and_counts_survive_the_limit() {
    let f = Fixture::new();
    // "receipt" is in both issue titles and in every pull request body.
    let (_, answer) = f.get("/api/v1/search?q=receipt", Some("admin")).await;
    let kinds: Vec<&str> = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|hit| hit["kind"] == "pull_request")
        .map(|hit| hit["snippet"].as_str().unwrap_or(""))
        .collect();
    // Both pulls match on the title ("receipt" is a word of it), so no snippet.
    assert_eq!(kinds, ["", ""]);

    // A body-only match carries the line that matched.
    let (_, answer) = f.get("/api/v1/search?q=finishes", Some("admin")).await;
    let issue = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["kind"] == "issue")
        .expect("body match");
    assert_eq!(
        issue["snippet"],
        "The receipt is written before the gate finishes."
    );

    // `limit` cuts the list but `counts` keeps the true total.
    let (_, answer) = f
        .get("/api/v1/search?q=receipt&kind=issue&limit=1", Some("admin"))
        .await;
    assert_eq!(answer["counts"]["issue"], 2);
    assert_eq!(titles(&answer, "issue").len(), 1);
}

#[tokio::test]
async fn a_number_reference_finds_the_pull_request_it_names() {
    let f = Fixture::new();
    let (_, answer) = f
        .get("/api/v1/search?q=gatekeeper%232", Some("admin"))
        .await;
    assert_eq!(
        titles(&answer, "pull_request"),
        ["#2 Write the gatekeeper receipt after the gate"]
    );
    // `name#n` names one repository: the private repo's #2 is not offered.
    assert_eq!(answer["counts"]["pull_request"], 1);
}

#[tokio::test]
async fn activity_is_admin_only_and_named_in_the_searched_kinds() {
    let f = Fixture::new();
    let (status, answer) = f
        .get("/api/v1/search?q=gate&kind=activity", Some("reader"))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    // Without an explicit kind a non-admin still gets an answer, and `kinds`
    // says the event log was not searched.
    let (status, answer) = f.get("/api/v1/search?q=gate", Some("reader")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        answer["kinds"],
        serde_json::json!(["repository", "pull_request", "issue", "todo"])
    );
    assert!(answer["counts"].get("activity").is_none(), "{answer}");
}

#[tokio::test]
async fn the_endpoint_refuses_what_it_cannot_search() {
    let f = Fixture::new();
    for (uri, expected) in [
        ("/api/v1/search", StatusCode::UNPROCESSABLE_ENTITY),
        ("/api/v1/search?q=%20", StatusCode::UNPROCESSABLE_ENTITY),
        (
            "/api/v1/search?q=gate&limit=0",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/api/v1/search?q=gate&limit=101",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/api/v1/search?q=gate&kind=commit",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (status, answer) = f.get(uri, Some("admin")).await;
        assert_eq!(status, expected, "{uri}: {answer}");
    }
    // Search is behind a login: it reads across every repository at once.
    let (status, _) = f.get("/api/v1/search?q=gate", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let _ = &f.state;
}

#[test]
fn a_query_number_is_read_from_every_spelling_a_person_types() {
    assert_eq!(query_number("12"), Some(12));
    assert_eq!(query_number("#12"), Some(12));
    assert_eq!(query_number("forge#12"), Some(12));
    assert_eq!(query_number("neverhuman/forge#12"), Some(12));
    assert_eq!(query_number("forge"), None);
    assert_eq!(query_number("#0"), None);
    assert_eq!(
        query_repo("neverhuman/forge#12").as_deref(),
        Some("neverhuman/forge")
    );
    assert_eq!(query_repo("Forge#12").as_deref(), Some("forge"));
    assert_eq!(query_repo("#12"), None);
    assert_eq!(query_repo("forge#next"), None);
}

#[test]
fn a_name_match_beats_a_word_match_beats_a_body_match() {
    assert_eq!(rank_name("forge", "forge"), Some(Rank::Exact));
    assert_eq!(rank_name("forgehammer", "forge"), Some(Rank::Prefix));
    assert_eq!(rank_name("jeryu/forge", "forge"), Some(Rank::Word));
    assert_eq!(rank_name("reforged", "forge"), Some(Rank::Name));
    assert_eq!(rank_name("anvil", "forge"), None);
    assert!(Rank::Exact < Rank::Prefix);
    assert!(Rank::Name < Rank::Body);
}

#[test]
fn a_snippet_is_the_matching_line_capped() {
    let body = "first line\nthe gate wrote a receipt\nlast line";
    assert_eq!(
        snippet(body, "receipt").as_deref(),
        Some("the gate wrote a receipt")
    );
    assert_eq!(snippet(body, "anvil"), None);
    let long = format!("{} needle", "x".repeat(MAX_SNIPPET_CHARS));
    let cut = snippet(&long, "needle").unwrap();
    assert_eq!(cut.chars().count(), MAX_SNIPPET_CHARS + 1);
    assert!(cut.ends_with('…'));
}

#[test]
fn a_query_value_is_percent_encoded_into_the_page_address() {
    assert_eq!(urlencoding("jeryu-web"), "jeryu-web");
    assert_eq!(urlencoding("a b/c"), "a%20b%2Fc");
}
