use super::*;
fn report() -> Report {
    Report {
        observed_at: "2026-10-01T00:00:00Z".into(),
        builds: vec![Build {
            id: "rc-1".into(),
            deliverable: "app".into(),
            built_at: "2026-09-30T00:00:00Z".into(),
            source_repo: "acme/app".into(),
            source_sha: "a".repeat(40),
            status: "built".into(),
            artifacts: vec![Artifact {
                repository: "registry/acme/app".into(),
                digest: format!("sha256:{}", "b".repeat(64)),
                tags: vec!["rc-1".into()],
            }],
            log_url: None,
        }],
        placements: Some(vec![Placement {
            deliverable: "app".into(),
            environment: "production".into(),
            target: "node1".into(),
            current: Some("rc-1".into()),
            previous: None,
            production: true,
        }]),
    }
}
fn open(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(include_str!(
        "../../../../../db/migrations/0029_build_inventory.sql"
    ))
    .unwrap();
    conn
}
#[test]
fn persists_builds_and_placements_across_restart_without_duplicate_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inventory.sqlite");
    let mut r = report();
    validate(&mut r).unwrap();
    {
        let conn = open(&path);
        record(&conn, "acme", &r).unwrap();
        record(&conn, "acme", &r).unwrap();
    }
    let value = list(&open(&path), "acme", &ListQuery::default()).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["placements"][0]["current"], "rc-1");
}
#[test]
fn transaction_refuses_changed_identity_and_unknown_placement_without_partial_writes() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!(
        "../../../../../db/migrations/0029_build_inventory.sql"
    ))
    .unwrap();
    let mut r = report();
    validate(&mut r).unwrap();
    record(&conn, "acme", &r).unwrap();
    r.builds[0].source_sha = "c".repeat(40);
    assert!(record(&conn, "acme", &r).is_err());
    r = report();
    r.builds[0].id = "rc-2".into();
    r.placements.as_mut().unwrap()[0].current = Some("absent".into());
    assert!(record(&conn, "acme", &r).is_err());
    assert_eq!(
        list(&conn, "acme", &ListQuery::default()).unwrap()["total"],
        1
    );
}
#[test]
fn older_placement_reports_cannot_restore_a_previous_running_build() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!(
        "../../../../../db/migrations/0029_build_inventory.sql"
    ))
    .unwrap();
    let mut r = report();
    validate(&mut r).unwrap();
    record(&conn, "acme", &r).unwrap();
    let mut newer = r.clone();
    newer.observed_at = "2026-10-02T00:00:00.000Z".into();
    newer.placements = Some(vec![]);
    record(&conn, "acme", &newer).unwrap();
    record(&conn, "acme", &r).unwrap();
    assert_eq!(
        list(&conn, "acme", &ListQuery::default()).unwrap()["placements"],
        json!([])
    );
}
#[test]
fn validation_rejects_bad_digest_duplicate_targets_and_future_time() {
    let mut r = report();
    r.builds[0].artifacts[0].digest = "latest".into();
    assert!(validate(&mut r).is_err());
    let mut r = report();
    let duplicate = r.placements.as_ref().unwrap()[0].clone();
    r.placements.as_mut().unwrap().push(duplicate);
    assert!(validate(&mut r).is_err());
    let mut r = report();
    r.observed_at = "2999-01-01T00:00:00Z".into();
    assert!(validate(&mut r).is_err());
}

#[test]
fn building_can_acquire_artifacts_once_but_published_content_cannot_change() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!(
        "../../../../../db/migrations/0029_build_inventory.sql"
    ))
    .unwrap();
    let mut first = report();
    validate(&mut first).unwrap();
    first.builds[0].status = "building".into();
    first.builds[0].artifacts.clear();
    first.placements = None;
    record(&conn, "acme", &first).unwrap();
    let mut built = report();
    built.observed_at = "2026-10-02T00:00:00Z".into();
    validate(&mut built).unwrap();
    record(&conn, "acme", &built).unwrap();
    built.observed_at = "2026-10-03T00:00:00.000Z".into();
    built.builds[0].artifacts[0].digest = format!("sha256:{}", "c".repeat(64));
    assert!(record(&conn, "acme", &built).is_err());
    assert_eq!(
        list(&conn, "acme", &ListQuery::default()).unwrap()["builds"][0]["status"],
        "built"
    );
}

#[test]
fn inventory_reads_are_admin_only_at_the_middleware_boundary() {
    use axum::http::Method;
    assert!(super::super::auth::admin_only_request(
        &Method::GET,
        "/api/v1/build-inventory/acme"
    ));
    assert!(!super::super::auth::admin_only_request(
        &Method::PUT,
        "/api/v1/build-inventory/acme"
    ));
}
