//! One bounded receive-pack inspection, including a scoped admin exception.

use std::collections::HashSet;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use jeryu_core::{ForgeError, PullRequest, UserRole};
use jeryu_gitd::pack::ReceivePackCommand;
use serde::Deserialize;
use serde_json::json;

use crate::web::{WebState, auth, pushed_after_merge};

pub(super) const MAX_PUSH_REFS: usize = 64;
const MAX_NOTICE_BRANCHES: usize = 16;
const OVERRIDE_HEADER: &str = "x-jeryu-merged-branch-override";

fn error(status: StatusCode, code: &str, message: &str) -> Box<Response> {
    Box::new((status, axum::Json(json!({
        "code": code, "message": message, "reason": code,
        "documentation_url": "/docs/errors.md",
        "jeryu_repair_hint": "correct the receive-pack commands, ref count, or exact administrator override before retrying; other Git protections still apply",
    }))).into_response())
}

pub(super) fn commands(body: &[u8]) -> Result<Vec<ReceivePackCommand>, Box<Response>> {
    let commands = jeryu_gitd::pack::receive_pack_commands(body).map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            "invalid_receive_pack",
            "receive-pack commands could not be parsed",
        )
    })?;
    // Git's large HTTP pushes use a flush-only connection probe. It names
    // no refs. Preserve only that exact no-op, never an unparsed body.
    if (commands.is_empty() && body != b"0000") || commands.len() > MAX_PUSH_REFS {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "receive_pack_ref_limit",
            "a push must contain from 1 to 64 ref updates",
        ));
    }
    let mut refs = HashSet::new();
    if commands
        .iter()
        .any(|command| !refs.insert(&command.ref_name))
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "duplicate_receive_pack_ref",
            "a push may update each ref only once",
        ));
    }
    Ok(commands)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Override {
    #[serde(rename = "ref")]
    ref_name: String,
    old_sha: String,
    new_sha: String,
    pull_request: u64,
    reason: String,
}

fn admin_override(
    state: &WebState,
    headers: &HeaderMap,
) -> Result<Option<(String, Override)>, Box<Response>> {
    let values: Vec<_> = headers.get_all(OVERRIDE_HEADER).iter().collect();
    if values.is_empty() {
        return Ok(None);
    }
    let Some(actor) = auth::authenticate_headers(state, headers).filter(|actor| {
        actor.source == auth::AuthSource::Bearer
            && actor.bot.is_none()
            && actor.account.role == UserRole::Admin
    }) else {
        return Err(error(
            StatusCode::FORBIDDEN,
            "merged_branch_override_forbidden",
            "an explicit administrator credential is required for this override",
        ));
    };
    if values.len() != 1 || values[0].as_bytes().len() > 1024 {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_merged_branch_override",
            "provide one override header of at most 1024 bytes",
        ));
    }
    let permit: Override = serde_json::from_slice(values[0].as_bytes()).map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            "invalid_merged_branch_override",
            "the override must name one ref, old_sha, new_sha, pull_request and reason",
        )
    })?;
    if permit.reason.trim().is_empty()
        || permit.reason.len() > 256
        || permit.reason.chars().any(char::is_control)
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_merged_branch_override",
            "an override reason must contain from 1 to 256 bytes without control characters",
        ));
    }
    Ok(Some((actor.account.login, permit)))
}

pub(super) struct PushPolicy {
    pulls: Vec<PullRequest>,
    now: DateTime<Utc>,
}

