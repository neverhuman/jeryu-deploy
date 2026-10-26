//! Live token usage: what each worker slot is spending on the attempt it is
//! running now, and the same counts over time. Contract:
//! `docs/pipeline-events.md`, "Token usage".
//!
//! A producer reports running totals for the slot's **current** attempt, on
//! its shift heartbeat (`usage`) or on a `todo.attempt_finished` event
//! (`detail.usage`). The totals reset when the slot moves to another todo, so
//! a rate is the delta between two consecutive samples of the same
//! `(slot, todo_id)` and a restart, a reset or a new attempt never double
//! counts. Samples live in `<data_dir>/shift.sqlite` (`usage_samples`,
//! `db/migrations/0015_usage_samples.sql`) and are pruned with the same
//! 30-day retention as the event log.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use jeryu_readmodel::contracts::WebEvent;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::WebState;
use super::pipeline::{EVENT_RETENTION_MS, PIPELINE_SCOPE};
use super::shift::{WorkerRow, migrate_shift_store, rfc3339_ms};
use super::strict_query::{QueryRefused, StrictFields, StrictQuery};
use super::workcells_support::{TypedError, typed_error};

/// The WebSocket kind a changed slot usage is published under.
pub(crate) const CHANGED_KIND: &str = "usage.changed";
/// At most one `usage.changed` frame per second, server-wide: a dozen slots
/// beating every few seconds would otherwise repaint the page faster than a
/// reader can see it change.
const PUBLISH_EVERY_MS: i64 = 1000;
const PRUNE_EVERY_MS: i64 = 60 * 60 * 1000;
/// The longest window `?hours=` reads: the retention of the samples.
const MAX_HOURS: i64 = 30 * 24;
const DEFAULT_HOURS: i64 = 24;
const DEFAULT_BUCKET_SECONDS: i64 = 5 * 60;
const MIN_BUCKET_SECONDS: i64 = 10;
const MAX_BUCKET_SECONDS: i64 = 24 * 60 * 60;
/// Most buckets one answer carries, so a month of ten-second buckets cannot
/// be asked for.
const MAX_BUCKETS: i64 = 2000;
const DOCS: &str = "docs/pipeline-events.md";

/// Running token totals for one attempt, as a producer reports them and as
/// `GET /api/v1/shift/workers` and `GET /api/v1/usage` return them. Counts are
/// cumulative for the slot's current todo and reset when the todo changes.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(crate) struct Usage {
    /// The model the attempt is running on, as the producer spells it.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_input_tokens: i64,
    #[serde(default)]
    pub cache_creation_input_tokens: i64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub turns: Option<i64>,
    /// When the producer last read these totals, RFC 3339.
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl Usage {
    /// The reported totals, checked and tidied: no count may be negative, and
    /// a blank model or timestamp is nothing rather than an empty string.
    /// `field` is what the refusal calls the object (`usage`, `detail.usage`).
    pub(crate) fn checked(self, field: &str) -> Result<Self, String> {
        for (name, count) in [
            ("input_tokens", self.input_tokens),
            ("output_tokens", self.output_tokens),
            ("cache_read_input_tokens", self.cache_read_input_tokens),
            (
                "cache_creation_input_tokens",
                self.cache_creation_input_tokens,
            ),
        ] {
            if count < 0 {
                return Err(format!("{field}.{name}: must not be negative"));
            }
        }
        if self
            .cost_usd
            .is_some_and(|cost| !cost.is_finite() || cost < 0.0)
        {
            return Err(format!("{field}.cost_usd: must be a non-negative number"));
        }
        if self.turns.is_some_and(|turns| turns < 0) {
            return Err(format!("{field}.turns: must not be negative"));
        }
        let blank = |value: Option<String>| {
            value
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Ok(Self {
            model: blank(self.model),
            updated_at: blank(self.updated_at),
            ..self
        })
    }

    /// The counts a rate is measured on: what the model read fresh plus what
    /// it wrote. A cached read is not new work.
    fn flowing_tokens(&self) -> i64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }
}

