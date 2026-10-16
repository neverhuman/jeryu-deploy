//! The weekly Scorecard digest job: once a week, build the digest from a
//! 7-day Scorecard report, record it so the attention inbox shows it, and
//! POST it to the configured webhook target.
//!
//! `JERYU_SCORECARD_DIGEST_INTERVAL_HOURS` sets the cadence (default 168, one
//! week; `0` disables). The loop wakes hourly and builds a digest only when
//! both the newest recorded digest and the last one this process built are
//! older than the interval, so deploys neither skip the week's digest nor
//! post another one on every restart.
//!
//! `JERYU_SCORECARD_DIGEST_WEBHOOK` is the outbound target. There is no
//! default: with nothing configured the digest is recorded for the attention
//! inbox and nothing is sent anywhere.
//!
//! Order matters for a crash: the digest event is recorded first, under an
//! `event_id` of the day it covers, so a crash or a restart can never post
//! the same week twice; a delivery the forge then fails to make is recorded as
//! `scorecard.digest_undelivered`, which asks a person to look, rather than
//! being retried into a stream of duplicate posts.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::super::WebState;
use super::super::pipeline::{EventsQuery, NewEvent, record};
use super::digest::{self, Digest};

const INTERVAL_ENV: &str = "JERYU_SCORECARD_DIGEST_INTERVAL_HOURS";
const TARGET_ENV: &str = "JERYU_SCORECARD_DIGEST_WEBHOOK";
const DEFAULT_INTERVAL_HOURS: u64 = 24 * 7;
const WAKE_EVERY: Duration = Duration::from_secs(60 * 60);
/// The window the digest reports: the week just gone against the week before,
/// which is what every Scorecard tile's `previous` is for a `7d` report.
pub(crate) const WINDOW: &str = "7d";
/// The recorded digest; `detail` is the whole payload.
pub(crate) const DIGEST_KIND: &str = "scorecard.digest";
/// A digest the forge could not deliver to the configured webhook.
pub(crate) const UNDELIVERED_KIND: &str = "scorecard.digest_undelivered";
/// How long the forge waits on the webhook target.
const POST_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawn the weekly job when a cadence is configured.
pub(crate) fn spawn(state: Arc<WebState>) {
    let raw = std::env::var(INTERVAL_ENV).ok();
    let interval = interval_from(raw.as_deref());
    eprintln!("{}", startup_line(raw.as_deref(), interval, target()));
    let Some(interval) = interval else {
        return;
    };
    tokio::spawn(async move {
        let mut last_built: Option<Instant> = None;
        let mut ticker = tokio::time::interval(WAKE_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let now = Utc::now();
            let recorded_age = newest_digest(&state).and_then(|at| age_of(at, now));
            if !due(interval, recorded_age, last_built.map(|at| at.elapsed())) {
                continue;
            }
            match post_digest(&state, now).await {
                Ok(()) => last_built = Some(Instant::now()),
                Err(reason) => {
                    eprintln!("jeryu-api scorecard: weekly digest not built: {reason}");
                }
            }
        }
    });
}

/// Build the week's digest, record it, and deliver it when a target is
/// configured. Recording comes first and is idempotent for the day, so a
/// delivery that fails costs the week one post and no duplicates.
pub(crate) async fn post_digest(state: &Arc<WebState>, now: DateTime<Utc>) -> Result<(), String> {
    let worker = state.clone();
    let digest = tokio::task::spawn_blocking(move || build(&worker, now))
        .await
        .map_err(|err| err.to_string())??;
    let Some(digest) = digest else {
        // The day's digest is already recorded: nothing to record or send.
        return Ok(());
    };
    deliver(state, &digest).await;
    Ok(())
}

/// Compute the digest and record it. `Ok(None)` means the day's digest was
/// already recorded, so this process has nothing to send.
///
/// Blocking: it reads the ledgers through [`super::report`].
fn build(state: &WebState, now: DateTime<Utc>) -> Result<Option<Digest>, String> {
    let report = super::report(state, WINDOW, None, None, now.timestamp_millis())?;
    let digest = digest::build(&report);
    let detail = serde_json::to_value(&digest).map_err(|err| err.to_string())?;
    let inserted = record(
        state,
        "forge",
        NewEvent {
            event_id: Some(format!("scorecard-digest-{}", now.format("%Y-%m-%d"))),
            needs_human: !digest.warnings.is_empty(),
            detail: Some(detail),
            ..NewEvent::forge(DIGEST_KIND, summary(&digest))
        },
    )?;
    Ok((!inserted.duplicate).then_some(digest))
}

/// One line for a reader of the event log: the window, and whether anything
/// is being traded away.
fn summary(digest: &Digest) -> String {
    let headline = digest
        .headline
        .iter()
        .take(2)
        .map(digest::phrase)
        .collect::<Vec<_>>()
        .join("; ");
    match digest.warnings.len() {
        0 => format!("Weekly scorecard ({}): {headline}", digest.window),
        1 => format!(
            "Weekly scorecard ({}): {headline}; 1 guardrail worse on rising volume",
            digest.window
        ),
        count => format!(
            "Weekly scorecard ({}): {headline}; {count} guardrails worse on rising volume",
            digest.window
        ),
    }
}

