//! Where a pull request came from, and what that lets it do.
//!
//! The GitHub repositories are read-only mirrors of this forge: an outside
//! contribution is opened there and an importer brings it here, where it is
//! gated, reviewed and merged. Such a pull request is `contributed`; one
//! written inside the forge is `internal`, which is what a pull request with
//! no recorded origin reads as.
//!
//! The marking is the importer's, never the author's. Only a global admin or a
//! `JERYU_PULL_IMPORTERS` identity may write it, and a caller may not mark
//! their own pull request at all, so a contribution cannot declare itself
//! trusted. The title, description and diff of a contributed pull request are
//! data, not instructions: nothing in them changes what this module decides.
//!
//! What `contributed` costs a pull request: it cannot merge until a login on
//! the configured maintainer list (`JERYU_PULL_MAINTAINERS`) has an effective
//! approval on the exact head. The hold is a merge-passport blocker, so the
//! pull request page states it, `POST .../merge` refuses it, and the merge
//! queue will not land it. An approval from anybody else — including an agent
//! reviewer — is advice and does not clear it. With no maintainer list
//! configured, no contribution merges at all.
//!
//! - `GET /api/v1/repos/:id/pulls/:number/origin`: the origin, who set it, and
//!   where the maintainer approval stands.
//! - `PUT /api/v1/repos/:id/pulls/:number/origin`: `{"origin": "contributed" |
//!   "internal"}`, for an importer or a global admin.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_core::{AccountSummary, PullRequest, ReviewState, UserRole, effective_reviews_for_head};
use jeryu_readmodel::contracts::MergePassportBlocker;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::shift::{migrate_shift_store, rfc3339_ms};
use super::{WebState, api_error};

/// Logins that may mark a pull request's origin without being a global admin,
/// comma-separated. Empty by default: a site names its importer identity, and
/// until it does only an admin can mark a contribution.
const IMPORTERS_ENV: &str = "JERYU_PULL_IMPORTERS";

/// Logins whose approval lets a contributed pull request merge,
/// comma-separated. Empty by default, which holds every contribution.
const MAINTAINERS_ENV: &str = "JERYU_PULL_MAINTAINERS";

/// The merge-passport blocker a held contribution carries. It is a passport
/// blocker code, not an error-envelope code: the merge refusal itself answers
/// `merge_blocked` and lists this among its blockers.
const CONTRIBUTED_BLOCKER: &str = "passport_blocked_contributed_origin";

/// Where a pull request came from. A pull request the importer has not marked
/// is `Internal`: that is what everything written inside the forge is, and the
/// mirrors cannot push a branch here at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PullOrigin {
    #[default]
    Internal,
    Contributed,
}

impl PullOrigin {
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Contributed => "contributed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "internal" => Some(Self::Internal),
            "contributed" => Some(Self::Contributed),
            _ => None,
        }
    }

    /// `origin:internal` or `origin:contributed`, the label every pull request
    /// summary carries so a client reads the trust marking from the row it
    /// already has instead of asking a second route.
    pub(crate) fn label(self) -> String {
        format!("origin:{}", self.wire_name())
    }
}

/// One recorded marking and who made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredOrigin {
    pub origin: PullOrigin,
    pub set_by: String,
    pub set_ms: i64,
}

/// `pull_request_origins` in `<data_dir>/shift.sqlite`
/// (`db/migrations/0014_pull_request_origins.sql`).
#[derive(Clone)]
pub(crate) struct PullOriginStore {
    inner: Arc<Mutex<Connection>>,
}

