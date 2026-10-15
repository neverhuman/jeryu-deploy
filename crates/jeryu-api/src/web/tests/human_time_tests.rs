//! The routes that charge a person's time, end to end: opening a pull
//! request's files and reviewing it, and the report that divides those
//! minutes by the changes that stuck. The accounts, repository and family
//! below are invented (acme/widgets, dana, a review bot).

use super::*;
use crate::web::human_time::{self, By, Window};
use crate::web::people::AccountKind;

const REPO: &str = "widgets";

fn fixture() -> (Arc<WebState>, jeryu_core::PullRequest) {
    let core = ForgeCore::new();
    core.create_account("dana", "dana-password", UserRole::User)
        .unwrap();
    core.create_account("pr-redteam", "bot-password", UserRole::User)
        .unwrap();
    core.create_repository(
        "acme",
        CreateRepositoryRequest {
            name: REPO.into(),
            ..Default::default()
        },
    )
    .unwrap();
    let pr = core
        .create_pull_request(
            "acme",
            REPO,
            "dana",
            CreatePullRequestRequest {
                title: "feature".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some("head-a".to_string()),
                base_sha: Some("base-a".to_string()),
                changed_files: vec!["crates/jeryu-api/src/web.rs".to_string()],
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    let state = Arc::new(WebState::new(core));
    // The agent reviewer reviews through the same routes as a person; an
    // administrator has marked it automation.
    state
        .site_settings
        .set(
            "account_kind:pr-redteam",
            Some("service"),
            "admin",
            chrono::Utc::now().timestamp_millis(),
        )
        .unwrap();
    (state, pr)
}

fn whole_window() -> Window {
    Window {
        since: chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(),
        until: chrono::NaiveDate::from_ymd_opt(2100, 1, 1).unwrap(),
    }
}

fn minutes(state: &WebState) -> human_time::HumanMinutesReport {
    state
        .events
        .with_conn(|conn| {
            human_time::report(conn, None, whole_window(), By::Repo, chrono::Utc::now())
        })
        .unwrap()
}

async fn open_the_files_view(state: &Arc<WebState>, login: &str, pr: &jeryu_core::PullRequest) {
    let response = crate::web::pulls::diff(
        State(state.clone()),
        authenticated_account(login),
        AxumPath((format!("acme/{REPO}"), pr.number)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

async fn submit_a_review(state: &Arc<WebState>, login: &str, pr: &jeryu_core::PullRequest) {
    let response = crate::web::pulls::review(
        State(state.clone()),
        authenticated_account(login),
        AxumPath((format!("acme/{REPO}"), pr.number)),
        axum::body::Bytes::from(
            serde_json::json!({
                "verdict": "comment",
                "expected_head_sha": "head-a",
                "body_markdown": "one question",
                "thread_comments": [],
                "evidence": null,
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn human_time_diff_polling_is_read_only_and_only_explicit_reviews_mark_attention() {
    let (state, pr) = fixture();
    for _ in 0..20 {
        open_the_files_view(&state, "dana", &pr).await;
    }
    assert_eq!(minutes(&state).totals.review_minutes, 0.0);
    assert!(minutes(&state).groups.is_empty());
    submit_a_review(&state, "dana", &pr).await;
    let report = minutes(&state);
    assert_eq!(report.totals.changes, 0, "nothing has landed yet");
    assert!(
        report.totals.review_minutes > 0.0,
        "review time was charged: {:?}",
        report.totals
    );
    assert_eq!(report.totals.minutes, report.totals.review_minutes);
    assert_eq!(report.groups.len(), 1);
    assert_eq!(report.groups[0].group, format!("acme/{REPO}"));
    assert!(
        !serde_json::to_string(&report).unwrap().contains("dana"),
        "the report never names a person"
    );
}

#[tokio::test]
async fn the_agent_reviewer_reviewing_through_the_same_routes_is_not_human_time() {
    let (state, pr) = fixture();
    open_the_files_view(&state, "pr-redteam", &pr).await;
    submit_a_review(&state, "pr-redteam", &pr).await;
    let report = minutes(&state);
    assert_eq!(report.totals.minutes, 0.0);
    assert!(report.groups.is_empty());
}

#[tokio::test]
async fn an_anonymous_read_of_the_files_view_charges_nobody() {
    let (state, pr) = fixture();
    open_the_files_view(&state, crate::web::auth::ANONYMOUS_LOGIN, &pr).await;
    assert_eq!(minutes(&state).totals.minutes, 0.0);
}

#[test]
fn the_account_kind_a_mark_is_filtered_on_is_the_one_people_resolve() {
    let (state, _pr) = fixture();
    assert_eq!(
        crate::web::people::kind_of(&state, "pr-redteam")
            .unwrap()
            .kind,
        AccountKind::Service
    );
    assert_eq!(
        crate::web::people::kind_of(&state, "dana").unwrap().kind,
        AccountKind::Human
    );
}

/// A beat of an interactive session, as a reporter sends it.
fn beat(
    operator: Option<&str>,
    links: Option<crate::web::agent_presence::PresenceLinks>,
    state_now: crate::web::agent_presence::PresenceState,
    last_active_ms: i64,
) -> crate::web::agent_presence::PresenceRow {
    crate::web::agent_presence::PresenceRow {
        session_id: "s-1".to_string(),
        reporter: "dana".to_string(),
        agent: "claude".to_string(),
        model: None,
        host: None,
        location: None,
        worker: None,
        operator: operator.map(str::to_string),
        lane: None,
        branch: None,
        title: None,
        state: state_now,
        repos: vec![format!("acme/{REPO}")],
        families: vec!["acme".to_string()],
        started_ms: last_active_ms - 600_000,
        last_active_ms,
        ttl_seconds: 300,
        received_ms: last_active_ms,
        expires_ms: last_active_ms + 300_000,
        ended_ms: None,
        cost_usd: None,
        tokens: None,
        links,
    }
}

#[test]
fn an_interactive_session_a_person_drives_is_their_time_on_the_change() {
    let (state, _pr) = fixture();
    let links = crate::web::agent_presence::PresenceLinks {
        pr: Some(format!("acme/{REPO}#1")),
        todo: None,
        agent_run: None,
    };
    let now = chrono::Utc::now().timestamp_millis();
    let active = crate::web::agent_presence::PresenceState::Active;
    human_time::session_mark(
        &state,
        &beat(Some("dana"), Some(links.clone()), active, now),
    );
    human_time::session_mark(
        &state,
        &beat(Some("dana"), Some(links.clone()), active, now + 120_000),
    );
    let report = minutes(&state);
    assert_eq!(
        report.totals.session_minutes, 3.0,
        "a floor and two minutes"
    );

    // An unattended session, a session nobody named an operator for, and one
    // that names no change are nobody's recorded time.
    human_time::session_mark(
        &state,
        &beat(
            Some("dana"),
            Some(links.clone()),
            crate::web::agent_presence::PresenceState::Idle,
            now + 300_000,
        ),
    );
    human_time::session_mark(&state, &beat(None, Some(links), active, now + 420_000));
    human_time::session_mark(&state, &beat(Some("dana"), None, active, now + 540_000));
    assert_eq!(minutes(&state).totals.session_minutes, 3.0);
}

#[test]
fn a_todo_action_a_person_takes_is_triage_time_on_that_lot() {
    let (state, _pr) = fixture();
    human_time::todo_mark(&state, "acme", "20261013-1200-abc", "dana");
    human_time::todo_mark(&state, "acme", "20261013-1200-abc", "pr-redteam");
    let report = state
        .events
        .with_conn(|conn| {
            human_time::report(conn, None, whole_window(), By::Family, chrono::Utc::now())
        })
        .unwrap();
    assert_eq!(report.totals.triage_minutes, 1.0);
    assert_eq!(report.groups.len(), 1);
    assert_eq!(report.groups[0].group, "acme");
}

/// `alice/jeryu` with a `feature` branch one commit ahead of `main`, the
/// commit authored by dana, and an open pull request for it.
struct PushFixture {
    _storage: tempfile::TempDir,
    state: Arc<WebState>,
    base: String,
    head: String,
}

fn git(cwd: &std::path::Path, args: &[&str], email: &str) -> String {
    let output = crate::test_git::git_command()
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "author")
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_NAME", "author")
        .env("GIT_COMMITTER_EMAIL", email)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

impl PushFixture {
    fn new(author_email: &str) -> Self {
        let storage = tempfile::tempdir().expect("storage");
        let bare = storage.path().join("alice").join("jeryu.git");
        let work = storage.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let g = |args: &[&str]| git(&work, args, "base@example.com");
        g(&["init", "--quiet", "-b", "main"]);
        std::fs::write(work.join("shared.txt"), "one\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "--quiet", "-m", "base"]);
        let base = g(&["rev-parse", "HEAD"]);
        g(&["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(work.join("feature.txt"), "feature\n").unwrap();
        git(&work, &["add", "."], author_email);
        git(&work, &["commit", "--quiet", "-m", "by hand"], author_email);
        let head = git(&work, &["rev-parse", "HEAD"], author_email);
        g(&["checkout", "--quiet", "main"]);
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        g(&["clone", "--quiet", "--bare", ".", bare.to_str().unwrap()]);
        git(
            &bare,
            &["symbolic-ref", "HEAD", "refs/heads/main"],
            "base@example.com",
        );

        let core = ForgeCore::new();
        core.create_account("dana", "dana-password", UserRole::User)
            .unwrap();
        core.create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
        core.create_pull_request(
            "alice",
            "jeryu",
            "dana",
            CreatePullRequestRequest {
                title: "the feature".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some(head.clone()),
                base_sha: Some(base.clone()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
        let state = Arc::new(WebState::new_with_git_storage(
            core,
            storage.path().to_path_buf(),
        ));
        // dana proved the address the commit is authored with.
        let now = chrono::Utc::now().timestamp_millis();
        state
            .account_email
            .claim("dana", "dana@example.com", now)
            .unwrap();
        state
            .account_email
            .mark_verified("dana", "dana@example.com", now)
            .unwrap();
        Self {
            _storage: storage,
            state,
            base,
            head,
        }
    }

    fn push(&self) {
        human_time::record_push(
            &self.state,
            "alice",
            "jeryu",
            &[crate::ci_bridge::RefUpdate {
                ref_name: "refs/heads/feature".to_string(),
                old_oid: self.base.clone(),
                new_oid: self.head.clone(),
            }],
        );
    }

    fn commit_minutes(&self) -> f64 {
        self.state
            .events
            .with_conn(|conn| {
                human_time::report(conn, None, whole_window(), By::Repo, chrono::Utc::now())
            })
            .unwrap()
            .totals
            .commit_minutes
    }
}

#[test]
fn a_commit_a_person_pushed_to_a_pull_requests_branch_is_their_time() {
    let fixture = PushFixture::new("dana@example.com");
    fixture.push();
    assert_eq!(fixture.commit_minutes(), 1.0);
    // The same push again is the same commit, not more time.
    fixture.push();
    assert_eq!(fixture.commit_minutes(), 1.0);
}

#[test]
fn a_commit_no_account_proved_the_address_of_is_charged_to_nobody() {
    let fixture = PushFixture::new("runner@ci.invalid");
    fixture.push();
    assert_eq!(fixture.commit_minutes(), 0.0);
}
