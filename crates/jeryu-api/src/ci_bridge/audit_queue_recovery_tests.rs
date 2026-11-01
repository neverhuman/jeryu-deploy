//! Recovery selection must be deterministic on persisted timestamp ties.
use super::*;

#[test]
fn equal_score_creation_times_prefer_failure_then_stable_identity() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        jeryu_core::CreateRepositoryRequest {
            name: "ties".into(),
            private: false,
            description: None,
            default_branch: Some("main".into()),
        },
    )
    .unwrap();
    let base = "b".repeat(40);
    let (version, digest) = super::super::jankurai::governed_auditor_identity().unwrap();
    let record = |branch: &str, value: u32| {
        core.record_jankurai_score("alice", "ties",
        jeryu_core::RecordJankuraiScoreRequest {
            branch: branch.into(), commit_sha: "a".repeat(40), score: Some(value),
            hard_findings: Some(0), decision: "scored".into(),
            report: Some(serde_json::json!({"score": value, "caps_applied": [],
                "decision": {"hard_findings": 0, "minimum_score": 85},
                "jeryu_audit_provenance": {"runner_id": "runner", "base_sha": base,
                    "audit_mode": "diff", "jankurai_version": version, "jankurai_sha256": digest}})),
            ..Default::default()
        }).unwrap()
    };
    let mut pass = record("pass", 98);
    let fail = record("fail", 40);
    pass.created_at = fail.created_at;
    for rows in [[pass.clone(), fail.clone()], [fail.clone(), pass.clone()]] {
        assert_eq!(recorded_for_recovery(&rows, &base).unwrap().score, Some(40));
    }
    let mut other_pass = pass.clone();
    other_pass.branch = "zzz".into();
    for rows in [
        [pass.clone(), other_pass.clone()],
        [other_pass.clone(), pass.clone()],
    ] {
        assert_eq!(recorded_for_recovery(&rows, &base).unwrap().branch, "zzz");
    }
}
