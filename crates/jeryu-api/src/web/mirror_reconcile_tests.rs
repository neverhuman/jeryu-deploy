//! What one reconcile pass records: the catch-up, the alarm, and the silence
//! in between. The git side of a reconcile is proven in `github_mirror`; here
//! the report is a value and the assertions are about bookkeeping.

use jeryu_core::CreateRepositoryRequest;

use super::*;
use crate::github_mirror::TagDrift;

fn core_with_repo() -> ForgeCore {
    let core = ForgeCore::new();
    core.create_repository(
        "acme",
        CreateRepositoryRequest {
            name: "demo".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .expect("create repo");
    core
}

fn at(minute: u32) -> DateTime<Utc> {
    use chrono::TimeZone;
    Utc.with_ymd_and_hms(2026, 9, 30, 12, minute, 0).unwrap()
}

fn report(state: MirrorSync) -> MirrorReconcile {
    MirrorReconcile {
        github_slug: "neverhuman/demo".to_string(),
        branch: "main".to_string(),
        forge_head: Some("f".repeat(40)),
        github_head: Some("9".repeat(40)),
        state,
        caught_up: false,
        github_only_commits: vec!["9".repeat(40)],
        tags: MirrorTagOutcome::default(),
        error: None,
    }
}

fn checks(core: &ForgeCore, name: &str) -> Vec<(String, String)> {
    core.list_check_runs("acme", "demo", None)
        .expect("list check-runs")
        .check_runs
        .into_iter()
        .filter(|run| run.name == name)
        .map(|run| {
            (
                format!("{:?}", run.conclusion).to_ascii_lowercase(),
                run.output.map(|out| out.summary).unwrap_or_default(),
            )
        })
        .collect()
}

#[test]
fn a_cadence_of_zero_turns_the_loop_off() {
    assert_eq!(interval_from(None), Some(Duration::from_secs(600)));
    assert_eq!(interval_from(Some("")), Some(Duration::from_secs(600)));
    assert_eq!(interval_from(Some("5")), Some(Duration::from_secs(300)));
    assert_eq!(interval_from(Some("0")), None);
    assert_eq!(interval_from(Some("later")), None);
}

#[test]
fn catching_up_a_lagging_mirror_records_the_push_and_dates_it() {
    let core = core_with_repo();
    let mut caught_up = report(MirrorSync::InSync);
    caught_up.caught_up = true;
    caught_up.github_only_commits.clear();
    caught_up.tags.pushed = vec!["v2".to_string()];

    let recorded = record(&core, "acme", "demo", caught_up, None, at(0));
    assert_eq!(recorded.state, MirrorSync::InSync);
    assert_eq!(
        recorded.last_push_at.as_deref(),
        Some(at(0).to_rfc3339()).as_deref()
    );
    assert!(!recorded.needs_a_person());
    let pushes = checks(&core, MIRROR_CHECK_NAME);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0].0, "some(success)");
    assert!(pushes[0].1.contains("fast-forwarded"), "{}", pushes[0].1);
    assert!(pushes[0].1.contains("pushed tags v2"), "{}", pushes[0].1);
    assert!(
        checks(&core, MIRROR_DIVERGED_CHECK_NAME).is_empty(),
        "a healthy mirror raises no divergence alarm"
    );
}