/// The per-model object todoq writes as a finished attempt's `detail.usage`
/// for the yield report (`{"<model>": {input, output, cache_read, cache_write,
/// thinking, cost_usd, cost_basis, ...}}`), or `None` for any other shape. A
/// flat [`Usage`] has no object-valued field, so the two never overlap.
pub(crate) fn per_model(
    reported: &serde_json::Value,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    match reported {
        serde_json::Value::Object(models)
            if !models.is_empty() && models.values().all(serde_json::Value::is_object) =>
        {
            Some(models)
        }
        _ => None,
    }
}

/// What a finished attempt's `detail.usage` reports as the slot's last
/// sample, in either shape: the flat [`Usage`] a heartbeat carries, or
/// todoq's per-model object summed over its models (`model` is the one model,
/// or `None` when the attempt ran on several). `None` when it is neither, or
/// when the sum is not a sample anyone could chart.
pub(crate) fn attempt_usage(reported: &serde_json::Value) -> Option<Usage> {
    let Some(models) = per_model(reported) else {
        return serde_json::from_value::<Usage>(reported.clone())
            .ok()
            .and_then(|usage| usage.checked("detail.usage").ok());
    };
    let count = |row: &serde_json::Value, key: &str| -> i64 {
        row.get(key)
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .unwrap_or(0)
    };
    let mut usage = Usage {
        model: (models.len() == 1)
            .then(|| models.keys().next().cloned())
            .flatten(),
        ..Usage::default()
    };
    for row in models.values() {
        usage.input_tokens = usage.input_tokens.saturating_add(count(row, "input"));
        usage.output_tokens = usage.output_tokens.saturating_add(count(row, "output"));
        usage.cache_read_input_tokens = usage
            .cache_read_input_tokens
            .saturating_add(count(row, "cache_read"));
        usage.cache_creation_input_tokens = usage
            .cache_creation_input_tokens
            .saturating_add(count(row, "cache_write"));
        if let Some(cost) = row.get("cost_usd").and_then(serde_json::Value::as_f64) {
            usage.cost_usd = Some(usage.cost_usd.unwrap_or(0.0) + cost);
        }
    }
    usage.checked("detail.usage").ok()
}

/// One report, with the slot it came from and when it arrived.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sample {
    pub ts_ms: i64,
    pub operator: String,
    pub host: String,
    pub slot: String,
    pub family: String,
    pub todo_id: Option<String>,
    pub usage: Usage,
}

/// Which slot a sample came from: the key `GET /api/v1/shift/workers` joins on.
type SlotKey = (String, String, String, String);
/// Which attempt a sample measured: the key deltas are taken within.
type AttemptKey = (String, String, String, String, Option<String>);

fn slot_key(sample: &Sample) -> SlotKey {
    (
        sample.operator.clone(),
        sample.host.clone(),
        sample.slot.clone(),
        sample.family.clone(),
    )
}

fn attempt_key(sample: &Sample) -> AttemptKey {
    let (operator, host, slot, family) = slot_key(sample);
    (operator, host, slot, family, sample.todo_id.clone())
}

#[derive(Clone)]
pub(crate) struct UsageStore {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    conn: Connection,
    last_prune_ms: i64,
    last_publish_ms: i64,
}

/// The sample columns, qualified so the same list reads both a plain select
/// and one joined against a newest-row-per-group subquery.
const COLUMNS: &str = "s.ts_ms, s.operator, s.host, s.slot, s.family, s.todo_id, s.model,
    s.input_tokens, s.output_tokens, s.cache_read_input_tokens, s.cache_creation_input_tokens,
    s.cost_usd, s.turns, s.updated_at";

