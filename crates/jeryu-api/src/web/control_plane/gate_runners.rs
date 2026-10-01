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
//! The forge's background timers (`scripts/release/auto-pin.sh` and
//! `auto-stage.sh`) report through the same contract with the
//! [`AUTOMATION_LABEL`] label. Their `last.conclusion` says what the timer last
//! did (`opened`, `staged`, `waiting`, `failed`), their work may have no pull
//! request (`pr` is optional for every label), and they tick every five
//! minutes, so a beat may carry `intervalSeconds`: the runner is then offline
//! after `max(180, 3 * intervalSeconds)` instead of the flat 180 seconds.
//! `/runners` lists them as automation; they hold no gate slot and their beats
//! emit no pipeline events, because the scripts post their own.
//!
//! Host deploy timers report through the same contract with the
//! [`DEPLOY_LABEL`] label and a `last.target` naming where they put the sha
//! (`pages-preview`, `staging`, ...). Their `last.conclusion` says how that
//! went (`deployed`, `failed`, `skipped`). They hold no gate slot and emit no
//! pipeline events either; the repository page reads them through
//! `GET /api/v1/repos/:id/automation` so a reader can see what a merge starts.
//!
//! Who may report: logins named in `JERYU_RUNNER_REPORTERS` (comma-separated,
//! default `gatebot,pragent`), or any forge admin. The rule exists so an
//! ordinary account cannot paint fake runners; admins are not ordinary
//! accounts, and the timers run as the admin `alton2`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Seconds without a heartbeat before a runner is shown offline, for a runner
/// that reports no `intervalSeconds` (or a short one).
pub(crate) const RUNNER_OFFLINE_AFTER_SECS: i64 = 180;
/// A runner that states its interval may miss two beats before it is offline.
const OFFLINE_AFTER_INTERVALS: i64 = 3;
const INTERVAL_SECONDS: std::ops::RangeInclusive<u64> = 30..=86_400;
const MAX_RUNNERS: usize = 256;
const DEFAULT_REPORTERS: &str = "gatebot,pragent";
/// Heartbeat label that marks a PR reviewer (pr-redteam) instead of a gate slot.
pub(crate) const REVIEWER_LABEL: &str = "redteam";
/// Heartbeat label that marks a background timer (auto-pin, auto-stage).
pub(crate) const AUTOMATION_LABEL: &str = "automation";
/// Heartbeat label that marks a host deploy timer.
pub(crate) const DEPLOY_LABEL: &str = "deploy";
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
/// What a background timer last did: opened a pull request, staged a release,
/// is waiting behind earlier work, or gave up on a head.
const AUTOMATION_CONCLUSIONS: &[&str] = &["opened", "staged", "waiting", "failed"];
/// What a deploy timer last did with the sha it picked up.
const DEPLOY_CONCLUSIONS: &[&str] = &["deployed", "failed", "skipped"];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GateRunnerHeartbeat {
    /// Stable id, e.g. `xbabe2/slot0`.
    pub runner_id: String,
    pub host: String,
    pub slot: u32,
    #[serde(default)]
    pub labels: Vec<String>,
    /// How often this runner beats, 30 to 86400. Stretches the offline
    /// threshold for slow tickers; absent means the flat 180 seconds.
    #[serde(default)]
    pub interval_seconds: Option<u64>,
    /// The gate this slot is running now, if any.
    #[serde(default)]
    pub current: Option<GateRunnerTask>,
    /// The last gate this slot finished.
    #[serde(default)]
    pub last: Option<GateRunnerResult>,
    /// The runner's own code: which repository and commit it runs. Absent
    /// when the runner does not know (or predates the field).
    #[serde(default)]
    pub code: Option<RunnerCode>,
}

