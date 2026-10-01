//! `GET /api/v1/repos/:id/automation`: the repository page's one answer to
//! "what runs here, and where is this copied to?".
//!
//! The repositories, identities, hosts and deploy targets below are invented.

use super::*;
use crate::github_mirror::{GithubMirror, GithubMirrorTarget, MirrorSync};
use crate::web::control_plane::{
    DEPLOY_LABEL, GateRunnerHeartbeat, GateRunnerResult, GateRunnerStore,
};
use crate::web::mirror_reconcile::MirrorRepoState;

const OWNER: &str = "acme";
const NAME: &str = "widget-www";
const HEAD: &str = "1f0c9a4b2d7e6f5a8c3b1d0e9f8a7b6c5d4e3f21";

fn forge() -> ForgeCore {
    let core = ForgeCore::new();
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
    core.create_account("acme-admin", "admin-password", UserRole::Admin)
        .expect("create admin");
    core
}

fn check(core: &ForgeCore, name: &str, conclusion: CheckConclusion) {
    core.create_check_run(
        OWNER,
        NAME,
        CreateCheckRunRequest {
            name: name.to_string(),
            head_sha: HEAD.to_string(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(conclusion),
            details_url: Some(format!("https://forge.invalid/checks/{name}")),
            output: None,
        },
    )
    .expect("create check run");
}

fn automation_of(state: &WebState, login: &str, admin: bool) -> Value {
    let repo = crate::web::repositories::find_repo(state, &format!("{OWNER}/{NAME}"))
        .expect("repository resolves");
    let account = if admin {
        authenticated_admin_account(login)
    } else {
        authenticated_account(login)
    };
    serde_json::to_value(crate::web::repo_automation::automation_view(
        state, &repo, &account.0,
    ))
    .expect("view serializes")
}

/// The checks the forge itself reports, the contexts the default branch
/// requires (including one that has never reported), and the two identities
/// that review and merge — one of which cannot, because it holds no grant.
#[test]
fn automation_names_every_check_and_the_merge_identity_that_cannot_merge() {
    let core = forge();
    check(&core, "jankurai/proof", CheckConclusion::Success);
    check(&core, "jeryu/autonomy", CheckConclusion::Failure);
    core.set_branch_protection(
        OWNER,
        NAME,
        "main",
        SetBranchProtectionRequest {
            required_status_checks: vec!["jankurai/proof".to_string(), format!("{NAME}/required")],
            required_approving_review_count: 1,
            ..SetBranchProtectionRequest::default()
        },
    )
    .expect("protect main");
    core.create_account("pragent", "review-password", UserRole::User)
        .expect("create reviewer");
    core.create_account("jain-merge-bot", "merge-password", UserRole::User)
        .expect("create merger");
    core.grant_repo_access("acme-admin", "pragent", OWNER, NAME, RepoAccessLevel::Write)
        .expect("grant the reviewer");
    let state = WebState::new(core);

    let view = automation_of(&state, "acme-admin", true);
    assert_eq!(view["repo"], format!("{OWNER}/{NAME}"));
    assert_eq!(
        view["requiredContexts"],
        json!(["jankurai/proof", format!("{NAME}/required")])
    );

    let checks = view["checks"].as_array().expect("checks");
    let named = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("check {name} listed: {view}"))
            .clone()
    };
    // Required contexts first, then the rest by name.
    assert_eq!(checks[0]["name"], "jankurai/proof");
    assert_eq!(named("jankurai/proof")["required"], true);
    assert_eq!(named("jankurai/proof")["lastConclusion"], "success");
    assert_eq!(named("jankurai/proof")["lastHeadSha"], HEAD);
    assert_eq!(
        named("jankurai/proof")["detailsUrl"],
        "https://forge.invalid/checks/jankurai/proof"
    );
    // A check nobody requires is still shown: it runs here.
    assert_eq!(named("jeryu/autonomy")["required"], false);
    assert_eq!(named("jeryu/autonomy")["lastConclusion"], "failure");
    // A required context that has never reported is the interesting one.
    let missing = named(&format!("{NAME}/required"));
    assert_eq!(missing["state"], "missing");
    assert_eq!(missing["required"], true);
    assert!(missing["lastConclusion"].is_null(), "{missing}");

    let actors = view["actors"].as_array().expect("actors");
    let kind = |kind: &str| {
        actors
            .iter()
            .find(|actor| actor["kind"] == kind)
            .unwrap_or_else(|| panic!("a {kind} is listed: {view}"))
            .clone()
    };
    let reviewer = kind("reviewer");
    assert_eq!(reviewer["identity"], "pragent");
    assert_eq!(reviewer["grant"]["present"], true);
    assert_eq!(reviewer["grant"]["held"], "write");

    let merger = kind("merger");
    assert_eq!(merger["identity"], "jain-merge-bot");
    assert_eq!(merger["grant"]["present"], false);
    assert_eq!(merger["grant"]["required"], "write");
    assert!(merger["grant"]["held"].is_null(), "{merger}");
    let warning = merger["grant"]["warning"].as_str().expect("a warning");
    assert!(warning.contains("jain-merge-bot"), "{warning}");
    assert!(warning.contains("403"), "{warning}");
    assert_eq!(view["warnings"][0], warning);
}