impl UsageStore {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|err| err.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|err| err.to_string())?;
        migrate_shift_store(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                conn,
                last_prune_ms: 0,
                last_publish_ms: 0,
            })),
        })
    }

    /// Store `sample` unless the slot already reported exactly these totals
    /// for the same todo, and say whether anything changed. A slot that beats
    /// every few seconds while its attempt thinks adds no row.
    pub(crate) fn insert(&self, sample: &Sample) -> Result<bool, String> {
        let mut inner = self.inner.lock().expect("usage sample mutex poisoned");
        let previous = inner
            .conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM usage_samples s
                      WHERE s.operator = ?1 AND s.host = ?2 AND s.slot = ?3 AND s.family = ?4
                      ORDER BY s.id DESC LIMIT 1"
                ),
                params![sample.operator, sample.host, sample.slot, sample.family],
                sample_from_row,
            )
            .optional()
            .map_err(|err| err.to_string())?;
        if previous.is_some_and(|last| last.todo_id == sample.todo_id && last.usage == sample.usage)
        {
            return Ok(false);
        }
        inner
            .conn
            .execute(
                "INSERT INTO usage_samples (ts_ms, operator, host, slot, family, todo_id, model,
                   input_tokens, output_tokens, cache_read_input_tokens,
                   cache_creation_input_tokens, cost_usd, turns, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    sample.ts_ms,
                    sample.operator,
                    sample.host,
                    sample.slot,
                    sample.family,
                    sample.todo_id,
                    sample.usage.model,
                    sample.usage.input_tokens,
                    sample.usage.output_tokens,
                    sample.usage.cache_read_input_tokens,
                    sample.usage.cache_creation_input_tokens,
                    sample.usage.cost_usd,
                    sample.usage.turns,
                    sample.usage.updated_at,
                ],
            )
            .map_err(|err| err.to_string())?;
        if sample.ts_ms - inner.last_prune_ms >= PRUNE_EVERY_MS {
            inner
                .conn
                .execute(
                    "DELETE FROM usage_samples WHERE ts_ms < ?1",
                    params![sample.ts_ms - EVENT_RETENTION_MS],
                )
                .map_err(|err| err.to_string())?;
            inner.last_prune_ms = sample.ts_ms;
        }
        Ok(true)
    }

    /// Whether a `usage.changed` frame may go out at `now_ms`: at most one a
    /// second, server-wide. Asking is what reserves the second.
    pub(crate) fn may_publish(&self, now_ms: i64) -> bool {
        let mut inner = self.inner.lock().expect("usage sample mutex poisoned");
        if now_ms - inner.last_publish_ms < PUBLISH_EVERY_MS {
            return false;
        }
        inner.last_publish_ms = now_ms;
        true
    }

    /// Every sample in `[from_ms, to_ms]`, oldest first.
    pub(crate) fn between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<Sample>, String> {
        self.select(
            &format!(
                "SELECT {COLUMNS} FROM usage_samples s
                  WHERE s.ts_ms >= ?1 AND s.ts_ms <= ?2 ORDER BY s.ts_ms, s.id"
            ),
            params![from_ms, to_ms],
        )
    }

    /// The newest sample of every attempt reported before `from_ms`: the
    /// baseline a window's first delta is taken against, so tokens counted in
    /// an earlier window are not counted again in this one.
    pub(crate) fn baselines(&self, from_ms: i64) -> Result<Vec<Sample>, String> {
        self.select(
            &format!(
                "SELECT {COLUMNS} FROM usage_samples s
                   JOIN (SELECT MAX(id) AS id FROM usage_samples WHERE ts_ms < ?1
                          GROUP BY operator, host, slot, family, todo_id) m ON s.id = m.id
                  ORDER BY s.ts_ms, s.id"
            ),
            params![from_ms],
        )
    }

    /// The newest sample of every slot that ever reported.
    pub(crate) fn latest_per_slot(&self) -> Result<Vec<Sample>, String> {
        self.select(
            &format!(
                "SELECT {COLUMNS} FROM usage_samples s
                   JOIN (SELECT MAX(id) AS id FROM usage_samples
                          GROUP BY operator, host, slot, family) m ON s.id = m.id
                  ORDER BY s.family, s.operator, s.host, s.slot"
            ),
            [],
        )
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> i64 {
        let inner = self.inner.lock().expect("usage sample mutex poisoned");
        inner
            .conn
            .query_row("SELECT COUNT(*) FROM usage_samples", [], |row| row.get(0))
            .expect("count usage samples")
    }

    fn select(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Sample>, String> {
        let inner = self.inner.lock().expect("usage sample mutex poisoned");
        let mut stmt = inner.conn.prepare(sql).map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params, sample_from_row)
            .map_err(|err| err.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())
    }
}

