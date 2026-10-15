//! Keeping every hosted repository's codegraph index populated.
//!
//! The `code.*` MCP tools and the codegraph REST facade read a repository's
//! own index (`<bare repo>/jeryu/codegraph.sqlite`), so a repository with no
//! index answers a symbol search with nothing.
//!
//! Two things fill them here: a push to a repository's default branch
//! refreshes that repository, and a timer refreshes any repository whose index
//! is empty or older than the configured interval, so a repository that was
//! pushed while the server was down, or whose refresh failed, is picked up
//! without another push.
//!
//! `JERYU_CODEGRAPH_REFRESH_INTERVAL_HOURS` sets the cadence (default 6; `0`
//! disables the timer, not the push refresh). The loop wakes every ten
//! minutes and refreshes only what is due.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use super::WebState;
use super::codegraph;
use super::repositories::find_repo;

const INTERVAL_ENV: &str = "JERYU_CODEGRAPH_REFRESH_INTERVAL_HOURS";
const DEFAULT_INTERVAL_HOURS: u64 = 6;
const WAKE_EVERY: Duration = Duration::from_secs(10 * 60);

/// Refresh the index of every repository a push moved its default branch on.
///
/// Called from the detached post-push work, so indexing never holds the push
/// acknowledgement. Other branches are left alone: one index tracks one ref,
/// and the default branch is what the code tools answer for.
pub(crate) fn refresh_pushed_branches(
    state: &Arc<WebState>,
    owner: &str,
    repo: &str,
    updates: &[crate::ci_bridge::RefUpdate],
) {
    let Some(found) = find_repo(state, &format!("{owner}/{repo}")) else {
        return;
    };
    let default_ref = format!("refs/heads/{}", found.default_branch);
    if !updates.iter().any(|update| update.ref_name == default_ref) {
        return;
    }
    refresh_one(state, &found.full_name, &found.default_branch);
}

/// Spawn the refresh timer when a cadence is configured.
pub(super) fn spawn(state: Arc<WebState>) {
    let raw = std::env::var(INTERVAL_ENV).ok();
    let interval = interval_from(raw.as_deref());
    eprintln!("{}", startup_line(raw.as_deref(), interval));
    let Some(interval) = interval else {
        return;
    };
    tokio::spawn(async move {
        let mut attempts = RefreshAttempts::new();
        let mut ticker = tokio::time::interval(WAKE_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let state = state.clone();
            // Indexing clones a checkout and walks a workspace: blocking work
            // that must not sit on the async runtime serving git and the API.
            attempts = tokio::task::spawn_blocking(move || {
                let mut attempts = attempts;
                refresh_due(&state, interval, &mut attempts);
                attempts
            })
            .await
            .unwrap_or_default();
        }
    });
}

/// When this process last tried each repository, keyed by `owner/name`. A
/// repository that cannot be indexed -- no Cargo workspace, no commit on its
/// default branch -- is then retried on the configured cadence instead of on
/// every wake-up. One entry per hosted repository, and repositories that are
/// gone drop out of it.
pub(super) type RefreshAttempts = HashMap<String, Instant>;

/// Refresh every repository whose index is due. Returns how many repositories
/// were refreshed, which the tests assert on.
pub(super) fn refresh_due(
    state: &Arc<WebState>,
    interval: Duration,
    attempts: &mut RefreshAttempts,
) -> usize {
    let mut refreshed = 0;
    let repos = state.core.list_repositories(None);
    for repo in &repos {
        let tried_recently = attempts
            .get(&repo.full_name)
            .is_some_and(|at| at.elapsed() < interval);
        if tried_recently {
            continue;
        }
        let Ok(managed) = state.repo_manager.open_parts(&repo.owner, &repo.name) else {
            continue;
        };
        if !index_is_due(&managed.path, interval) {
            continue;
        }
        // Recorded whatever the outcome: a repository the indexer cannot read
        // must not be reattempted on every tick.
        attempts.insert(repo.full_name.clone(), Instant::now());
        if refresh_one(state, &repo.full_name, &repo.default_branch) {
            refreshed += 1;
        }
    }
    attempts.retain(|full_name, _| repos.iter().any(|repo| &repo.full_name == full_name));
    refreshed
}

