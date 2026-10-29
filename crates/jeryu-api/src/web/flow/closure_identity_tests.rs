//! Lots recorded under a repository's earlier slug.
//!
//! The ledger keeps the `owner/name` a repository had when the event was
//! recorded; [`super::identity`] resolves it to the slug the repository is
//! hosted under now. These cover a repository that moved, one still open
//! there, a different repository that later takes the vacated slug, and a
//! slug nobody hosts. The identities are invented; no fixture names a real
//! repository.

use super::closure::{OpenVisit, snapshot};
use crate::web::WebState;
use jeryu_core::PullRequestState;

/// A lot recorded under a repository's earlier slug is reconciled against
/// the repository it moved to, and only against that repository: a pull
/// still open there stays open, and a different repository that later
/// takes the vacated slug answers for it instead of the moved one.
#[test]
fn a_moved_repository_reconciles_recorded_lots_under_its_current_slug() {
    use crate::web::pipeline::NewEvent;
    use jeryu_core::{
        CreateOrganizationRequest, CreatePullRequestRequest, CreateRepositoryRequest, ForgeCore,
        UpdatePullRequestRequest,
    };
    let root = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    let repository = |owner: &str, name: &str| {
        core.create_repository(
            owner,
            CreateRepositoryRequest {
                name: name.into(),
                private: false,
                description: None,
                default_branch: Some("main".into()),
            },
        )
        .unwrap()
    };
    let pull = |owner: &str, name: &str, title: &str, head: &str| {
        core.create_pull_request(
            owner,
            name,
            "author",
            CreatePullRequestRequest {
                title: title.into(),
                head: head.into(),
                base: "main".into(),
                head_sha: Some("c".repeat(40)),
                ..Default::default()
            },
        )
        .unwrap()
    };
    repository("nimbus-split", "nimbus-lambda");
    let landed = pull("nimbus-split", "nimbus-lambda", "lock refresh", "lock");
    let still_open = pull("nimbus-split", "nimbus-lambda", "held work", "held");
    core.create_organization(CreateOrganizationRequest {
        login: "cirrus".into(),
        display_name: None,
    })
    .unwrap();
    // The move the ledger never saw: the rows keep `nimbus-split/...`.
    core.rename_repository(
        "operator",
        "nimbus-split",
        "nimbus-lambda",
        "cirrus",
        "nimbus-lambda",
    )
    .unwrap();
    core.update_pull_request(
        "cirrus",
        "nimbus-lambda",
        landed.number,
        UpdatePullRequestRequest {
            state: Some(PullRequestState::Merged),
            ..Default::default()
        },
    )
    .unwrap();
    let state = WebState::new_with_git_storage(core.clone(), root.path().to_path_buf());
    state
        .events
        .insert(
            "fixture",
            &NewEvent {
                source: "fixture".into(),
                kind: "pr.opened".into(),
                summary: "opened".into(),
                family: Some("nimbus".into()),
                repo: Some("nimbus-split/nimbus-lambda".into()),
                pr: Some(landed.number as i64),
                ..Default::default()
            },
            1000,
        )
        .unwrap();
    let recorded = OpenVisit {
        lot: "nimbus-split/nimbus-lambda#2".into(),
        station: "review".into(),
        entered: 1_568_000_000_000,
        repo: Some("nimbus-split/nimbus-lambda".into()),
        pr: Some(landed.number),
        family: Some("nimbus".into()),
        started: true,
        held: false,
        todo: false,
        landed: false,
        back_from: None,
    };
    let mut gate = recorded.clone();
    gate.station = "gate".into();
    let mut held = recorded.clone();
    held.pr = Some(still_open.number);
    held.lot = "nimbus-split/nimbus-lambda#3".into();
    held.held = true;
    let visits = vec![recorded.clone(), gate.clone(), held.clone()];

    let moved = snapshot(&state, &visits, None);
    assert!(
        moved.excluded.contains(&recorded.key()) && moved.excluded.contains(&gate.key()),
        "a merged pull closes its stale station visits under the recorded slug"
    );
    assert_eq!(moved.merged_upstream_records, 2);
    assert!(moved.items.is_empty(), "a merge is not unresolved closure");
    assert!(
        !moved.excluded.contains(&held.key()),
        "a pull still open in the moved repository stays open work"
    );
    assert!(
        moved.problems.is_empty(),
        "the recorded slug resolves, so nothing is unreadable: {:?}",
        moved.problems
    );
    assert_eq!(moved.repositories[0]["repo"], "nimbus-split/nimbus-lambda");
    assert_eq!(moved.repositories[0]["held"], 1);

    // Closed, not merged: the closure links the repository's current page.
    core.update_pull_request(
        "cirrus",
        "nimbus-lambda",
        still_open.number,
        UpdatePullRequestRequest {
            state: Some(PullRequestState::Closed),
            ..Default::default()
        },
    )
    .unwrap();
    let closed = snapshot(&state, &[held.clone()], None);
    assert_eq!(closed.items.len(), 1);
    assert_eq!(
        closed.items[0].href,
        format!(
            "/repos/jeryu/cirrus/nimbus-lambda/pulls/{}",
            still_open.number
        )
    );
    assert_eq!(closed.items[0].disposition, "needs_disposition");

    // A new repository takes the vacated slug: it is a different
    // repository, so the moved one's merged pull says nothing about it.
    repository("nimbus-split", "nimbus-lambda");
    let fresh = pull("nimbus-split", "nimbus-lambda", "fresh work", "fresh");
    assert_eq!(fresh.number, landed.number, "same number, other repository");
    let reoccupied = snapshot(&state, &[recorded.clone(), gate.clone()], None);
    assert!(
        reoccupied.excluded.is_empty(),
        "a live repository at the slug wins over the alias"
    );
    assert_eq!(reoccupied.merged_upstream_records, 0);
}
/// A slug no repository hosts and no alias names keeps its recorded
/// identity: the lot stays in recorded work and is reported unreadable,
/// never closed on a guess.
#[test]
fn an_unknown_recorded_slug_is_reported_not_closed() {
    let root = tempfile::tempdir().unwrap();
    let state =
        WebState::new_with_git_storage(jeryu_core::ForgeCore::new(), root.path().to_path_buf());
    let visit = OpenVisit {
        lot: "nimbus-split/nimbus-lambda#2".into(),
        station: "gate".into(),
        entered: 1_568_000_000_000,
        repo: Some("nimbus-split/nimbus-lambda".into()),
        pr: Some(2),
        family: Some("nimbus".into()),
        started: true,
        held: false,
        todo: false,
        landed: false,
        back_from: None,
    };
    let snapshot = snapshot(&state, &[visit], None);
    assert!(snapshot.excluded.is_empty());
    assert!(snapshot.items.is_empty());
    assert_eq!(snapshot.problems.len(), 1);
    assert!(snapshot.problems[0].contains("remain in recorded work"));
}
