//! Wire types for `/api/v1/events` (see `docs/pipeline-events.md`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const MAX_BATCH: usize = 50;
pub(crate) const MAX_SUMMARY_CHARS: usize = 300;
pub(crate) const MAX_REASON_CHARS: usize = 1000;
pub(crate) const MAX_ACTOR_CHARS: usize = 128;
pub(crate) const MAX_KIND_CHARS: usize = 64;
pub(crate) const MAX_OUTCOME_CHARS: usize = 32;
pub(crate) const MAX_LOG_URL_CHARS: usize = 512;
pub(crate) const MAX_KEY_CHARS: usize = 200;
pub(crate) const MAX_EVENT_ID_CHARS: usize = 64;
/// `schema_version` of every `/api/v1/events` response.
pub(crate) const EVENTS_SCHEMA: &str = "jeryu.pipeline_events/v1";
pub(crate) const MAX_LOG_TAIL_BYTES: usize = 16 * 1024;
pub(crate) const MAX_DETAIL_BYTES: usize = 8 * 1024;

/// An event as a producer describes it: no `seq`, `ts` or `reporter`, which
/// the server assigns. Unknown fields are ignored, so a producer that echoes
/// a stored event back cannot choose its own reporter or sequence.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct NewEvent {
    /// The producer's own name for this event. Posting the same id again (as
    /// the same login) returns the stored event instead of a duplicate, so a
    /// producer may retry a POST whose response it never saw.
    #[serde(default)]
    pub event_id: Option<String>,
    pub source: String,
    pub kind: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub pr: Option<i64>,
    #[serde(default)]
    pub sha: Option<String>,
    #[serde(default)]
    pub todo_id: Option<String>,
    #[serde(default)]
    pub shift: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub needs_human: bool,
    pub summary: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub seconds: Option<i64>,
    #[serde(default)]
    pub log_tail: Option<String>,
    #[serde(default)]
    pub log_url: Option<String>,
    #[serde(default)]
    pub detail: Option<Value>,
}

impl NewEvent {
    /// A server-emitted event (`source = "forge"`).
    pub(crate) fn forge(kind: &str, summary: impl Into<String>) -> Self {
        Self {
            source: "forge".to_string(),
            kind: kind.to_string(),
            summary: summary.into(),
            ..Self::default()
        }
    }
}

/// A stored event, as `GET /api/v1/events` and the `pipeline` WebSocket scope
/// return it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct Event {
    pub seq: i64,
    pub ts: String,
    pub event_id: Option<String>,
    pub source: String,
    pub kind: String,
    pub reporter: String,
    pub actor: Option<String>,
    /// Canonical family key (see `crate::web::family`).
    pub family: Option<String>,
    /// What a reader is shown for `family`.
    pub family_label: Option<String>,
    pub repo: Option<String>,
    pub pr: Option<i64>,
    pub sha: Option<String>,
    pub todo_id: Option<String>,
    pub shift: Option<String>,
    pub outcome: Option<String>,
    pub needs_human: bool,
    pub summary: String,
    pub reason: Option<String>,
    pub cost_usd: Option<f64>,
    pub seconds: Option<i64>,
    pub log_tail: Option<String>,
    pub log_url: Option<String>,
    pub detail: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct EventsQuery {
    pub after_seq: Option<i64>,
    /// Another name for `after_seq`, the spelling clients guess first.
    pub since: Option<i64>,
    pub before_seq: Option<i64>,
    pub limit: Option<i64>,
    /// Either spelling of a family key; the server canonicalises it.
    pub family: Option<String>,
    pub repo: Option<String>,
    pub pr: Option<i64>,
    pub todo_id: Option<String>,
    pub source: Option<String>,
    pub kind: Option<String>,
    pub needs_human: Option<bool>,
}

impl crate::web::strict_query::StrictFields for EventsQuery {
    const KEYS: &'static [&'static str] = &[
        "after_seq",
        "since",
        "before_seq",
        "limit",
        "family",
        "repo",
        "pr",
        "todo_id",
        "source",
        "kind",
        "needs_human",
    ];

    /// The route keeps its own published code for a refused query, and names
    /// the types its cursors take: the keys alone do not say that.
    const CODE: &'static str = "events_invalid_query";

    const REPAIR_HINT: &'static str =
        "after_seq, before_seq, limit and pr are integers; needs_human is true or false";
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct EventsResponse {
    pub schema_version: &'static str,
    pub events: Vec<Event>,
    pub latest_seq: i64,
    /// The row limit this page was read with.
    pub limit: i64,
}

fn is_lower(c: char) -> bool {
    c.is_ascii_lowercase()
}

/// `^[a-z][a-z0-9-]{0,31}$`
fn valid_source(source: &str) -> bool {
    let mut chars = source.chars();
    chars.next().is_some_and(is_lower)
        && source.len() <= 32
        && chars.all(|c| is_lower(c) || c.is_ascii_digit() || c == '-')
}

/// A `kind` query filter: an exact kind (`todo.claimed`) or a prefix ending in
/// a dot (`todo.`). Anything else can only ever match nothing, and answering
/// that with an empty page tells an agent "no such events" when the truth is
/// "no such kind".
pub(super) fn valid_kind_filter(filter: &str) -> bool {
    match filter.strip_suffix('.') {
        Some(prefix) => !prefix.is_empty() && valid_kind(&format!("{prefix}.x")),
        None => valid_kind(filter),
    }
}

