//! `GET /api/v1/review-wanted`: the long poll that starts a review when a pull
//! request is opened or pushed to, instead of on the reviewer's next pass.
//!
//! The repository, accounts and commits below are invented.

use super::*;
use crate::web::review_wanted::{self, ReviewWantedStore};

const OWNER: &str = "acme";
const NAME: &str = "widgets";
const PRIVATE: &str = "ledger";
const BASE: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
const HEAD: &str = "5b8e1d9c7a6f4e3b2d1c0a9f8e7d6c5b4a392817";
const PUSHED: &str = "6c9f2e0d8b7a5f4c3e2d1b0a9f8e7d6c5b4a3928";

struct Fixture {
    state: Arc<WebState>,
}

impl Fixture {
    /// `acme/widgets` public and `acme/ledger` private, with `acme-admin` an
    /// admin and `dana` granted read on `widgets` only, so a signal for the
    /// private repository is never handed to `dana`.
    fn new() -> Self {
        let core = ForgeCore::new();
        core.create_account("acme-admin", "admin-password", UserRole::Admin)
            .expect("create admin");
        core.create_account("dana", "dana-password", UserRole::User)
            .expect("create reader");
        for (name, private) in [(NAME, false), (PRIVATE, true)] {
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
        }
        core.grant_repo_access("acme-admin", "dana", OWNER, NAME, RepoAccessLevel::Read)
            .expect("grant the reader the public repository");
        Self {
            state: Arc::new(WebState::new(core)),
        }
    }

    /// Opens a pull request in `repo` and records the signal the forge records
    /// when one is opened.
    fn open_pull(&self, repo: &str, branch: &str, head: &str) -> jeryu_core::PullRequest {
        let pr = self
            .state
            .core
            .create_pull_request(
                OWNER,
                repo,
                "dana",
                CreatePullRequestRequest {
                    title: format!("work on {branch}"),
                    head: branch.to_string(),
                    base: "main".to_string(),
                    head_sha: Some(head.to_string()),
                    base_sha: Some(BASE.to_string()),
                    ..CreatePullRequestRequest::default()
                },
            )
            .expect("open pull request");
        review_wanted::record_opened(&self.state, &pr);
        pr
    }

    /// The push path: the refs moved, and `on_push` already advanced the open
    /// pull request's head to the pushed commit before the signal is recorded.
    fn push(&self, repo: &str, branch: &str, sha: &str) {
        self.state
            .core
            .refresh_pull_request_heads_for_ref(OWNER, repo, branch, sha)
            .expect("refresh pull request heads");
        review_wanted::record_push(
            &self.state,
            OWNER,
            &format!("{repo}.git"),
            &[crate::ci_bridge::RefUpdate {
                ref_name: format!("refs/heads/{branch}"),
                old_oid: HEAD.to_string(),
                new_oid: sha.to_string(),
            }],
        );
    }

    async fn poll_as(&self, who: Extension<AccountSummary>, after: u64, wait: u64) -> Value {
        let response = review_wanted::list(
            State(self.state.clone()),
            who,
            Query(
                serde_json::from_value(json!({ "after": after, "wait": wait }))
                    .expect("review-wanted query"),
            ),
        )
        .await;
        response_json(response).await
    }

    async fn poll(&self, after: u64) -> Value {
        self.poll_as(authenticated_admin_account("acme-admin"), after, 0)
            .await
    }
}

