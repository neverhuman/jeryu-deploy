use super::*;
use audit_queue::{
    AUDIT_QUEUE_CAPACITY, AuditQueue, AuditTicket, CLAIM_LEASE_SECONDS, EnqueueOutcome, skip_reason,
};
use chrono::{Duration, Utc};
use jeryu_core::CreateRepositoryRequest;
use jeryu_gitd::refs::GitRef;
use std::fs;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_out(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn init_version_repo(root: &Path) -> (String, String) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "ci@example.invalid"]);
    git(root, &["config", "user.name", "CI"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/demo\"]\n\n[workspace.package]\nversion = \"4.0.0\"\nedition = \"2024\"\nlicense = \"Apache-2.0\"\nrust-version = \"1.95\"\n",
    );
    write(
        root,
        "crates/demo/Cargo.toml",
        "[package]\nname = \"demo\"\nversion.workspace = true\nedition.workspace = true\nlicense.workspace = true\nrust-version.workspace = true\n\n[lib]\npath = \"src/lib.rs\"\n",
    );
    write(root, "crates/demo/src/lib.rs", "pub fn demo() {}\n");
    write(
        root,
        "CHANGELOG.md",
        "# Changelog\n\n## Unreleased\n\n- seed\n",
    );
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "chore: base"]);
    let base = git_out(root, &["rev-parse", "HEAD"]);

    write(root, "docs/feature.md", "feature\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "feat: add dashboard signal"]);
    let head = git_out(root, &["rev-parse", "HEAD"]);
    (base, head)
}

fn clone_bare(work: &Path, bare: &Path) {
    let bare_str = bare.to_string_lossy().to_string();
    git(work, &["clone", "--bare", "--no-local", ".", &bare_str]);
}