fn sample_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Sample> {
    Ok(Sample {
        ts_ms: row.get(0)?,
        operator: row.get(1)?,
        host: row.get(2)?,
        slot: row.get(3)?,
        family: row.get(4)?,
        todo_id: row.get(5)?,
        usage: Usage {
            model: row.get(6)?,
            input_tokens: row.get(7)?,
            output_tokens: row.get(8)?,
            cache_read_input_tokens: row.get(9)?,
            cache_creation_input_tokens: row.get(10)?,
            cost_usd: row.get(11)?,
            turns: row.get(12)?,
            updated_at: row.get(13)?,
        },
    })
}

/// `GET /api/v1/usage?hours=24&bucket=5m[&family=]`
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct UsageQuery {
    /// Existing Activity clients use range=24h.
    pub range: Option<String>,
    pub hours: Option<i64>,
    /// A bucket width such as `30s`, `5m` or `1h`.
    pub bucket: Option<String>,
    /// Either spelling of a family key; the server canonicalises it.
    pub family: Option<String>,
}

impl StrictFields for UsageQuery {
    const KEYS: &'static [&'static str] = &["hours", "bucket", "family", "range"];

    /// The route keeps its own published code for a refused query: the keys
    /// alone do not say what a bucket width looks like.
    const CODE: &'static str = "usage_invalid_query";

    const REPAIR_HINT: &'static str =
        "hours is an integer from 1 to 720; bucket is a width such as 30s, 5m or 1h";

    fn check_values(&self) -> Result<(), String> {
        super::strict_query::filter_one_of("range", self.range.as_ref(), &["24h"])?;
        if self.range.is_some() && self.hours.is_some_and(|hours| hours != 24) {
            return Err("range=24h conflicts with hours; send one window".to_string());
        }
        if let Some(hours) = self.hours
            && !(1..=MAX_HOURS).contains(&hours)
        {
            return Err(format!(
                "hours: must be from 1 to {MAX_HOURS}, got {hours}; it is not clamped"
            ));
        }
        let bucket_seconds = match &self.bucket {
            Some(bucket) => parse_bucket(bucket)?,
            None => DEFAULT_BUCKET_SECONDS,
        };
        let hours = self.hours.unwrap_or(DEFAULT_HOURS);
        let buckets = hours * 60 * 60 / bucket_seconds;
        if buckets > MAX_BUCKETS {
            return Err(format!(
                "bucket: {buckets} buckets over {hours} hours is more than {MAX_BUCKETS}; ask for a wider bucket"
            ));
        }
        Ok(())
    }
}

/// `30s`, `5m`, `1h` as seconds, or why the value is not a width.
fn parse_bucket(bucket: &str) -> Result<i64, String> {
    let bucket = bucket.trim();
    let bad =
        || format!("bucket: {bucket:?} is not a width; use a count and a unit such as 30s, 5m, 1h");
    let (count, unit) = bucket.split_at(
        bucket
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index),
    );
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        _ => return Err(bad()),
    };
    let count: i64 = count.parse().map_err(|_| bad())?;
    let width = count.saturating_mul(seconds);
    if !(MIN_BUCKET_SECONDS..=MAX_BUCKET_SECONDS).contains(&width) {
        return Err(format!(
            "bucket: must be from {MIN_BUCKET_SECONDS}s to {MAX_BUCKET_SECONDS}s, got {bucket:?}"
        ));
    }
    Ok(width)
}

/// What was spent over one span: the four token counters and the money.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub(crate) struct UsageCounts {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cost_usd: f64,
}

