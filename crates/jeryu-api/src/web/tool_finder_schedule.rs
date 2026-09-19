//! Always-on tool-finder: re-runs the system-wide duplicate-code scan on a
//! schedule so Shared tools → Findings stays current without anyone pressing
//! "Run live scan".
//!
//! `JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS` sets the cadence (default 24; `0`
//! disables). The loop wakes hourly and starts a scan only when both the
//! persisted scan and the last scan this process started are older than the
//! interval, so frequent deploys neither skip the nightly scan nor rescan on
//! every restart. It reuses `start_system_scan`, so a scheduled scan streams
//! progress like a manual one and never overlaps one already running.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::WebState;
use super::tool_finder::{StartScanError, scan_created_at, start_system_scan};

const INTERVAL_ENV: &str = "JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS";
const DEFAULT_INTERVAL_HOURS: u64 = 24;
const WAKE_EVERY: Duration = Duration::from_secs(60 * 60);

/// Spawn the scheduler when a cadence is configured and manifests are wired.
pub(super) fn spawn(state: Arc<WebState>) {
    let raw = std::env::var(INTERVAL_ENV).ok();
    let interval = interval_from(raw.as_deref());
    let has_manifests = !state.split_manifests.is_empty();
    eprintln!("{}", startup_line(raw.as_deref(), interval, has_manifests));
    let Some(interval) = interval else {
        return;
    };
    if !has_manifests {
        return;
    }
    tokio::spawn(async move {
        let mut last_started: Option<Instant> = None;
        let mut ticker = tokio::time::interval(WAKE_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let persisted_age = scan_created_at(&state).and_then(|at| age_of_millis(&at));
            let started_age = last_started.map(|at| at.elapsed());
            if !scan_due(interval, persisted_age, started_age) {
                continue;
            }
            match start_system_scan(&state) {
                Ok(_) => last_started = Some(Instant::now()),
                Err(StartScanError::Busy(_)) => {}
                Err(StartScanError::NoManifests) => return,
            }
        }
    });
}

/// Parse the configured cadence; `None` means the scheduler is off.
fn interval_from(raw: Option<&str>) -> Option<Duration> {
    let hours = match raw.map(str::trim) {
        None | Some("") => DEFAULT_INTERVAL_HOURS,
        Some(value) => value.parse::<u64>().ok()?,
    };
    (hours > 0).then(|| Duration::from_secs(hours * 60 * 60))
}

/// The one line logged at startup so the effective cadence is visible.
fn startup_line(raw: Option<&str>, interval: Option<Duration>, has_manifests: bool) -> String {
    let source = match raw.map(str::trim) {
        None | Some("") => "default".to_string(),
        Some(value) => format!("{INTERVAL_ENV}={value}"),
    };
    match (interval, has_manifests) {
        (None, _) => format!("tool-finder: scheduled scan disabled ({source})"),
        (Some(_), false) => {
            "tool-finder: scheduled scan disabled (no split manifests configured)".to_string()
        }
        (Some(interval), true) => format!(
            "tool-finder: scheduled scan every {}h ({source}; set {INTERVAL_ENV}=0 to disable)",
            interval.as_secs() / 3600
        ),
    }
}

/// A scan is due when neither the persisted scan nor this process's last
/// start is newer than `interval`.
fn scan_due(
    interval: Duration,
    persisted_age: Option<Duration>,
    started_age: Option<Duration>,
) -> bool {
    let stale = |age: Option<Duration>| age.is_none_or(|age| age >= interval);
    stale(persisted_age) && stale(started_age)
}

/// Age of a unix-millis timestamp string, as the cluster rows store it.
fn age_of_millis(raw: &str) -> Option<Duration> {
    let millis = raw.trim().parse::<u64>().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    Some(now.saturating_sub(Duration::from_millis(millis)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(60 * 60);

    #[test]
    fn interval_defaults_to_a_day_and_zero_disables() {
        assert_eq!(interval_from(None), Some(24 * HOUR));
        assert_eq!(interval_from(Some(" ")), Some(24 * HOUR));
        assert_eq!(interval_from(Some("6")), Some(6 * HOUR));
        assert_eq!(interval_from(Some("0")), None);
        assert_eq!(interval_from(Some("nightly")), None);
    }

    #[test]
    fn startup_line_states_the_effective_interval() {
        let line = startup_line(None, interval_from(None), true);
        assert!(line.contains("every 24h (default;"), "{line}");
        assert!(
            line.contains(&format!("{INTERVAL_ENV}=0 to disable")),
            "{line}"
        );
        let line = startup_line(Some("6"), interval_from(Some("6")), true);
        assert!(
            line.contains(&format!("every 6h ({INTERVAL_ENV}=6;")),
            "{line}"
        );
        let line = startup_line(Some("0"), interval_from(Some("0")), true);
        assert_eq!(
            line,
            format!("tool-finder: scheduled scan disabled ({INTERVAL_ENV}=0)")
        );
        let line = startup_line(None, interval_from(None), false);
        assert!(line.contains("no split manifests"), "{line}");
    }

    #[test]
    fn due_only_when_every_known_scan_is_stale() {
        let day = 24 * HOUR;
        assert!(scan_due(day, None, None));
        assert!(scan_due(day, Some(25 * HOUR), None));
        assert!(!scan_due(day, Some(2 * HOUR), None));
        // A just-started scan that has not persisted yet blocks a rescan.
        assert!(!scan_due(day, None, Some(HOUR)));
        assert!(scan_due(day, Some(30 * HOUR), Some(25 * HOUR)));
    }

    #[test]
    fn ages_unix_millis_and_rejects_garbage() {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let age = age_of_millis(&(now_ms - 2 * 60 * 60 * 1000).to_string()).unwrap();
        assert!(age >= 2 * HOUR && age < 2 * HOUR + Duration::from_secs(60));
        assert_eq!(age_of_millis("2026-09-18"), None);
    }
}
