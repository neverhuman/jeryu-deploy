//! Tests for the commit and line counts of `GET /api/v1/events/histogram`.

use std::path::Path;

use axum::http::{Method as HttpMethod, StatusCode};
use chrono::{Duration, Utc};
use jeryu_core::{CreateRepositoryRequest, ForgeCore, UserRole};
use serde_json::Value;
use tower::ServiceExt;

use super::commits::{Commit, generated, parse_log};
use super::tests::{body_json, request};
use crate::web::{WebState, app};

#[test]
fn the_log_is_read_per_commit_without_binary_or_generated_files() {
    let out = "\u{1}1700000000\n\
               \n\
               10\t2\tsrc/lib.rs\n\
               -\t-\tlogo.png\n\
               900\t850\tCargo.lock\n\
               3\t1\tapps/web/dist/index.js\n\
               \u{1}1700000600\n\
               \n\
               0\t40\tdocs/{old.md => new.md}\n\
               \u{1}1700001200\n";
    assert_eq!(
        parse_log(out),
        vec![
            Commit {
                authored_ms: 1_700_000_000_000,
                added: 10,
                deleted: 2
            },
            Commit {
                authored_ms: 1_700_000_600_000,
                added: 0,
                deleted: 40
            },
            // A commit that changed only binary files still counts.
            Commit {
                authored_ms: 1_700_001_200_000,
                added: 0,
                deleted: 0
            },
        ]
    );
}

#[test]
fn generated_files_are_lockfiles_bundles_and_generated_code() {
    for path in [
        "Cargo.lock",
        "apps/web/package-lock.json",
        "apps/web/dist/assets/index-abc.js",
        "contracts/generated/RepositoryRole.ts",
        "static/app.min.js",
        "vendor/lib/x.go",
    ] {
        assert!(generated(path), "{path}");
    }
    for path in [
        "src/dist.rs",
        "docs/generated-zones.md",
        "apps/web/src/main.tsx",
        "lockfile.rs",
    ] {
        assert!(!generated(path), "{path}");
    }
}

fn git(cwd: &Path, args: &[&str], authored: Option<&str>) -> String {
    let mut command = crate::test_git::git_command();
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "jeryu-test")
        .env("GIT_AUTHOR_EMAIL", "jeryu-test@example.com")
        .env("GIT_COMMITTER_NAME", "jeryu-test")
        .env("GIT_COMMITTER_EMAIL", "jeryu-test@example.com");
    if let Some(date) = authored {
        command.env("GIT_AUTHOR_DATE", date);
    }
    let output = command.output().expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(work: &Path, file: &str, body: &str, authored: &str) {
    std::fs::write(work.join(file), body).unwrap();
    git(work, &["add", "."], None);
    git(work, &["commit", "--quiet", "-m", file], Some(authored));
}

#[tokio::test]
async fn commits_on_main_count_in_the_bucket_they_were_written() {
    let storage = tempfile::tempdir().unwrap();
    let work = storage.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let hours_ago = |hours: i64| (Utc::now() - Duration::hours(hours)).to_rfc3339();

    git(&work, &["init", "--quiet", "-b", "main"], None);
    // Written three days ago and committed now, as a rebase-merge leaves it:
    // outside a 24 h window, whatever its commit date says.
    commit(&work, "old.txt", "a\nb\n", &hours_ago(72));
    // Five lines added, then two of them replaced: 7 added, 2 deleted.
    commit(&work, "new.txt", "1\n2\n3\n4\n5\n", &hours_ago(2));
    commit(&work, "new.txt", "1\n2\n3\nx\ny\n", &hours_ago(2));
    // A lockfile refresh counts as a commit but its lines do not.
    commit(&work, "Cargo.lock", "lots\nof\nlines\n", &hours_ago(2));
    // Branch work that never reached main is not counted.
    git(&work, &["checkout", "--quiet", "-b", "feature"], None);
    commit(&work, "feature.txt", "f\n", &hours_ago(1));
    git(&work, &["checkout", "--quiet", "main"], None);

    let bare = storage.path().join("alice").join("jeryu.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
        &work,
        &["clone", "--quiet", "--bare", ".", bare.to_str().unwrap()],
        None,
    );

    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let admin = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
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
    let state = WebState::new_with_git_storage(core, storage.path().to_path_buf())
        .with_auth(true, false, false);
    let router = app(state, Path::new("/tmp/jeryu-no-spa"));
    let get = |uri: &'static str| {
        let router = router.clone();
        let admin = admin.clone();
        async move {
            let response = router
                .oneshot(request(HttpMethod::GET, uri, &admin, None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            body_json(response).await
        }
    };

    let day: Value = get("/api/v1/events/histogram").await;
    assert_eq!(day["totals"]["commits"], 3, "{}", day["totals"]);
    assert_eq!(day["totals"]["lines_added"], 7);
    assert_eq!(day["totals"]["lines_deleted"], 2);
    let busy: Vec<&Value> = day["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|bucket| bucket["commits"] != 0)
        .collect();
    assert_eq!(busy.len(), 1, "all three were written in one hour");
    assert_eq!(busy[0]["total"], 0, "commits are not pipeline events");

    let week: Value = get("/api/v1/events/histogram?range=7d").await;
    assert_eq!(week["totals"]["commits"], 4, "the three-day-old commit too");
    assert_eq!(week["totals"]["lines_added"], 9);

    let scoped: Value = get("/api/v1/events/histogram?repo=alice/jeryu").await;
    assert_eq!(scoped["totals"]["commits"], 3);
    let other: Value = get("/api/v1/events/histogram?repo=alice/other").await;
    assert_eq!(other["totals"]["commits"], 0, "another repository's scope");
}
