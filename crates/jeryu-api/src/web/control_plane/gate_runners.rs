//! Live PR gate runners reported by heartbeat.
//!
//! Gate runners (jain-deploy `scripts/pr-gate-runner.sh`, one per slot) post a
//! heartbeat when a slot starts a gate, finishes one, and on every idle tick.
//! The store keeps the latest report per runner in memory; `/fleet` renders it.
//! A runner that stops reporting is shown offline after
//! [`RUNNER_OFFLINE_AFTER_SECS`] rather than silently dropped, and a restarted
//! forge repopulates within one tick because runners report every minute.
//!
//! pr-redteam, the agent PR reviewer, reports through the same contract with
//! the [`REVIEWER_LABEL`] label; its `last.conclusion` is a review verdict
//! (`approve`, `hold`, or an outcome with no usable verdict) instead of a gate
//! result, and `/runners` renders it as a reviewer rather than a gate slot.
//!
//! Only logins named in `JERYU_RUNNER_REPORTERS` (comma-separated, default
//! `gatebot,pragent`) may report, so an ordinary account cannot paint fake
//! runners.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Seconds without a heartbeat before a runner is shown offline.
pub(crate) const RUNNER_OFFLINE_AFTER_SECS: i64 = 180;
const MAX_RUNNERS: usize = 256;
const DEFAULT_REPORTERS: &str = "gatebot,pragent";
/// Heartbeat label that marks a PR reviewer (pr-redteam) instead of a gate slot.
pub(crate) const REVIEWER_LABEL: &str = "redteam";
const GATE_CONCLUSIONS: &[&str] = &["success", "failure", "error"];
/// Every decision pr-redteam records for a finished review pass.
const REVIEW_CONCLUSIONS: &[&str] = &[
    "approve",
    "hold",
    "failed",
    "interrupted",
    "publication_rejected",
    "too_large",
];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GateRunnerHeartbeat {
    /// Stable id, e.g. `xbabe2/slot0`.
    pub runner_id: String,
    pub host: String,
    pub slot: u32,
    #[serde(default)]
    pub labels: Vec<String>,
    /// The gate this slot is running now, if any.
    #[serde(default)]
    pub current: Option<GateRunnerTask>,
    /// The last gate this slot finished.
    #[serde(default)]
    pub last: Option<GateRunnerResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GateRunnerTask {
    pub repo: String,
    pub pr: u64,
    pub sha: String,
    pub recipe: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GateRunnerResult {
    pub repo: String,
    pub pr: u64,
    pub sha: String,
    pub recipe: String,
    pub conclusion: String,
    pub seconds: u64,
    pub finished_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateRunnerRecord {
    pub heartbeat: GateRunnerHeartbeat,
    pub reporter: String,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HeartbeatAccepted {
    pub accepted: bool,
    pub runner_id: String,
    pub offline_after_seconds: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct GateRunnerStore {
    runners: Arc<Mutex<BTreeMap<String, GateRunnerRecord>>>,
    reporters: Arc<Vec<String>>,
}

impl GateRunnerStore {
    pub(crate) fn from_env() -> Self {
        let configured = std::env::var("JERYU_RUNNER_REPORTERS")
            .unwrap_or_else(|_| DEFAULT_REPORTERS.to_string());
        Self::with_reporters(configured.split(','))
    }

    pub(crate) fn with_reporters<'a>(logins: impl IntoIterator<Item = &'a str>) -> Self {
        let reporters = logins
            .into_iter()
            .map(str::trim)
            .filter(|login| !login.is_empty())
            .map(str::to_string)
            .collect();
        Self {
            runners: Arc::new(Mutex::new(BTreeMap::new())),
            reporters: Arc::new(reporters),
        }
    }

    pub(crate) fn may_report(&self, login: &str) -> bool {
        self.reporters.iter().any(|reporter| reporter == login)
    }

    pub(crate) fn record(
        &self,
        heartbeat: GateRunnerHeartbeat,
        reporter: &str,
        now: DateTime<Utc>,
    ) -> Result<HeartbeatAccepted, String> {
        validate(&heartbeat)?;
        let mut runners = self.runners.lock().expect("gate runner store lock");
        if !runners.contains_key(&heartbeat.runner_id) && runners.len() >= MAX_RUNNERS {
            return Err(format!("at most {MAX_RUNNERS} runners may report"));
        }
        let runner_id = heartbeat.runner_id.clone();
        runners.insert(
            runner_id.clone(),
            GateRunnerRecord {
                heartbeat,
                reporter: reporter.to_string(),
                received_at: now,
            },
        );
        Ok(HeartbeatAccepted {
            accepted: true,
            runner_id,
            offline_after_seconds: RUNNER_OFFLINE_AFTER_SECS,
        })
    }

    pub(crate) fn snapshot(&self) -> Vec<GateRunnerRecord> {
        self.runners
            .lock()
            .expect("gate runner store lock")
            .values()
            .cloned()
            .collect()
    }
}

pub(crate) fn is_online(record: &GateRunnerRecord, now: DateTime<Utc>) -> bool {
    (now - record.received_at).num_seconds() <= RUNNER_OFFLINE_AFTER_SECS
}

/// A heartbeat from a PR reviewer rather than a gate runner slot.
pub(crate) fn is_reviewer(heartbeat: &GateRunnerHeartbeat) -> bool {
    heartbeat.labels.iter().any(|label| label == REVIEWER_LABEL)
}

fn validate(heartbeat: &GateRunnerHeartbeat) -> Result<(), String> {
    check_token("runnerId", &heartbeat.runner_id, 128, "._/-")?;
    check_token("host", &heartbeat.host, 64, ".-")?;
    if heartbeat.labels.len() > 16 {
        return Err("labels: at most 16".to_string());
    }
    for label in &heartbeat.labels {
        check_token("labels", label, 64, "._/:- ")?;
    }
    if let Some(task) = &heartbeat.current {
        check_gate("current", &task.repo, &task.sha, &task.recipe)?;
    }
    if let Some(result) = &heartbeat.last {
        check_gate("last", &result.repo, &result.sha, &result.recipe)?;
        let (allowed, expected) = if is_reviewer(heartbeat) {
            (REVIEW_CONCLUSIONS, REVIEW_CONCLUSIONS.join(", "))
        } else {
            (GATE_CONCLUSIONS, "success, failure or error".to_string())
        };
        if !allowed.contains(&result.conclusion.as_str()) {
            return Err(format!("last.conclusion: expected {expected}"));
        }
    }
    Ok(())
}

fn check_gate(field: &str, repo: &str, sha: &str, recipe: &str) -> Result<(), String> {
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!("{field}.repo: expected owner/name"));
    };
    check_token(&format!("{field}.repo"), owner, 64, "._-")?;
    check_token(&format!("{field}.repo"), name, 64, "._-")?;
    if !(7..=64).contains(&sha.len()) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{field}.sha: expected 7-64 hex digits"));
    }
    check_token(&format!("{field}.recipe"), recipe, 64, "._/ -")
}

fn check_token(field: &str, value: &str, max: usize, extra: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > max {
        return Err(format!("{field}: expected 1-{max} characters"));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
    {
        return Err(format!("{field}: unexpected character"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn beat(runner_id: &str) -> GateRunnerHeartbeat {
        GateRunnerHeartbeat {
            runner_id: runner_id.to_string(),
            host: "xbabe2".to_string(),
            slot: 0,
            labels: vec!["pr-gate".to_string()],
            current: Some(GateRunnerTask {
                repo: "veox/jain-web".to_string(),
                pr: 13,
                sha: "abc30d78ca5eadc15694dd1434d9f8f99c44a0d3".to_string(),
                recipe: "just required".to_string(),
                started_at: Utc::now(),
            }),
            last: Some(GateRunnerResult {
                repo: "veox/jain-deploy".to_string(),
                pr: 31,
                sha: "3926cbd".to_string(),
                recipe: "just required".to_string(),
                conclusion: "success".to_string(),
                seconds: 46,
                finished_at: Utc::now(),
            }),
        }
    }

    #[test]
    fn only_configured_reporters_may_report() {
        let store = GateRunnerStore::with_reporters(" gatebot , ci-bot,".split(','));
        assert!(store.may_report("gatebot"));
        assert!(store.may_report("ci-bot"));
        assert!(!store.may_report("alton"));
        assert!(!store.may_report(""));
    }

    #[test]
    fn latest_heartbeat_per_runner_wins() {
        let store = GateRunnerStore::with_reporters(["gatebot"]);
        let now = Utc::now();
        store.record(beat("xbabe2/slot0"), "gatebot", now).unwrap();
        let mut idle = beat("xbabe2/slot0");
        idle.current = None;
        store
            .record(idle, "gatebot", now + Duration::seconds(30))
            .unwrap();
        store.record(beat("xbabe2/slot1"), "gatebot", now).unwrap();
        let runners = store.snapshot();
        assert_eq!(runners.len(), 2);
        assert_eq!(runners[0].heartbeat.runner_id, "xbabe2/slot0");
        assert!(runners[0].heartbeat.current.is_none());
    }

    #[test]
    fn runner_goes_offline_after_silence() {
        let store = GateRunnerStore::with_reporters(["gatebot"]);
        let then = Utc::now();
        store.record(beat("xbabe2/slot0"), "gatebot", then).unwrap();
        let record = &store.snapshot()[0];
        assert!(is_online(
            record,
            then + Duration::seconds(RUNNER_OFFLINE_AFTER_SECS)
        ));
        assert!(!is_online(
            record,
            then + Duration::seconds(RUNNER_OFFLINE_AFTER_SECS + 1)
        ));
    }

    #[test]
    fn malformed_heartbeats_are_refused() {
        let store = GateRunnerStore::with_reporters(["gatebot"]);
        let now = Utc::now();
        let mut bad_id = beat("xbabe2/slot0");
        bad_id.runner_id = "<script>".to_string();
        assert!(store.record(bad_id, "gatebot", now).is_err());
        let mut bad_repo = beat("xbabe2/slot0");
        bad_repo.current.as_mut().unwrap().repo = "jain-web".to_string();
        assert!(store.record(bad_repo, "gatebot", now).is_err());
        let mut bad_sha = beat("xbabe2/slot0");
        bad_sha.last.as_mut().unwrap().sha = "not-a-sha".to_string();
        assert!(store.record(bad_sha, "gatebot", now).is_err());
        let mut bad_conclusion = beat("xbabe2/slot0");
        bad_conclusion.last.as_mut().unwrap().conclusion = "green".to_string();
        assert!(store.record(bad_conclusion, "gatebot", now).is_err());
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn reviewer_beats_carry_review_verdicts() {
        let store = GateRunnerStore::with_reporters(["pragent"]);
        let now = Utc::now();
        let mut review = beat("xbabe0/redteam");
        review.labels = vec![REVIEWER_LABEL.to_string()];
        for conclusion in ["approve", "hold", "failed"] {
            review.last.as_mut().unwrap().conclusion = conclusion.to_string();
            assert!(store.record(review.clone(), "pragent", now).is_ok());
        }
        review.last.as_mut().unwrap().conclusion = "success".to_string();
        assert!(store.record(review, "pragent", now).is_err());
        let mut gate = beat("xbabe2/slot0");
        gate.last.as_mut().unwrap().conclusion = "approve".to_string();
        assert!(store.record(gate, "pragent", now).is_err());
    }

    #[test]
    fn pragent_reports_by_default() {
        let store = GateRunnerStore::with_reporters(DEFAULT_REPORTERS.split(','));
        assert!(store.may_report("pragent"));
        assert!(store.may_report("gatebot"));
        assert!(!store.may_report("alton"));
    }

    #[test]
    fn heartbeat_json_is_strict() {
        let accepted: Result<GateRunnerHeartbeat, _> = serde_json::from_str(
            r#"{"runnerId":"xbabe2/slot0","host":"xbabe2","slot":0,"surprise":true}"#,
        );
        assert!(accepted.is_err());
    }

    #[test]
    fn store_is_bounded() {
        let store = GateRunnerStore::with_reporters(["gatebot"]);
        let now = Utc::now();
        for slot in 0..MAX_RUNNERS {
            store
                .record(beat(&format!("xbabe2/slot{slot}")), "gatebot", now)
                .unwrap();
        }
        assert!(store.record(beat("xbabe2/extra"), "gatebot", now).is_err());
        assert!(store.record(beat("xbabe2/slot0"), "gatebot", now).is_ok());
    }
}
