//! Wire shapes of the agent presence API and the checks a beat must pass
//! before anything is looked up. Limits are the contract's
//! (`docs/agent-presence.md`).

use serde::{Deserialize, Serialize};

use crate::web::paging::{PageInfo, PageParams};
use crate::web::strict_query::StrictFields;

pub(crate) const SESSION_ID_MAX: usize = 64;
pub(crate) const AGENT_MAX: usize = 32;
pub(crate) const SHORT_MAX: usize = 64;
pub(crate) const BRANCH_MAX: usize = 200;
pub(crate) const TITLE_MAX: usize = 160;
pub(crate) const LINK_MAX: usize = 200;
pub(crate) const REPOS_MAX: usize = 64;
pub(crate) const FAMILIES_MAX: usize = 8;
pub(crate) const TTL_MIN: i64 = 30;
pub(crate) const TTL_MAX: i64 = 3600;
pub(crate) const TTL_DEFAULT: i64 = 300;

/// What a session says it is doing. `stale` is never reported: the server
/// derives it from the expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresenceState {
    Active,
    Idle,
    Ended,
}

impl PresenceState {
    pub(crate) const NAMES: [&'static str; 3] = ["active", "idle", "ended"];

    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "idle" => Some(Self::Idle),
            "ended" => Some(Self::Ended),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Ended => "ended",
        }
    }
}

/// Token counts so far. Every field is optional on the wire.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tokens {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
}

/// Where a session's work can be followed. A reporter sends `pr` and `todo`;
/// the server adds `agent_run` for jeryu's own runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Links {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_run: Option<String>,
}

/// `POST /api/v1/agent-presence`: the whole current record of one session.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PresenceBeat {
    pub session_id: String,
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub operator: Option<String>,
    #[serde(default)]
    pub lane: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    pub state: String,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub families: Vec<String>,
    pub started_at: i64,
    pub last_active_at: i64,
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub tokens: Option<Tokens>,
    #[serde(default)]
    pub links: Option<IncomingLinks>,
}

/// The links a reporter may send: `agent_run` is the server's to set.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IncomingLinks {
    #[serde(default)]
    pub pr: Option<String>,
    #[serde(default)]
    pub todo: Option<String>,
}

/// A beat that passed the shape checks; repos and families are not yet
/// resolved against the forge.
#[derive(Clone, Debug)]
pub(crate) struct CheckedBeat {
    pub session_id: String,
    pub agent: String,
    pub model: Option<String>,
    pub host: Option<String>,
    pub operator: Option<String>,
    pub lane: Option<String>,
    pub branch: Option<String>,
    pub title: Option<String>,
    pub state: PresenceState,
    pub repos: Vec<String>,
    pub families: Vec<String>,
    pub started_ms: i64,
    pub last_active_ms: i64,
    pub ttl_seconds: i64,
    pub cost_usd: Option<f64>,
    pub tokens: Option<Tokens>,
    pub links: Option<Links>,
}

/// Why a beat is malformed; `ScopeEmpty` has its own error code.
#[derive(Debug, PartialEq)]
pub(crate) enum BeatError {
    Invalid(String),
    ScopeEmpty,
}

/// `[A-Za-z0-9._:-]{1,64}`.
pub(crate) fn valid_session_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= SESSION_ID_MAX
        && raw
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '-'))
}

fn text(name: &str, value: &str, max: usize) -> Result<(), String> {
    if value.chars().count() > max {
        return Err(format!("{name} is longer than {max} characters"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{name} must not contain control characters"));
    }
    Ok(())
}

/// A blank optional field is the same as an absent one.
fn optional(name: &str, value: Option<String>, max: usize) -> Result<Option<String>, String> {
    let Some(value) = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    text(name, &value, max)?;
    Ok(Some(value))
}

/// Distinct, trimmed, non-blank entries in their first order.
fn distinct(values: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        let value = value.trim().to_string();
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

fn valid_pr_link(value: &str) -> bool {
    let Some((repo, number)) = value.rsplit_once('#') else {
        return false;
    };
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let slug = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    };
    slug(owner)
        && slug(name)
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
        && number.parse::<u64>().is_ok_and(|n| n > 0)
}

fn valid_todo_link(value: &str) -> bool {
    value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | ':' | '_' | '-'))
        && value != "."
        && value != ".."
        && !value.starts_with("javascript:")
}