/// What code a runner runs: its own repository and installed commit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunnerCode {
    /// `owner/name` of the repository the runner's code comes from, or any
    /// other 1-200 character label without control characters.
    pub repo: String,
    /// The installed commit, 7 to 64 lowercase hex digits.
    pub commit: String,
    /// A free-form version label (a tag, `5.0.0`), at most 100 characters.
    #[serde(default)]
    pub version: Option<String>,
    /// When this code was installed on the runner.
    #[serde(default, alias = "installed_at")]
    pub installed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GateRunnerTask {
    pub repo: String,
    /// Absent or null for work that has no pull request (a staged commit).
    #[serde(default)]
    pub pr: Option<u64>,
    pub sha: String,
    pub recipe: String,
    /// Where a deploy timer is putting this sha; absent for every other label.
    #[serde(default)]
    pub target: Option<String>,
    /// `started_at` is accepted too: pr-redteam spells it that way, and the
    /// contract denies unknown fields, so every one of its beats was refused.
    #[serde(alias = "started_at")]
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GateRunnerResult {
    pub repo: String,
    /// Absent or null for work that has no pull request (a staged commit).
    #[serde(default)]
    pub pr: Option<u64>,
    pub sha: String,
    pub recipe: String,
    pub conclusion: String,
    /// Where a deploy timer put this sha; absent for every other label.
    #[serde(default)]
    pub target: Option<String>,
    /// Why a pass that needs a person ended the way it did, in the runner's own
    /// words (`no usable base ref: refs/heads/main does not exist`). Optional:
    /// a runner that sends none still gets a reason derived from `conclusion`.
    #[serde(default)]
    pub reason: Option<String>,
    pub seconds: u64,
    #[serde(alias = "finished_at")]
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

    /// A named reporter or any forge admin. The rule exists so an ordinary
    /// account cannot paint fake runners; admins are not ordinary accounts,
    /// and the release timers run as the admin `alton2`.
    pub(crate) fn may_report(&self, login: &str, admin: bool) -> bool {
        admin || self.reporters.iter().any(|reporter| reporter == login)
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
        let offline_after_seconds = offline_after_secs(&heartbeat);
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
            offline_after_seconds,
        })
    }

    /// The runner's last accepted heartbeat, if it has reported since startup.
    pub(crate) fn previous(&self, runner_id: &str) -> Option<GateRunnerHeartbeat> {
        self.runners
            .lock()
            .expect("gate runner store lock")
            .get(runner_id)
            .map(|record| record.heartbeat.clone())
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

/// Seconds of silence after which this runner is offline:
/// `max(180, 3 * intervalSeconds)`, so a five-minute timer is not shown
/// offline between two healthy ticks.
pub(crate) fn offline_after_secs(heartbeat: &GateRunnerHeartbeat) -> i64 {
    heartbeat
        .interval_seconds
        .and_then(|interval| i64::try_from(interval).ok())
        .map_or(RUNNER_OFFLINE_AFTER_SECS, |interval| {
            RUNNER_OFFLINE_AFTER_SECS.max(interval.saturating_mul(OFFLINE_AFTER_INTERVALS))
        })
}

pub(crate) fn is_online(record: &GateRunnerRecord, now: DateTime<Utc>) -> bool {
    (now - record.received_at).num_seconds() <= offline_after_secs(&record.heartbeat)
}

/// A heartbeat from a PR reviewer rather than a gate runner slot.
pub(crate) fn is_reviewer(heartbeat: &GateRunnerHeartbeat) -> bool {
    heartbeat.labels.iter().any(|label| label == REVIEWER_LABEL)
}

/// A heartbeat from a background timer rather than a gate runner slot.
pub(crate) fn is_automation(heartbeat: &GateRunnerHeartbeat) -> bool {
    heartbeat
        .labels
        .iter()
        .any(|label| label == AUTOMATION_LABEL)
}

/// A heartbeat from a host deploy timer rather than a gate runner slot.
pub(crate) fn is_deploy(heartbeat: &GateRunnerHeartbeat) -> bool {
    heartbeat.labels.iter().any(|label| label == DEPLOY_LABEL)
}

/// Where a finished pass put its sha, for the labels that have a target.
pub(crate) fn gate_runner_target(result: &GateRunnerResult) -> Option<&str> {
    result.target.as_deref()
}

/// A gate runner slot: not the reviewer, a background timer or a deploy timer.
/// Only these count as gate capacity, and only these keep `gate_runner_down`
/// quiet.
pub(crate) fn holds_gate_slot(heartbeat: &GateRunnerHeartbeat) -> bool {
    !is_reviewer(heartbeat) && !is_automation(heartbeat) && !is_deploy(heartbeat)
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
    let kinds = [
        (is_reviewer(heartbeat), REVIEWER_LABEL),
        (is_automation(heartbeat), AUTOMATION_LABEL),
        (is_deploy(heartbeat), DEPLOY_LABEL),
    ];
    let claimed: Vec<&str> = kinds
        .iter()
        .filter(|(present, _)| *present)
        .map(|(_, label)| *label)
        .collect();
    if claimed.len() > 1 {
        return Err(format!("labels: {} are exclusive", claimed.join(" and ")));
    }
    if let Some(interval) = heartbeat.interval_seconds
        && !INTERVAL_SECONDS.contains(&interval)
    {
        return Err(format!(
            "intervalSeconds: expected {} to {}",
            INTERVAL_SECONDS.start(),
            INTERVAL_SECONDS.end()
        ));
    }
    if let Some(task) = &heartbeat.current {
        check_gate("current", &task.repo, &task.sha, &task.recipe)?;
        check_target("current", task.target.as_deref(), is_deploy(heartbeat))?;
    }
    if let Some(result) = &heartbeat.last {
        check_gate("last", &result.repo, &result.sha, &result.recipe)?;
        check_target("last", result.target.as_deref(), is_deploy(heartbeat))?;
        let (allowed, expected) = if is_deploy(heartbeat) {
            (DEPLOY_CONCLUSIONS, DEPLOY_CONCLUSIONS.join(", "))
        } else if is_automation(heartbeat) {
            (AUTOMATION_CONCLUSIONS, AUTOMATION_CONCLUSIONS.join(", "))
        } else if is_reviewer(heartbeat) {
            (REVIEW_CONCLUSIONS, REVIEW_CONCLUSIONS.join(", "))
        } else {
            (GATE_CONCLUSIONS, "success, failure or error".to_string())
        };
        if !allowed.contains(&result.conclusion.as_str()) {
            return Err(format!("last.conclusion: expected {expected}"));
        }
    }
    if let Some(code) = &heartbeat.code {
        check_code(code)?;
    }
    Ok(())
}

const CODE_REPO_MAX: usize = 200;
const CODE_VERSION_MAX: usize = 100;

fn check_code(code: &RunnerCode) -> Result<(), String> {
    check_text("code.repo", &code.repo, CODE_REPO_MAX)?;
    if !(7..=64).contains(&code.commit.len())
        || !code
            .commit
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err("code.commit: expected 7-64 lowercase hex digits".to_string());
    }
    if let Some(version) = &code.version {
        check_text("code.version", version, CODE_VERSION_MAX)?;
    }
    Ok(())
}

