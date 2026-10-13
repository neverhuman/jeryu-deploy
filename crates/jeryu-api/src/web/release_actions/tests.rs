use super::*;

#[test]
fn full_open_queue_refuses_growth_but_keeps_duplicate_requests_idempotent() {
    let store = ReleaseActionStore::default();
    for i in 0..MAX_ACTIONS {
        let mut new = deploy(REL);
        new.repo = format!("acme/repo-{i}");
        store.record(new, "operator", "t0".into()).unwrap();
    }
    let mut first = deploy(REL);
    first.repo = "acme/repo-0".into();
    assert!(!store.record(first, "operator", "t1".into()).unwrap().1);
    assert!(store.record(deploy(REL), "operator", "t1".into()).is_err());
    assert_eq!(store.all().len(), MAX_ACTIONS);
}

#[test]
fn expired_pending_and_claimed_asks_are_terminal_and_cannot_run_again() {
    let store = ReleaseActionStore::default();
    let (pending, _) = record(
        &store,
        deploy(REL),
        "operator",
        "2026-10-13T10:00:00Z".into(),
    );
    let mut other = deploy(REL);
    other.repo = "acme/other".into();
    let (claimed, _) = record(&store, other, "operator", "2026-10-13T10:00:00Z".into());
    store
        .claim(&claimed.id, "host", "2026-10-13T10:01:00Z".into())
        .unwrap();
    let actions = store.all_at("2026-10-13T10:15:00Z");
    assert_eq!(
        actions.iter().find(|a| a.id == pending.id).unwrap().state,
        ActionState::Expired
    );
    assert_eq!(
        actions.iter().find(|a| a.id == claimed.id).unwrap().state,
        ActionState::Claimed
    );
    assert_eq!(
        store.claim(&pending.id, "host", "2026-10-13T10:16:00Z".into()),
        Err(ActionState::Expired)
    );
    let actions = store.all_at("2026-10-13T11:31:00Z");
    let unknown = actions.iter().find(|a| a.id == claimed.id).unwrap();
    assert_eq!(unknown.state, ActionState::Expired);
    assert!(
        unknown
            .message
            .as_deref()
            .unwrap()
            .contains("outcome unknown")
    );
    assert!(
        store
            .finish(
                &claimed.id,
                "host",
                ActionResult {
                    ok: true,
                    message: None,
                    log_tail: None
                },
                "2026-10-13T11:32:00Z".into()
            )
            .is_err()
    );
}

#[test]
fn cancel_and_claim_are_mutually_exclusive() {
    let store = ReleaseActionStore::default();
    let (first, _) = record(&store, deploy(REL), "operator", "t0".into());
    assert_eq!(
        store.cancel(&first.id, "t1".into()).unwrap().state,
        ActionState::Cancelled
    );
    assert_eq!(
        store.claim(&first.id, "host", "t2".into()),
        Err(ActionState::Cancelled)
    );
    let (next, _) = record(&store, deploy(REL), "operator", "t3".into());
    store.claim(&next.id, "host", "t4".into()).unwrap();
    assert_eq!(
        store.cancel(&next.id, "t5".into()),
        Err(Some(ActionState::Claimed))
    );
}

#[test]
fn pollers_and_auto_pin_requests_are_scoped_to_each_repository() {
    let store = ReleaseActionStore::default();
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-13T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    store.check_in("acme/first", "host", now).unwrap();
    assert_eq!(store.poller_repos_for("host", now), vec!["acme/first"]);
    assert!(store.poller_repos_for("other-host", now).is_empty());
    assert_eq!(store.poller_repos(now), vec!["acme/first"]);
    assert!(
        store
            .poller_repos(now + chrono::Duration::seconds(90))
            .is_empty()
    );
    let (pin, _) = record(
        &store,
        NewAction {
            action: ActionKind::AutoPin,
            repo: "acme/first".into(),
            release: None,
            head: None,
        },
        "operator",
        "t0".into(),
    );
    let queue = ReleaseQueue {
        poller_repos: vec!["acme/first".into()],
        open: vec![pin],
    };
    assert!(queue.polls("acme/first"));
    assert!(!queue.polls("acme/second"));
    assert!(queue.open_auto_pin("acme/first").is_some());
    assert!(queue.open_auto_pin("acme/second").is_none());
}

fn record(
    store: &ReleaseActionStore,
    new: NewAction,
    by: &str,
    now: String,
) -> (ReleaseAction, bool) {
    store.record(new, by, now).expect("queue space")
}