/// The merge identity that holds its grant raises no warning at all: the page
/// must not cry wolf on a repository whose automation is wired correctly.
#[test]
fn a_granted_merge_identity_raises_no_warning() {
    let core = forge();
    core.create_account("jain-merge-bot", "merge-password", UserRole::User)
        .expect("create merger");
    core.grant_repo_access(
        "acme-admin",
        "jain-merge-bot",
        OWNER,
        NAME,
        RepoAccessLevel::Write,
    )
    .expect("grant the merger");
    let state = WebState::new(core);
    let view = automation_of(&state, "acme-admin", true);
    let merger = view["actors"]
        .as_array()
        .expect("actors")
        .iter()
        .find(|actor| actor["kind"] == "merger")
        .expect("a merger")
        .clone();
    assert_eq!(merger["grant"]["present"], true, "{view}");
    assert!(merger["grant"]["warning"].is_null(), "{merger}");
    assert_eq!(view["warnings"], json!([]), "{view}");
    // No identity on the forge at all is no actor, not a warning about one.
    assert!(
        !view["actors"]
            .as_array()
            .expect("actors")
            .iter()
            .any(|actor| actor["kind"] == "reviewer"),
        "{view}"
    );
}

/// A host deploy timer reports through the runner heartbeat with the `deploy`
/// label; the repository page shows what it last put where, and how that went.
#[test]
fn a_deploy_timer_reports_what_it_shipped_and_where() {
    let core = forge();
    let mut state = WebState::new(core);
    state.gate_runners = GateRunnerStore::with_reporters(["gatebot"]);
    let beat = GateRunnerHeartbeat {
        runner_id: "buildhost1/publish".to_string(),
        host: "buildhost1".to_string(),
        slot: 0,
        labels: vec![DEPLOY_LABEL.to_string()],
        interval_seconds: Some(300),
        current: None,
        last: Some(GateRunnerResult {
            repo: format!("{OWNER}/{NAME}"),
            pr: None,
            sha: HEAD.to_string(),
            recipe: "publish main".to_string(),
            conclusion: "deployed".to_string(),
            target: Some("edge-pages".to_string()),
            reason: None,
            seconds: 42,
            finished_at: chrono::Utc::now(),
        }),
        code: None,
        tools: Vec::new(),
    };
    state
        .gate_runners
        .record(beat, "gatebot", chrono::Utc::now())
        .expect("the deploy beat is accepted");

    let view = automation_of(&state, "acme-admin", true);
    let deployer = view["actors"]
        .as_array()
        .expect("actors")
        .iter()
        .find(|actor| actor["kind"] == "deployer")
        .unwrap_or_else(|| panic!("a deployer is listed: {view}"))
        .clone();
    assert_eq!(deployer["identity"], "buildhost1/publish");
    assert_eq!(deployer["state"], "online");
    assert_eq!(deployer["lastRun"]["conclusion"], "deployed");
    assert_eq!(deployer["lastRun"]["target"], "edge-pages");
    assert_eq!(deployer["lastRun"]["sha"], HEAD);

    // A runner that never touched this repository stays off its page.
    assert!(
        !view["actors"]
            .as_array()
            .expect("actors")
            .iter()
            .any(|actor| actor["identity"] == "buildhost1/slot0"),
        "{view}"
    );
}

