//! Read-only real-data dev helpers for local validation.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};

use super::flow;
use super::pipeline::{Event, EventStore};

const PAGE_LIMIT: i64 = 500;
const TOKEN_MODE: u32 = 0o600;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullConfig {
    pub base_url: String,
    pub token_file: PathBuf,
    pub data_dir: PathBuf,
    pub since: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompareConfig {
    pub data_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullSummary {
    pub events_pulled: usize,
    pub duplicate_events: usize,
    pub first_seq: Option<i64>,
    pub last_seq: i64,
    pub flow_ledger_rows: i64,
    pub cost_ledger_rows: i64,
    pub aggregates: Vec<&'static str>,
}

impl PullSummary {
    pub fn line(&self) -> String {
        let range = match self.first_seq {
            Some(first) => format!("{first}..{}", self.last_seq),
            None => format!("none; cursor {}", self.last_seq),
        };
        format!(
            "events pulled: {}; duplicates: {}; seq range: {}; ledgers rebuilt: flow={}, cost={}; source aggregates: {}",
            self.events_pulled,
            self.duplicate_events,
            range,
            self.flow_ledger_rows,
            self.cost_ledger_rows,
            self.aggregates.join(", ")
        )
    }
}

pub async fn pull_events(config: PullConfig) -> Result<PullSummary, String> {
    ensure_not_in_git_worktree(&config.data_dir)?;
    let token = read_token(&config.token_file)?;
    fs::create_dir_all(&config.data_dir).map_err(|err| err.to_string())?;

    let base = source_base(&config.base_url)?;
    let client = reqwest::Client::new();
    let shift_path = config.data_dir.join("shift.sqlite");
    let store = EventStore::open(&shift_path)?;
    let since = Since::parse(config.since.as_deref())?;
    let mut after_seq = match since {
        Since::Seq(seq) => seq,
        Since::Date(_) => 0,
        Since::Resume => store.latest_seq()?,
    };
    let mut events_pulled = 0;
    let mut duplicate_events = 0;
    let mut first_seq = None;
    loop {
        let page = get_json::<EventsPage>(
            &client,
            &token,
            page_url(
                &base,
                "/api/v1/events",
                &[("after_seq", after_seq), ("limit", PAGE_LIMIT)],
            )?,
        )
        .await?;
        if page.events.is_empty() {
            break;
        }
        for event in page.events {
            after_seq = after_seq.max(event.seq);
            if since.skips(&event)? {
                continue;
            }
            let inserted = store.import(&event)?;
            if inserted.duplicate {
                duplicate_events += 1;
            } else {
                first_seq.get_or_insert(event.seq);
                events_pulled += 1;
            }
        }
        if !page.has_more {
            break;
        }
    }
    let aggregates = pull_aggregates(&client, &token, &base, &config.data_dir).await?;
    let (flow_ledger_rows, cost_ledger_rows) = store.projection_counts()?;
    Ok(PullSummary {
        events_pulled,
        duplicate_events,
        first_seq,
        last_seq: store.latest_seq()?,
        flow_ledger_rows,
        cost_ledger_rows,
        aggregates,
    })
}

pub fn compare(config: CompareConfig) -> Result<String, String> {
    let shift_path = config.data_dir.join("shift.sqlite");
    let store = EventStore::open(&shift_path)?;
    let local_flow = local_flow(&store)?;
    let source_flow_path = config.data_dir.join("source-flow.json");
    let mut lines = Vec::new();
    if source_flow_path.exists() {
        let source_flow = read_json(&source_flow_path)?;
        compare_values("flow", &local_flow, &source_flow, &mut lines);
    } else {
        lines.push("flow: no source-flow.json; run dev pull-events first".to_string());
    }
    for name in ["yield", "scorecard"] {
        let path = config.data_dir.join(format!("source-{name}.json"));
        if path.exists() {
            lines.push(format!(
                "{name}: source aggregate saved; local rebuilt comparison is not available in this command yet"
            ));
        }
    }
    if lines.is_empty() {
        lines.push("compare: no mismatches".to_string());
    }
    Ok(lines.join("\n"))
}

fn local_flow(store: &EventStore) -> Result<Value, String> {
    let now_ms = Utc::now().timestamp_millis();
    let since_ms = flow::report::window_start(now_ms, flow::DEFAULT_DAYS);
    let visits = store.flow_visits(None, since_ms)?;
    let reached = store.flow_reached(None)?;
    serde_json::to_value(flow::report::build(
        &flow::Request {
            family: None,
            days: flow::DEFAULT_DAYS,
            now_ms,
        },
        &visits,
        &reached,
    ))
    .map_err(|err| err.to_string())
}

fn compare_values(name: &str, local: &Value, source: &Value, lines: &mut Vec<String>) {
    let ignore: BTreeSet<&str> = ["generated_at", "window.since", "window.until"]
        .into_iter()
        .collect();
    let mut mismatches = Vec::new();
    diff_json("", local, source, &ignore, &mut mismatches);
    if mismatches.is_empty() {
        lines.push(format!("{name}: match"));
    } else {
        lines.push(format!("{name}: {} mismatched metric(s)", mismatches.len()));
        let lots = mismatch_lots(local, source);
        for mismatch in mismatches.into_iter().take(40) {
            lines.push(format!(
                "{name}.{path}: local={local_value} source={source_value}",
                path = mismatch.path,
                local_value = short_json(&mismatch.local),
                source_value = short_json(&mismatch.source)
            ));
        }
        if !lots.is_empty() {
            lines.push(format!(
                "{name}: lots behind mismatch: {}",
                lots.into_iter().take(40).collect::<Vec<_>>().join(", ")
            ));
        }
    }
}

#[derive(Clone, Debug)]
struct Mismatch {
    path: String,
    local: Value,
    source: Value,
}

fn diff_json(
    path: &str,
    local: &Value,
    source: &Value,
    ignore: &BTreeSet<&str>,
    out: &mut Vec<Mismatch>,
) {
    if ignore.contains(path) {
        return;
    }
    match (local, source) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<&str> = left
                .keys()
                .chain(right.keys())
                .map(String::as_str)
                .collect();
            for key in keys {
                let next = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                diff_json(
                    &next,
                    left.get(key).unwrap_or(&Value::Null),
                    right.get(key).unwrap_or(&Value::Null),
                    ignore,
                    out,
                );
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            if path == "lots" {
                if lot_ids(local) != lot_ids(source) {
                    out.push(Mismatch {
                        path: path.to_string(),
                        local: json!(lot_ids(local)),
                        source: json!(lot_ids(source)),
                    });
                }
                return;
            }
            for index in 0..left.len().max(right.len()) {
                diff_json(
                    &format!("{path}[{index}]"),
                    left.get(index).unwrap_or(&Value::Null),
                    right.get(index).unwrap_or(&Value::Null),
                    ignore,
                    out,
                );
            }
        }
        _ if local == source => {}
        _ => out.push(Mismatch {
            path: path.to_string(),
            local: local.clone(),
            source: source.clone(),
        }),
    }
}

fn mismatch_lots(local: &Value, source: &Value) -> Vec<String> {
    let mut lots: BTreeSet<String> = lot_ids(local).into_iter().collect();
    lots.extend(lot_ids(source));
    lots.into_iter().collect()
}

fn lot_ids(value: &Value) -> Vec<String> {
    value
        .get("lots")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|lot| lot.get("lot_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn short_json(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    };
    const MAX: usize = 120;
    if text.len() <= MAX {
        text
    } else {
        format!("{}...", &text[..MAX])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Since {
    Resume,
    Seq(i64),
    Date(i64),
}

impl Since {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Self::Resume);
        };
        if let Ok(seq) = value.parse::<i64>() {
            return Ok(Self::Seq(seq));
        }
        if let Ok(at) = DateTime::parse_from_rfc3339(value) {
            return Ok(Self::Date(at.timestamp_millis()));
        }
        let day = NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(|_| "--since must be an event seq, RFC3339 timestamp, or YYYY-MM-DD")?;
        let at = Utc
            .from_utc_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight is valid"))
            .timestamp_millis();
        Ok(Self::Date(at))
    }

    fn skips(self, event: &Event) -> Result<bool, String> {
        let Self::Date(since_ms) = self else {
            return Ok(false);
        };
        let ts_ms = DateTime::parse_from_rfc3339(&event.ts)
            .map_err(|err| format!("event {} has invalid ts: {err}", event.seq))?
            .timestamp_millis();
        Ok(ts_ms < since_ms)
    }
}

#[derive(Debug, Deserialize)]
struct EventsPage {
    events: Vec<Event>,
    #[serde(default)]
    has_more: bool,
}

async fn pull_aggregates(
    client: &reqwest::Client,
    token: &str,
    base: &Url,
    data_dir: &Path,
) -> Result<Vec<&'static str>, String> {
    let mut pulled = Vec::new();
    for (name, path) in [
        ("yield", "/api/v1/yield"),
        ("flow", "/api/v1/flow"),
        ("scorecard", "/api/v1/scorecard"),
    ] {
        let url = base
            .join(path.trim_start_matches('/'))
            .map_err(|err| err.to_string())?;
        let response = client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|err| err.to_string())?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        let status = response.status();
        if !status.is_success() {
            return Err(format!("{path} returned {status}"));
        }
        let value: Value = response.json().await.map_err(|err| err.to_string())?;
        let file = fs::File::create(data_dir.join(format!("source-{name}.json")))
            .map_err(|err| err.to_string())?;
        serde_json::to_writer_pretty(file, &value).map_err(|err| err.to_string())?;
        pulled.push(name);
    }
    Ok(pulled)
}

