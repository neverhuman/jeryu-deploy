//! Durable build identities plus the newest complete placement observation.
//! Builds that were never deployed remain visible; tags never identify a build.
use super::{WebState, api_error};
use axum::{
    Json,
    body::Bytes,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Duration, Utc};
use jeryu_core::{AccountSummary, UserRole};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    pub repository: String,
    pub digest: String,
    pub tags: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Build {
    pub id: String,
    pub deliverable: String,
    pub built_at: String,
    pub source_repo: String,
    pub source_sha: String,
    pub status: String,
    pub artifacts: Vec<Artifact>,
    pub log_url: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Placement {
    pub deliverable: String,
    pub environment: String,
    pub target: String,
    pub current: Option<String>,
    pub previous: Option<String>,
    pub production: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Report {
    pub observed_at: String,
    pub builds: Vec<Build>,
    /// None reports builds only. Some is a complete target observation for
    /// this family, including an empty list when no targets remain configured.
    pub placements: Option<Vec<Placement>>,
}
#[derive(Default, Deserialize)]
pub(crate) struct ListQuery {
    deliverable: Option<String>,
    offset: Option<usize>,
}

fn timestamp(value: &str) -> Result<String, String> {
    let date =
        DateTime::parse_from_rfc3339(value).map_err(|_| "Invalid observation/build timestamp")?;
    if date > Utc::now() + Duration::minutes(5) {
        return Err("Timestamp is too far in the future".into());
    }
    Ok(date
        .with_timezone(&Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}
fn text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 500 && !value.chars().any(char::is_control)
}
fn identity(build: &Build) -> Result<String, String> {
    // Tags and status may change. Built time, deliverable, source, and artifact
    // content are fixed once acknowledged; mirror locations may be added.
    serde_json::to_string(&(
        &build.deliverable,
        &build.built_at,
        &build.source_repo,
        &build.source_sha,
    ))
    .map_err(|e| e.to_string())
}
fn validate(report: &mut Report) -> Result<(), String> {
    report.observed_at = timestamp(&report.observed_at)?;
    if report.builds.len() > 500 {
        return Err("At most 500 builds per report".into());
    }
    let mut ids = BTreeSet::new();
    for build in &mut report.builds {
        build.built_at = timestamp(&build.built_at)?;
        if build.built_at > report.observed_at
            || !text(&build.id)
            || !text(&build.deliverable)
            || !text(&build.source_repo)
            || !build.source_repo.contains('/')
            || !matches!(build.source_sha.len(), 40 | 64)
            || !build.source_sha.bytes().all(|b| b.is_ascii_hexdigit())
            || !matches!(
                build.status.as_str(),
                "building" | "built" | "failed" | "rejected"
            )
            || build.artifacts.len() > 16
            || !ids.insert(build.id.clone())
        {
            return Err("Invalid or duplicate build identity, time, source, or status".into());
        }
        if build.log_url.as_ref().is_some_and(|url| {
            !text(url)
                || !(url.starts_with("https://")
                    || url.starts_with("http://")
                    || (url.starts_with('/') && !url.starts_with("//")))
        }) {
            return Err("Build log URL must be HTTP(S) or a same-origin path".into());
        }
        for artifact in &build.artifacts {
            if !text(&artifact.repository)
                || artifact.repository.contains('@')
                || artifact.tags.len() > 64
                || artifact.tags.iter().any(|tag| !text(tag))
                || artifact.digest.len() != 71
                || !artifact.digest.starts_with("sha256:")
                || !artifact.digest[7..].bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err("Invalid image repository, sha256 digest, or tags".into());
            }
        }
    }
    if let Some(placements) = &report.placements {
        let mut targets = BTreeSet::new();
        if placements.len() > 128 {
            return Err("At most 128 deployment targets per report".into());
        }
        for p in placements {
            if !text(&p.deliverable)
                || !text(&p.environment)
                || !text(&p.target)
                || p.current.as_ref().is_some_and(|id| !text(id))
                || p.previous.as_ref().is_some_and(|id| !text(id))
                || (p.current.is_some() && p.current == p.previous)
                || !targets.insert((&p.deliverable, &p.environment, &p.target))
            {
                return Err("Invalid or duplicate deployment target".into());
            }
        }
    }
    Ok(())
}
fn record(conn: &Connection, family: &str, report: &Report) -> Result<(), String> {
    let transaction = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    for build in &report.builds {
        let key = identity(build)?;
        let prior: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT identity, observed_at, body FROM build_inventory WHERE family=?1 AND id=?2",
                params![family, build.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some((prior_key, observed, prior_body)) = prior {
            if prior_key != key {
                return Err(format!(
                    "Build {} already has another immutable identity",
                    build.id
                ));
            }
            let prior_build: Build =
                serde_json::from_str(&prior_body).map_err(|e| e.to_string())?;
            let prior_digests: BTreeSet<_> =
                prior_build.artifacts.iter().map(|a| &a.digest).collect();
            let next_digests: BTreeSet<_> = build.artifacts.iter().map(|a| &a.digest).collect();
            if !prior_digests.is_empty() && prior_digests != next_digests {
                return Err(format!(
                    "Build {} already has other artifact content",
                    build.id
                ));
            }
            if observed >= report.observed_at {
                continue;
            }
        }
        let body = serde_json::to_string(build).map_err(|e| e.to_string())?;
        transaction.execute("INSERT INTO build_inventory(family,id,deliverable,built_at,identity,observed_at,body) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(family,id) DO UPDATE SET observed_at=excluded.observed_at, body=excluded.body", params![family, build.id, build.deliverable, build.built_at, key, report.observed_at, body]).map_err(|e| e.to_string())?;
    }
    if let Some(placements) = &report.placements {
        for placement in placements {
            for id in [&placement.current, &placement.previous]
                .into_iter()
                .flatten()
            {
                let exists: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM build_inventory WHERE family=?1 AND id=?2 AND deliverable=?3)", params![family, id, placement.deliverable], |r| r.get(0)).map_err(|e| e.to_string())?;
                if !exists {
                    return Err(format!("Placement references an unknown build: {id}"));
                }
            }
        }
        transaction.execute("INSERT INTO build_placements(family,observed_at,body) VALUES(?1,?2,?3) ON CONFLICT(family) DO UPDATE SET observed_at=excluded.observed_at, body=excluded.body WHERE excluded.observed_at > build_placements.observed_at", params![family, report.observed_at, serde_json::to_string(placements).map_err(|e| e.to_string())?]).map_err(|e| e.to_string())?;
    }
    transaction.commit().map_err(|e| e.to_string())
}

pub(crate) async fn put(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Path(family): Path<String>,
    body: Bytes,
) -> Response {
    if !state
        .release_boards
        .may_report(&account.login, account.role == UserRole::Admin)
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "A release-board reporter or administrator must report builds",
        );
    }
    if body.len() > 1024 * 1024 {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "A build report is at most 1 MiB",
        );
    }
    let family = super::family::canonical(&family);
    if !text(&family)
        || !family
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "Invalid family key",
        );
    }
    let mut report = match serde_json::from_slice::<Report>(&body) {
        Ok(r) => r,
        Err(_) => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                "Invalid build inventory report",
            );
        }
    };
    if let Err(message) = validate(&mut report) {
        return api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", &message);
    }
    match state.events.with_conn(|conn| record(conn, &family, &report)) {
        Ok(()) => Json(json!({"family": family, "observed_at": report.observed_at, "accepted": report.builds.len()})).into_response(),
        Err(message) if message.starts_with("Build ") || message.starts_with("Placement ") => api_error(StatusCode::CONFLICT, "invalid_input", &message),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "storage_failed", "Build inventory could not be saved"),
    }
}