impl PullOriginStore {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|err| err.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|err| err.to_string())?;
        migrate_shift_store(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    pub(crate) fn get(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<StoredOrigin>, String> {
        let conn = self.inner.lock().expect("pull origin mutex poisoned");
        conn.query_row(
            "SELECT origin, set_by, set_ms FROM pull_request_origins
             WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3",
            params![owner, repo, number as i64],
            |row| {
                let origin: String = row.get(0)?;
                Ok(StoredOrigin {
                    origin: PullOrigin::parse(&origin).unwrap_or(PullOrigin::Contributed),
                    set_by: row.get(1)?,
                    set_ms: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(|err| err.to_string())
    }

    pub(crate) fn set(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        origin: PullOrigin,
        set_by: &str,
        now_ms: i64,
    ) -> Result<(), String> {
        let conn = self.inner.lock().expect("pull origin mutex poisoned");
        conn.execute(
            "INSERT INTO pull_request_origins
               (repo_owner, repo_name, pr_number, origin, set_by, set_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(repo_owner, repo_name, pr_number) DO UPDATE SET
               origin = excluded.origin,
               set_by = excluded.set_by,
               set_ms = excluded.set_ms",
            params![
                owner,
                repo,
                number as i64,
                origin.wire_name(),
                set_by,
                now_ms
            ],
        )
        .map(|_| ())
        .map_err(|err| err.to_string())
    }
}

/// Logins that may mark an origin, read once at startup. A test injects the
/// list through [`WebState`] instead (the crate forbids `unsafe`, so
/// `std::env::set_var` is unavailable to tests).
pub(in crate::web) fn importers_from_env() -> Vec<String> {
    login_list(std::env::var(IMPORTERS_ENV).unwrap_or_default().as_str())
}

/// Logins whose approval clears a contributed pull request, read once at
/// startup. Empty holds every contribution.
pub(in crate::web) fn maintainers_from_env() -> Vec<String> {
    login_list(std::env::var(MAINTAINERS_ENV).unwrap_or_default().as_str())
}

pub(in crate::web) fn login_list(configured: &str) -> Vec<String> {
    configured
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether this account may mark a pull request's origin at all.
fn may_mark(account: &AccountSummary, importers: &[String]) -> bool {
    account.role == UserRole::Admin || importers.iter().any(|login| login == &account.login)
}

/// Where a pull request's origin leaves its merge gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::web) struct OriginPosture {
    pub origin: PullOrigin,
    pub set_by: Option<String>,
    pub set_at: Option<String>,
    /// Effective approvals on the exact head from a configured maintainer.
    pub maintainer_approvals: u32,
    /// Maintainer logins the site configured, so a reader of a hold can tell
    /// "nobody has approved yet" from "no maintainer list exists".
    pub maintainers: Vec<String>,
}

impl OriginPosture {
    pub(in crate::web) fn contributed(&self) -> bool {
        self.origin == PullOrigin::Contributed
    }

    /// True while a contributed pull request has no maintainer approval on
    /// this head. Internal pull requests are never held by this gate.
    pub(in crate::web) fn blocks_merge(&self) -> bool {
        self.contributed() && self.maintainer_approvals == 0
    }

    pub(in crate::web) fn blocker(&self) -> Option<MergePassportBlocker> {
        if !self.blocks_merge() {
            return None;
        }
        let details = if self.maintainers.is_empty() {
            format!("no maintainer list is configured ({MAINTAINERS_ENV})")
        } else {
            format!("maintainers: {}", self.maintainers.join(", "))
        };
        Some(MergePassportBlocker {
            code: CONTRIBUTED_BLOCKER.to_string(),
            message: "This pull request came from outside the forge: it merges only after a \
                      configured maintainer approves this exact head. Any other approval, \
                      including an agent reviewer's, is advice."
                .to_string(),
            details: Some(details),
        })
    }

    pub(in crate::web) fn reason(&self) -> Option<String> {
        self.blocks_merge().then(|| {
            "contributed pull request without a maintainer approval on this head".to_string()
        })
    }
}

/// The origin posture of one pull request. A store that cannot be read is
/// treated as a contribution: a trust marking nobody can read is not a reason
/// to let a merge through.
pub(in crate::web) fn posture(state: &WebState, pr: &PullRequest) -> OriginPosture {
    let stored = match state.pull_origin.get(&pr.owner, &pr.repo, pr.number) {
        Ok(stored) => stored,
        Err(_) => Some(StoredOrigin {
            origin: PullOrigin::Contributed,
            set_by: String::new(),
            set_ms: 0,
        }),
    };
    let origin = stored
        .as_ref()
        .map_or(PullOrigin::Internal, |row| row.origin);
    let maintainers = state.pull_maintainers.as_ref().clone();
    OriginPosture {
        origin,
        set_by: stored
            .as_ref()
            .map(|row| row.set_by.clone())
            .filter(|login| !login.is_empty()),
        set_at: stored
            .as_ref()
            .filter(|row| row.set_ms > 0)
            .map(|row| rfc3339_ms(row.set_ms)),
        maintainer_approvals: maintainer_approvals(state, pr, &maintainers),
        maintainers,
    }
}

/// Effective approvals on the exact head whose author is a configured
/// maintainer. A stale approval (one left on an earlier head) is not counted,
/// which is already how `effective_reviews_for_head` reads the review log.
fn maintainer_approvals(state: &WebState, pr: &PullRequest, maintainers: &[String]) -> u32 {
    if maintainers.is_empty() {
        return 0;
    }
    let reviews = state
        .github
        .core()
        .list_reviews(&pr.owner, &pr.repo, pr.number)
        .unwrap_or_default();
    effective_reviews_for_head(&reviews, &pr.head.sha)
        .iter()
        .filter(|review| review.state == ReviewState::Approved)
        .filter(|review| maintainers.iter().any(|login| login == &review.author))
        .count() as u32
}

#[derive(Debug, Serialize)]
struct OriginResponse {
    origin: &'static str,
    /// `true` for a pull request the importer marked as coming from outside.
    contributed: bool,
    set_by: Option<String>,
    set_at: Option<String>,
    /// Whether a configured maintainer has approved this exact head.
    maintainer_approved: bool,
    maintainer_approvals: u32,
    maintainers: Vec<String>,
    /// Why the merge is held by this gate, or `None`.
    merge_hold: Option<String>,
    head_sha: String,
}

fn origin_response(posture: &OriginPosture, pr: &PullRequest) -> OriginResponse {
    OriginResponse {
        origin: posture.origin.wire_name(),
        contributed: posture.contributed(),
        set_by: posture.set_by.clone(),
        set_at: posture.set_at.clone(),
        maintainer_approved: posture.maintainer_approvals > 0,
        maintainer_approvals: posture.maintainer_approvals,
        maintainers: posture.maintainers.clone(),
        merge_hold: posture.reason(),
        head_sha: pr.head.sha.clone(),
    }
}

#[derive(Debug, Deserialize)]
struct OriginUpdate {
    origin: String,
}

fn resolve(state: &WebState, id: &str, number: u64) -> Option<PullRequest> {
    let repo = super::repositories::find_repo(state, id)?;
    state
        .github
        .core()
        .get_pull_request(&repo.owner, &repo.name, number)
        .ok()
}

/// `GET /api/v1/repos/:id/pulls/:number/origin`.
pub(super) async fn show(
    State(state): State<Arc<WebState>>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    let Some(pr) = resolve(&state, &id, number) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "pull request not found");
    };
    Json(origin_response(&posture(&state, &pr), &pr)).into_response()
}

/// `PUT /api/v1/repos/:id/pulls/:number/origin`.
pub(super) async fn set(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
    body: Bytes,
) -> AxumResponse {
    if !may_mark(&account, &state.pull_importers) {
        return api_error(
            StatusCode::FORBIDDEN,
            "pull_origin_importer_required",
            "only a global admin or a JERYU_PULL_IMPORTERS login may mark a pull request's origin",
        );
    }
    let Ok(update) = serde_json::from_slice::<OriginUpdate>(&body) else {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "pull_origin_invalid_request",
            "send {\"origin\": \"contributed\"} or {\"origin\": \"internal\"}",
        );
    };
    let Some(origin) = PullOrigin::parse(&update.origin) else {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "pull_origin_invalid_request",
            "origin must be \"internal\" or \"contributed\"",
        );
    };
    let Some(pr) = resolve(&state, &id, number) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "pull request not found");
    };
    // The marking says how much to trust this pull request, so its own author
    // never gets to make it — not even an author who is otherwise an importer.
    if pr.author == account.login {
        return api_error(
            StatusCode::FORBIDDEN,
            "pull_origin_author_forbidden",
            "a pull request's own author may not mark its origin",
        );
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    if let Err(error) = state.pull_origin.set(
        &pr.owner,
        &pr.repo,
        pr.number,
        origin,
        &account.login,
        now_ms,
    ) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("could not record the pull request origin: {error}"),
        );
    }
    Json(origin_response(&posture(&state, &pr), &pr)).into_response()
}