async fn get_json<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    token: &str,
    url: Url,
) -> Result<T, String> {
    let response = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("source GET returned {status}"));
    }
    response.json().await.map_err(|err| err.to_string())
}

fn page_url(base: &Url, path: &str, query: &[(&str, i64)]) -> Result<Url, String> {
    let mut url = base
        .join(path.trim_start_matches('/'))
        .map_err(|err| err.to_string())?;
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, &value.to_string());
        }
    }
    Ok(url)
}

fn source_base(value: &str) -> Result<Url, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("source URL is required".to_string());
    }
    let mut base = Url::parse(trimmed).map_err(|err| err.to_string())?;
    if !matches!(base.scheme(), "http" | "https") {
        return Err("source URL must be http or https".to_string());
    }
    if base.query().is_some() || base.fragment().is_some() {
        return Err("source URL must not include a query or fragment".to_string());
    }
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    Ok(base)
}

fn read_token(path: &Path) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|err| err.to_string())?;
    let mode = meta.permissions().mode() & 0o777;
    if mode != TOKEN_MODE {
        return Err(format!(
            "token file must be mode 600; found {mode:o} at {}",
            path.display()
        ));
    }
    let token = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("token file is empty".to_string());
    }
    Ok(token)
}

fn ensure_not_in_git_worktree(path: &Path) -> Result<(), String> {
    let probe = nearest_existing_parent(path)?;
    let output = Command::new("git")
        .args(["-C"])
        .arg(&probe)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(|err| err.to_string())?;
    if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true" {
        return Err(format!(
            "--data-dir must not be inside a git work tree: {}",
            path.display()
        ));
    }
    Ok(())
}