fn install_main_blocking_hook(bare: &Path) {
    let hook = bare.join("hooks").join("pre-receive");
    fs::create_dir_all(hook.parent().expect("hook parent")).unwrap();
    fs::write(
        &hook,
        "#!/usr/bin/env bash\nset -euo pipefail\nwhile read -r _old _new ref; do\n  if [[ \"$ref\" == \"refs/heads/main\" ]]; then\n    echo 'direct main push blocked by test hook' >&2\n    exit 1\n  fi\ndone\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn ref_update(ref_name: &str, previous_oid: &str, new_oid: &str) -> RefUpdate {
    RefUpdate {
        ref_name: ref_name.to_owned(),
        old_oid: previous_oid.to_owned(),
        new_oid: new_oid.to_owned(),
    }
}

#[test]
fn malformed_or_nonzero_jankurai_reports_are_never_green() {
    let valid = serde_json::json!({
        "score": 92,
        "caps_applied": [],
        "decision": {"hard_findings": 0, "minimum_score": 85}
    });
    let (request, pass) = jankurai_score_request("main", "abc", Some(valid.clone()), 0);
    assert!(pass);
    assert_eq!(request.decision, "scored");

    let hostile_reports = [
        (valid, 9),
        (
            serde_json::json!({
                "score": 92,
                "caps_applied": [],
                "decision": {"minimum_score": 85}
            }),
            0,
        ),
        (
            serde_json::json!({
                "score": 101,
                "caps_applied": [],
                "decision": {"hard_findings": 0, "minimum_score": 85}
            }),
            0,
        ),
        (
            serde_json::json!({
                "score": 92,
                "caps_applied": [7],
                "decision": {"hard_findings": 0, "minimum_score": 85}
            }),
            0,
        ),
    ];
    for (report, exit_code) in hostile_reports {
        let (request, pass) = jankurai_score_request("main", "abc", Some(report), exit_code);
        assert!(!pass);
        assert_eq!(request.decision, "tool-failed");
        assert_eq!(request.score, None);
    }

    let (request, pass) = jankurai_score_request(
        "main",
        "abc",
        Some(serde_json::json!({
            "score": 80,
            "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 85}
        })),
        0,
    );
    assert!(!pass);
    assert_eq!(
        request.decision, "scored",
        "a valid red audit is not a tool error"
    );
    assert_eq!(request.score, Some(80));

    let (request, pass) = jankurai_score_request(
        "main",
        "abc",
        Some(serde_json::json!({
            "score": 80,
            "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 0}
        })),
        0,
    );
    assert!(!pass, "candidate policy cannot lower the host score floor");
    assert_eq!(request.decision, "scored");
}

#[test]
fn branch_head_skips_tag_only_release_workflow() {
    let workflow = r#"
name: release
on:
  push:
    tags: ['v*']
  workflow_dispatch:
jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - run: bash ops/ci/release.sh
"#;

    assert!(!workflow_runs_for_branch_head(
        workflow,
        "refs/heads/codex/feature"
    ));
}

#[test]
fn branch_head_runs_pull_request_workflow_even_with_main_push_filter() {
    let workflow = r#"
name: web
on:
  push:
    branches:
      - main
  pull_request:
jobs:
  web:
    runs-on: ubuntu-latest
    steps:
      - run: bash ops/ci/web.sh
"#;

    assert!(workflow_runs_for_branch_head(
        workflow,
        "refs/heads/codex/feature"
    ));
}

#[test]
fn branch_push_filter_matches_only_named_branches() {
    let workflow = r#"
name: branch-only
on:
  push:
    branches: [main, release/*]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - run: true
"#;

    assert!(workflow_runs_for_branch_head(workflow, "refs/heads/main"));
    assert!(workflow_runs_for_branch_head(
        workflow,
        "refs/heads/release/next"
    ));
    assert!(!workflow_runs_for_branch_head(
        workflow,
        "refs/heads/codex/feature"
    ));
}

#[test]
fn inline_on_list_runs_for_branch_push() {
    let workflow = r#"
name: ci
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - run: true
"#;

    assert!(workflow_runs_for_branch_head(
        workflow,
        "refs/heads/codex/feature"
    ));
}

#[test]
fn workflow_toolchain_bootstrap_step_is_skipped_locally() {
    assert!(is_workflow_toolchain_bootstrap(
        "rustup toolchain install 1.95.0 --profile minimal"
    ));
    assert!(!is_workflow_toolchain_bootstrap(
        "rustup toolchain install 1.95.0 --profile minimal\ncargo test"
    ));
    assert!(!is_workflow_toolchain_bootstrap("bash ops/ci/ci-fast.sh"));
}

#[test]
fn ref_updates_track_ref_name_and_previous_oid() {
    let before = vec![
        GitRef {
            name: "refs/heads/main".to_owned(),
            oid: "aaa".to_owned(),
        },
        GitRef {
            name: "refs/heads/feature".to_owned(),
            oid: "bbb".to_owned(),
        },
    ];
    let after = vec![
        GitRef {
            name: "refs/heads/main".to_owned(),
            oid: "ccc".to_owned(),
        },
        GitRef {
            name: "refs/heads/feature".to_owned(),
            oid: "bbb".to_owned(),
        },
    ];

    let updates = ref_updates(&before, &after);

    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].ref_name, "refs/heads/main");
    assert_eq!(updates[0].old_oid, "aaa");
    assert_eq!(updates[0].new_oid, "ccc");
}

#[test]
fn ref_updates_diff_a_large_snapshot_by_name_not_by_position() {
    let before: Vec<GitRef> = (0..500)
        .map(|index| GitRef {
            name: format!("refs/heads/branch-{index}"),
            oid: format!("oid-{index}"),
        })
        .collect();
    // Reversed order, one moved tip, one new branch, one deleted branch and a
    // tag: the diff must key on the ref name alone.
    let mut after: Vec<GitRef> = before.iter().rev().cloned().collect();
    after.retain(|r| r.name != "refs/heads/branch-7");
    after
        .iter_mut()
        .find(|r| r.name == "refs/heads/branch-42")
        .unwrap()
        .oid = "moved".to_owned();
    after.push(GitRef {
        name: "refs/heads/fresh".to_owned(),
        oid: "new-oid".to_owned(),
    });
    after.push(GitRef {
        name: "refs/tags/v1".to_owned(),
        oid: "tag-oid".to_owned(),
    });

    let updates = ref_updates(&before, &after);

    let mut named: Vec<(String, String, String)> = updates
        .into_iter()
        .map(|u| (u.ref_name, u.old_oid, u.new_oid))
        .collect();
    named.sort();
    assert_eq!(
        named,
        vec![
            (
                "refs/heads/branch-42".to_owned(),
                "oid-42".to_owned(),
                "moved".to_owned()
            ),
            (
                "refs/heads/fresh".to_owned(),
                ZERO_OID.to_owned(),
                "new-oid".to_owned()
            ),
        ]
    );
}

fn assert_reviewed_main_preserved(explicit_version: bool) {
    use jeryu_core::{CreateReviewRequest, ReviewState};
    use jeryu_gitd::GitdConfig;
    use std::sync::Arc;

    let work = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (base, mut head) = init_version_repo(work.path());
    if explicit_version {
        let manifest = fs::read_to_string(work.path().join("Cargo.toml")).unwrap();
        write(
            work.path(),
            "Cargo.toml",
            &manifest.replace("4.0.0", "4.1.0"),
        );
        write(
            work.path(),
            "CHANGELOG.md",
            "# Changelog\n\n## v4.1.0\n\n- Reviewed feature.\n",
        );
        git(work.path(), &["add", "Cargo.toml", "CHANGELOG.md"]);
        git(
            work.path(),
            &["commit", "-q", "-m", "chore(release): prepare v4.1.0"],
        );
        head = git_out(work.path(), &["rev-parse", "HEAD"]);
    }
    // Neither candidate uses the legacy recursion marker: preservation must
    // follow from the bridge's behavior, including an explicit version update.
    assert!(!git_out(work.path(), &["log", "-1", "--format=%s"]).contains("[skip-version]"));
    git(work.path(), &["tag", "demo-v4.0.0-split.0", &base]);
    let manager = Arc::new(RepoManager::new(GitdConfig::new(storage.path())));
    let bare = storage.path().join("jeryu/demo.git");
    fs::create_dir_all(bare.parent().unwrap()).unwrap();
    clone_bare(work.path(), &bare);
    git(&bare, &["update-ref", "refs/heads/feature", &head]);
    git(&bare, &["update-ref", "refs/heads/main", &base, &head]);
    install_main_blocking_hook(&bare);
    let direct = Command::new("git")
        .current_dir(work.path())
        .args(["push", bare.to_str().unwrap(), "HEAD:refs/heads/main"])
        .output()
        .unwrap();
    assert!(
        !direct.status.success(),
        "direct main push must be rejected"
    );
    assert!(String::from_utf8_lossy(&direct.stderr).contains("direct main push blocked"));

    let router =
        crate::GithubRouter::with_core(ForgeCore::new()).with_repo_manager(manager.clone());
    let created = router.post(
        "/repos",
        r#"{"owner":"jeryu","name":"demo","default_branch":"main"}"#,
    );
    assert_eq!(created.status, 201, "{}", created.body);
    let opened = router.post(
        "/repos/jeryu/demo/pulls",
        &serde_json::json!({
            "title": "Reviewed feature", "head": "feature", "base": "main",
            "head_sha": head, "base_sha": base, "actor": "author"
        })
        .to_string(),
    );
    assert_eq!(opened.status, 201, "{}", opened.body);
    let number = serde_json::from_str::<serde_json::Value>(&opened.body).unwrap()["number"]
        .as_u64()
        .unwrap();
    let protected = router.put("/repos/jeryu/demo/branches/main/protection",
        r#"{"required_approving_review_count":1,"required_status_checks":["demo/required"],"enforce_admins":true,"required_linear_history":true}"#);
    assert_eq!(protected.status, 200, "{}", protected.body);
    let policy: serde_json::Value = serde_json::from_str(&protected.body).unwrap();
    assert_eq!(
        policy["required_pull_request_reviews"]["required_approving_review_count"],
        1
    );
    assert_eq!(
        policy["required_status_checks"]["contexts"],
        serde_json::json!(["demo/required"])
    );
    assert_eq!(policy["enforce_admins"]["enabled"], true);
    assert_eq!(policy["required_linear_history"]["enabled"], true);
    let merge_path = format!("/repos/jeryu/demo/pulls/{number}/merge");
    let unapproved = router.put(&merge_path, "{}");
    assert_ne!(
        unapproved.status, 200,
        "unapproved candidate must be blocked"
    );
    assert_eq!(git_out(&bare, &["rev-parse", "refs/heads/main"]), base);
    router
        .core()
        .create_review(
            "jeryu",
            "demo",
            number,
            "reviewer",
            CreateReviewRequest {
                body: None,
                event: ReviewState::Approved,
                comments: vec![],
                expected_head_sha: Some(head.clone()),
            },
        )
        .unwrap();
    let unchecked = router.put(&merge_path, "{}");
    assert_ne!(
        unchecked.status, 200,
        "candidate without its required check must be blocked"
    );
    assert_eq!(git_out(&bare, &["rev-parse", "refs/heads/main"]), base);
    // In-process fixture evidence exercises the real protection decision; it is
    // never published to a forge or reported as a production required check.
    router
        .core()
        .create_check_run(
            "jeryu",
            "demo",
            CreateCheckRunRequest {
                name: "demo/required".to_string(),
                head_sha: head.clone(),
                status: Some(CheckRunStatus::Completed),
                conclusion: Some(CheckConclusion::Success),
                ..Default::default()
            },
        )
        .unwrap();
    let merged = router.put(&merge_path, "{}");
    assert_eq!(merged.status, 200, "{}", merged.body);
    let response: serde_json::Value = serde_json::from_str(&merged.body).unwrap();
    assert_eq!(response["sha"], head);
    assert_eq!(
        git_out(&bare, &["rev-parse", "refs/heads/main"]),
        head,
        "the post-merge push bridge must not append an unreviewed version commit"
    );
    assert_eq!(
        git_out(&bare, &["rev-parse", "refs/heads/main^{tree}"]),
        git_out(work.path(), &["rev-parse", "HEAD^{tree}"])
    );
    for path in ["Cargo.toml", "CHANGELOG.md"] {
        assert_eq!(
            git_out(&bare, &["show", &format!("refs/heads/main:{path}")]),
            fs::read_to_string(work.path().join(path)).unwrap().trim()
        );
    }
    let final_pr = router.get(&format!("/repos/jeryu/demo/pulls/{number}"));
    assert_eq!(final_pr.status, 200, "{}", final_pr.body);
    let final_pr: serde_json::Value = serde_json::from_str(&final_pr.body).unwrap();
    assert_eq!(final_pr["merged"], true);
    assert_eq!(final_pr["merge_commit_sha"], head);
    let refs = git_out(&bare, &["show-ref"]);
    // A duplicate callback must also leave every branch and immutable tag at
    // the same objects, even though advisory check recording may repeat.
    on_push(
        router.core(),
        &manager,
        "jeryu",
        "demo",
        &[ref_update("refs/heads/main", &base, &head)],
        "http://127.0.0.1:8787",
    );
    assert_eq!(git_out(&bare, &["show-ref"]), refs);
    assert_eq!(
        git_out(&bare, &["rev-parse", "refs/tags/demo-v4.0.0-split.0"]),
        base
    );
}

#[test]
fn reviewed_main_feature_stays_at_exact_approved_head() {
    assert_reviewed_main_preserved(false);
}

#[test]
fn reviewed_main_explicit_version_stays_at_exact_approved_head() {
    assert_reviewed_main_preserved(true);
}

#[test]
fn mock_flag_gates_workflow_check_run_seeding() {
    // The production forge (no JERYU_CI_MOCK) must NOT seed GitHub Actions check-runs
    // — it has no Actions runners, so they only produced all-red noise; host-ci's
    // `jeryu/ci` is the real gate. Only the in-process CI-seeding-flow tests opt in.
    // Pure predicate, so this never mutates the shared process env (which would race
    // the parallel seeded-CI tests that read JERYU_CI_MOCK).
    assert!(
        !mock_flag_set(None),
        "unset -> production posture, no seeding"
    );
    assert!(!mock_flag_set(Some("")));
    assert!(!mock_flag_set(Some("0")));
    assert!(!mock_flag_set(Some("  0  ")));
    assert!(mock_flag_set(Some("1")), "opt-in for tests");
    assert!(mock_flag_set(Some("true")));
}

// --- the audit queue a push writes ---

fn demo_core() -> ForgeCore {
    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "demo".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core
}

fn ticket(branch: &str, head: &str) -> AuditTicket {
    AuditTicket::new("jeryu", "demo", branch, head, "0".repeat(40).as_str())
}

/// The push path writes work and publishes a pending proof. It runs no
/// auditor: nothing here clones, checks out, or scores the head.
#[test]
fn a_pushed_main_queues_one_audit_and_leaves_the_proof_pending() {
    let work = tempfile::tempdir().unwrap();
    let bare = tempfile::tempdir().unwrap();
    let (base, head) = init_version_repo(work.path());
    clone_bare(work.path(), bare.path());
    let core = demo_core();

    queue_head_audit(
        &core,
        "git",
        bare.path(),
        "jeryu",
        "demo",
        &ref_update("refs/heads/main", &base, &head),
        "http://forge.test",
    );

    let queued: Vec<AuditTicket> = audit_queue::queue()
        .lock()
        .unwrap()
        .tickets()
        .iter()
        .filter(|ticket| ticket.head_sha == head)
        .cloned()
        .collect();
    assert_eq!(queued.len(), 1, "one ticket per head");
    assert_eq!(queued[0].branch, "main");
    assert_eq!(queued[0].base_sha, base);
    assert!(queued[0].claimed_by.is_none());

    assert!(
        core.list_jankurai_scores("jeryu", "demo", None, Some(&head))
            .unwrap()
            .is_empty(),
        "the forge records no score of its own"
    );
    let checks = core.list_check_runs("jeryu", "demo", Some(&head)).unwrap();
    let proof = checks
        .check_runs
        .iter()
        .find(|check| check.name == JANKURAI_PROOF_CHECK)
        .expect("a pending proof is published");
    assert_eq!(proof.status, CheckRunStatus::InProgress);
    assert_eq!(proof.conclusion, None, "it must never go green by default");
    assert_eq!(
        proof.output.as_ref().unwrap().title,
        "queued for a gate runner"
    );

    // A repeated push callback for the same head adds no second job.
    queue_head_audit(
        &core,
        "git",
        bare.path(),
        "jeryu",
        "demo",
        &ref_update("refs/heads/main", &base, &head),
        "http://forge.test",
    );
    let still_one = audit_queue::queue()
        .lock()
        .unwrap()
        .tickets()
        .iter()
        .filter(|ticket| ticket.head_sha == head)
        .count();
    assert_eq!(still_one, 1);
    audit_queue::queue()
        .lock()
        .unwrap()
        .take("jeryu", "demo", &head);
}

/// A branch with no merge-base is not audited against the empty tree: its
/// check says so, and it is neither a tool failure nor a job.
#[test]
fn a_head_without_a_base_is_not_audited_at_all() {
    let work = tempfile::tempdir().unwrap();
    let bare = tempfile::tempdir().unwrap();
    let (_, head) = init_version_repo(work.path());
    clone_bare(work.path(), bare.path());
    git(bare.path(), &["update-ref", "refs/heads/orphan", &head]);
    git(bare.path(), &["update-ref", "-d", "refs/heads/main"]);
    let core = demo_core();
    core.create_pull_request(
        "jeryu",
        "demo",
        "author",
        jeryu_core::CreatePullRequestRequest {
            title: "orphan".to_string(),
            head: "orphan".to_string(),
            base: "main".to_string(),
            head_sha: Some(head.clone()),
            base_sha: Some(head.clone()),
            ..Default::default()
        },
    )
    .unwrap();

    queue_head_audit(
        &core,
        "git",
        bare.path(),
        "jeryu",
        "demo",
        &ref_update("refs/heads/orphan", ZERO_OID, &head),
        "http://forge.test",
    );

    assert!(
        !audit_queue::queue()
            .lock()
            .unwrap()
            .tickets()
            .iter()
            // The queue is process-wide and other tests queue this same fixture
            // head on their own branches, so look at this test's branch only.
            .any(|ticket| ticket.branch == "orphan" && ticket.head_sha == head),
        "a baseless head queues no whole-repository audit"
    );
    let checks = core.list_check_runs("jeryu", "demo", Some(&head)).unwrap();
    let proof = checks
        .check_runs
        .iter()
        .find(|check| check.name == JANKURAI_PROOF_CHECK)
        .expect("the head still gets a proof");
    assert_eq!(proof.conclusion, Some(CheckConclusion::Neutral));
    assert_eq!(proof.output.as_ref().unwrap().title, "no base branch yet");
}

/// Only pull request heads and the protected main are audited.
#[test]
fn only_pull_heads_and_the_protected_main_are_audited() {
    for branch in [
        "import/2026-09-28",
        "preserve/history",
        "archive/old",
        "archives/older",
        "bot/auto-pin",
        "auto/pin-web-0a7480c6fa87",
        "import",
        "archive",
    ] {
        assert!(
            skip_reason(branch, false).is_some(),
            "{branch} creates no audit job"
        );
        assert!(
            skip_reason(branch, true).is_some(),
            "{branch} creates no audit job even with a pull request"
        );
    }
    assert_eq!(skip_reason("main", false), None);
    assert_eq!(skip_reason("codex/feature", true), None);
    assert!(
        skip_reason("codex/feature", false).is_some(),
        "a branch nobody opened a pull request for is not audited"
    );
}

/// The queue keeps one ticket per head, drops superseded tips, hands claimed
/// work to one runner at a time, and recovers a lease a runner abandoned.
#[test]
fn the_queue_deduplicates_supersedes_and_recovers_leases() {
    let mut queue = AuditQueue::default();
    assert_eq!(
        queue.enqueue(ticket("codex/feature", &"a".repeat(40))),
        EnqueueOutcome::Queued
    );
    assert_eq!(
        queue.enqueue(ticket("codex/feature", &"a".repeat(40))),
        EnqueueOutcome::AlreadyQueued
    );
    assert_eq!(
        queue.enqueue(ticket("codex/feature", &"b".repeat(40))),
        EnqueueOutcome::Superseded(1),
        "only the branch tip is worth auditing"
    );
    assert_eq!(queue.tickets().len(), 1);

    let claimed = queue.claim("xbabe2/slot0", 4);
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].head_sha, "b".repeat(40));
    assert!(
        queue.claim("xbabe2/slot1", 4).is_empty(),
        "claimed work is not handed out twice"
    );

    // A claimed ticket is running work, so a newer tip does not evict it.
    assert_eq!(
        queue.enqueue(ticket("codex/feature", &"c".repeat(40))),
        EnqueueOutcome::Queued
    );
    assert_eq!(queue.tickets().len(), 2);

    // A runner that never reported loses the lease and the head is claimable
    // again, so one lost runner does not strand a head as pending forever.
    for ticket in queue.tickets_for_test() {
        ticket.claimed_at = Some(Utc::now() - Duration::seconds(CLAIM_LEASE_SECONDS + 1));
    }
    let reclaimed = queue.claim("xbabe2/slot1", 4);
    assert_eq!(reclaimed.len(), 2);

    let taken = queue.take("jeryu", "demo", &"b".repeat(40)).unwrap();
    assert_eq!(taken.branch, "codex/feature");
    assert!(queue.take("jeryu", "demo", &"b".repeat(40)).is_none());
}

/// A bulk push cannot grow the forge without bound; the oldest unclaimed work
/// is dropped, and those heads keep a pending proof rather than a green one.
#[test]
fn the_queue_is_bounded() {
    let mut queue = AuditQueue::default();
    for index in 0..(AUDIT_QUEUE_CAPACITY + 50) {
        queue.enqueue(ticket(
            &format!("codex/branch-{index}"),
            &format!("{index:040x}"),
        ));
    }
    assert_eq!(queue.tickets().len(), AUDIT_QUEUE_CAPACITY);
}

/// Only the governed auditor's own report is authoritative.
#[test]
fn a_report_from_another_binary_is_refused() {
    let (version, sha256) = governed_auditor_identity().expect("the pinned receipt agrees");
    assert!(verify_reported_auditor(&version, &sha256).is_ok());
    assert!(verify_reported_auditor(&version, &sha256.to_uppercase()).is_ok());
    assert!(verify_reported_auditor("jankurai 1.6.10", &sha256).is_err());
    assert!(verify_reported_auditor(&version, &"0".repeat(64)).is_err());
    assert!(verify_reported_auditor("", "").is_err());
}

/// A runner's report becomes the head's score and completes the proof — the
/// forge reads the verdict out of the report rather than taking one on faith.
#[test]
fn a_recorded_report_completes_the_proof_from_the_report_itself() {
    let core = demo_core();
    let head = "d".repeat(40);
    let score = record_audited_head(
        &core,
        &AuditedHead {
            owner: "jeryu",
            repo: "demo",
            branch: "main",
            head_sha: &head,
            origin_base_url: "http://forge.test",
        },
        Some(serde_json::json!({
            "score": 92,
            "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 85}
        })),
        0,
    )
    .unwrap();
    assert_eq!(score.score, Some(92));
    assert_eq!(score.decision, "scored");

    let checks = core.list_check_runs("jeryu", "demo", Some(&head)).unwrap();
    let proof = checks
        .check_runs
        .iter()
        .find(|check| check.name == JANKURAI_PROOF_CHECK)
        .unwrap();
    assert_eq!(proof.status, CheckRunStatus::Completed);
    assert_eq!(proof.conclusion, Some(CheckConclusion::Success));
    assert_eq!(
        proof.details_url.as_deref(),
        Some(format!("https://forge.test/quality-gate/heads/jeryu/demo/{head}").as_str())
    );

    // A head that already carries a score is not queued again, whoever scored
    // it: one audit per head across the forge and `<repo>/required`.
    let work = tempfile::tempdir().unwrap();
    let bare = tempfile::tempdir().unwrap();
    let (base, _) = init_version_repo(work.path());
    clone_bare(work.path(), bare.path());
    queue_head_audit(
        &core,
        "git",
        bare.path(),
        "jeryu",
        "demo",
        &ref_update("refs/heads/main", &base, &head),
        "http://forge.test",
    );
    assert!(
        !audit_queue::queue()
            .lock()
            .unwrap()
            .tickets()
            .iter()
            .any(|ticket| ticket.head_sha == head)
    );
}

/// The bulk push that wedged the forge: hundreds of import branches in one
/// go. Every head is decided from the refs alone, so nothing clones, nothing
/// checks out, and nothing audits — the queue stays empty and the sandbox
/// directory an audit would have needed is never created.
#[test]
fn a_bulk_push_of_two_hundred_branches_runs_no_audit_on_the_forge() {
    let work = tempfile::tempdir().unwrap();
    let bare = tempfile::tempdir().unwrap();
    let (_, head) = init_version_repo(work.path());
    clone_bare(work.path(), bare.path());
    let core = demo_core();

    let started = std::time::Instant::now();
    let wall_clock_start = std::time::SystemTime::now();
    for index in 0..220 {
        queue_head_audit(
            &core,
            "git",
            bare.path(),
            "jeryu",
            "demo",
            &ref_update(&format!("refs/heads/import/batch-{index}"), ZERO_OID, &head),
            "http://forge.test",
        );
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "a bulk push must not do per-head work"
    );

    assert!(
        !audit_queue::queue()
            .lock()
            .unwrap()
            .tickets()
            .iter()
            // The queue is process-wide and other tests queue `jeryu/demo` heads
            // in parallel, so only this test's import branches count.
            .any(|ticket| ticket.owner == "jeryu"
                && ticket.repo == "demo"
                && ticket.branch.starts_with("import/batch-")),
        "imported branches queue no audit"
    );
    assert_eq!(
        core.list_check_runs("jeryu", "demo", Some(&head))
            .unwrap()
            .total_count,
        0,
        "and publish no check of their own"
    );
    let sandboxes = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("jeryu-jankurai-")
        })
        // Only what this push created: an earlier build of the forge left its
        // own audit sandboxes in the shared temp directory.
        .filter(|entry| {
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified >= wall_clock_start)
        })
        .count();
    assert_eq!(sandboxes, 0, "the forge materializes no audit sandbox");
}

/// A runner's report that produced no score still says why, and a second
/// submission of the same verdict for the same head posts nothing new:
/// `<repo>/required` and a runner may both report a head, and a head keeps one
/// `jankurai/proof`.
#[test]
fn a_failed_report_names_its_reason_and_posts_one_proof_per_head() {
    let core = demo_core();
    let head = "f".repeat(40);
    let submit = || {
        record_audited_head(
            &core,
            &AuditedHead {
                owner: "jeryu",
                repo: "proof-once",
                branch: "main",
                head_sha: &head,
                origin_base_url: "http://forge.test",
            },
            None,
            3,
        )
        .unwrap()
    };
    let first = submit();
    let second = submit();
    assert_eq!(first.decision, "tool-failed");
    assert_eq!(second.decision, "tool-failed");

    let checks = core
        .list_check_runs("jeryu", "proof-once", Some(&head))
        .unwrap();
    let proofs: Vec<_> = checks
        .check_runs
        .iter()
        .filter(|check| check.name == JANKURAI_PROOF_CHECK)
        .collect();
    assert_eq!(proofs.len(), 1, "one jankurai/proof per head, not one per report");
    let proof = proofs[0];
    assert_eq!(proof.conclusion, Some(CheckConclusion::Failure));
    assert_eq!(
        proof.details_url.as_deref(),
        Some(format!("https://forge.test/quality-gate/heads/jeryu/proof-once/{head}").as_str()),
        "the proof must link the report over a public https page"
    );
    let output = proof.output.as_ref().expect("proof check carries output");
    assert_eq!(
        output.title,
        "jankurai audit produced no score: the audit wrote no report at all"
    );
    assert!(output.summary.contains("exit 3"), "{}", output.summary);
}

#[test]
fn tool_failed_proofs_explain_every_way_the_audit_can_produce_no_score() {
    // A reason the host recorded travels to the title, the summary and the text.
    let reason = "the auditor exited 101: thread panicked".to_string();
    let (request, pass) =
        jankurai_score_request_with_reason("shift/2026-09-29", "abc", None, -1, Some(reason.clone()));
    assert!(!pass);
    let output = jankurai_proof_output(&request, pass);
    assert!(output.title.contains(&reason), "{}", output.title);
    assert!(output.summary.contains(&reason), "{}", output.summary);
    assert!(output.text.is_some_and(|text| text.contains("thread panicked")));

    // An audit that wrote a report the host cannot read says that instead of
    // repeating the bare decision word.
    let (request, pass) = jankurai_score_request_with_reason(
        "main",
        "abc",
        Some(serde_json::json!({"score": 101})),
        0,
        None,
    );
    let output = jankurai_proof_output(&request, pass);
    assert!(
        output.title.contains("not a valid diff-score JSON"),
        "{}",
        output.title
    );

    // No report and no reason at all still names the fact.
    let (request, pass) = jankurai_score_request("main", "abc", None, 3);
    let output = jankurai_proof_output(&request, pass);
    assert_eq!(
        output.title,
        "jankurai audit produced no score: the audit wrote no report at all"
    );
    assert!(output.summary.contains("exit 3"), "{}", output.summary);
    assert!(output.summary.contains("tool-failed"), "{}", output.summary);
}

#[test]
fn a_below_floor_proof_lists_the_findings_a_reader_must_fix() {
    let report = serde_json::json!({
        "score": 47,
        "caps_applied": ["missing-agent-readable-docs"],
        "decision": {"hard_findings": 0, "minimum_score": 85},
        "findings": [
            {"rule_id": "authz-or-data-isolation-gap", "path": "crates/jeryu-api/src/web.rs",
             "line": 412, "problem": "the route reads another account's rows"}
        ]
    });
    let (request, pass) = jankurai_score_request("main", "abc", Some(report), 0);
    assert!(!pass);
    let output = jankurai_proof_output(&request, pass);
    assert_eq!(output.title, "score 47 < floor 85");
    assert!(
        output
            .summary
            .contains("caps applied: missing-agent-readable-docs"),
        "{}",
        output.summary
    );
    let text = output.text.expect("a failing proof lists its findings");
    assert!(
        text.contains("authz-or-data-isolation-gap at crates/jeryu-api/src/web.rs:412"),
        "{text}"
    );
}

#[test]
fn proof_links_the_public_report_page_and_never_a_local_address() {
    let path = "/quality-gate/heads/veox-ai/veox-telemetry/0d63244";
    assert_eq!(
        proof_details_url(
            Some("https://git.neverhuman.org"),
            "http://127.0.0.1:8787",
            path
        )
        .as_deref(),
        Some("https://git.neverhuman.org/quality-gate/heads/veox-ai/veox-telemetry/0d63244"),
        "the configured public origin wins over the pushing client's Host"
    );
    assert_eq!(
        proof_details_url(None, "http://git.neverhuman.org", path).as_deref(),
        Some("https://git.neverhuman.org/quality-gate/heads/veox-ai/veox-telemetry/0d63244")
    );
    for local in [
        "http://127.0.0.1:8787",
        "https://localhost:8787",
        "http://[::1]:8787",
        "http://0.0.0.0",
        "",
    ] {
        assert_eq!(
            proof_details_url(None, local, path),
            None,
            "a link only this host can open is not a report link: {local}"
        );
    }
}

/// The local pre-approval gate (`ops/ci/jankurai-gate.sh`) and the hosted
/// `jankurai/proof` must reach the same verdict, in the same words, from the
/// same report — that is the whole promise of running it before the PR. The
/// script renders the report; this compares what it printed against what the
/// hosted check would carry for the same report.
#[cfg(unix)]
#[test]
fn jankurai_gate_script_prints_the_hosted_proof_verdict() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let script = repo_root.join("ops/ci/jankurai-gate.sh");
    let sandbox = tempfile::tempdir().unwrap();

    let cases = [
        serde_json::json!({
            "score": 92, "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 85}
        }),
        // Below the floor, with the findings a reader must fix.
        serde_json::json!({
            "score": 47, "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 85},
            "findings": (0..7).map(|index| serde_json::json!({
                "rule_id": "dead-language", "path": format!("src/a{index}.rs"),
                "line": index + 1, "problem": "the word is not neutral"
            })).collect::<Vec<_>>()
        }),
        // Over the floor but capped, and over it with a hard finding.
        serde_json::json!({
            "score": 96, "caps_applied": ["missing-agent-readable-docs", "no-proof-lanes"],
            "decision": {"hard_findings": 0, "minimum_score": 85}
        }),
        serde_json::json!({
            "score": 96, "caps_applied": [],
            "decision": {"hard_findings": 2, "minimum_score": 85}
        }),
        // A repository floor stricter than the host's own wins; a laxer one loses.
        serde_json::json!({
            "score": 92, "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 95}
        }),
        serde_json::json!({
            "score": 80, "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 60}
        }),
        // An audit whose report the host cannot read fails closed, with the reason.
        serde_json::json!({"score": 101}),
        serde_json::json!({"host_error": "the auditor exited 101: thread panicked"}),
    ];

    for (index, report) in cases.iter().enumerate() {
        let path = sandbox.path().join(format!("report-{index}.json"));
        fs::write(&path, serde_json::to_vec(report).unwrap()).unwrap();
        let output = Command::new("bash")
            .arg(&script)
            .arg("--report")
            .arg(&path)
            // The rollout switch decides whether a failing verdict refuses the
            // PR; this asserts the verdict itself, so the gate is on.
            .env("JERYU_JANKURAI_GATE", "1")
            .current_dir(&repo_root)
            .output()
            .unwrap();
        let printed = String::from_utf8_lossy(&output.stdout);
        // The script's own last paragraph reports the per-repo rollout; the
        // verdict itself is everything before it.
        let verdict = printed
            .split_once("\njankurai-gate:")
            .map(|(verdict, _)| verdict)
            .unwrap_or(&printed)
            .trim()
            .to_string();

        let (request, pass) = jankurai_score_request("main", "abc", Some(report.clone()), 0);
        let hosted = jankurai_proof_output(&request, pass);
        let mut expected = format!("{}\n\n{}", hosted.title, hosted.summary);
        if let Some(text) = hosted.text.as_deref() {
            expected.push_str(&format!("\n\n{text}"));
        }
        assert_eq!(verdict, expected.trim(), "report {index}: {report}");
        assert_eq!(
            output.status.success(),
            pass,
            "report {index} exit status must be the hosted verdict: {printed}"
        );
    }
}