/// Free text for display: 1 to `max` characters, no control characters.
fn check_text(field: &str, value: &str, max: usize) -> Result<(), String> {
    if value.is_empty() || value.chars().count() > max {
        return Err(format!("{field}: expected 1-{max} characters"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{field}: unexpected character"));
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

/// A deploy beat must name its target; no other label may carry one, so the
/// page never shows a target for a pass that did not deploy anything.
fn check_target(field: &str, target: Option<&str>, deploy: bool) -> Result<(), String> {
    match (target, deploy) {
        (Some(target), true) => check_token(&format!("{field}.target"), target, 64, "._/:- "),
        (None, true) => Err(format!(
            "{field}.target: required with the {DEPLOY_LABEL} label"
        )),
        (Some(_), false) => Err(format!(
            "{field}.target: only the {DEPLOY_LABEL} label carries a target"
        )),
        (None, false) => Ok(()),
    }
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
            interval_seconds: None,
            current: Some(GateRunnerTask {
                repo: "veox/jain-web".to_string(),
                pr: Some(13),
                sha: "abc30d78ca5eadc15694dd1434d9f8f99c44a0d3".to_string(),
                recipe: "just required".to_string(),
                target: None,
                started_at: Utc::now(),
            }),
            last: Some(GateRunnerResult {
                repo: "veox/jain-deploy".to_string(),
                pr: Some(31),
                sha: "3926cbd".to_string(),
                recipe: "just required".to_string(),
                conclusion: "success".to_string(),
                target: None,
                reason: None,
                seconds: 46,
                finished_at: Utc::now(),
            }),
            code: None,
        }
    }

    #[test]
    fn only_configured_reporters_may_report() {
        let store = GateRunnerStore::with_reporters(" gatebot , ci-bot,".split(','));
        assert!(store.may_report("gatebot", false));
        assert!(store.may_report("ci-bot", false));
        assert!(!store.may_report("alton", false));
        assert!(!store.may_report("", false));
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

    /// A deploy timer says where it put the sha and how that went. Its target
    /// is mandatory, its verdicts are its own, it holds no gate slot, and no
    /// other label may claim a target.
    #[test]
    fn deploy_beats_carry_a_target_and_a_deploy_verdict() {
        let store = GateRunnerStore::with_reporters(["alton2"]);
        let now = Utc::now();
        let mut deploy = beat("buildhost1/publish");
        deploy.labels = vec![DEPLOY_LABEL.to_string()];
        deploy.current = None;
        deploy.last.as_mut().unwrap().target = Some("edge-pages".to_string());
        for conclusion in ["deployed", "failed", "skipped"] {
            deploy.last.as_mut().unwrap().conclusion = conclusion.to_string();
            assert!(store.record(deploy.clone(), "alton2", now).is_ok());
        }
        assert!(!holds_gate_slot(&deploy), "a deployer holds no gate slot");

        // A gate verdict is not a deploy verdict.
        deploy.last.as_mut().unwrap().conclusion = "success".to_string();
        assert!(store.record(deploy.clone(), "alton2", now).is_err());

        // A deploy beat with no target says nothing useful.
        deploy.last.as_mut().unwrap().conclusion = "deployed".to_string();
        deploy.last.as_mut().unwrap().target = None;
        assert!(store.record(deploy.clone(), "alton2", now).is_err());

        // Only a deploy beat carries a target.
        let mut gate = beat("xbabe2/slot0");
        gate.last.as_mut().unwrap().target = Some("edge-pages".to_string());
        assert!(store.record(gate, "alton2", now).is_err());

        // Two kinds at once is a contradiction, not a runner.
        let mut both = beat("buildhost1/publish");
        both.labels = vec![DEPLOY_LABEL.to_string(), REVIEWER_LABEL.to_string()];
        assert!(store.record(both, "alton2", now).is_err());
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
        assert!(store.may_report("pragent", false));
        assert!(store.may_report("gatebot", false));
        assert!(!store.may_report("alton", false));
    }

    #[test]
    fn the_beat_pr_redteam_really_sends_is_accepted() {
        // Verbatim shape from ~/pr-redteam (2026-09-19): snake_case stamps. The
        // contract denied them as unknown fields, so no reviewer ever showed
        // on /runners, and the refusal was plain text the tool did not log.
        let beat: GateRunnerHeartbeat = serde_json::from_str(
            r#"{"runnerId":"xbabe0/redteam","host":"xbabe0","slot":0,"labels":["redteam"],
                "current":{"repo":"jeryu/jeryu-web","pr":43,"sha":"30106a749a37f0d8","recipe":"redteam-review","started_at":"2026-09-19T17:47:30Z"},
                "last":{"repo":"jeryu/jeryu-deploy","pr":63,"sha":"3ccfa84d8e8f07cd","recipe":"redteam-review","conclusion":"approve","seconds":14,"finished_at":"2026-09-19T17:47:48Z"}}"#,
        )
        .expect("snake_case stamps are accepted");
        assert!(is_reviewer(&beat));
        let store = GateRunnerStore::with_reporters(["pragent"]);
        assert!(store.record(beat, "pragent", Utc::now()).is_ok());
        // The camelCase spelling the gate runner sends still works.
        let camel: Result<GateRunnerResult, _> = serde_json::from_str(
            r#"{"repo":"veox/jain-web","pr":1,"sha":"abc30d78","recipe":"just required","conclusion":"success","seconds":9,"finishedAt":"2026-09-19T17:47:48Z"}"#,
        );
        assert!(camel.is_ok());
    }

    fn timer(runner_id: &str) -> GateRunnerHeartbeat {
        serde_json::from_str(&format!(
            r#"{{"runnerId":"{runner_id}","host":"xbabe0","slot":0,"labels":["automation"],
                "intervalSeconds":300,
                "last":{{"repo":"jeryu/jeryu-deploy","sha":"77dc3310aa","recipe":"auto-stage",
                         "conclusion":"staged","seconds":0,"finishedAt":"2026-09-20T04:10:00Z"}}}}"#
        ))
        .expect("an automation beat without pr parses")
    }

    #[test]
    fn automation_beats_need_no_pull_request() {
        let store = GateRunnerStore::with_reporters(["gatebot"]);
        let beat = timer("xbabe0/auto-stage");
        assert!(is_automation(&beat));
        assert_eq!(beat.last.as_ref().unwrap().pr, None);
        let accepted = store.record(beat, "alton2", Utc::now()).unwrap();
        assert_eq!(accepted.offline_after_seconds, 900);
        assert_eq!(store.snapshot()[0].heartbeat.runner_id, "xbabe0/auto-stage");
        // An explicit null is the same as leaving `pr` out, for every label.
        let gate: GateRunnerTask = serde_json::from_str(
            r#"{"repo":"veox/jain-web","pr":null,"sha":"abc30d78","recipe":"just required","startedAt":"2026-09-19T17:47:48Z"}"#,
        )
        .unwrap();
        assert_eq!(gate.pr, None);
    }

    #[test]
    fn automation_beats_carry_their_own_conclusions() {
        let store = GateRunnerStore::with_reporters(["gatebot"]);
        let now = Utc::now();
        let mut pin = timer("xbabe0/auto-pin");
        for conclusion in AUTOMATION_CONCLUSIONS {
            pin.last.as_mut().unwrap().conclusion = (*conclusion).to_string();
            assert!(store.record(pin.clone(), "alton2", now).is_ok());
        }
        for conclusion in ["success", "approve", "done"] {
            pin.last.as_mut().unwrap().conclusion = conclusion.to_string();
            let refused = store.record(pin.clone(), "alton2", now).unwrap_err();
            assert_eq!(
                refused,
                "last.conclusion: expected opened, staged, waiting, failed"
            );
        }
        let mut gate = beat("xbabe2/slot0");
        gate.last.as_mut().unwrap().conclusion = "staged".to_string();
        assert!(store.record(gate, "gatebot", now).is_err());
        let mut both = timer("xbabe0/auto-pin");
        both.labels.push(REVIEWER_LABEL.to_string());
        assert!(store.record(both, "alton2", now).is_err());
    }

    #[test]
    fn interval_is_bounded() {
        let store = GateRunnerStore::with_reporters(["gatebot"]);
        let now = Utc::now();
        for (interval, ok) in [(29, false), (30, true), (86_400, true), (86_401, false)] {
            let mut beat = timer("xbabe0/auto-pin");
            beat.interval_seconds = Some(interval);
            let recorded = store.record(beat, "alton2", now);
            assert_eq!(recorded.is_ok(), ok, "interval {interval}");
            if let Err(reason) = recorded {
                assert_eq!(reason, "intervalSeconds: expected 30 to 86400");
            }
        }
        // A short interval never shortens the flat threshold.
        let mut quick = timer("xbabe0/auto-pin");
        quick.interval_seconds = Some(30);
        assert_eq!(offline_after_secs(&quick), RUNNER_OFFLINE_AFTER_SECS);
        assert_eq!(
            offline_after_secs(&beat("xbabe2/slot0")),
            RUNNER_OFFLINE_AFTER_SECS
        );
    }

    #[test]
    fn offline_threshold_honours_the_interval() {
        let store = GateRunnerStore::with_reporters(["gatebot"]);
        let then = Utc::now();
        store
            .record(timer("xbabe0/auto-pin"), "alton2", then)
            .unwrap();
        let record = &store.snapshot()[0];
        assert!(is_online(record, then + Duration::seconds(181)));
        assert!(is_online(record, then + Duration::seconds(900)));
        assert!(!is_online(record, then + Duration::seconds(901)));
    }

    #[test]
    fn a_timer_or_a_reviewer_is_not_a_gate_slot() {
        // The inbox raises `gate_runner_down` when nothing that holds a gate
        // slot is online: a beating timer must not hide a dead gate host.
        assert!(holds_gate_slot(&beat("xbabe2/slot0")));
        assert!(!holds_gate_slot(&timer("xbabe0/auto-pin")));
        let mut review = beat("xbabe0/redteam");
        review.labels = vec![REVIEWER_LABEL.to_string()];
        assert!(!holds_gate_slot(&review));
    }

    #[test]
    fn admins_report_without_being_named() {
        let store = GateRunnerStore::with_reporters(DEFAULT_REPORTERS.split(','));
        assert!(store.may_report("alton2", true));
        assert!(!store.may_report("alton2", false));
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

    #[test]
    fn code_is_accepted_and_kept() {
        let parsed: GateRunnerHeartbeat = serde_json::from_str(
            r#"{"runnerId":"gate-a/slot0","host":"gate-a","slot":0,
                "code":{"repo":"acme/gate-scripts","commit":"0123456789abcdef0123456789abcdef01234567",
                        "version":"gate-scripts-v1.2.0","installedAt":"2026-09-30T12:00:00Z"}}"#,
        )
        .expect("a beat with code parses");
        let code = parsed.code.clone().expect("code kept");
        assert_eq!(code.repo, "acme/gate-scripts");
        assert_eq!(code.version.as_deref(), Some("gate-scripts-v1.2.0"));
        assert!(code.installed_at.is_some());
        let store = GateRunnerStore::with_reporters(["gatebot"]);
        assert!(store.record(parsed, "gatebot", Utc::now()).is_ok());
        assert!(store.snapshot()[0].heartbeat.code.is_some());

        // Only repo and commit are required; snake_case installed_at is accepted.
        let minimal: GateRunnerHeartbeat = serde_json::from_str(
            r#"{"runnerId":"gate-a/slot1","host":"gate-a","slot":1,
                "code":{"repo":"acme/gate-scripts","commit":"abc1234","installed_at":"2026-09-30T12:00:00Z"}}"#,
        )
        .expect("a minimal code parses");
        assert!(store.record(minimal, "gatebot", Utc::now()).is_ok());
    }

    #[test]
    fn code_is_strict() {
        let unknown: Result<GateRunnerHeartbeat, _> = serde_json::from_str(
            r#"{"runnerId":"gate-a/slot0","host":"gate-a","slot":0,
                "code":{"repo":"acme/gate-scripts","commit":"abc1234","branch":"main"}}"#,
        );
        assert!(unknown.is_err(), "unknown code fields are refused");
        let missing: Result<GateRunnerHeartbeat, _> = serde_json::from_str(
            r#"{"runnerId":"gate-a/slot0","host":"gate-a","slot":0,"code":{"repo":"acme/gate-scripts"}}"#,
        );
        assert!(missing.is_err(), "commit is required");

        let store = GateRunnerStore::with_reporters(["gatebot"]);
        let now = Utc::now();
        let with = |repo: &str, commit: &str, version: Option<&str>| {
            let mut beat = beat("gate-a/slot0");
            beat.code = Some(RunnerCode {
                repo: repo.to_string(),
                commit: commit.to_string(),
                version: version.map(str::to_string),
                installed_at: None,
            });
            beat
        };
        let cases = [
            (with("acme/gate-scripts", "abc1234", None), None),
            (
                with(&"r".repeat(200), &"a".repeat(64), Some(&"v".repeat(100))),
                None,
            ),
            (
                with("", "abc1234", None),
                Some("code.repo: expected 1-200 characters"),
            ),
            (
                with(&"r".repeat(201), "abc1234", None),
                Some("code.repo: expected 1-200 characters"),
            ),
            (
                with("acme/gate\nscripts", "abc1234", None),
                Some("code.repo: unexpected character"),
            ),
            (
                with("acme/gate-scripts", "abc123", None),
                Some("code.commit: expected 7-64 lowercase hex digits"),
            ),
            (
                with("acme/gate-scripts", &"a".repeat(65), None),
                Some("code.commit: expected 7-64 lowercase hex digits"),
            ),
            (
                with("acme/gate-scripts", "ABC1234", None),
                Some("code.commit: expected 7-64 lowercase hex digits"),
            ),
            (
                with("acme/gate-scripts", "not-a-sha", None),
                Some("code.commit: expected 7-64 lowercase hex digits"),
            ),
            (
                with("acme/gate-scripts", "abc1234", Some("")),
                Some("code.version: expected 1-100 characters"),
            ),
            (
                with("acme/gate-scripts", "abc1234", Some(&"v".repeat(101))),
                Some("code.version: expected 1-100 characters"),
            ),
            (
                with("acme/gate-scripts", "abc1234", Some("v1\t2")),
                Some("code.version: unexpected character"),
            ),
        ];
        for (beat, expected) in cases {
            let recorded = store.record(beat, "gatebot", now);
            assert_eq!(recorded.err().as_deref(), expected);
        }
    }
}