impl PresenceBeat {
    /// Check every field against the contract's limits.
    pub(crate) fn check(self) -> Result<CheckedBeat, BeatError> {
        let invalid = BeatError::Invalid;
        if !valid_session_id(&self.session_id) {
            return Err(invalid(format!(
                "session_id must be 1 to {SESSION_ID_MAX} characters of [A-Za-z0-9._:-]"
            )));
        }
        let agent = self.agent.trim().to_string();
        if agent.is_empty() {
            return Err(invalid("agent must not be empty".to_string()));
        }
        text("agent", &agent, AGENT_MAX).map_err(invalid)?;
        let state = PresenceState::parse(self.state.trim())
            .ok_or_else(|| invalid(format!("state must be one of {:?}", PresenceState::NAMES)))?;
        let lane = optional("lane", self.lane, SHORT_MAX).map_err(invalid)?;
        if lane
            .as_deref()
            .is_some_and(|lane| lane.contains(['/', '\\']))
        {
            return Err(invalid(
                "lane is a folder name, never a path: send its last component".to_string(),
            ));
        }
        let repos = distinct(self.repos);
        let families = distinct(self.families);
        if repos.len() > REPOS_MAX {
            return Err(invalid(format!(
                "repos lists more than {REPOS_MAX} entries"
            )));
        }
        if families.len() > FAMILIES_MAX {
            return Err(invalid(format!(
                "families lists more than {FAMILIES_MAX} entries"
            )));
        }
        if repos.is_empty() && families.is_empty() {
            return Err(BeatError::ScopeEmpty);
        }
        if self.started_at < 0 || self.last_active_at < 0 {
            return Err(invalid(
                "started_at and last_active_at are milliseconds since the epoch".to_string(),
            ));
        }
        let ttl_seconds = self.ttl_seconds.unwrap_or(TTL_DEFAULT);
        if !(TTL_MIN..=TTL_MAX).contains(&ttl_seconds) {
            return Err(invalid(format!(
                "ttl_seconds must be from {TTL_MIN} to {TTL_MAX}"
            )));
        }
        if let Some(cost) = self.cost_usd
            && !(cost.is_finite() && cost >= 0.0)
        {
            return Err(invalid(
                "cost_usd must be a number of at least 0".to_string(),
            ));
        }
        let links = match self.links {
            None => None,
            Some(links) => {
                let pr = optional("links.pr", links.pr, LINK_MAX).map_err(invalid)?;
                let todo = optional("links.todo", links.todo, LINK_MAX).map_err(invalid)?;
                if pr.as_deref().is_some_and(|value| !valid_pr_link(value)) {
                    return Err(invalid(
                        "links.pr must be owner/name#positive-number".into(),
                    ));
                }
                if todo.as_deref().is_some_and(|value| !valid_todo_link(value)) {
                    return Err(invalid("links.todo must be a todo id using letters, digits, dots, colons, underscores and hyphens".into()));
                }
                Some(Links {
                    pr,
                    todo,
                    agent_run: None,
                })
                .filter(|links| links.pr.is_some() || links.todo.is_some())
            }
        };
        Ok(CheckedBeat {
            session_id: self.session_id,
            agent,
            model: optional("model", self.model, SHORT_MAX).map_err(invalid)?,
            host: optional("host", self.host, SHORT_MAX).map_err(invalid)?,
            operator: optional("operator", self.operator, SHORT_MAX).map_err(invalid)?,
            lane,
            branch: optional("branch", self.branch, BRANCH_MAX).map_err(invalid)?,
            title: optional("title", self.title, TITLE_MAX).map_err(invalid)?,
            state,
            repos,
            families,
            started_ms: self.started_at,
            last_active_ms: self.last_active_at,
            ttl_seconds,
            cost_usd: self.cost_usd,
            tokens: self.tokens,
            links,
        })
    }
}

/// Where an item comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    External,
    AgentRun,
    QueueWorker,
    Reviewer,
    Gate,
}

impl Source {
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::AgentRun => "agent_run",
            Self::QueueWorker => "queue_worker",
            Self::Reviewer => "reviewer",
            Self::Gate => "gate",
        }
    }
}

/// How an item reached a repository's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Via {
    Repo,
    Family,
}

/// One agent on a list.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct PresenceItem {
    /// `<source>:<session_id>`, unique in the list.
    pub id: String,
    pub source: Source,
    pub session_id: String,
    /// Authenticated reporter; external display labels remain self-reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reporter: Option<String>,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `active`, `idle`, `stale` or `ended`.
    pub state: &'static str,
    /// Set on a repository's list; absent on the family/overview list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Absent when the source does not know when the work started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    pub last_active_at: i64,
    /// Absent for a source with no expiry of its own (a running agent run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    pub links: Links,
    /// Reported only to global admins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub collides_with: Vec<String>,
    /// The family/overview list only: the repositories the caller may read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repos: Option<Vec<String>>,
    /// The family/overview list only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub families: Option<Vec<String>>,
}

impl PresenceItem {
    pub(crate) fn live(&self) -> bool {
        matches!(self.state, "active" | "idle")
    }

    /// Live first, then stale, then ended.
    pub(crate) fn rank(&self) -> u8 {
        match self.state {
            "active" | "idle" => 0,
            "stale" => 1,
            _ => 2,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct PresenceList {
    pub items: Vec<PresenceItem>,
    pub total: usize,
    pub page: PageInfo,
}

/// `POST /api/v1/agent-presence` answer.
#[derive(Debug, Serialize)]
pub(crate) struct BeatAccepted {
    pub ok: bool,
    pub session_id: String,
    pub expires_at: i64,
}

/// `GET /api/v1/agent-presence` query.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct OverviewQuery {
    pub family: Option<String>,
    #[serde(flatten)]
    pub paging: PageParams,
}

impl StrictFields for OverviewQuery {
    const KEYS: &'static [&'static str] = &["family", "limit", "per_page", "page"];
}

/// `GET /api/v1/repos/:id/agent-presence` query.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct RepoQuery {
    #[serde(flatten)]
    pub paging: PageParams,
}

impl StrictFields for RepoQuery {
    const KEYS: &'static [&'static str] = &["limit", "per_page", "page"];
}
