//! `GET /api/v1/gate-wanted`: the long poll that starts a gate on a push
//! instead of on the runner's next timer tick.
//!
//! The repository, accounts and commits below are invented.

use super::*;
use crate::web::gate_wanted::{self, GateWantedStore};

const OWNER: &str = "acme";
const NAME: &str = "widgets";
const PRIVATE: &str = "ledger";
const HEAD: &str = "5b8e1d9c7a6f4e3b2d1c0a9f8e7d6c5b4a392817";
const MOVED: &str = "6c9f2e0d8b7a5f4c3e2d1b0a9f8e7d6c5b4a3928";
const QUEUED: &str = "7d0a3f1e9c8b6a5d4f3e2c1b0a9f8e7d6c5b4a39";

struct Fixture {
    state: Arc<WebState>,
}

impl Fixture {
    /// `acme/widgets` public and `acme/ledger` private, with `acme-admin` an
    /// admin, `dana` granted read on `widgets` only, and the private repo there so
    /// a signal for it is never handed to `dana`.
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

    async fn poll_as(&self, who: Extension<AccountSummary>, after: u64, wait: u64) -> Value {
        let response = gate_wanted::list(
            State(self.state.clone()),
            who,
            Query(
                serde_json::from_value(json!({ "after": after, "wait": wait }))
                    .expect("gate-wanted query"),
            ),
        )
        .await;
        response_json(response).await
    }

    async fn poll(&self, after: u64) -> Value {
        self.poll_as(authenticated_admin_account("acme-admin"), after, 0)
            .await
    }

    fn push(&self, repo: &str, ref_name: &str, sha: &str) {
        gate_wanted::record_push(
            &self.state,
            OWNER,
            &format!("{repo}.git"),
            &[crate::ci_bridge::RefUpdate {
                ref_name: ref_name.to_string(),
                old_oid: "0".repeat(40),
                new_oid: sha.to_string(),
            }],
        );
    }
}

#[tokio::test]
async fn a_push_a_queue_commit_and_a_regate_each_record_one_signal_the_runner_reads_once() {
    let fx = Fixture::new();
    let empty = fx.poll(0).await;
    assert_eq!(empty["seq"], 0);
    assert_eq!(empty["signals"], json!([]));

    fx.push(NAME, "refs/heads/dana/feature-1", HEAD);
    gate_wanted::record_queue_commit(&fx.state, &format!("{OWNER}/{NAME}"), 7, QUEUED);
    gate_wanted::record_regate(&fx.state, &format!("{OWNER}/{NAME}"), 7, MOVED);

    let body = fx.poll(0).await;
    assert_eq!(body["seq"], 3);
    let signals = body["signals"].as_array().expect("signals").clone();
    assert_eq!(signals.len(), 3, "{body}");
    // The push names the ref it moved, and the repository without the `.git`
    // suffix the git transport was called with.
    assert_eq!(signals[0]["repo"], format!("{OWNER}/{NAME}"));
    assert_eq!(signals[0]["ref_name"], "refs/heads/dana/feature-1");
    assert_eq!(signals[0]["sha"], HEAD);
    assert_eq!(signals[0]["source"], "push");
    assert_eq!(signals[0]["seq"], 1);
    assert!(signals[0].get("number").is_none(), "{body}");
    // A queue commit and a re-gate name a pull request instead of a ref.
    assert_eq!(signals[1]["source"], "queue");
    assert_eq!(signals[1]["number"], 7);
    assert_eq!(signals[1]["sha"], QUEUED);
    assert!(signals[1].get("ref_name").is_none(), "{body}");
    assert_eq!(signals[2]["source"], "regate");
    assert_eq!(signals[2]["sha"], MOVED);
    assert!(
        signals[2]["at"].as_str().is_some_and(|at| !at.is_empty()),
        "{body}"
    );

    // Asking for more than what it read leaves the sequence and reports nothing
    // again, so one signal starts one gate.
    let caught_up = fx.poll(3).await;
    assert_eq!(caught_up["seq"], 3);
    assert_eq!(caught_up["signals"], json!([]));
}

#[tokio::test]
async fn a_signal_arriving_during_a_long_poll_answers_it_instead_of_waiting_it_out() {
    let fx = Fixture::new();
    let state = fx.state.clone();
    // The push happens while the poll is open: the answer has to be the signal,
    // not the timeout, or a push would still cost a timer tick.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate_wanted::record_queue_commit(&state, &format!("{OWNER}/{NAME}"), 7, QUEUED);
    });
    let started = std::time::Instant::now();
    let body = fx
        .poll_as(authenticated_admin_account("acme-admin"), 0, 30)
        .await;
    assert_eq!(body["seq"], 1, "{body}");
    assert_eq!(body["signals"][0]["sha"], QUEUED);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the poll waited {:?} for a signal that arrived in 50ms",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_long_poll_with_nothing_to_report_answers_the_current_sequence_when_it_expires() {
    let fx = Fixture::new();
    fx.push(NAME, "refs/heads/main", HEAD);
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
    fx.push(NAME, "refs/heads/main", HEAD);
    // What a runner sees when the forge restarted under it: it must resync on
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
    fx.push(PRIVATE, "refs/heads/main", HEAD);
    fx.push(NAME, "refs/heads/main", MOVED);

    let admin = fx.poll(0).await;
    assert_eq!(admin["signals"].as_array().map(Vec::len), Some(2));

    let dana = fx.poll_as(authenticated_account("dana"), 0, 0).await;
    let signals = dana["signals"].as_array().expect("signals").clone();
    assert_eq!(signals.len(), 1, "{dana}");
    assert_eq!(signals[0]["repo"], format!("{OWNER}/{NAME}"));
    // The sequence is the forge's, not the reader's share of it: the next poll
    // must not re-walk the signal it was not allowed to see.
    assert_eq!(dana["seq"], 2);
}

/// A runner far behind gets the newest signals and its timer covers the rest,
/// which is cheaper than keeping a backlog nobody will act on.
#[test]
fn the_store_forgets_the_oldest_signal_rather_than_grow_without_bound() {
    let store = GateWantedStore::default();
    for n in 0..600u64 {
        store.record(gate_wanted::NewGateWanted {
            repo: format!("{OWNER}/{NAME}"),
            ref_name: Some(format!("refs/heads/branch-{n}")),
            number: None,
            sha: HEAD.to_string(),
            source: gate_wanted::SOURCE_PUSH,
        });
    }
    assert_eq!(store.seq(), 600);
    let (seq, kept) = store.since(0);
    assert_eq!(seq, 600);
    assert_eq!(kept.len(), 512);
    assert_eq!(kept.first().map(|signal| signal.seq), Some(89));
    assert_eq!(kept.last().map(|signal| signal.seq), Some(600));
}
