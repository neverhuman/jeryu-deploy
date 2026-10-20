//! todoq's todo file format, byte-compatible with `todoq/todoq/todo.py`.
//!
//! A todo is one markdown file: TOML front matter between `+++` fences, then
//! the body. `dump` writes every known field in todoq's fixed order, followed
//! by any unknown keys in the order the file had them, so a round trip through
//! the server leaves a file todoq wrote byte-for-byte unchanged.

use chrono::{DateTime, Utc};
use toml_edit::{DocumentMut, Value};

use super::types::{Acceptance, Attempt, BlockKind, Criterion, ShiftTodo, TodoStatus};

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
    "assignee",
    "triaged",
    "worked_by",
    "reworks",
];

/// One todo as todoq models it, plus the unknown keys it must preserve.
#[derive(Clone, Debug)]
pub(crate) struct TodoFile {
    pub id: String,
    pub family: String,
    pub title: String,
    pub reworks: Vec<String>,
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
    /// The person who has taken the todo on (a forge login), empty when
    /// nobody has. todoq never claims a todo with an assignee. Written only
    /// when set, like `park_until`, so older files round-trip unchanged.
    pub assignee: String,
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
            reworks: Vec::new(),
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
            assignee: String::new(),
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
        todo.reworks = match take("reworks") {
            Some(Value::String(id)) => vec![id.value().clone()],
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
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
        todo.assignee = text_of("assignee");
        todo.triaged = take("triaged").and_then(Value::as_bool).unwrap_or(true);
        todo.worked_by = take("worked_by")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().cloned().collect())
            .unwrap_or_default();
        todo.extra = fields
            .into_iter()
            .filter(|(k, _)| !ORDER.contains(&k.as_str()))
            .collect();
        super::hypothesis::Hypothesis::from_file(&todo)?;
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
        if !self.assignee.is_empty() {
            out.push_str(&format!("assignee = {}\n", quote(&self.assignee)));
        }
        if !self.reworks.is_empty() {
            out.push_str(&format!(
                "reworks = [{}]\n",
                self.reworks
                    .iter()
                    .map(|id| quote(id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
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

    /// A fact the claiming worker wrote about the attempt in progress
    /// (`claimed_at`, `claim_model`); only a claimed todo has one in progress.
    fn claim_fact(&self, key: &str) -> Option<String> {
        if self.status != TodoStatus::Claimed {
            return None;
        }
        self.extra
            .iter()
            .find(|(name, _)| name == key)
            .and_then(|(_, value)| value.as_str())
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    }

    pub(crate) fn attempts_list(&self) -> Vec<Attempt> {
        self.worked_by.iter().filter_map(attempt_from).collect()
    }

    pub(crate) fn to_api(&self, now: DateTime<Utc>) -> ShiftTodo {
        let family = crate::web::family::canonical(&self.family);
        ShiftTodo {
            hypothesis: super::hypothesis::Hypothesis::from_file(self).unwrap_or_default(),
            outcome_check_due_at: None,
            id: self.id.clone(),
            family_label: crate::web::family::label(&family),
            family,
            title: self.title.clone(),
            reworks: self.reworks.clone(),
            body: self.body.clone(),
            repos: self.repos.clone(),
            mode: self.mode.clone(),
            priority: self.priority,
            blocked_by: self.blocked_by.clone(),
            // Filled per family by `chain::annotate`, which needs the other todos.
            blocked_on: Vec::new(),
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
            landing: super::landing::from_file(self),
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
            triage_cost_usd: self
                .extra
                .iter()
                .find(|(key, _)| key == "triage_cost_usd")
                .and_then(|(_, value)| number(value)),
            claimed_at: self.claim_fact("claimed_at"),
            claim_model: self.claim_fact("claim_model"),
            note: self.note.clone(),
            park_until: self.park_until.clone(),
            assignee: (!self.assignee.is_empty()).then(|| self.assignee.clone()),
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
            acceptance: self.acceptance(),
        }
    }

    /// The body's acceptance criteria, byte-compatible with `todoq/criteria.py`.
    pub(crate) fn acceptance(&self) -> Acceptance {
        let items = parse_criteria(&self.body);
        let marked_none = self
            .extra
            .iter()
            .any(|(key, value)| key == "criteria" && value.as_str() == Some("none"));
        let state = match (items.is_empty(), marked_none) {
            (false, _) => "set",
            (true, true) => "none",
            (true, false) => "unknown",
        };
        Acceptance {
            state: state.to_string(),
            total: items.len() as i64,
            met: items.iter().filter(|item| item.checked).count() as i64,
            items,
        }
    }
}

/// The `## Acceptance criteria` section's `- [ ]` / `- [x]` lines, with the
/// indented `checked:` / `unmet:` line under each as its evidence. The section
/// ends at the next heading of the same or higher level; fenced code is skipped.
pub(crate) fn parse_criteria(body: &str) -> Vec<Criterion> {
    let mut items: Vec<Criterion> = Vec::new();
    let (mut fenced, mut level, mut seen) = (false, None::<usize>, false);
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        let heading = (1..=6).contains(&hashes) && line[hashes..].starts_with(' ');
        if heading {
            let title = line[hashes..].trim().trim_end_matches('#').trim();
            if title.eq_ignore_ascii_case("acceptance criteria") && !seen {
                (level, seen) = (Some(hashes), true);
                continue;
            }
            if level.is_some_and(|open| hashes <= open) {
                level = None;
            }
            continue;
        }
        if level.is_none() {
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("- [")
            .or_else(|| line.strip_prefix("* ["))
        {
            let mut chars = rest.chars();
            let mark = chars.next();
            let text = chars.as_str().strip_prefix("] ").map(str::trim);
            if let (Some(mark @ (' ' | 'x' | 'X')), Some(text)) = (mark, text)
                && !text.is_empty()
            {
                items.push(Criterion {
                    text: text.to_string(),
                    checked: mark != ' ',
                    evidence: None,
                });
                continue;
            }
        }
        let indented = line.trim_start();
        if indented.len() < line.len()
            && let Some(last) = items.last_mut()
            && let Some(evidence) = indented
                .strip_prefix("- checked:")
                .or_else(|| indented.strip_prefix("- unmet:"))
                .or_else(|| indented.strip_prefix("* checked:"))
                .or_else(|| indented.strip_prefix("* unmet:"))
        {
            let evidence = evidence.trim();
            last.evidence = (!evidence.is_empty()).then(|| evidence.to_string());
        }
    }
    items
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
    let cost_usd = table.get("cost_usd").and_then(number);
    let optional = |key: &str| {
        table
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
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
        agent: optional("agent"),
        factory: optional("factory"),
        run: optional("run"),
        experiment: optional("experiment"),
        arm_p: table.get("arm_p").and_then(number),
        cost_estimated: table.get("cost_estimated").and_then(Value::as_bool),
        usage: table
            .get("usage")
            .filter(|v| v.is_inline_table())
            .map(to_json),
    })
}

/// A TOML number as `f64`; anything else is not a number.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Float(f) => Some(*f.value()).filter(|v| v.is_finite()),
        Value::Integer(i) => Some(*i.value() as f64),
        _ => None,
    }
}

/// A TOML value as JSON: tables become objects and numbers stay numbers. A
/// float JSON cannot spell (nan, inf) becomes null.
fn to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(s) => serde_json::Value::String(s.value().clone()),
        Value::Integer(i) => serde_json::Value::from(*i.value()),
        Value::Float(f) => serde_json::Number::from_f64(*f.value())
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Boolean(b) => serde_json::Value::Bool(*b.value()),
        Value::Datetime(d) => serde_json::Value::String(d.value().to_string()),
        Value::Array(arr) => serde_json::Value::Array(arr.iter().map(to_json).collect()),
        Value::InlineTable(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(key, v)| (key.to_string(), to_json(v)))
                .collect(),
        ),
    }
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
