//! Merge queue over real bare repositories: enqueue, replay, gate, land.

use super::*;
use axum::extract::Query;
use jeryu_core::{
    AccountStatus, CheckConclusion, CommitStatusState, CreateCheckRunRequest,
    CreateCommitStatusRequest, CreatePullRequestRequest, CreateRepositoryRequest,
    CreateReviewRequest, ReviewState,
};
use std::process::Command;

fn account(login: &str, role: UserRole) -> Extension<AccountSummary> {
    Extension(AccountSummary {
        login: login.to_string(),
        display_name: login.to_string(),
        role,
        status: AccountStatus::Active,
        auth_epoch: 0,
        must_change_password: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    })
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "jeryu-test")
        .env("GIT_AUTHOR_EMAIL", "jeryu-test@example.com")
        .env("GIT_COMMITTER_NAME", "jeryu-test")
        .env("GIT_COMMITTER_EMAIL", "jeryu-test@example.com")
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository `alice/jeryu` whose `feature` branch adds `feature.txt`, an
/// approved PR for it with a green check on its head, and a work clone.
struct Fixture {
    _storage: tempfile::TempDir,
    bare: PathBuf,
    work: PathBuf,
    state: Arc<WebState>,
    repo_id: String,
    number: u64,
    head: String,
}

impl Fixture {
    fn new(feature_file: &str, feature_body: &str) -> Self {
        let storage = tempfile::tempdir().expect("storage");
        let bare = storage.path().join("alice").join("jeryu.git");
        let work = storage.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "--quiet", "-b", "main"]);
        std::fs::write(work.join("shared.txt"), "one\ntwo\nthree\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "base"]);
        let base = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(work.join(feature_file), feature_body).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "feature"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["checkout", "--quiet", "main"]);
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git(
            &work,
            &["clone", "--quiet", "--bare", ".", bare.to_str().unwrap()],
        );
        git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        let core = ForgeCore::new();
        let repo = core
            .create_repository(
                "alice",
                CreateRepositoryRequest {
                    name: "jeryu".to_string(),
                    private: false,
                    description: None,
                    default_branch: Some("main".to_string()),
                },
            )
            .unwrap();
        let pr = core
            .create_pull_request(
                "alice",
                "jeryu",
                "alice",
                CreatePullRequestRequest {
                    title: "queued".to_string(),
                    head: "feature".to_string(),
                    base: "main".to_string(),
                    head_sha: Some(head.clone()),
                    base_sha: Some(base),
                    changed_files: vec![feature_file.to_string()],
                    ..CreatePullRequestRequest::default()
                },
            )
            .unwrap();
        core.create_check_run(
            "alice",
            "jeryu",
            CreateCheckRunRequest {
                name: "ci/fast".to_string(),
                head_sha: head.clone(),
                status: Some(jeryu_core::CheckRunStatus::Completed),
                conclusion: Some(CheckConclusion::Success),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();
        core.create_review(
            "alice",
            "jeryu",
            pr.number,
            "pragent",
            CreateReviewRequest {
                body: None,
                event: ReviewState::Approved,
                comments: Vec::new(),
                expected_head_sha: Some(head.clone()),
            },
        )
        .unwrap();
        let state = Arc::new(WebState::new_with_git_storage(
            core,
            storage.path().to_path_buf(),
        ));
        Fixture {
            _storage: storage,
            bare,
            work,
            state,
            repo_id: repo.id.to_string(),
            number: pr.number,
            head,
        }
    }

    /// Commit `body` to `file` on main and push it, as another PR landing.
    fn advance_main(&self, file: &str, body: &str) -> String {
        std::fs::write(self.work.join(file), body).unwrap();
        git(&self.work, &["add", "."]);
        git(&self.work, &["commit", "--quiet", "-m", "main moves"]);
        git(
            &self.work,
            &["push", "--quiet", self.bare.to_str().unwrap(), "main"],
        );
        git(&self.work, &["rev-parse", "HEAD"])
    }

    fn main(&self) -> String {
        git(&self.bare, &["rev-parse", "refs/heads/main"])
    }

    fn has_ref(&self, name: &str) -> bool {
        Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", name])
            .current_dir(&self.bare)
            .status()
            .unwrap()
            .success()
    }

    fn status(&self, sha: &str, state: CommitStatusState) {
        self.state
            .core
            .create_commit_status(
                "alice",
                "jeryu",
                sha,
                "gatebot",
                CreateCommitStatusRequest {
                    state,
                    context: "ci/fast".to_string(),
                    description: None,
                    target_url: None,
                },
            )
            .unwrap();
    }

    async fn enqueue(&self) -> (StatusCode, Value) {
        self.enqueue_as(account("alton2", UserRole::Admin)).await
    }

    async fn enqueue_as(&self, who: Extension<AccountSummary>) -> (StatusCode, Value) {
        let response = merge_queue::enqueue(
            State(self.state.clone()),
            who,
            AxumPath((self.repo_id.clone(), self.number)),
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Pipeline events recorded so far, oldest first, as `(kind, needs_human)`.
    fn events(&self) -> Vec<(String, bool)> {
        self.state
            .events
            .query(&crate::web::pipeline::EventsQuery {
                after_seq: Some(0),
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .map(|event| {
                assert_eq!(event.repo.as_deref(), Some("alice/jeryu"));
                assert_eq!(event.pr, Some(1));
                (event.kind, event.needs_human)
            })
            .collect()
    }

    fn entry(&self) -> merge_queue::QueueEntry {
        self.state
            .merge_queue
            .entries(&self.state, |_| true)
            .pop()
            .expect("an entry")
    }
}

#[tokio::test]
async fn a_pr_already_on_the_tip_lands_as_itself() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["queue_sha"], fx.head);
    assert_eq!(body["approvers"][0]["login"], "pragent");
    assert_eq!(body["approvers"][0]["automation"], true);

    assert_eq!(merge_queue::tick(&fx.state), 1);
    let entry = fx.entry();
    assert_eq!(entry.state, merge_queue::QueueState::Landed);
    assert_eq!(fx.main(), fx.head);
    assert!(!fx.has_ref("refs/queue/main/1"));
}

#[tokio::test]
async fn a_pr_behind_main_is_replayed_gated_and_landed() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let moved = fx.advance_main("other.txt", "other\n");

    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let queue_sha = body["queue_sha"].as_str().unwrap().to_string();
    assert_ne!(queue_sha, fx.head, "a replay is a new commit");
    assert_eq!(body["base_sha"], moved);
    assert_eq!(
        git(&fx.bare, &["rev-parse", "refs/queue/main/1"]),
        queue_sha
    );
    assert_eq!(
        git(&fx.bare, &["rev-parse", &format!("{queue_sha}^")]),
        moved,
        "the replay sits on the current tip"
    );

    // Nothing on the queue commit yet: the queue waits.
    assert_eq!(merge_queue::tick(&fx.state), 0);
    assert_eq!(fx.main(), moved);

    fx.status(&queue_sha, CommitStatusState::Success);
    assert_eq!(merge_queue::tick(&fx.state), 1);
    let entry = fx.entry();
    assert_eq!(entry.state, merge_queue::QueueState::Landed, "{entry:?}");
    assert_eq!(
        fx.main(),
        queue_sha,
        "main fast-forwards to the gated commit"
    );
    assert_eq!(entry.landed_sha.as_deref(), Some(queue_sha.as_str()));
    assert!(!fx.has_ref("refs/queue/main/1"));
    assert!(fx.has_ref("refs/queue-meta/main/1"));
    let pr = fx
        .state
        .core
        .get_pull_request("alice", "jeryu", fx.number)
        .unwrap();
    assert!(pr.merged);
    assert_eq!(
        fx.events(),
        [
            ("queue.enqueued".to_string(), false),
            ("queue.landed".to_string(), false),
            ("pr.merged".to_string(), false),
        ],
        "a waiting tick emits nothing; landing emits the queue and the merge"
    );
}

#[tokio::test]
async fn a_conflicting_replay_is_refused_and_main_is_untouched() {
    let fx = Fixture::new("shared.txt", "one\nFEATURE\nthree\n");
    let moved = fx.advance_main("shared.txt", "one\nMAIN\nthree\n");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "queue_conflict", "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("shared.txt"),
        "{body}"
    );
    assert_eq!(fx.main(), moved);
    assert!(!fx.has_ref("refs/queue/main/1"));
    assert_eq!(
        fx.events(),
        [("queue.refused".to_string(), true)],
        "a PR that cannot be replayed needs somebody to rebase it"
    );
}