pub(crate) async fn get(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Path(family): Path<String>,
    Query(query): Query<ListQuery>,
) -> Response {
    if account.role != UserRole::Admin {
        return api_error(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "Build inventory requires an administrator",
        );
    }
    let family = super::family::canonical(&family);
    match state.events.with_conn(|conn| list(conn, &family, &query)) {
        Ok(value) => Json(value).into_response(),
        Err(_) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            "Build inventory could not be read",
        ),
    }
}
fn list(conn: &Connection, family: &str, query: &ListQuery) -> Result<serde_json::Value, String> {
    let mut statement = conn
        .prepare(
            "SELECT DISTINCT deliverable FROM build_inventory WHERE family=?1 ORDER BY deliverable",
        )
        .map_err(|e| e.to_string())?;
    let deliverables = statement
        .query_map([family], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let deliverable = query
        .deliverable
        .as_deref()
        .or_else(|| deliverables.first().map(String::as_str))
        .unwrap_or("");
    let total: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM build_inventory WHERE family=?1 AND deliverable=?2",
            params![family, deliverable],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    let offset = query.offset.unwrap_or(0).min(total);
    let mut statement = conn.prepare("SELECT body,observed_at FROM build_inventory WHERE family=?1 AND deliverable=?2 ORDER BY built_at DESC,id LIMIT 100 OFFSET ?3").map_err(|e| e.to_string())?;
    let rows = statement
        .query_map(params![family, deliverable, offset], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let builds = rows
        .iter()
        .map(|(body, at)| {
            let mut value: serde_json::Value =
                serde_json::from_str(body).map_err(|e| e.to_string())?;
            value["observed_at"] = json!(at);
            Ok(value)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let placement: Option<(String, String)> = conn
        .query_row(
            "SELECT observed_at,body FROM build_placements WHERE family=?1",
            [family],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let (observed_at, placements) = match placement {
        Some((at, body)) => (
            Some(at),
            serde_json::from_str::<serde_json::Value>(&body).map_err(|e| e.to_string())?,
        ),
        None => (None, json!([])),
    };
    Ok(
        json!({"family":family,"deliverable":deliverable,"deliverables":deliverables,"builds":builds,"placements":placements,"placements_observed_at":observed_at,"total":total,"next_offset":(offset+rows.len()<total).then_some(offset+rows.len())}),
    )
}

#[cfg(test)]
mod tests;