#[tokio::test]
async fn opening_a_pull_request_and_pushing_to_it_each_record_one_signal_read_once() {
    let fx = Fixture::new();
    let empty = fx.poll(0).await;
    assert_eq!(empty["seq"], 0);
    assert_eq!(empty["signals"], json!([]));

    let pr = fx.open_pull(NAME, "dana/feature-1", HEAD);
    fx.push(NAME, "dana/feature-1", PUSHED);

    let body = fx.poll(0).await;
    assert_eq!(body["seq"], 2, "{body}");
    let signals = body["signals"].as_array().expect("signals").clone();
    assert_eq!(signals.len(), 2, "{body}");
    // The open names the repository without a `.git` suffix, the pull request
    // and the exact head a review binds to.
    assert_eq!(signals[0]["repo"], format!("{OWNER}/{NAME}"));
    assert_eq!(signals[0]["number"], pr.number);
    assert_eq!(signals[0]["head_sha"], HEAD);
    assert_eq!(signals[0]["base_ref"], "main");
    assert_eq!(signals[0]["draft"], false);
    assert_eq!(signals[0]["kind"], "pull.opened");
    assert_eq!(signals[0]["seq"], 1);
    // The push names the pushed head, not the head the pull request opened at.
    assert_eq!(signals[1]["kind"], "pull.synchronized");
    assert_eq!(signals[1]["number"], pr.number);
    assert_eq!(signals[1]["head_sha"], PUSHED);
    assert!(
        signals[1]["at"].as_str().is_some_and(|at| !at.is_empty()),
        "{body}"
    );

    // Asking for more than what it read leaves the sequence and reports nothing
    // again, so one signal starts one review.
    let caught_up = fx.poll(2).await;
    assert_eq!(caught_up["seq"], 2);
    assert_eq!(caught_up["signals"], json!([]));
}

/// The production hook, not the recorder: a successful create on the
/// GitHub-compatible edge (`POST /repos/{owner}/{repo}/pulls`, which is how
/// every client and agent opens a pull request here) is what records the
/// `pull.opened` signal.
#[tokio::test]
async fn a_create_on_the_github_edge_records_the_opened_signal() {
    let fx = Fixture::new();
    let pr = fx
        .state
        .core
        .create_pull_request(
            OWNER,
            NAME,
            "dana",
            CreatePullRequestRequest {
                title: "work on dana/feature-1".to_string(),
                head: "dana/feature-1".to_string(),
                base: "main".to_string(),
                head_sha: Some(HEAD.to_string()),
                base_sha: Some(BASE.to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .expect("open pull request");
    crate::web::pipeline::emit::github_edge(
        &fx.state,
        true,
        &format!("/repos/{OWNER}/{NAME}/pulls"),
        "dana",
        "{}",
        201,
        &json!({ "number": pr.number }).to_string(),
    );

    let body = fx.poll(0).await;
    assert_eq!(body["signals"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body["signals"][0]["kind"], "pull.opened");
    assert_eq!(body["signals"][0]["number"], pr.number);
    assert_eq!(body["signals"][0]["head_sha"], HEAD);
}

#[tokio::test]
async fn a_push_that_moved_no_pull_request_head_records_nothing() {
    let fx = Fixture::new();
    fx.open_pull(NAME, "dana/feature-1", HEAD);
    let after_open = fx.poll(0).await["seq"].as_u64().expect("seq");

    // A push to a branch no open pull request heads: the reviewer has nothing
    // to do, so there is no signal to make it look.
    fx.push(NAME, "main", PUSHED);

    let body = fx.poll(after_open).await;
    assert_eq!(body["signals"], json!([]), "{body}");
    assert_eq!(body["seq"], after_open, "{body}");
}

#[tokio::test]
async fn a_push_to_a_closed_pull_requests_branch_records_nothing() {
    let fx = Fixture::new();
    let pr = fx.open_pull(NAME, "dana/feature-1", HEAD);
    fx.state
        .core
        .update_pull_request(
            OWNER,
            NAME,
            pr.number,
            jeryu_core::UpdatePullRequestRequest {
                state: Some(jeryu_core::PullRequestState::Closed),
                ..jeryu_core::UpdatePullRequestRequest::default()
            },
        )
        .expect("close the pull request");
    let after_open = fx.poll(0).await["seq"].as_u64().expect("seq");

    fx.push(NAME, "dana/feature-1", PUSHED);

    let body = fx.poll(after_open).await;
    assert_eq!(body["signals"], json!([]), "{body}");
}

#[tokio::test]
async fn a_signal_arriving_during_a_long_poll_answers_it_instead_of_waiting_it_out() {
    let fx = Fixture::new();
    let state = fx.state.clone();
    // The push happens while the poll is open: the answer has to be the signal,
    // not the timeout, or a push would still cost a pass.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        state.review_wanted.record(review_wanted::NewReviewWanted {
            repo: format!("{OWNER}/{NAME}"),
            number: 7,
            head_sha: PUSHED.to_string(),
            base_ref: "main".to_string(),
            draft: false,
            kind: review_wanted::KIND_SYNCHRONIZED,
        });
    });
    let started = std::time::Instant::now();
    let body = fx
        .poll_as(authenticated_admin_account("acme-admin"), 0, 30)
        .await;
    assert_eq!(body["seq"], 1, "{body}");
    assert_eq!(body["signals"][0]["head_sha"], PUSHED);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the poll waited {:?} for a signal that arrived in 50ms",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_long_poll_with_nothing_to_report_answers_the_current_sequence_when_it_expires() {
    let fx = Fixture::new();
    fx.open_pull(NAME, "dana/feature-1", HEAD);
    // `after` is already the newest signal, so there is nothing to hand over
    // and the poll expires. A one-second wait keeps the test honest about
    // blocking without making it slow.
    let started = std::time::Instant::now();
    let body = fx
        .poll_as(authenticated_admin_account("acme-admin"), 1, 1)
        .await;
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "the poll returned in {:?} without waiting",
        started.elapsed()
    );
    assert_eq!(body["seq"], 1, "{body}");
    assert_eq!(body["signals"], json!([]));
}