fn nearest_existing_parent(path: &Path) -> Result<PathBuf, String> {
    let mut probe = path;
    loop {
        if probe.exists() {
            return Ok(probe.to_path_buf());
        }
        probe = probe
            .parent()
            .ok_or_else(|| format!("no existing parent for {}", path.display()))?;
    }
}

fn read_json(path: &Path) -> Result<Value, String> {
    let file = fs::File::open(path).map_err(|err| err.to_string())?;
    serde_json::from_reader(file).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::{Json, Router};
    use std::sync::Arc;
    use tokio::net::TcpListener;

    #[test]
    fn since_accepts_seq_and_dates() {
        assert_eq!(Since::parse(Some("42")).unwrap(), Since::Seq(42));
        assert!(matches!(
            Since::parse(Some("2026-10-01")).unwrap(),
            Since::Date(_)
        ));
        assert!(matches!(
            Since::parse(Some("2026-10-01T12:00:00Z")).unwrap(),
            Since::Date(_)
        ));
    }

    #[test]
    fn compare_reports_lot_ids_without_full_events() {
        let local = json!({"lots": [{"lot_id": "todo-a"}], "line": {"wip": 1}});
        let source = json!({"lots": [{"lot_id": "todo-b"}], "line": {"wip": 2}});
        let mut lines = Vec::new();
        compare_values("flow", &local, &source, &mut lines);
        let report = lines.join("\n");
        assert!(report.contains("flow.line.wip"));
        assert!(report.contains("todo-a"));
        assert!(report.contains("todo-b"));
    }

    #[tokio::test]
    async fn pull_reads_only_gets_and_imports_invented_events() {
        #[derive(Clone)]
        struct Fake {
            events: Arc<Vec<Value>>,
        }

        #[derive(Deserialize)]
        struct Page {
            after_seq: Option<i64>,
            limit: Option<usize>,
        }

        async fn events(
            axum::extract::State(fake): axum::extract::State<Fake>,
            Query(page): Query<Page>,
            headers: HeaderMap,
        ) -> impl IntoResponse {
            assert_eq!(
                headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer secret-token")
            );
            let after = page.after_seq.unwrap_or(0);
            let limit = page.limit.unwrap_or(500);
            let events: Vec<Value> = fake
                .events
                .iter()
                .filter(|event| event["seq"].as_i64().unwrap() > after)
                .take(limit)
                .cloned()
                .collect();
            let has_more = fake
                .events
                .iter()
                .filter(|event| event["seq"].as_i64().unwrap() > after)
                .count()
                > events.len();
            Json(json!({
                "schema_version": "jeryu.pipeline_events/v1",
                "events": events,
                "latest_seq": 2,
                "limit": limit,
                "has_more": has_more,
                "next_cursor": 2
            }))
            .into_response()
        }

        async fn aggregate() -> impl IntoResponse {
            Json(json!({
                "schema_version": "fake.aggregate/v1",
                "line": {"wip": 1},
                "lots": [{"lot_id": "todo-a"}]
            }))
        }

        async fn missing() -> impl IntoResponse {
            StatusCode::NOT_FOUND
        }

        async fn refused_write() -> StatusCode {
            panic!("pull-events must never call write routes");
        }

        let fake = Fake {
            events: Arc::new(vec![
                json!({
                    "seq": 1,
                    "ts": "2026-10-01T00:00:00.000Z",
                    "event_id": "evt-1",
                    "source": "forge",
                    "kind": "todo.filed",
                    "reporter": "reporter-a",
                    "actor": null,
                    "family": "acme",
                    "family_label": "Acme",
                    "repo": "acme/widgets",
                    "pr": null,
                    "sha": null,
                    "todo_id": "todo-a",
                    "shift": null,
                    "outcome": null,
                    "needs_human": false,
                    "summary": "filed invented todo",
                    "reason": null,
                    "cost_usd": null,
                    "seconds": null,
                    "log_tail": null,
                    "log_url": null,
                    "detail": null
                }),
                json!({
                    "seq": 2,
                    "ts": "2026-10-01T00:01:00.000Z",
                    "event_id": "evt-2",
                    "source": "forge",
                    "kind": "pr.reviewed",
                    "reporter": "reporter-a",
                    "actor": null,
                    "family": "acme",
                    "family_label": "Acme",
                    "repo": "acme/widgets",
                    "pr": 7,
                    "sha": null,
                    "todo_id": "todo-a",
                    "shift": null,
                    "outcome": "approved",
                    "needs_human": false,
                    "summary": "reviewed invented pull",
                    "reason": null,
                    "cost_usd": 1.25,
                    "seconds": 60,
                    "log_tail": null,
                    "log_url": null,
                    "detail": {"model": "test"}
                }),
            ]),
        };
        let app = Router::new()
            .route("/api/v1/events", get(events).post(refused_write))
            .route("/api/v1/yield", get(aggregate))
            .route("/api/v1/flow", get(aggregate))
            .route("/api/v1/scorecard", get(missing))
            .with_state(fake);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let data_dir = tempfile::tempdir().unwrap();
        let token_dir = tempfile::tempdir().unwrap();
        let token_file = token_dir.path().join("token");
        fs::write(&token_file, "secret-token\n").unwrap();
        fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();

        let summary = pull_events(PullConfig {
            base_url: format!("http://{addr}"),
            token_file,
            data_dir: data_dir.path().to_path_buf(),
            since: None,
        })
        .await
        .unwrap();
        assert_eq!(summary.events_pulled, 2);
        assert_eq!(summary.first_seq, Some(1));
        assert_eq!(summary.last_seq, 2);
        assert_eq!(summary.cost_ledger_rows, 1);
        assert!(data_dir.path().join("source-yield.json").exists());
        assert!(data_dir.path().join("source-flow.json").exists());
        assert!(!data_dir.path().join("source-scorecard.json").exists());
    }
}