#[test]
fn a_diverged_mirror_alarms_once_and_clears_when_it_is_settled() {
    let core = core_with_repo();
    let diverged = record(
        &core,
        "acme",
        "demo",
        report(MirrorSync::Diverged),
        None,
        at(0),
    );
    assert!(diverged.needs_a_person());
    let alarms = checks(&core, MIRROR_DIVERGED_CHECK_NAME);
    assert_eq!(alarms.len(), 1, "{alarms:?}");
    assert_eq!(alarms[0].0, "some(failure)");
    assert!(
        alarms[0].1.contains("has diverged from the forge"),
        "{}",
        alarms[0].1
    );
    assert!(
        alarms[0].1.contains(&"9".repeat(40)),
        "the alarm names the GitHub-only commit: {}",
        alarms[0].1
    );
    assert!(
        alarms[0].1.contains("nothing was forced"),
        "{}",
        alarms[0].1
    );
    assert!(
        checks(&core, MIRROR_CHECK_NAME).is_empty(),
        "nothing was pushed, so nothing is recorded as pushed"
    );

    // Still diverged ten minutes later: the same alarm is not written again.
    let again = record(
        &core,
        "acme",
        "demo",
        report(MirrorSync::Diverged),
        Some(&diverged),
        at(10),
    );
    assert_eq!(checks(&core, MIRROR_DIVERGED_CHECK_NAME).len(), 1);

    // Somebody brought the work onto the forge: the alarm is cleared once.
    let settled = record(
        &core,
        "acme",
        "demo",
        report(MirrorSync::InSync),
        Some(&again),
        at(20),
    );
    assert!(!settled.needs_a_person());
    let alarms = checks(&core, MIRROR_DIVERGED_CHECK_NAME);
    assert_eq!(alarms.len(), 2, "{alarms:?}");
    assert_eq!(alarms[1].0, "some(success)");
}

#[test]
fn a_tag_the_mirror_refuses_to_move_needs_a_person() {
    let core = core_with_repo();
    let mut drifted = report(MirrorSync::InSync);
    drifted.github_only_commits.clear();
    drifted.tags.drift = vec![TagDrift {
        tag: "v1".to_string(),
        forge_oid: Some("a".repeat(40)),
        github_oid: Some("b".repeat(40)),
        detail: "GitHub holds v1 at another commit".to_string(),
    }];
    let recorded = record(&core, "acme", "demo", drifted, None, at(0));
    assert!(recorded.needs_a_person());
    assert_eq!(recorded.tag_drift.len(), 1);
    let alarms = checks(&core, MIRROR_DIVERGED_CHECK_NAME);
    assert_eq!(alarms.len(), 1, "{alarms:?}");
    assert!(
        alarms[0].1.contains("GitHub holds v1 at another commit"),
        "{}",
        alarms[0].1
    );
}

#[test]
fn a_tag_push_folds_into_what_the_repo_page_shows() {
    let mut held = MirrorRepoState {
        repo: "acme/demo".to_string(),
        github_slug: "neverhuman/demo".to_string(),
        branch: "main".to_string(),
        state: MirrorSync::InSync,
        forge_head: None,
        github_head: None,
        checked_at: at(0).to_rfc3339(),
        last_push_at: None,
        github_only_commits: Vec::new(),
        tags_pushed: Vec::new(),
        tag_drift: Vec::new(),
        error: None,
    };
    let outcome = MirrorTagOutcome {
        pushed: vec!["v2".to_string()],
        already_present: Vec::new(),
        drift: vec![TagDrift {
            tag: "v1".to_string(),
            forge_oid: Some("a".repeat(40)),
            github_oid: Some("b".repeat(40)),
            detail: "GitHub holds v1 at another commit".to_string(),
        }],
        error: None,
    };
    merge_tags(&mut held, &outcome);
    assert_eq!(held.tags_pushed, vec!["v2".to_string()]);
    assert_eq!(held.tag_drift.len(), 1);
    assert!(held.needs_a_person());

    // The same tag reported twice stays one row; a tag that later pushes drops
    // out of the drift list.
    merge_tags(&mut held, &outcome);
    assert_eq!(held.tag_drift.len(), 1);
    assert_eq!(held.tags_pushed, vec!["v2".to_string()]);
    merge_tags(
        &mut held,
        &MirrorTagOutcome {
            pushed: vec!["v1".to_string()],
            ..MirrorTagOutcome::default()
        },
    );
    assert!(held.tag_drift.is_empty());
    assert!(!held.needs_a_person());
}