/// POST the digest to the configured target, recording a failure as an event
/// a person is asked to look at. Nothing is sent when no target is set.
async fn deliver(state: &Arc<WebState>, digest: &Digest) {
    let target = match target() {
        Target::None => return,
        Target::Malformed(reason) => {
            undelivered(state, digest, &reason);
            return;
        }
        Target::Url(url) => url,
    };
    if let Err(reason) = send(&target, digest).await {
        undelivered(state, digest, &reason);
    }
}

async fn send(target: &str, digest: &Digest) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(POST_TIMEOUT)
        .build()
        .map_err(|err| err.to_string())?;
    let response = client
        .post(target)
        .json(digest)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{target} answered {status}"));
    }
    Ok(())
}

/// Record that the week's digest was built but not delivered. The digest
/// itself is already in the log and in the attention inbox; this says the
/// outbound post did not happen, so nobody waits for one that never came.
fn undelivered(state: &Arc<WebState>, digest: &Digest, reason: &str) {
    eprintln!("jeryu-api scorecard: weekly digest not delivered: {reason}");
    let event = NewEvent {
        event_id: Some(format!(
            "scorecard-digest-undelivered-{}",
            digest.generated_at
        )),
        needs_human: true,
        reason: Some(reason.to_string()),
        ..NewEvent::forge(
            UNDELIVERED_KIND,
            format!(
                "Weekly scorecard digest of {} was not delivered to the configured webhook",
                digest.generated_at
            ),
        )
    };
    if let Err(reason) = record(state, "forge", event) {
        eprintln!("jeryu-api scorecard: dropped {UNDELIVERED_KIND} event: {reason}");
    }
}

/// Where the digest is posted, as configured.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
    /// Nothing configured: the digest stays in the attention inbox.
    None,
    Url(String),
    /// Configured but not a target the forge will post to, and why.
    Malformed(String),
}

fn target() -> Target {
    target_from(std::env::var(TARGET_ENV).ok().as_deref())
}

/// The configured target, checked before anything is sent to it: an absolute
/// `http` or `https` URL. Anything else is refused by name rather than being
/// posted to, so a typo cannot send the numbers somewhere unintended.
fn target_from(raw: Option<&str>) -> Target {
    let raw = match raw.map(str::trim) {
        None | Some("") => return Target::None,
        Some(raw) => raw,
    };
    match reqwest::Url::parse(raw) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.has_host() => {
            Target::Url(url.to_string())
        }
        Ok(url) => Target::Malformed(format!(
            "{TARGET_ENV}: expected an http or https URL with a host, not {}",
            url.scheme()
        )),
        Err(error) => Target::Malformed(format!("{TARGET_ENV}: {error}")),
    }
}

/// Parse the configured cadence; `None` means the job is off. A value that is
/// not a whole number of hours disables it rather than falling back to the
/// default, so a typo is visible in the startup line.
fn interval_from(raw: Option<&str>) -> Option<Duration> {
    let hours = match raw.map(str::trim) {
        None | Some("") => DEFAULT_INTERVAL_HOURS,
        Some(value) => value.parse::<u64>().ok()?,
    };
    (hours > 0).then(|| Duration::from_secs(hours * 60 * 60))
}

/// The one line logged at startup, so the cadence and the target in force are
/// visible without reading the environment.
fn startup_line(raw: Option<&str>, interval: Option<Duration>, target: Target) -> String {
    let source = match raw.map(str::trim) {
        None | Some("") => "default".to_string(),
        Some(value) => format!("{INTERVAL_ENV}={value}"),
    };
    let where_to = match target {
        Target::None => format!("attention inbox only (no {TARGET_ENV})"),
        Target::Url(url) => format!("attention inbox and {url}"),
        Target::Malformed(reason) => format!("attention inbox only ({reason})"),
    };
    match interval {
        None => format!("scorecard: weekly digest disabled ({source})"),
        Some(interval) => format!(
            "scorecard: weekly digest every {}h to {where_to} ({source}; set {INTERVAL_ENV}=0 to disable)",
            interval.as_secs() / 3600
        ),
    }
}

/// A digest is due when neither the newest recorded one nor this process's
/// last one is newer than `interval`.
fn due(interval: Duration, recorded_age: Option<Duration>, built_age: Option<Duration>) -> bool {
    let stale = |age: Option<Duration>| age.is_none_or(|age| age >= interval);
    stale(recorded_age) && stale(built_age)
}

/// When the newest recorded digest was recorded. The event log keeps 30 days,
/// so a week-old digest is still there to be found.
fn newest_digest(state: &WebState) -> Option<String> {
    state
        .events
        .query(&EventsQuery {
            kind: Some(DIGEST_KIND.to_string()),
            limit: Some(1),
            ..Default::default()
        })
        .ok()?
        .into_iter()
        .next()
        .map(|event| event.ts)
}

/// Age of an RFC 3339 timestamp as the event log stores it.
fn age_of(raw: String, now: DateTime<Utc>) -> Option<Duration> {
    let at = DateTime::parse_from_rfc3339(raw.trim())
        .ok()?
        .with_timezone(&Utc);
    (now - at).to_std().ok()
}

#[cfg(test)]
mod tests;
