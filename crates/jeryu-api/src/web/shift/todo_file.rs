//! todoq's todo file format, byte-compatible with `todoq/todoq/todo.py`.
//!
//! A todo is one markdown file: TOML front matter between `+++` fences, then
//! the body. `dump` writes every known field in todoq's fixed order, followed
//! by any unknown keys in the order the file had them, so a round trip through
//! the server leaves a file todoq wrote byte-for-byte unchanged.

use chrono::{DateTime, Utc};
use toml_edit::{DocumentMut, Value};

use super::types::{Attempt, BlockKind, ShiftTodo, TodoStatus};

pub(crate) const FENCE: &str = "+++";
pub(crate) const MODES: &[&str] = &["now", "night"];

/// todoq's `_ORDER`: the fixed field order `dump` writes.
const ORDER: &[&str] = &[
    "id",
    "family",
    "title",
    "repos",
    "mode",
    "priority",
    "blocked_by",
    "status",
    "attempts",
    "requested_by",
    "filed_at",
    "claim_by",
    "lease_until",
    "shift",
    "change_set",
    "commits",
    "merged",
    "note",
    "park_until",
    "triaged",
    "worked_by",
];

/// One todo as todoq models it, plus the unknown keys it must preserve.
#[derive(Clone, Debug)]
pub(crate) struct TodoFile {
    pub id: String,
    pub family: String,
    pub title: String,
    pub body: String,
    pub repos: Vec<String>,
    pub mode: String,
    pub priority: i64,
    pub blocked_by: Vec<String>,
    pub status: TodoStatus,
    pub attempts: i64,
    pub requested_by: String,
    pub filed_at: String,
    pub claim_by: String,
    pub lease_until: String,
    pub shift: String,
    pub change_set: String,
    pub commits: Vec<(String, String)>,
    pub merged: bool,
    pub note: String,
    /// When a parked todo comes back by itself, RFC 3339; empty for a park
    /// with no date. Written only when it is set, so a file todoq wrote
    /// without the key round-trips byte-for-byte.
    pub park_until: String,
    pub triaged: bool,
    /// Attempt records kept as raw TOML so their key order round-trips.
    pub worked_by: Vec<Value>,
    /// Front-matter keys this version does not know, preserved verbatim.
    pub extra: Vec<(String, Value)>,
}

impl TodoFile {
    pub(crate) fn new(id: String, family: String, title: String) -> Self {
        Self {
            id,
            family,
            title,
            body: String::new(),
            repos: Vec::new(),
            mode: "night".to_string(),
            priority: 3,
            blocked_by: Vec::new(),
            status: TodoStatus::Open,
            attempts: 0,
            requested_by: String::new(),
            filed_at: String::new(),
            claim_by: String::new(),
            lease_until: String::new(),
            shift: String::new(),
            change_set: String::new(),
            commits: Vec::new(),
            merged: false,
            note: String::new(),
            park_until: String::new(),
            triaged: true,
            worked_by: Vec::new(),
            extra: Vec::new(),
        }
    }

    /// todoq's `Todo.filename`.
    pub(crate) fn filename(&self) -> String {
        format!("{}-{}.md", self.id, slugify(&self.title, 48))
    }