#[tokio::test]
async fn an_after_ahead_of_the_forge_is_told_the_smaller_sequence_at_once() {
    let fx = Fixture::new();
    fx.open_pull(NAME, "dana/feature-1", HEAD);
    // What a reviewer sees when the forge restarted under it: it must resync on
    // the smaller sequence rather than read the kept backlog as new work.
    let started = std::time::Instant::now();
    let body = fx
        .poll_as(authenticated_admin_account("acme-admin"), 99, 30)
        .await;
    assert_eq!(body["seq"], 1, "{body}");
    assert_eq!(body["signals"], json!([]));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a restarted forge held the poll for {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_reader_is_handed_signals_only_for_repositories_it_can_read() {
    let fx = Fixture::new();
    fx.open_pull(PRIVATE, "dana/ledger-fix", HEAD);
    fx.open_pull(NAME, "dana/feature-1", PUSHED);

    let admin = fx.poll(0).await;
    assert_eq!(admin["signals"].as_array().map(Vec::len), Some(2));

    // `dana` may read `widgets` and not `ledger`, and is not the owner of
    // either namespace.
    let dana = fx.poll_as(authenticated_account("dana"), 0, 0).await;
    let signals = dana["signals"].as_array().expect("signals").clone();
    assert_eq!(signals.len(), 1, "{dana}");
    assert_eq!(signals[0]["repo"], format!("{OWNER}/{NAME}"));
    // The sequence is the forge's, not the reader's share of it: the next poll
    // must not re-walk the signal it was not allowed to see.
    assert_eq!(dana["seq"], 2);
}

/// A reviewer far behind gets the newest signals and its pass covers the rest,
/// which is cheaper than keeping a backlog nobody will act on.
#[test]
fn the_store_forgets_the_oldest_signal_rather_than_grow_without_bound() {
    let store = ReviewWantedStore::default();
    for n in 0..600u64 {
        store.record(review_wanted::NewReviewWanted {
            repo: format!("{OWNER}/{NAME}"),
            number: n + 1,
            head_sha: HEAD.to_string(),
            base_ref: "main".to_string(),
            draft: false,
            kind: review_wanted::KIND_OPENED,
        });
    }
    assert_eq!(store.seq(), 600);
    let (seq, kept) = store.since(0);
    assert_eq!(seq, 600);
    assert_eq!(kept.len(), 512);
    assert_eq!(kept.first().map(|signal| signal.seq), Some(89));
    assert_eq!(kept.last().map(|signal| signal.seq), Some(600));
}
