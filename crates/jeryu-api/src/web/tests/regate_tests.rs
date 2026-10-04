//! `POST /api/v1/repos/:id/pulls/:number/regate` and the list the gate runner
//! reads, `GET /api/v1/gate-regate`.
//!
//! The repository, accounts and commits below are invented.

use super::*;
use crate::web::regate::{self, RegateRequest, RegateStore};

const OWNER: &str = "acme";
const NAME: &str = "widgets";
const HEAD: &str = "9c1f7b3d5e4a2c8b6d0f9e8a7b6c5d4e3f2a1b09";
const MOVED: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";

struct Fixture {
    state: Arc<WebState>,
    number: u64,
}

impl Fixture {
    /// `acme/widgets#7`, open at [`HEAD`], with `acme-admin` an admin, the
    /// author `dana` holding write, and `mallory` holding nothing.
    fn new() -> Self {
        let core = ForgeCore::new();
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
        // The six pull requests before it are never created, so the number the
        // route is called with is the one the forge answers for.
        let mut number = 0;
        while number < 7 {
            number = core
                .create_pull_request(
                    OWNER,
                    NAME,
                    "dana",
                    CreatePullRequestRequest {
                        title: format!("PR {}", number + 1),
                        head: format!("dana/feature-{}", number + 1),
                        base: "main".to_string(),
                        head_sha: Some(HEAD.to_string()),
                        ..Default::default()
                    },
                )
                .expect("open a pull request")
                .number;
        }
        Self {
            state: Arc::new(WebState::new(core)),
            number,
        }
    }

    fn core(&self) -> &jeryu_core::ForgeCore {
        &self.state.core
    }

    async fn regate_as(&self, who: Extension<AccountSummary>) -> (StatusCode, Value) {
        let response = regate::request(
            State(self.state.clone()),
            who,
            AxumPath((format!("{OWNER}/{NAME}"), self.number)),
        )
        .await;
        let status = response.status();
        (status, response_json(response).await)
    }

    async fn regate(&self) -> (StatusCode, Value) {
        self.regate_as(authenticated_account("dana")).await
    }

    async fn requests_for(&self, who: Extension<AccountSummary>, state: Option<&str>) -> Value {
        let response = regate::list(
            State(self.state.clone()),
            who,
            Query(serde_json::from_value(json!({ "state": state })).expect("list query")),
        )
        .await;
        response_json(response).await["requests"].clone()
    }

    async fn pending(&self) -> Value {
        self.requests_for(authenticated_admin_account("acme-admin"), None)
            .await
    }
}

#[tokio::test]
async fn a_regate_is_recorded_against_the_current_head_and_read_back_as_pending() {
    let fx = Fixture::new();
    assert_eq!(fx.pending().await, json!([]));

    let (status, body) = fx.regate().await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["repo"], format!("{OWNER}/{NAME}"));
    assert_eq!(body["number"], 7);
    assert_eq!(body["head_sha"], HEAD);
    assert_eq!(body["requested_by"], "dana");
    assert!(
        body["requested_at"]
            .as_str()
            .is_some_and(|at| !at.is_empty()),
        "{body}"
    );

    let pending = fx.pending().await;
    assert_eq!(pending.as_array().map(Vec::len), Some(1), "{pending}");
    assert_eq!(pending[0], body);
}

#[tokio::test]
async fn a_request_whose_head_moved_or_whose_pull_request_closed_is_no_longer_pending() {
    let fx = Fixture::new();
    // The head the runner would gate is not the head that was asked for: the
    // new head is gated because it is new, not because of this request.
    fx.state.regate_requests.record(RegateRequest {
        repo: format!("{OWNER}/{NAME}"),
        number: fx.number,
        head_sha: MOVED.to_string(),
        requested_at: "2026-10-04T09:00:00Z".to_string(),
        requested_by: "dana".to_string(),
    });
    assert_eq!(fx.pending().await, json!([]));
    // It is still what was asked, and `state=all` says so.
    let all = fx
        .requests_for(authenticated_admin_account("acme-admin"), Some("all"))
        .await;
    assert_eq!(all[0]["head_sha"], MOVED);

    let (status, _) = fx.regate().await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(fx.pending().await[0]["head_sha"], HEAD);
    fx.core()
        .update_pull_request(
            OWNER,
            NAME,
            fx.number,
            jeryu_core::UpdatePullRequestRequest {
                state: Some(jeryu_core::PullRequestState::Closed),
                ..Default::default()
            },
        )
        .expect("close the pull request");
    assert_eq!(fx.pending().await, json!([]));
}

#[tokio::test]
async fn only_an_open_pull_request_of_a_repository_the_caller_writes_can_be_re_gated() {
    let fx = Fixture::new();
    let (status, body) = fx.regate_as(authenticated_account("mallory")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "permission_denied");
    assert_eq!(fx.pending().await, json!([]));

    let missing = regate::request(
        State(fx.state.clone()),
        authenticated_account("dana"),
        AxumPath((format!("{OWNER}/{NAME}"), 70)),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    fx.core()
        .update_pull_request(
            OWNER,
            NAME,
            fx.number,
            jeryu_core::UpdatePullRequestRequest {
                draft: Some(true),
                ..Default::default()
            },
        )
        .expect("convert to draft");
    let (status, body) = fx.regate().await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "not_open");
}

#[tokio::test]
async fn the_newest_ask_per_pull_request_is_the_one_the_runner_reads() {
    let fx = Fixture::new();
    let (_, first) = fx.regate().await;
    let (_, second) = fx
        .regate_as(authenticated_admin_account("acme-admin"))
        .await;
    let pending = fx.pending().await;
    assert_eq!(pending.as_array().map(Vec::len), Some(1), "{pending}");
    assert_eq!(pending[0]["requested_by"], "acme-admin");
    assert!(
        pending[0]["requested_at"].as_str() >= first["requested_at"].as_str(),
        "{second}"
    );
}

#[test]
fn the_store_forgets_the_oldest_ask_rather_than_refuse_a_new_one() {
    let store = RegateStore::default();
    let ask = |number: u64, at: &str| RegateRequest {
        repo: format!("{OWNER}/{NAME}"),
        number,
        head_sha: HEAD.to_string(),
        requested_at: at.to_string(),
        requested_by: "dana".to_string(),
    };
    for number in 0..2000 {
        store.record(ask(
            number,
            &format!("2026-10-04T09:{:02}:00Z", number % 60),
        ));
    }
    let kept = store.all().len();
    assert!(kept > 0 && kept <= 1024, "{kept} requests kept");
}
