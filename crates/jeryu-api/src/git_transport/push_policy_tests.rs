use super::*;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Path as AxumPath, State};
use axum::http::{Request, header};
use jeryu_core::{CreatePullRequestRequest, CreateRepositoryRequest, ForgeCore, RepoAccessLevel};
use std::cell::Cell;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = crate::test_git::git_command()
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

struct Fixture {
    root: tempfile::TempDir,
    state: Arc<WebState>,
    bare: std::path::PathBuf,
    old: String,
    new: String,
    number: u64,
    admin: String,
    writer: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let bare = root.path().join("git/alice/repo.git");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let base = String::from_utf8(git(&work, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_string();
        git(&work, &["switch", "-q", "-c", "feature"]);
        git(&work, &["commit", "-q", "--allow-empty", "-m", "feature"]);
        let old = String::from_utf8(git(&work, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_string();
        git(
            &work,
            &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
        );
        git(&bare, &["update-ref", "refs/heads/main", &old]);
        git(&work, &["commit", "-q", "--allow-empty", "-m", "late work"]);
        let new = String::from_utf8(git(&work, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_string();
        git(
            &bare,
            &[
                "fetch",
                "-q",
                work.to_str().unwrap(),
                "HEAD:refs/test/incoming",
            ],
        );
        let core = ForgeCore::open_sqlite(root.path().join("forge.sqlite")).unwrap();
        core.create_account("admin", "fixture administrator password", UserRole::Admin)
            .unwrap();
        core.create_account("writer", "fixture writer password", UserRole::User)
            .unwrap();
        let admin = core
            .create_personal_access_token("admin", "test", None)
            .unwrap()
            .secret;
        let writer = core
            .create_personal_access_token("writer", "test", None)
            .unwrap()
            .secret;
        core.create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "repo".into(),
                private: false,
                default_branch: Some("main".into()),
                ..Default::default()
            },
        )
        .unwrap();
        core.grant_repo_access("admin", "writer", "alice", "repo", RepoAccessLevel::Write)
            .unwrap();
        let pull = core
            .create_pull_request(
                "alice",
                "repo",
                "writer",
                CreatePullRequestRequest {
                    title: "feature".into(),
                    head: "feature".into(),
                    base: "main".into(),
                    head_sha: Some(old.clone()),
                    base_sha: Some(base),
                    ..Default::default()
                },
            )
            .unwrap();
        let base = core
            .evaluate_merge_readiness("alice", "repo", pull.number, None)
            .expect("readiness")
            .merge_base()
            .expect("ready to merge");
        core.finalize_merge("alice", "repo", pull.number, old.clone(), None, &base)
            .unwrap();
        let state = Arc::new(WebState::authenticated_git_test_state(
            core,
            root.path().join("git"),
        ));
        Self {
            root,
            state,
            bare,
            old,
            new,
            number: pull.number,
            admin,
            writer,
        }
    }

    fn permit(&self) -> String {
        json!({"ref":"refs/heads/feature", "old_sha": self.old, "new_sha": self.new,
            "pull_request": self.number, "reason":"advance the shared integration branch"})
        .to_string()
    }

    fn body(&self, refs: usize) -> Vec<u8> {
        let mut body = Vec::new();
        for n in 0..refs {
            let name = if n == 0 {
                "feature".to_string()
            } else {
                format!("extra-{n}")
            };
            let old = if n == 0 {
                &self.old
            } else {
                crate::ci_bridge::ZERO_OID
            };
            let line = if n == 0 {
                format!("{old} {} refs/heads/{name}\0report-status\n", self.new)
            } else {
                format!("{old} {} refs/heads/{name}\n", self.new)
            };
            body.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
            body.extend_from_slice(line.as_bytes());
        }
        body.extend_from_slice(b"0000");
        // The objects already exist under refs/test/incoming; a genuine
        // empty pack still lets receive-pack perform its normal CAS update.
        body.extend(git(&self.bare, &["pack-objects", "--stdout", "--revs"]));
        body
    }

    async fn push(
        &self,
        token: &str,
        permit: Option<String>,
        body: Vec<u8>,
    ) -> (StatusCode, String) {
        let mut request = Request::builder()
            .method("POST")
            .uri("/git/alice/repo.git/git-receive-pack")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(
                header::CONTENT_TYPE,
                "application/x-git-receive-pack-request",
            );
        if let Some(permit) = permit {
            request = request.header(OVERRIDE_HEADER, permit);
        }
        let response = crate::git_transport::git_receive_pack(
            State(self.state.clone()),
            ConnectInfo("127.0.0.1:23456".parse::<SocketAddr>().unwrap()),
            AxumPath(("alice".to_string(), "repo.git".to_string())),
            request.body(Body::from(body)).unwrap(),
        )
        .await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    fn head(&self) -> String {
        String::from_utf8(git(&self.bare, &["rev-parse", "refs/heads/feature"]))
            .unwrap()
            .trim()
            .to_string()
    }
}

#[tokio::test]
async fn admin_override_is_exact_audited_and_does_not_grant_writer_privileges() {
    let f = Fixture::new();
    let (_, reply) = f.push(&f.admin, None, f.body(1)).await;
    assert!(reply.contains("already merged"), "{reply}");
    assert_eq!(f.head(), f.old);
    let (status, _) = f.push(&f.writer, Some(f.permit()), f.body(1)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(f.head(), f.old);
    let mut wrong: serde_json::Value = serde_json::from_str(&f.permit()).unwrap();
    wrong["new_sha"] = json!(f.old);
    let (_, reply) = f.push(&f.admin, Some(wrong.to_string()), f.body(1)).await;
    assert!(reply.contains("already merged"));
    assert_eq!(f.head(), f.old);
    let another = f
        .state
        .core
        .create_pull_request(
            "alice",
            "repo",
            "writer",
            CreatePullRequestRequest {
                title: "another merged branch".into(),
                head: "extra-1".into(),
                base: "main".into(),
                head_sha: Some(f.old.clone()),
                base_sha: Some(f.old.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let base = f
        .state
        .core
        .evaluate_merge_readiness("alice", "repo", another.number, None)
        .expect("readiness")
        .merge_base()
        .expect("ready to merge");
    f.state
        .core
        .finalize_merge("alice", "repo", another.number, f.old.clone(), None, &base)
        .unwrap();
    let (_, reply) = f.push(&f.admin, Some(f.permit()), f.body(2)).await;
    assert!(reply.contains("ng refs/heads/extra-1"), "{reply}");
    assert_eq!(
        f.head(),
        f.old,
        "an override for one ref cannot admit a second blocked ref"
    );
    let subject = format!("alice/repo:refs/heads/feature:{}:{}", f.old, f.new);
    assert!(f.state.core.list_audit(&subject).unwrap().is_empty());
    let (status, reply) = f.push(&f.admin, Some(f.permit()), f.body(1)).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(reply.contains("ok refs/heads/feature"), "{reply}");
    assert_eq!(f.head(), f.new);
    let subject = format!("alice/repo:refs/heads/feature:{}:{}", f.old, f.new);
    let rows = f.state.core.list_audit(&subject).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].actor, "admin");
    assert_eq!(rows[0].phase, "requested");
    assert_eq!(rows[0].detail["ref"], "refs/heads/feature");
    assert_eq!(rows[0].detail["old_sha"], f.old);
    assert_eq!(rows[0].detail["new_sha"], f.new);
    assert_eq!(rows[0].detail["pull_request"], f.number);
    assert_eq!(
        rows[0].detail["reason"],
        "advance the shared integration branch"
    );
}

#[tokio::test]
async fn malformed_and_over_limit_requests_leave_refs_unchanged() {
    let f = Fixture::new();
    for body in [
        b"zzzz".to_vec(),
        b"0001".to_vec(),
        b"0008abc".to_vec(),
        b"0000garbage".to_vec(),
        Vec::new(),
        f.body(MAX_PUSH_REFS + 1),
    ] {
        let (status, _) = f.push(&f.admin, None, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(f.head(), f.old);
    }
}

#[tokio::test]
async fn the_exact_flush_only_probe_is_preserved_without_reading_prs_or_moving_refs() {
    let f = Fixture::new();
    let probe = commands(b"0000").unwrap();
    assert!(probe.is_empty());
    let _policy = PushPolicy::inspect(
        &f.state,
        "alice",
        "repo",
        &probe,
        &HeaderMap::new(),
        false,
        || panic!("a no-op probe must not read PRs"),
    )
    .unwrap();
    let (status, _) = f.push(&f.admin, None, b"0000".to_vec()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.head(), f.old);
}

#[tokio::test]
async fn an_override_that_cannot_be_audited_never_moves_the_ref() {
    let f = Fixture::new();
    rusqlite::Connection::open(f.root.path().join("forge.sqlite"))
        .unwrap()
        .execute("DROP TABLE forge_audit_log", [])
        .unwrap();
    let (status, reply) = f.push(&f.admin, Some(f.permit()), f.body(1)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{reply}");
    assert!(reply.contains("override_audit_failed"));
    assert_eq!(f.head(), f.old);
}

#[test]
fn one_pr_read_is_reused_for_guard_and_capped_notices_on_a_multi_ref_push() {
    let f = Fixture::new();
    // A new live PR clears the merged guard; the existing read then serves
    // all 64 branch checks and the post-push notice pass.
    f.state
        .core
        .create_pull_request(
            "alice",
            "repo",
            "writer",
            CreatePullRequestRequest {
                title: "shared branch continuation".into(),
                head: "feature".into(),
                base: "main".into(),
                head_sha: Some(f.old.clone()),
                base_sha: Some(f.old.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let commands = commands(&f.body(MAX_PUSH_REFS)).unwrap();
    let reads = Cell::new(0);
    let policy = PushPolicy::inspect(
        &f.state,
        "alice",
        "repo",
        &commands,
        &HeaderMap::new(),
        false,
        || {
            reads.set(reads.get() + 1);
            f.state.core.list_pull_requests("alice", "repo", None)
        },
    )
    .unwrap();
    let branches = super::super::pushed_branches(&commands);
    let lines = policy.notice_lines(&f.state, "alice", "repo", &branches);
    assert_eq!(reads.get(), 1);
    assert!(lines.len() <= MAX_NOTICE_BRANCHES * 3 + 1);
    assert!(lines.last().unwrap().contains("16 branches"));
}

/// `refs/green/*` is the release automation's trust anchor and `refs/queue*/*`
/// the merge queue's state: no client push may create one, administrator
/// included. Only the forge writes them, from the gate's own verdicts.
#[tokio::test]
async fn forge_owned_refs_cannot_be_pushed_by_any_client() {
    let f = Fixture::new();
    for name in [
        "refs/green/main",
        "refs/green/trunk",
        "refs/queue/main/1",
        "refs/queue-meta/main/1",
    ] {
        for token in [&f.admin, &f.writer] {
            let line = format!(
                "{} {} {name}\0report-status\n",
                crate::ci_bridge::ZERO_OID,
                f.new
            );
            let mut body = format!("{:04x}", line.len() + 4).into_bytes();
            body.extend_from_slice(line.as_bytes());
            body.extend_from_slice(b"0000");
            body.extend(git(&f.bare, &["pack-objects", "--stdout", "--revs"]));
            let (status, reply) = f.push(token, None, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{name}: {reply}");
            assert!(reply.contains("cannot be pushed"), "{name}: {reply}");
            let exists = crate::test_git::git_command()
                .arg("-C")
                .arg(&f.bare)
                .args(["rev-parse", "--verify", "-q", name])
                .stdin(Stdio::null())
                .output()
                .unwrap()
                .status
                .success();
            assert!(!exists, "a refused push created {name}");
        }
    }
    // A branch whose name merely spells the green ref is an ordinary branch;
    // the readers match the ref name exactly, so it impersonates nothing.
    assert!(!crate::green_ref::is_green_ref(
        "refs/heads/refs/green/main"
    ));
}