impl UsageCounts {
    fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.cost_usd += other.cost_usd;
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct UsageBucket {
    pub start: String,
    #[serde(flatten)]
    pub counts: UsageCounts,
}

/// What one slot is spending on the attempt it is running now.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct LiveUsage {
    pub slot: String,
    pub operator: String,
    pub host: String,
    pub family: String,
    pub todo_id: Option<String>,
    #[serde(flatten)]
    pub usage: Usage,
    /// Fresh input plus output tokens a minute over this attempt's samples in
    /// the window; `null` until the attempt has two samples apart in time.
    pub tokens_per_minute: Option<f64>,
    /// When the server stored this sample. A slot's newest sample is kept
    /// whatever its age, so it can be days older than the window.
    pub sampled_at: String,
    /// True when the sample was stored inside the answer's window, so its
    /// spend can show up in `totals`.
    pub in_window: bool,
    /// Whether the slot is working this attempt now, from its heartbeats:
    /// `true` when its newest beat is healthy and names the same todo,
    /// `false` when its newest beat inside the heartbeat retention is stale
    /// or names another todo (or none), `null` when the slot has not beaten
    /// inside the retention at all.
    pub current: Option<bool>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct UsageResponse {
    pub from: String,
    pub to: String,
    pub bucket_seconds: i64,
    pub buckets: Vec<UsageBucket>,
    pub live: Vec<LiveUsage>,
    pub totals: UsageCounts,
}

/// The rise from `previous` to `current`, never negative: a producer that
/// restarted its counters, or started another attempt, reports totals smaller
/// than the ones before them, and that is no spend at all.
fn delta(previous: Option<&Usage>, current: &Usage) -> UsageCounts {
    let rise = |before: i64, after: i64| after.saturating_sub(before).max(0);
    let before = previous.cloned().unwrap_or_default();
    UsageCounts {
        input_tokens: rise(before.input_tokens, current.input_tokens),
        output_tokens: rise(before.output_tokens, current.output_tokens),
        cache_read_input_tokens: rise(
            before.cache_read_input_tokens,
            current.cache_read_input_tokens,
        ),
        cache_creation_input_tokens: rise(
            before.cache_creation_input_tokens,
            current.cache_creation_input_tokens,
        ),
        cost_usd: (current.cost_usd.unwrap_or(0.0) - before.cost_usd.unwrap_or(0.0)).max(0.0),
    }
}

/// Turn cumulative samples into per-bucket deltas and per-slot live rows.
///
/// Deltas are taken within one `(slot, todo_id)`, against the attempt's newest
/// sample before the window when it has one, so the same tokens are never
/// counted in two windows. Buckets are whole multiples of `bucket_ms` from the
/// bucket the window opens in, and the empty ones are kept: a chart needs the
/// gap. `latest` is the newest sample of each slot, which is what the live
/// rows report.
pub(crate) fn build(
    samples: &[Sample],
    baselines: &[Sample],
    latest: &[Sample],
    from_ms: i64,
    to_ms: i64,
    bucket_ms: i64,
) -> UsageResponse {
    let mut buckets: BTreeMap<i64, UsageCounts> = BTreeMap::new();
    let mut at = from_ms.div_euclid(bucket_ms) * bucket_ms;
    while at <= to_ms {
        buckets.insert(at, UsageCounts::default());
        at += bucket_ms;
    }
    let mut previous: BTreeMap<AttemptKey, Usage> = baselines
        .iter()
        .map(|sample| (attempt_key(sample), sample.usage.clone()))
        .collect();
    // Fresh tokens and the span they were reported over, per attempt: what a
    // live row's rate is read from.
    let mut flow: BTreeMap<AttemptKey, Flow> = BTreeMap::new();
    let mut totals = UsageCounts::default();
    for sample in samples {
        let key = attempt_key(sample);
        let step = delta(previous.get(&key), &sample.usage);
        let bucket = sample.ts_ms.div_euclid(bucket_ms) * bucket_ms;
        if let Some(counts) = buckets.get_mut(&bucket) {
            counts.add(&step);
        }
        totals.add(&step);
        let fresh = step
            .input_tokens
            .saturating_add(step.output_tokens)
            .saturating_add(step.cache_creation_input_tokens);
        let span = flow.entry(key.clone()).or_insert(Flow {
            tokens: 0,
            // An attempt already running when the window opened was measured
            // from the window's edge, not from its first sample inside it.
            from: if previous.contains_key(&key) {
                from_ms
            } else {
                sample.ts_ms
            },
            to: sample.ts_ms,
        });
        span.tokens = span.tokens.saturating_add(fresh);
        span.to = sample.ts_ms;
        previous.insert(key, sample.usage.clone());
    }
    let live = latest
        .iter()
        .map(|sample| LiveUsage {
            slot: sample.slot.clone(),
            operator: sample.operator.clone(),
            host: sample.host.clone(),
            family: sample.family.clone(),
            todo_id: sample.todo_id.clone(),
            usage: sample.usage.clone(),
            tokens_per_minute: flow.get(&attempt_key(sample)).and_then(Flow::per_minute),
            sampled_at: rfc3339_ms(sample.ts_ms),
            in_window: sample.ts_ms >= from_ms && sample.ts_ms <= to_ms,
            current: None,
        })
        .collect();
    UsageResponse {
        from: rfc3339_ms(from_ms),
        to: rfc3339_ms(to_ms),
        bucket_seconds: bucket_ms / 1000,
        buckets: buckets
            .into_iter()
            .map(|(start, counts)| UsageBucket {
                start: rfc3339_ms(start),
                counts,
            })
            .collect(),
        live,
        totals,
    }
}

/// Say, for each live row, whether its slot is working that attempt now.
/// `latest` is the newest heartbeat of every slot inside the heartbeat
/// retention (`HeartbeatStore::latest`).
pub(crate) fn mark_current(
    live: &mut [LiveUsage],
    latest: &[super::shift::StoredHeartbeat],
    now_ms: i64,
) {
    let beats: BTreeMap<SlotKey, &super::shift::StoredHeartbeat> = latest
        .iter()
        .map(|row| {
            (
                (
                    row.heartbeat.operator.clone(),
                    row.heartbeat.host.clone(),
                    row.heartbeat.slot.clone(),
                    row.heartbeat.family.clone(),
                ),
                row,
            )
        })
        .collect();
    for row in live {
        let key = (
            row.operator.clone(),
            row.host.clone(),
            row.slot.clone(),
            row.family.clone(),
        );
        row.current = beats.get(&key).map(|beat| {
            now_ms.saturating_sub(beat.received_ms) <= super::shift::HEALTHY_MS
                && beat.heartbeat.todo_id == row.todo_id
        });
    }
}

/// How many fresh tokens one attempt reported, and over what span.
struct Flow {
    tokens: i64,
    from: i64,
    to: i64,
}

impl Flow {
    fn per_minute(&self) -> Option<f64> {
        let minutes = (self.to - self.from) as f64 / 60_000.0;
        (minutes > 0.0).then(|| ((self.tokens as f64 / minutes) * 10.0).round() / 10.0)
    }
}

fn usage_store_error(reason: &str) -> AxumResponse {
    typed_error(TypedError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "usage_store_failed",
        purpose: "read live token usage",
        reason,
        common_fixes: &[
            "check that <data_dir>/shift.sqlite is writable and not corrupt",
            "check the server log for the store error this reports",
        ],
        docs_url: DOCS,
        repair_hint: "check <data_dir>/shift.sqlite",
        message: reason,
    })
}