#[tokio::test]
async fn a_failed_queue_gate_is_retried_once_then_fails() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let moved = fx.advance_main("other.txt", "other\n");
    let (_, body) = fx.enqueue().await;
    let first = body["queue_sha"].as_str().unwrap().to_string();

    fx.status(&first, CommitStatusState::Failure);
    merge_queue::tick(&fx.state);
    let entry = fx.entry();
    assert_eq!(entry.state, merge_queue::QueueState::Building);
    assert_eq!(entry.attempts.len(), 2);
    assert_ne!(entry.queue_sha, first, "the retry is a fresh commit");
    assert_eq!(
        git(&fx.bare, &["rev-parse", "refs/queue/main/1"]),
        entry.queue_sha
    );

    fx.status(&entry.queue_sha, CommitStatusState::Failure);
    assert_eq!(merge_queue::tick(&fx.state), 1);
    let entry = fx.entry();
    assert_eq!(entry.state, merge_queue::QueueState::Failed);
    assert_eq!(fx.main(), moved);
    assert!(!fx.has_ref("refs/queue/main/1"));
    assert_eq!(
        fx.events(),
        [
            ("queue.enqueued".to_string(), false),
            ("queue.building".to_string(), false),
            ("queue.failed".to_string(), true),
        ]
    );
}