/// The mirror section: where the copy lives, which refs travel, the sha it
/// holds, and — when it is not level with the forge — one sentence saying so.
#[test]
fn a_behind_mirror_names_its_target_last_pushed_sha_and_the_gap() {
    let mirror = GithubMirror::with_targets(
        [(
            format!("{OWNER}/{NAME}"),
            GithubMirrorTarget {
                github_slug: "acme-oss/widget-www".to_string(),
                branch: "main".to_string(),
                destination_override: None,
            },
        )]
        .into_iter()
        .collect(),
    );
    let state = WebState::new(forge()).with_github_mirror(Arc::new(mirror));
    state.mirror_state.record(MirrorRepoState {
        repo: format!("{OWNER}/{NAME}"),
        github_slug: "acme-oss/widget-www".to_string(),
        branch: "main".to_string(),
        state: MirrorSync::Behind,
        forge_head: Some(HEAD.to_string()),
        github_head: Some("9".repeat(40)),
        checked_at: "2026-09-30T12:00:00+00:00".to_string(),
        last_push_at: Some("2026-09-29T09:30:00+00:00".to_string()),
        github_only_commits: Vec::new(),
        tags_pushed: Vec::new(),
        tag_drift: Vec::new(),
        error: Some("push rejected: no key on this host".to_string()),
    });

    let view = automation_of(&state, "acme-admin", true);
    let mirrors = view["mirrors"].as_array().expect("mirrors");
    assert_eq!(mirrors.len(), 1, "{view}");
    let mirror = &mirrors[0];
    assert_eq!(mirror["target"], "https://github.com/acme-oss/widget-www");
    assert_eq!(mirror["direction"], "push");
    assert_eq!(mirror["refs"], json!(["refs/heads/main", "refs/tags/*"]));
    assert_eq!(mirror["state"], "behind");
    assert_eq!(mirror["behind"], true);
    assert_eq!(mirror["forgeHead"], HEAD);
    assert_eq!(mirror["lastPushedSha"], "9".repeat(40));
    assert_eq!(mirror["lastPushedAt"], "2026-09-29T09:30:00+00:00");
    assert_eq!(mirror["lastError"], "push rejected: no key on this host");
    let warnings = view["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.as_str().is_some_and(|w| w.contains("behind"))),
        "{view}"
    );

    // A repository with no mirror target says so with an empty list, not with
    // a row that claims a mirror nobody configured.
    let unmirrored = automation_of(&WebState::new(forge()), "acme-admin", true);
    assert_eq!(unmirrored["mirrors"], json!([]));
}

/// Grants are the page's answer to "who may act here", and core's rule is that
/// only a repository admin may read them. A reader sees the automation and no
/// grant list, and is told which it is.
#[test]
fn grants_are_listed_for_a_repository_admin_only() {
    let core = forge();
    core.create_account("dana", "reader-password", UserRole::User)
        .expect("create reader");
    core.grant_repo_access("acme-admin", "dana", OWNER, NAME, RepoAccessLevel::Read)
        .expect("grant the reader");
    let state = WebState::new(core);

    let admin_view = automation_of(&state, "acme-admin", true);
    assert_eq!(admin_view["grantsVisible"], true);
    let grants = admin_view["grants"].as_array().expect("grants");
    assert_eq!(grants.len(), 1, "{admin_view}");
    assert_eq!(grants[0]["login"], "dana");
    assert_eq!(grants[0]["access"], "read");
    assert_eq!(grants[0]["grantedBy"], "acme-admin");

    let reader_view = automation_of(&state, "dana", false);
    assert_eq!(reader_view["grantsVisible"], false);
    assert_eq!(reader_view["grants"], json!([]));
    // The automation itself stays readable: that is the point of the page.
    assert_eq!(reader_view["repo"], format!("{OWNER}/{NAME}"));
}

/// The route is mounted where the SPA looks for it, and an unknown repository
/// is a 404 rather than an empty page pretending nothing runs.
#[tokio::test]
async fn the_route_answers_for_a_repository_and_404s_for_the_rest() {
    let state = Arc::new(WebState::new(forge()));
    let response = crate::web::repo_automation::show(
        State(state.clone()),
        authenticated_admin_account("acme-admin"),
        AxumPath(format!("{OWNER}/{NAME}")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let missing = crate::web::repo_automation::show(
        State(state),
        authenticated_admin_account("acme-admin"),
        AxumPath(format!("{OWNER}/nothing-here")),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}
