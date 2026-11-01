//! Loading persisted queue metadata is atomic and retried without a restart.
//! Adapted from finished worker rescue 2d5e718879ce5388e92b08bda639e0b1575a562e (acb449).
use super::*;
use crate::web::merge_queue_tests::{Fixture, account, git};

async fn assert_load_recovery(malformed: bool) {
    let fx = Fixture::new("feature.txt", "feature\n");
    fx.advance_main("other.txt", "other\n");
    let (status, _) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED);
    let metadata = git(&fx.bare, &["rev-parse", "refs/queue-meta/main/1"]);
    let mut second: QueueEntry =
        serde_json::from_str(&git(&fx.bare, &["cat-file", "blob", &metadata])).unwrap();
    second.number = 2;
    let git_repo = git_for(&fx.state, &fx.bare);
    git_repo
        .write_blob_ref(
            "refs/queue-meta/main/2",
            &serde_json::to_string(&second).unwrap(),
        )
        .unwrap();
    let second_metadata = git(&fx.bare, &["rev-parse", "refs/queue-meta/main/2"]);

    // Retain an entry already in memory while reloading the persisted index.
    let queue = &fx.state.merge_queue;
    queue.lock().loaded = false;
    let before = queue.lock().entries.clone();
    if malformed {
        git_repo
            .write_blob_ref("refs/queue-meta/main/2", "{invalid json")
            .unwrap();
    } else {
        // A ref naming a commit is readable as an object, but not as a blob.
        // This fails after the healthy first ref has already been read.
        git(
            &fx.bare,
            &["update-ref", "refs/queue-meta/main/2", &fx.head],
        );
    }
    let base = git(&fx.bare, &["rev-parse", "refs/heads/main"]);
    assert_eq!(tick(&fx.state), 0);
    {
        let index = queue.lock();
        assert!(!index.loaded);
        assert_eq!(index.entries, before);
    }
    let response = list_all(
        State(fx.state.clone()),
        account("rel-bot", UserRole::Admin),
        strict_query::StrictQuery(serde_json::from_value(json!({})).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let diagnostic = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(diagnostic.contains("queue_metadata_error"), "{diagnostic}");
    assert!(
        diagnostic.contains("refs/queue-meta/main/2"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("restore"), "{diagnostic}");
    assert!(building_on_base(&fx.state.repo_manager.config().git_bin, &fx.bare, "main").is_err());
    assert!(
        authored_audit(
            &fx.state,
            "alice",
            "jeryu",
            &second.queue_sha,
            &fx.main(),
            "alice"
        )
        .is_err()
    );
    assert_eq!(git(&fx.bare, &["rev-parse", "refs/heads/main"]), base);
    assert_eq!(
        git(&fx.bare, &["rev-parse", "refs/queue-meta/main/1"]),
        metadata
    );

    git(
        &fx.bare,
        &["update-ref", "refs/queue-meta/main/2", &second_metadata],
    );
    // A later tick retries and restores both entries in the same process.
    tick(&fx.state);
    let index = queue.lock();
    assert!(index.loaded);
    assert_eq!(index.entries.len(), 2);
    assert!(
        index
            .entries
            .contains_key(&("alice".into(), "jeryu".into(), 1))
    );
    assert!(
        index
            .entries
            .contains_key(&("alice".into(), "jeryu".into(), 2))
    );
}

#[tokio::test]
async fn unreadable_metadata_preserves_entries_and_retries() {
    assert_load_recovery(false).await;
}

#[tokio::test]
async fn malformed_metadata_preserves_entries_and_retries() {
    assert_load_recovery(true).await;
}