/// `GET /api/v1/usage` (admin-only by path, see `auth::admin_only_request`).
pub(crate) async fn usage(
    State(state): State<Arc<WebState>>,
    query: Result<StrictQuery<UsageQuery>, QueryRefused>,
) -> AxumResponse {
    let StrictQuery(query) = match query {
        Ok(query) => query,
        // A key this route does not read, or a window it cannot parse: either
        // way the chart it would answer is not the chart that was asked for.
        Err(refused) => return refused.into_response(),
    };
    // A family nobody hosts is a typo in the filter: an empty chart would read
    // as a family that spent nothing.
    let family = match super::family::filter(&state, query.family.as_deref()) {
        Ok(family) => family,
        Err(response) => return *response,
    };
    let bucket_seconds = match query.bucket.as_deref() {
        Some(bucket) => match parse_bucket(bucket) {
            Ok(seconds) => seconds,
            Err(reason) => return bad_query(&reason),
        },
        None => DEFAULT_BUCKET_SECONDS,
    };
    let hours = query.hours.unwrap_or(DEFAULT_HOURS);
    let to = Utc::now().timestamp_millis();
    let from = to - hours * 60 * 60 * 1000;
    let keep = |samples: Vec<Sample>| -> Vec<Sample> {
        samples
            .into_iter()
            .filter(|sample| {
                family
                    .as_ref()
                    .is_none_or(|family| &super::family::canonical(&sample.family) == family)
            })
            .collect()
    };
    let read = state.usage.between(from, to).and_then(|samples| {
        Ok((
            samples,
            state.usage.baselines(from)?,
            state.usage.latest_per_slot()?,
        ))
    });
    let read = read.and_then(|read| {
        Ok((
            read,
            state
                .shift
                .heartbeats
                .latest(to - super::shift::HEARTBEAT_RETENTION_MS)?,
        ))
    });
    match read {
        Ok(((samples, baselines, latest), beats)) => {
            let mut answer = build(
                &keep(samples),
                &keep(baselines),
                &keep(latest),
                from,
                to,
                bucket_seconds * 1000,
            );
            mark_current(&mut answer.live, &beats, to);
            Json(answer).into_response()
        }
        Err(reason) => usage_store_error(&reason),
    }
}