    /// Parse a todo file exactly as todoq's `load` does.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        if !text.starts_with("+++\n") {
            return Err("todo file must start with a +++ front-matter fence".to_string());
        }
        let end = text[FENCE.len()..]
            .find("\n+++")
            .map(|at| at + FENCE.len())
            .ok_or("todo front matter is not closed with +++")?;
        let meta = text.get(FENCE.len() + 1..end).unwrap_or("");
        let body = text.get(end + FENCE.len() + 2..).unwrap_or("");
        let doc: DocumentMut = meta
            .parse()
            .map_err(|err| format!("front matter is not TOML: {err}"))?;
        let mut fields: Vec<(String, Value)> = Vec::new();
        for (key, item) in doc.as_table().iter() {
            let value = item
                .clone()
                .into_value()
                .map_err(|_| format!("front-matter key {key} is not a value"))?;
            fields.push((key.to_string(), value));
        }
        if !fields.iter().any(|(k, _)| k == "requested_by")
            && let Some((_, filed_by)) = fields.iter().find(|(k, _)| k == "filed_by")
        {
            fields.push(("requested_by".to_string(), filed_by.clone()));
        }
        fields.retain(|(k, _)| k != "filed_by");

        let take = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        let text_of = |key: &str| take(key).and_then(Value::as_str).unwrap_or("").to_string();
        let list_of = |key: &str| {
            take(key)
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let id = take("id").and_then(Value::as_str).ok_or("todo has no id")?;
        let mut todo = Self::new(id.to_string(), text_of("family"), text_of("title"));
        todo.body = body.trim_matches('\n').to_string();
        todo.repos = list_of("repos");
        if let Some(mode) = take("mode").and_then(Value::as_str) {
            todo.mode = mode.to_string();
        }
        todo.priority = take("priority").and_then(Value::as_integer).unwrap_or(3);
        todo.blocked_by = list_of("blocked_by");
        if let Some(status) = take("status").and_then(Value::as_str) {
            todo.status =
                TodoStatus::parse(status).ok_or_else(|| format!("unknown status {status:?}"))?;
        }
        todo.attempts = take("attempts").and_then(Value::as_integer).unwrap_or(0);
        todo.requested_by = text_of("requested_by");
        todo.filed_at = text_of("filed_at");
        todo.claim_by = text_of("claim_by");
        todo.lease_until = text_of("lease_until");
        todo.shift = text_of("shift");
        todo.change_set = text_of("change_set");
        todo.commits = take("commits")
            .and_then(Value::as_inline_table)
            .map(|table| {
                table
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|sha| (k.to_string(), sha.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        todo.merged = take("merged").and_then(Value::as_bool).unwrap_or(false);
        todo.note = text_of("note");
        todo.park_until = text_of("park_until");
        todo.triaged = take("triaged").and_then(Value::as_bool).unwrap_or(true);
        todo.worked_by = take("worked_by")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().cloned().collect())
            .unwrap_or_default();
        todo.extra = fields
            .into_iter()
            .filter(|(k, _)| !ORDER.contains(&k.as_str()))
            .collect();
        if !MODES.contains(&todo.mode.as_str()) {
            return Err(format!("unknown mode {:?}", todo.mode));
        }
        Ok(todo)
    }

    /// Write the file exactly as todoq's `dump` does.
    pub(crate) fn dump(&self) -> String {
        let strings = |items: &[String]| {
            format!(
                "[{}]",
                items
                    .iter()
                    .map(|s| quote(s))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let commits = if self.commits.is_empty() {
            "{}".to_string()
        } else {
            let inner: Vec<String> = self
                .commits
                .iter()
                .map(|(repo, sha)| format!("{} = {}", quote(repo), quote(sha)))
                .collect();
            format!("{{ {} }}", inner.join(", "))
        };
        let worked_by = format!(
            "[{}]",
            self.worked_by
                .iter()
                .map(render)
                .collect::<Vec<_>>()
                .join(", ")
        );
        let known: [(&str, String); 20] = [
            ("id", quote(&self.id)),
            ("family", quote(&self.family)),
            ("title", quote(&self.title)),
            ("repos", strings(&self.repos)),
            ("mode", quote(&self.mode)),
            ("priority", self.priority.to_string()),
            ("blocked_by", strings(&self.blocked_by)),
            ("status", quote(self.status.as_str())),
            ("attempts", self.attempts.to_string()),
            ("requested_by", quote(&self.requested_by)),
            ("filed_at", quote(&self.filed_at)),
            ("claim_by", quote(&self.claim_by)),
            ("lease_until", quote(&self.lease_until)),
            ("shift", quote(&self.shift)),
            ("change_set", quote(&self.change_set)),
            ("commits", commits),
            ("merged", self.merged.to_string()),
            ("note", quote(&self.note)),
            ("triaged", self.triaged.to_string()),
            ("worked_by", worked_by),
        ];
        let mut out = String::from("+++\n");
        for (key, value) in known {
            out.push_str(&format!("{key} = {value}\n"));
        }
        if !self.park_until.is_empty() {
            out.push_str(&format!("park_until = {}\n", quote(&self.park_until)));
        }
        for (key, value) in &self.extra {
            out.push_str(&format!("{key} = {}\n", render(value)));
        }
        out.push_str("+++\n");
        let body = self.body.trim_matches('\n');
        if !body.is_empty() {
            out.push_str(body);
            out.push('\n');
        }
        out
    }

    /// `status == claimed` with a lease that has not run out.
    pub(crate) fn lease_live(&self, now: DateTime<Utc>) -> bool {
        self.status == TodoStatus::Claimed
            && DateTime::parse_from_rfc3339(&self.lease_until)
                .map(|until| until.with_timezone(&Utc) > now)
                .unwrap_or(false)
    }

    pub(crate) fn attempts_list(&self) -> Vec<Attempt> {
        self.worked_by.iter().filter_map(attempt_from).collect()
    }

    pub(crate) fn to_api(&self, now: DateTime<Utc>) -> ShiftTodo {
        ShiftTodo {
            id: self.id.clone(),
            family: self.family.clone(),
            title: self.title.clone(),
            body: self.body.clone(),
            repos: self.repos.clone(),
            mode: self.mode.clone(),
            priority: self.priority,
            blocked_by: self.blocked_by.clone(),
            status: self.status,
            attempts: self.attempts,
            requested_by: self.requested_by.clone(),
            filed_at: self.filed_at.clone(),
            claim_by: self.claim_by.clone(),
            lease_until: self.lease_until.clone(),
            lease_live: self.lease_live(now),
            shift: self.shift.clone(),
            change_set: self.change_set.clone(),
            commits: self.commits.iter().cloned().collect(),
            merged: self.merged,
            released: None,
            pr: None,
            prs: Vec::new(),
            cost_usd: {
                let costs: Vec<f64> = self
                    .attempts_list()
                    .iter()
                    .filter_map(|attempt| attempt.cost_usd)
                    .collect();
                (!costs.is_empty()).then(|| costs.iter().sum())
            },
            note: self.note.clone(),
            park_until: self.park_until.clone(),
            block_kind: BlockKind::derive(
                self.status,
                &self.title,
                &self.note,
                &self
                    .attempts_list()
                    .last()
                    .map(|attempt| attempt.outcome.clone())
                    .unwrap_or_default(),
            ),
            triaged: self.triaged,
            worked_by: self.attempts_list(),
        }
    }
}

fn attempt_from(value: &Value) -> Option<Attempt> {
    let table = value.as_inline_table()?;
    let text = |key: &str| {
        table
            .get(key)
            .map(|v| match v {
                Value::String(s) => s.value().clone(),
                Value::Integer(i) => i.value().to_string(),
                Value::Float(f) => f.value().to_string(),
                Value::Boolean(b) => b.value().to_string(),
                other => other.to_string().trim().to_string(),
            })
            .unwrap_or_default()
    };
    let cost_usd = table.get("cost_usd").and_then(|v| match v {
        Value::Float(f) => Some(*f.value()),
        Value::Integer(i) => Some(*i.value() as f64),
        _ => None,
    });
    Some(Attempt {
        by: text("by"),
        host: text("host"),
        slot: text("slot"),
        model: text("model"),
        session: text("session"),
        started: text("started"),
        ended: text("ended"),
        outcome: text("outcome"),
        cost_usd,
        note: text("note"),
        shift: text("shift"),
    })
}

/// A TOML basic string, spelled the way Python's `json.dumps(ensure_ascii=False)` does.
pub(crate) fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

/// Render a value the way todoq's `_toml_value` does.
fn render(value: &Value) -> String {
    match value {
        Value::String(s) => quote(s.value()),
        Value::Integer(i) => i.value().to_string(),
        Value::Float(f) => render_float(*f.value()),
        Value::Boolean(b) => b.value().to_string(),
        Value::Datetime(d) => d.value().to_string(),
        Value::Array(arr) => format!(
            "[{}]",
            arr.iter().map(render).collect::<Vec<_>>().join(", ")
        ),
        Value::InlineTable(table) => {
            if table.is_empty() {
                "{}".to_string()
            } else {
                let inner: Vec<String> = table
                    .iter()
                    .map(|(k, v)| format!("{} = {}", quote(k), render(v)))
                    .collect();
                format!("{{ {} }}", inner.join(", "))
            }
        }
    }
}

/// Python's `repr(round(value, 6))`, close enough to read back identically.
fn render_float(value: f64) -> String {
    if !value.is_finite() {
        return if value.is_nan() {
            "nan".to_string()
        } else if value > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    let rounded = (value * 1e6).round() / 1e6;
    format!("{rounded:?}")
}

/// todoq's `slugify`.
pub(crate) fn slugify(text: &str, limit: usize) -> String {
    let mut slug = String::new();
    let mut gap = false;
    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            if gap && !slug.is_empty() {
                slug.push('-');
            }
            gap = false;
            slug.push(ch);
        } else {
            gap = true;
        }
    }
    let mut cut: String = slug.chars().take(limit).collect();
    while cut.ends_with('-') {
        cut.pop();
    }
    if cut.is_empty() {
        "todo".to_string()
    } else {
        cut
    }
}

/// todoq's `new_id`: time-ordered plus 24 random bits.
pub(crate) fn new_id(now: DateTime<Utc>) -> String {
    let random = uuid::Uuid::new_v4();
    let bytes = random.as_bytes();
    format!(
        "{}-{:02x}{:02x}{:02x}",
        now.format("%Y%m%d-%H%M%S"),
        bytes[0],
        bytes[1],
        bytes[2]
    )
}

/// todoq's `iso`.
pub(crate) fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
