//! Closing a pull request as replaced: the link it records, the two APIs that
//! show it, what it refuses, and the backfill that reads the pointers already
//! written in comments.
//!
//! The repositories, accounts and numbers below are invented.

use super::*;

use axum::body::Body;
use axum::http::{Method as HttpMethod, Request, header};
use tower::ServiceExt;

const OWNER: &str = "acme";
const NAME: &str = "widgets";
const HEAD: &str = "4f1c9a8b7d6e5f403122b3c4d5e6f708192a3b4c";

struct Fixture {
    _dir: tempfile::TempDir,
    state: Arc<WebState>,
    router: axum::Router,
    /// `acme-admin` holds admin, `dana` write, `mallory` nothing.
    admin: String,
    dana: String,
    mallory: String,
}

impl Fixture {
    /// `acme/widgets` with `pulls` open pull requests of `dana`, all on the
    /// same base, numbered from 1.
    fn new(pulls: u64) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).expect("open forge");
        core.create_account("acme-admin", "admin-password", UserRole::Admin)
            .expect("create admin");
        core.create_account("dana", "dana-password", UserRole::User)
            .expect("create author");
        core.create_account("mallory", "mallory-password", UserRole::User)
            .expect("create stranger");
        core.create_repository(
            OWNER,
            CreateRepositoryRequest {
                name: NAME.to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .expect("create repository");
        core.grant_repo_access("acme-admin", "dana", OWNER, NAME, RepoAccessLevel::Write)
            .expect("grant the author write");
        for index in 1..=pulls {
            core.create_pull_request(
                OWNER,
                NAME,
                "dana",
                CreatePullRequestRequest {
                    title: format!("change {index}"),
                    head: format!("dana/feature-{index}"),
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
        let (admin, dana, mallory) = (token("acme-admin"), token("dana"), token("mallory"));
        let state = Arc::new(
            WebState::new_with_git_storage(core, dir.path().to_path_buf())
                .with_auth(true, false, false),
        );
        Self {
            _dir: dir,
            router: crate::web::router(state.clone()),
            state,
            admin,
            dana,
            mallory,
        }
    }

    async fn call(
        &self,
        method: HttpMethod,
        uri: &str,
        token: &str,
        body: &str,
    ) -> (StatusCode, Value) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .expect("the router answers");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn close_as_replaced_v3(
        &self,
        number: u64,
        superseded_by: &str,
        token: &str,
    ) -> (StatusCode, Value) {
        self.call(
            HttpMethod::PATCH,
            &format!("/api/v3/repos/{OWNER}/{NAME}/pulls/{number}"),
            token,
            &format!("{{\"state\":\"closed\",\"superseded_by\":{superseded_by}}}"),
        )
        .await
    }

    async fn get(&self, uri: &str, token: &str) -> (StatusCode, Value) {
        self.call(HttpMethod::GET, uri, token, "").await
    }

    fn link(&self, number: u64) -> Option<u64> {
        self.state
            .github
            .pull_replacements()
            .superseded_by(OWNER, NAME, number)
            .map(|link| link.number)
    }
}

/// The `errors` array the GitHub edge answers a refusal with.
fn edge_code(body: &Value) -> String {
    body["errors"][0]["code"].as_str().unwrap_or("").to_string()
}

#[tokio::test]
async fn closing_as_replaced_records_the_link_comments_it_and_both_apis_show_it() {
    let fx = Fixture::new(2);
    let (status, closed) = fx.close_as_replaced_v3(1, "2", &fx.dana).await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(closed["state"], "closed");
    assert_eq!(closed["superseded_by"], 2);

    // The pointer comment outlives the close, so a client with no field for
    // the link still reads what replaced it.
    let reviews = fx.state.core.list_reviews(OWNER, NAME, 1).expect("reviews");
    assert!(
        reviews
            .iter()
            .any(|review| review.body.as_deref().unwrap_or("").contains("#2")),
        "{reviews:?}"
    );

    // The GitHub edge, both ways round.
    let (_, replaced) = fx
        .get(&format!("/api/v3/repos/{OWNER}/{NAME}/pulls/1"), &fx.dana)
        .await;
    assert_eq!(replaced["superseded_by"], 2);
    assert_eq!(replaced["supersedes"], json!([]));
    let (_, replacing) = fx
        .get(&format!("/api/v3/repos/{OWNER}/{NAME}/pulls/2"), &fx.dana)
        .await;
    assert_eq!(replacing["superseded_by"], Value::Null);
    assert_eq!(replacing["supersedes"], json!([1]));

    // The web API: the page payload and the list row.
    let (_, detail) = fx
        .get(&format!("/api/v1/repos/{OWNER}/{NAME}/pulls/1"), &fx.dana)
        .await;
    assert_eq!(detail["superseded_by"], 2);
    assert_eq!(detail["superseded_by_state"], "open");
    assert_eq!(detail["summary"]["superseded_by"], 2);
    let (_, list) = fx
        .get(
            &format!("/api/v1/repos/{OWNER}/{NAME}/pulls?state=all"),
            &fx.dana,
        )
        .await;
    let rows = list["items"].as_array().expect("rows");
    let row = |number: u64| {
        rows.iter()
            .find(|row| row["number"] == number)
            .unwrap_or_else(|| panic!("no row for #{number}: {list}"))
    };
    assert_eq!(row(1)["superseded_by"], 2);
    assert_eq!(row(2)["supersedes"], json!([1]));
}

#[tokio::test]
async fn one_replacement_carries_several_replaced_pull_requests() {
    let fx = Fixture::new(3);
    for replaced in [1, 2] {
        let (status, _) = fx.close_as_replaced_v3(replaced, "3", &fx.dana).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (_, replacing) = fx
        .get(&format!("/api/v3/repos/{OWNER}/{NAME}/pulls/3"), &fx.dana)
        .await;
    assert_eq!(replacing["supersedes"], json!([1, 2]));
}

#[tokio::test]
async fn a_link_to_another_repository_to_itself_or_one_that_cycles_is_refused() {
    let fx = Fixture::new(3);
    let (status, body) = fx.close_as_replaced_v3(1, "1", &fx.dana).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(edge_code(&body), "pull_replacement_self");

    let (status, body) = fx
        .close_as_replaced_v3(1, "\"globex/widgets#2\"", &fx.dana)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(edge_code(&body), "pull_replacement_cross_repo");

    let (status, body) = fx.close_as_replaced_v3(1, "99", &fx.dana).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(edge_code(&body), "pull_replacement_unknown");

    // Nothing refused left a link or closed anything.
    assert_eq!(fx.link(1), None);
    let (_, still_open) = fx
        .get(&format!("/api/v3/repos/{OWNER}/{NAME}/pulls/1"), &fx.dana)
        .await;
    assert_eq!(still_open["state"], "open");

    // #1 -> #2 -> #3, and then #3 -> #1 would close the circle.
    assert_eq!(
        fx.close_as_replaced_v3(1, "2", &fx.dana).await.0,
        StatusCode::OK
    );
    assert_eq!(
        fx.close_as_replaced_v3(2, "3", &fx.dana).await.0,
        StatusCode::OK
    );
    let (status, body) = fx.close_as_replaced_v3(3, "1", &fx.dana).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(edge_code(&body), "pull_replacement_cycle");
    assert_eq!(fx.link(3), None);
}

#[tokio::test]
async fn a_merged_pull_request_was_not_replaced() {
    let fx = Fixture::new(2);
    fx.state
        .core
        .update_pull_request(
            OWNER,
            NAME,
            1,
            jeryu_core::UpdatePullRequestRequest {
                state: Some(jeryu_core::PullRequestState::Merged),
                ..Default::default()
            },
        )
        .expect("merge it in the store");
    let (status, body) = fx.close_as_replaced_v3(1, "2", &fx.dana).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(edge_code(&body), "pull_replacement_merged");
    assert_eq!(fx.link(1), None);
}

#[tokio::test]
async fn recording_a_replacement_needs_write_access_on_both_apis() {
    let fx = Fixture::new(2);
    let (status, _) = fx.close_as_replaced_v3(1, "2", &fx.mallory).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(fx.link(1), None);

    let v1 = format!("/api/v1/repos/{OWNER}/{NAME}/pulls/1/close");
    let (status, body) = fx
        .call(HttpMethod::POST, &v1, &fx.mallory, "{\"superseded_by\": 2}")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "permission_denied");
    assert_eq!(fx.link(1), None);

    // The v1 equivalent of the same call, by somebody who may write.
    let (status, detail) = fx
        .call(HttpMethod::POST, &v1, &fx.dana, "{\"superseded_by\": 2}")
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["superseded_by"], 2);
    assert_eq!(detail["summary"]["state"], "closed");
    assert_eq!(fx.link(1), Some(2));
    assert!(
        fx.state
            .core
            .list_audit(&format!("{OWNER}/{NAME}#1"))
            .expect("audit")
            .iter()
            .any(|row| row.action == "pull_request.close_as_replaced")
    );
}

#[tokio::test]
async fn an_admin_may_record_a_replacement_and_a_v1_body_must_name_a_number() {
    let fx = Fixture::new(2);
    let v1 = format!("/api/v1/repos/{OWNER}/{NAME}/pulls/1/close");
    let (status, body) = fx
        .call(
            HttpMethod::POST,
            &v1,
            &fx.admin,
            "{\"superseded_by\": \"https://example.invalid/globex/widgets/pull/2\"}",
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "pull_replacement_cross_repo");
    let (status, detail) = fx
        .call(HttpMethod::POST, &v1, &fx.admin, "{\"superseded_by\": 2}")
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(fx.link(1), Some(2));
}

#[tokio::test]
async fn the_backfill_reads_the_pointers_already_written_and_leaves_recorded_links_alone() {
    let fx = Fixture::new(4);
    // #1 was closed with the comment the agents write today.
    fx.state
        .core
        .create_review(
            OWNER,
            NAME,
            1,
            "dana",
            CreateReviewRequest {
                body: Some("Landed via #2 after the base was frozen.".to_string()),
                event: ReviewState::Commented,
                comments: Vec::new(),
                expected_head_sha: Some(HEAD.to_string()),
            },
        )
        .expect("comment the pointer");
    for number in [1, 3] {
        fx.state
            .core
            .update_pull_request(
                OWNER,
                NAME,
                number,
                jeryu_core::UpdatePullRequestRequest {
                    state: Some(jeryu_core::PullRequestState::Closed),
                    ..Default::default()
                },
            )
            .expect("close it");
    }
    // #4 says what it replaced itself.
    fx.state
        .core
        .update_pull_request(
            OWNER,
            NAME,
            4,
            jeryu_core::UpdatePullRequestRequest {
                body: Some("Cherry-picked onto main; replaces #3.".to_string()),
                ..Default::default()
            },
        )
        .expect("describe the replacement");

    assert_eq!(fx.state.github.backfill_pull_replacements(), 2);
    assert_eq!(fx.link(1), Some(2));
    assert_eq!(fx.link(3), Some(4));

    // The page's own close dialog writes its pointer to the pull request's
    // issue thread instead, and that is read too.
    let pr = fx
        .state
        .core
        .get_pull_request(OWNER, NAME, 2)
        .expect("the replacement");
    fx.state
        .core
        .add_issue_comment(
            OWNER,
            NAME,
            pr.issue_number,
            "dana",
            jeryu_core::CreateCommentRequest {
                body: "Superseded by #4; this branch is not going anywhere.".to_string(),
            },
        )
        .expect("comment the thread");
    fx.state
        .core
        .update_pull_request(
            OWNER,
            NAME,
            2,
            jeryu_core::UpdatePullRequestRequest {
                state: Some(jeryu_core::PullRequestState::Closed),
                ..Default::default()
            },
        )
        .expect("close it");
    assert_eq!(fx.state.github.backfill_pull_replacements(), 1);
    assert_eq!(fx.link(2), Some(4));
    // A further pass records nothing: the links are already there.
    assert_eq!(fx.state.github.backfill_pull_replacements(), 0);
}

#[tokio::test]
async fn flow_follows_the_chain_to_the_pull_request_that_is_live() {
    let fx = Fixture::new(3);
    assert_eq!(
        fx.close_as_replaced_v3(1, "2", &fx.dana).await.0,
        StatusCode::OK
    );
    assert_eq!(
        fx.close_as_replaced_v3(2, "3", &fx.dana).await.0,
        StatusCode::OK
    );
    let found = crate::web::pipeline::attention::superseded_open_pulls(&fx.state);
    let one = found
        .iter()
        .find(|hit| hit.number == 1)
        .unwrap_or_else(|| panic!("no replacement for #1: {found:?}"));
    assert_eq!(one.replacement_number, 3);
    assert_eq!(one.replacement_repo, format!("{OWNER}/{NAME}"));
    assert_eq!(one.replacement_state, "open");
}