/// `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$`, at most [`MAX_KIND_CHARS`].
fn valid_kind(kind: &str) -> bool {
    let segment = |s: &str| {
        let mut chars = s.chars();
        chars.next().is_some_and(is_lower)
            && chars.all(|c| is_lower(c) || c.is_ascii_digit() || c == '_')
    };
    kind.len() <= MAX_KIND_CHARS && kind.split('.').count() >= 2 && kind.split('.').all(segment)
}

fn valid_repo(repo: &str) -> bool {
    let part = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let mut parts = repo.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(owner), Some(name), None) if part(owner) && part(name)
    ) && repo.len() <= MAX_KEY_CHARS
}

fn clip_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Keep the last [`MAX_LOG_TAIL_BYTES`] bytes of a log, cut on a char boundary.
pub(crate) fn clip_log_tail(log: &str) -> String {
    if log.len() <= MAX_LOG_TAIL_BYTES {
        return log.to_string();
    }
    let mut start = log.len() - MAX_LOG_TAIL_BYTES;
    while !log.is_char_boundary(start) {
        start += 1;
    }
    log[start..].to_string()
}

fn blank(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn key(name: &str, value: Option<String>) -> Result<Option<String>, String> {
    let value = blank(value);
    match &value {
        Some(v) if v.chars().count() > MAX_KEY_CHARS => {
            Err(format!("{name}: at most {MAX_KEY_CHARS} characters"))
        }
        Some(v) if v.chars().any(char::is_control) => {
            Err(format!("{name}: control characters are not allowed"))
        }
        _ => Ok(value),
    }
}

/// Check an event against the contract and normalise it for storage.
///
/// Identity fields (`source`, `kind`, `repo`, `sha`, `outcome`, `log_url`,
/// `detail` size) are rejected when malformed. Human text is clipped instead:
/// `summary`, `reason` and `actor` to their character limits, and `log_tail`
/// to its last 16 KiB, because producers send these best-effort and a long
/// line is no reason to lose the event.
pub(crate) fn normalize(event: NewEvent) -> Result<NewEvent, String> {
    let event_id = blank(event.event_id);
    if let Some(id) = &event_id
        && !(id.len() <= MAX_EVENT_ID_CHARS
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')))
    {
        return Err(format!(
            "event_id: at most {MAX_EVENT_ID_CHARS} characters of [A-Za-z0-9._:-]"
        ));
    }
    if !valid_source(&event.source) {
        return Err("source: expected ^[a-z][a-z0-9-]{0,31}$".to_string());
    }
    if !valid_kind(&event.kind) {
        return Err(format!(
            "kind: expected dotted lower-case words such as todo.claimed, at most {MAX_KIND_CHARS} characters"
        ));
    }
    let summary = event.summary.trim();
    if summary.is_empty() {
        return Err("summary: must not be empty".to_string());
    }
    let repo = blank(event.repo);
    if let Some(repo) = &repo
        && !valid_repo(repo)
    {
        return Err("repo: expected owner/name".to_string());
    }
    let sha = blank(event.sha).map(|s| s.to_ascii_lowercase());
    if let Some(sha) = &sha
        && !((7..=64).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return Err("sha: expected 7 to 64 hex characters".to_string());
    }
    if event.pr.is_some_and(|pr| pr < 1) {
        return Err("pr: must be a positive number".to_string());
    }
    let outcome = blank(event.outcome);
    if let Some(outcome) = &outcome
        && !(outcome.len() <= MAX_OUTCOME_CHARS
            && outcome
                .chars()
                .all(|c| is_lower(c) || c.is_ascii_digit() || c == '_'))
    {
        return Err(format!(
            "outcome: expected a lower-case word of at most {MAX_OUTCOME_CHARS} characters"
        ));
    }
    let log_url = blank(event.log_url);
    if let Some(url) = &log_url
        && (url.chars().count() > MAX_LOG_URL_CHARS || url.chars().any(char::is_control))
    {
        return Err(format!("log_url: at most {MAX_LOG_URL_CHARS} characters"));
    }
    if event.cost_usd.is_some_and(|c| !c.is_finite() || c < 0.0) {
        return Err("cost_usd: must be a non-negative number".to_string());
    }
    if event.seconds.is_some_and(|s| s < 0) {
        return Err("seconds: must not be negative".to_string());
    }
    let detail = match event.detail {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) => {
            let value = Value::Object(map);
            if value.to_string().len() > MAX_DETAIL_BYTES {
                return Err(format!("detail: at most {MAX_DETAIL_BYTES} bytes of JSON"));
            }
            Some(value)
        }
        Some(_) => return Err("detail: must be a JSON object".to_string()),
    };
    Ok(NewEvent {
        event_id,
        source: event.source,
        kind: event.kind,
        actor: blank(event.actor).map(|a| clip_chars(&a, MAX_ACTOR_CHARS)),
        family: key("family", event.family)?.map(|family| crate::web::family::canonical(&family)),
        repo,
        pr: event.pr,
        sha,
        todo_id: key("todo_id", event.todo_id)?,
        shift: key("shift", event.shift)?,
        outcome,
        needs_human: event.needs_human,
        summary: clip_chars(summary, MAX_SUMMARY_CHARS),
        reason: blank(event.reason).map(|r| clip_chars(&r, MAX_REASON_CHARS)),
        cost_usd: event.cost_usd,
        seconds: event.seconds,
        log_tail: event
            .log_tail
            .filter(|l| !l.trim().is_empty())
            .map(|l| clip_log_tail(&l)),
        log_url,
        detail,
    })
}