#[tokio::test]
async fn a_base_that_moves_under_a_green_queue_commit_is_rebuilt_not_landed() {
    let fx = Fixture::new("feature.txt", "feature\n");
    fx.advance_main("other.txt", "other\n");
    let (_, body) = fx.enqueue().await;
    let first = body["queue_sha"].as_str().unwrap().to_string();
    let moved_again = fx.advance_main("third.txt", "third\n");

    fx.status(&first, CommitStatusState::Success);
    merge_queue::tick(&fx.state);
    let entry = fx.entry();
    assert_eq!(entry.state, merge_queue::QueueState::Building);
    assert_eq!(
        fx.main(),
        moved_again,
        "never land onto a base it was not gated on"
    );
    assert_eq!(entry.base_sha, moved_again);
    assert_ne!(entry.queue_sha, first);
}

#[tokio::test]
async fn a_new_pr_head_dequeues() {
    let fx = Fixture::new("feature.txt", "feature\n");
    fx.advance_main("other.txt", "other\n");
    fx.enqueue().await;
    // The author pushes again: the PR head moves off what was approved.
    fx.state
        .core
        .refresh_pull_request_heads_for_ref("alice", "jeryu", "feature", &fx.main())
        .unwrap();
    merge_queue::tick(&fx.state);
    assert_eq!(fx.entry().state, merge_queue::QueueState::Dequeued);
}

#[tokio::test]
async fn the_queue_survives_a_restart_and_lists_building_entries() {
    let fx = Fixture::new("feature.txt", "feature\n");
    fx.advance_main("other.txt", "other\n");
    fx.enqueue().await;

    // A fresh index over the same repositories reloads from refs/queue-meta.
    let reloaded = Arc::new((*fx.state).clone());
    let reloaded = Arc::new(WebState {
        merge_queue: Arc::default(),
        ..(*reloaded).clone()
    });
    let response = merge_queue::list_all(
        State(reloaded),
        account("alton2", UserRole::Admin),
        Query(serde_json::from_value(json!({ "state": "building" })).unwrap()),
    )
    .await;
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["entries"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["entries"][0]["queue_ref"], "refs/queue/main/1");
}

#[tokio::test]
async fn queue_refs_are_advertised_to_fetchers_and_refused_to_pushers() {
    let fx = Fixture::new("feature.txt", "feature\n");
    fx.advance_main("other.txt", "other\n");
    fx.enqueue().await;
    let advertised = git(&fx.work, &["ls-remote", fx.bare.to_str().unwrap()]);
    assert!(advertised.contains("refs/queue/main/1"), "{advertised}");

    assert!(merge_queue::is_queue_owned_ref("refs/queue/main/1"));
    assert!(merge_queue::is_queue_owned_ref("refs/queue-meta/main/1"));
    assert!(!merge_queue::is_queue_owned_ref("refs/heads/queue"));
    assert!(!merge_queue::is_queue_owned_ref("refs/queued/x"));

    let zero = "0".repeat(40);
    let line = format!("{zero} {} refs/queue/main/9\0report-status\n", fx.head);
    let mut body = format!("{:04x}{line}", line.len() + 4).into_bytes();
    body.extend_from_slice(b"0000");
    let commands = jeryu_gitd::pack::receive_pack_commands(&body).unwrap();
    assert!(
        commands
            .iter()
            .any(|command| merge_queue::is_queue_owned_ref(&command.ref_name))
    );
}

#[tokio::test]
async fn enqueueing_needs_write_access() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let (status, body) = fx.enqueue_as(account("mallory", UserRole::User)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!fx.has_ref("refs/queue/main/1"));
}

#[tokio::test]
async fn a_fast_forward_pr_reuses_its_head_result_for_the_same_commit() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // The head already contains the base tip: the replay is the head itself,
    // so the tree and the gate inputs are the same commit, not a copy.
    assert_eq!(body["queue_sha"], fx.head);
    assert_eq!(
        git(&fx.bare, &["rev-parse", &format!("{}^{{tree}}", fx.head)]),
        git(&fx.bare, &["rev-parse", "refs/queue/main/1^{tree}"])
    );

    assert_eq!(merge_queue::tick(&fx.state), 1);
    let entry = fx.entry();
    assert_eq!(entry.state, merge_queue::QueueState::Landed, "{entry:?}");
    assert_eq!(entry.attempts.len(), 1, "no second gate was requested");
    assert_eq!(entry.landed_sha.as_deref(), Some(fx.head.as_str()));
    assert_eq!(fx.main(), fx.head);
}

#[tokio::test]
async fn a_replay_never_borrows_the_head_result() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let moved = fx.advance_main("other.txt", "other\n");
    fx.status(&fx.head, CommitStatusState::Success);
    let (_, body) = fx.enqueue().await;
    let queue_sha = body["queue_sha"].as_str().unwrap().to_string();
    assert_ne!(queue_sha, fx.head);
    for _ in 0..3 {
        merge_queue::tick(&fx.state);
    }
    assert_eq!(fx.entry().state, merge_queue::QueueState::Building);
    assert_eq!(fx.main(), moved, "a green head does not land a new commit");
}