impl PushPolicy {
    /// The reader is FnOnce: guard and notices cannot reload PRs per branch.
    pub(super) fn inspect(
        state: &WebState,
        owner: &str,
        repo: &str,
        commands: &[ReceivePackCommand],
        headers: &HeaderMap,
        sideband: bool,
        read_pulls: impl FnOnce() -> Result<Vec<PullRequest>, ForgeError>,
    ) -> Result<Self, Box<Response>> {
        let permit = admin_override(state, headers)?;
        if commands.is_empty() {
            if permit.is_some() {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "merged_branch_override_mismatch",
                    "an override cannot authorize a no-op probe",
                ));
            }
            return Ok(Self {
                pulls: Vec::new(),
                now: Utc::now(),
            });
        }
        let default = state
            .core
            .get_repository(owner, repo)
            .map_err(|_| {
                error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "push_policy_unavailable",
                    "could not read the repository push policy",
                )
            })?
            .default_branch;
        let pulls = read_pulls().map_err(|_| {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "push_policy_unavailable",
                "could not read pull requests for the push policy",
            )
        })?;
        let now = Utc::now();
        let mut used_override = false;
        for command in commands
            .iter()
            .filter(|command| command.new_oid != crate::ci_bridge::ZERO_OID)
        {
            let Some(branch) = command.ref_name.strip_prefix("refs/heads/") else {
                continue;
            };
            let Some(blocked) = pushed_after_merge::merged_pull_request_branch_among(
                &pulls, owner, repo, &default, branch, now,
            ) else {
                continue;
            };
            if command.old_oid == crate::ci_bridge::ZERO_OID && command.new_oid == blocked.head_sha
            {
                continue;
            }
            if permit.as_ref().is_some_and(|(_, permit)| {
                permit.ref_name == command.ref_name
                    && permit.old_sha == command.old_oid
                    && permit.new_sha == command.new_oid
                    && permit.pull_request == blocked.number
            }) {
                used_override = true;
                continue;
            }
            return Err(Box::new(super::gitd_to_axum_response(
                &super::receive_pack_refusal(
                    commands,
                    &command.ref_name,
                    &pushed_after_merge::receive_refusal_message(&blocked),
                    sideband,
                ),
            )));
        }
        if let Some((actor, permit)) = permit {
            if !used_override {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "merged_branch_override_mismatch",
                    "the override must match one refused merged-branch update exactly",
                ));
            }
            // A tuple-specific subject keeps this verification local to the
            // requested update. An in-memory core has no durable audit trail.
            let subject = format!(
                "{owner}/{repo}:{}:{}:{}",
                permit.ref_name, permit.old_sha, permit.new_sha
            );
            let audit = state
                .core
                .append_audit_as(
                    &actor,
                    "git.merged_branch_override",
                    &subject,
                    "requested",
                    json!({
                        "repository": format!("{owner}/{repo}"), "ref": permit.ref_name,
                        "old_sha": permit.old_sha, "new_sha": permit.new_sha,
                        "pull_request": permit.pull_request, "reason": permit.reason,
                    }),
                )
                .map_err(|_| {
                    error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "override_audit_failed",
                        "the override could not be recorded; no refs were changed",
                    )
                })?;
            if !state
                .core
                .list_audit(&subject)
                .is_ok_and(|rows| rows.iter().any(|row| row.id == audit))
            {
                return Err(error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "override_audit_failed",
                    "a durable audit record is required; no refs were changed",
                ));
            }
        }
        Ok(Self { pulls, now })
    }

    pub(super) fn notice_lines(
        &self,
        state: &WebState,
        owner: &str,
        repo: &str,
        branches: &[String],
    ) -> Vec<String> {
        let mut lines: Vec<_> = branches
            .iter()
            .take(MAX_NOTICE_BRANCHES)
            .filter_map(|branch| {
                pushed_after_merge::stranded_among(
                    state,
                    owner,
                    repo,
                    &self.pulls,
                    branch,
                    self.now,
                )
            })
            .flat_map(|stranded| pushed_after_merge::remote_message(&stranded))
            .collect();
        if branches.len() > MAX_NOTICE_BRANCHES {
            lines.push("Stranded-work notices cover at most 16 branches per push; check Needs you for additional branches.".to_string());
        }
        lines
    }
}

#[cfg(test)]
#[path = "push_policy_tests.rs"]
mod tests;