fn deploy(release: &str) -> NewAction {
    NewAction {
        action: ActionKind::Deploy,
        repo: "acme/forge-deploy".to_string(),
        release: Some(release.to_string()),
        head: None,
    }
}

const REL: &str = "prod-20261012T171717Z-f1b1fb4-unsigned";

#[test]
fn a_second_ask_for_the_same_step_is_the_first() {
    let store = ReleaseActionStore::default();
    let (first, created) = record(
        &store,
        deploy(REL),
        "operator",
        "2026-10-12T18:00:00Z".into(),
    );
    assert!(created);
    let (again, created) = record(
        &store,
        deploy(REL),
        "operator",
        "2026-10-12T18:00:05Z".into(),
    );
    assert!(!created);
    assert_eq!(again.id, first.id);
    assert_eq!(store.all().len(), 1);
}

#[test]
fn an_ask_is_claimed_once_and_finished_once() {
    let store = ReleaseActionStore::default();
    let (ask, _) = record(&store, deploy(REL), "operator", "t0".into());
    let claimed = store.claim(&ask.id, "release-host", "t1".into()).unwrap();
    assert_eq!(claimed.state, ActionState::Claimed);
    assert_eq!(
        store.claim(&ask.id, "another-host", "t2".into()),
        Err(ActionState::Claimed)
    );
    let done = store
        .finish(
            &ask.id,
            "release-host",
            ActionResult {
                ok: true,
                message: Some("live".into()),
                log_tail: None,
            },
            "t3".into(),
        )
        .unwrap();
    assert_eq!(done.state, ActionState::Succeeded);
    assert!(!store.all().iter().any(ReleaseAction::open));
    // Finished, so a new click is a new ask.
    let (next, created) = record(&store, deploy(REL), "operator", "t4".into());
    assert!(created);
    assert_ne!(next.id, ask.id);
}

#[test]
fn only_a_claimed_ask_takes_a_result() {
    let store = ReleaseActionStore::default();
    let (ask, _) = record(&store, deploy(REL), "operator", "t0".into());
    let result = ActionResult {
        ok: false,
        message: None,
        log_tail: None,
    };
    assert_eq!(
        store.finish(&ask.id, "host", result.clone(), "t1".into()),
        Err(Some(ActionState::Pending))
    );
    assert_eq!(
        store.finish("ra-missing", "host", result, "t1".into()),
        Err(None)
    );
}

#[test]
fn each_action_takes_exactly_its_own_argument() {
    assert!(validate(&deploy(REL)).is_ok());
    assert!(validate(&deploy("prod-1; rm -rf ~")).is_err());
    assert!(validate(&deploy("../../etc/passwd")).is_err());
    let mut bad_repo = deploy(REL);
    bad_repo.repo = "acme/forge deploy".into();
    assert!(validate(&bad_repo).is_err());
    let pin = NewAction {
        action: ActionKind::AutoPin,
        repo: "acme/forge-deploy".into(),
        release: None,
        head: None,
    };
    assert!(validate(&pin).is_ok());
    let retry = |head: &str| NewAction {
        action: ActionKind::AutoPinRetry,
        repo: "acme/forge-deploy".into(),
        release: None,
        head: Some(head.into()),
    };
    assert!(validate(&retry(&"a".repeat(40))).is_ok());
    assert!(validate(&retry("abc123")).is_err());
    assert!(validate(&retry(&"A".repeat(40))).is_err());
}

#[test]
fn a_full_store_forgets_a_finished_ask_never_an_open_one() {
    let store = ReleaseActionStore::default();
    let (done, _) = record(
        &store,
        deploy(REL),
        "operator",
        "2026-01-01T00:00:00Z".into(),
    );
    store.claim(&done.id, "host", "t".into()).unwrap();
    store
        .finish(
            &done.id,
            "host",
            ActionResult {
                ok: true,
                message: None,
                log_tail: None,
            },
            "t".into(),
        )
        .unwrap();
    for i in 1..MAX_ACTIONS {
        let release = format!("prod-20261012T{:06}Z-f1b1fb4-unsigned", i);
        record(
            &store,
            deploy(&release),
            "operator",
            format!("2026-10-12T00:00:{i:05}Z"),
        );
    }
    assert_eq!(store.all().len(), MAX_ACTIONS);
    record(
        &store,
        deploy("prod-20261013T000000Z-f1b1fb4-unsigned"),
        "operator",
        "z".into(),
    );
    assert!(!store.all().iter().any(|a| a.id == done.id));
    assert!(store.all().iter().all(|a| a.open()));
}