/// Refresh one repository, reporting the outcome on the server log. A failure
/// is expected and harmless for a repository with no Cargo workspace or no
/// commit on its default branch yet, so it never stops the pass.
fn refresh_one(state: &Arc<WebState>, full_name: &str, ref_name: &str) -> bool {
    match codegraph::refresh_repo_index(state, full_name, ref_name) {
        Ok(symbols) => {
            eprintln!("codegraph: indexed {full_name} at {ref_name} ({symbols} symbols)");
            true
        }
        Err(error) => {
            eprintln!("codegraph: {full_name} at {ref_name} not indexed: {error}");
            false
        }
    }
}

/// A repository is due when it has no index file, when the index holds no
/// symbol at all, or when the file was last written longer ago than the
/// interval. The file's own write time is the receipt: a refresh persists the
/// snapshot in one transaction at the end, so an index that was never built
/// reads as empty and is due whatever its timestamp says.
fn index_is_due(bare: &Path, interval: Duration) -> bool {
    if !codegraph::repo_store_path(bare).is_file() {
        return true;
    }
    if codegraph::repo_index_is_empty(bare).unwrap_or(true) {
        return true;
    }
    match index_age(&codegraph::repo_store_path(bare)) {
        Some(age) => age >= interval,
        None => true,
    }
}

fn index_age(store: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(store).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Parse the configured cadence; `None` means the timer is off.
fn interval_from(raw: Option<&str>) -> Option<Duration> {
    let hours = match raw.map(str::trim) {
        None | Some("") => DEFAULT_INTERVAL_HOURS,
        Some(value) => value.parse::<u64>().ok()?,
    };
    (hours > 0).then(|| Duration::from_secs(hours * 60 * 60))
}

/// The one line logged at startup so the effective cadence is visible.
fn startup_line(raw: Option<&str>, interval: Option<Duration>) -> String {
    let source = match raw.map(str::trim) {
        None | Some("") => "default".to_string(),
        Some(value) => format!("{INTERVAL_ENV}={value}"),
    };
    match interval {
        None => format!(
            "codegraph: scheduled index refresh disabled ({source}); pushes to a default branch still refresh"
        ),
        Some(interval) => format!(
            "codegraph: index refresh every {}h ({source}; set {INTERVAL_ENV}=0 to disable)",
            interval.as_secs() / 3600
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(60 * 60);

    #[test]
    fn interval_defaults_to_six_hours_and_zero_disables() {
        assert_eq!(interval_from(None), Some(6 * HOUR));
        assert_eq!(interval_from(Some(" ")), Some(6 * HOUR));
        assert_eq!(interval_from(Some("1")), Some(HOUR));
        assert_eq!(interval_from(Some("0")), None);
        assert_eq!(interval_from(Some("nightly")), None);
    }

    #[test]
    fn startup_line_names_the_cadence_and_the_push_refresh() {
        assert!(startup_line(Some("0"), None).contains("pushes to a default branch"));
        assert!(
            startup_line(None, Some(6 * HOUR)).contains("every 6h"),
            "{}",
            startup_line(None, Some(6 * HOUR))
        );
    }

    #[test]
    fn an_absent_or_empty_index_is_due_and_a_fresh_populated_one_is_not() {
        let dir = tempfile::tempdir().expect("bare repo fixture");
        // Never built: no file at all.
        assert!(index_is_due(dir.path(), HOUR));
        // Built but empty: the schema file exists and is new, and it is still
        // due because it answers no symbol.
        let store = codegraph::open_repo_store(dir.path()).expect("open repo store");
        assert!(index_is_due(dir.path(), HOUR));
        store
            .persist(&jeryu_codegraph::GraphSnapshot {
                symbols: vec![jeryu_codegraph::SymbolRow {
                    crate_name: "jeryu-api".to_string(),
                    file: "crates/jeryu-api/src/lib.rs".to_string(),
                    symbol: "required_access".to_string(),
                    kind: "public".to_string(),
                    is_public: true,
                    line: 3,
                }],
                ..Default::default()
            })
            .expect("persist snapshot");
        assert!(!index_is_due(dir.path(), HOUR));
        // An index older than the interval is due again.
        assert!(index_is_due(dir.path(), Duration::ZERO));
    }
}