fn bad_query(reason: &str) -> AxumResponse {
    typed_error(TypedError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: UsageQuery::CODE,
        purpose: "read live token usage",
        reason,
        common_fixes: &[
            "this route reads only hours, bucket, family",
            "drop the query to read the last 24 hours in 5m buckets",
        ],
        docs_url: DOCS,
        repair_hint: UsageQuery::REPAIR_HINT,
        message: reason,
    })
}

/// Store one report and, when it changed anything, publish `usage.changed`.
/// Best-effort like [`super::pipeline::emit`]: a store failure is written to
/// the server log and never reaches the response it rode on.
pub(crate) fn record(state: &WebState, sample: Sample) {
    match state.usage.insert(&sample) {
        Ok(false) => {}
        Ok(true) => {
            if state.usage.may_publish(sample.ts_ms) {
                publish_changed(state, &sample);
            }
        }
        Err(reason) => eprintln!(
            "jeryu-api usage: dropped a {} sample: {reason}",
            sample.slot
        ),
    }
}

fn publish_changed(state: &WebState, sample: &Sample) {
    let entity = format!("{}/{}", sample.operator, sample.slot);
    let summary = match &sample.todo_id {
        Some(todo) => format!(
            "{} {}: {} tokens on {todo}",
            sample.slot,
            sample.family,
            sample.usage.flowing_tokens()
        ),
        None => format!("{} {}: token usage changed", sample.slot, sample.family),
    };
    let payload = json!({
        "slot": sample.slot,
        "operator": sample.operator,
        "host": sample.host,
        "family": sample.family,
        "todo_id": sample.todo_id,
        "usage": sample.usage,
    });
    let timestamp = rfc3339_ms(sample.ts_ms);
    state.ws.publish(PIPELINE_SCOPE, move |seq| WebEvent {
        seq,
        timestamp,
        scope: PIPELINE_SCOPE.to_string(),
        kind: CHANGED_KIND.to_string(),
        entity,
        summary,
        payload,
    });
}

/// Fill in each worker row's `usage` from the newest sample its slot reported
/// for the todo it is working now. A slot whose producer reports no usage, or
/// whose newest sample measured another attempt, keeps `null`.
pub(crate) fn attach_live(state: &WebState, rows: &mut [WorkerRow]) {
    let latest = state.usage.latest_per_slot().unwrap_or_default();
    if latest.is_empty() {
        return;
    }
    let by_slot: BTreeMap<SlotKey, &Sample> = latest
        .iter()
        .map(|sample| (slot_key(sample), sample))
        .collect();
    for row in rows {
        let key = (
            row.heartbeat.operator.clone(),
            row.heartbeat.host.clone(),
            row.heartbeat.slot.clone(),
            row.heartbeat.family.clone(),
        );
        row.heartbeat.usage = by_slot
            .get(&key)
            .filter(|sample| sample.todo_id == row.heartbeat.todo_id)
            .map(|sample| sample.usage.clone());
    }
}

/// The operator, host and slot an event's `actor` names (`dana@build-1.example/w1`,
/// `dev/w1`, or just an operator), so a `todo.attempt_finished` event's usage
/// lands on the slot that reported it.
pub(crate) fn actor_slot(actor: Option<&str>) -> (String, String, String) {
    let actor = actor.unwrap_or_default().trim();
    let (who, slot) = match actor.rsplit_once('/') {
        Some((who, slot)) => (who, slot),
        None => (actor, ""),
    };
    let (operator, host) = match who.split_once('@') {
        Some((operator, host)) => (operator, host),
        None => (who, ""),
    };
    (operator.to_string(), host.to_string(), slot.to_string())
}
