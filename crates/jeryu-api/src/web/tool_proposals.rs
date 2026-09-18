//! Approve/reject a proposed shared tool — the decision step between the
//! tool-finder's proposals and a tool being built.
//!
//! `POST /api/v1/tool-finder/proposals/:tool_id/decision` edits
//! `jeryu-tool/tools-registry.toml` in place (via `toml_edit`, so the rest of
//! the hand-maintained file keeps its comments and layout):
//!
//! * `approve` — `status = "proposed"` → `"building"`; the build task filed
//!   with the proposal stays open for whoever builds it.
//! * `reject`  — the `[[tool]]` entry and its still-open build tasks are
//!   removed, and the origin cluster is ignored so the finder stops
//!   resurfacing it.
//!
//! Admin-only through the `/api/v1/tool-finder/` prefix in `auth.rs`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_core::AccountSummary;
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, value};

use super::WebState;
use super::tool_status_messages::{STATUS_BUILDING, STATUS_PROPOSED};
use super::workcells_support::{TypedError, typed_error};

const DOCS: &str = "/docs/tools-registry.md";
const TASK_STATUS_OPEN: &str = "open";

/// Serializes registry read-modify-write cycles within this process.
static REGISTRY_WRITE: Mutex<()> = Mutex::new(());

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Decision {
    Approve,
    Reject,
}

#[derive(Debug, Deserialize)]
struct DecisionRequest {
    decision: Decision,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct DecisionReceipt {
    tool_id: String,
    decision: &'static str,
    /// The tool's status after the decision; `None` once rejected (removed).
    status: Option<&'static str>,
    removed_tasks: Vec<String>,
    decided_by: String,
}

/// A receipt plus the rejected tool's origin cluster (not serialized).
struct Applied {
    receipt: DecisionReceipt,
    rejected_cluster: Option<String>,
}

#[derive(Debug)]
enum DecisionError {
    RegistryUnavailable(String),
    NotFound,
    NotProposed(String),
    Io(String),
}

pub(super) async fn decide(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(tool_id): AxumPath<String>,
    body: Bytes,
) -> AxumResponse {
    let request: DecisionRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return decision_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "tool_proposal_invalid_request",
                &error.to_string(),
                &["send JSON {\"decision\": \"approve\" | \"reject\", \"reason\"?: string}"],
            );
        }
    };
    let Some(registry_path) = state.tool_registry_path.as_deref() else {
        return decision_error(
            StatusCode::FAILED_DEPENDENCY,
            "tool_proposal_registry_unavailable",
            "no tools-registry.toml wired (missing --split-manifest)",
            &["start the server with --split-manifest so jeryu-tool's registry resolves"],
        );
    };
    let applied = match apply_decision(registry_path, &tool_id, request.decision, &account.login) {
        Ok(applied) => applied,
        Err(DecisionError::RegistryUnavailable(reason)) => {
            return decision_error(
                StatusCode::FAILED_DEPENDENCY,
                "tool_proposal_registry_unavailable",
                &reason,
                &["verify jeryu-tool/tools-registry.toml exists and parses"],
            );
        }
        Err(DecisionError::NotFound) => {
            return decision_error(
                StatusCode::NOT_FOUND,
                "tool_proposal_not_found",
                "the registry has no tool with that id",
                &["GET /api/v1/tools/registry/summary for current tool ids"],
            );
        }
        Err(DecisionError::NotProposed(status)) => {
            return decision_error(
                StatusCode::CONFLICT,
                "tool_proposal_not_proposed",
                &format!("tool is {status:?}; only proposed tools can be decided"),
                &["refresh the proposals list; someone may have decided it already"],
            );
        }
        Err(DecisionError::Io(reason)) => {
            return decision_error(
                StatusCode::FAILED_DEPENDENCY,
                "tool_proposal_registry_write_failed",
                &reason,
                &["verify the jeryu-tool checkout is writable"],
            );
        }
    };
    if let Some(cluster) = &applied.rejected_cluster {
        let reason = request
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .unwrap_or("proposal rejected");
        // Best effort: the registry decision already stands; a failed ignore
        // only means the finder may resurface the cluster.
        let _ = state
            .codegraph_store
            .ignore_tool_build_cluster(cluster, reason, &account.login);
    }
    Json(applied.receipt).into_response()
}

fn apply_decision(
    registry_path: &Path,
    tool_id: &str,
    decision: Decision,
    decided_by: &str,
) -> Result<Applied, DecisionError> {
    let _guard = REGISTRY_WRITE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let text = std::fs::read_to_string(registry_path)
        .map_err(|error| DecisionError::RegistryUnavailable(error.to_string()))?;
    let mut doc: DocumentMut = text.parse().map_err(|error: toml_edit::TomlError| {
        DecisionError::RegistryUnavailable(error.to_string())
    })?;
    let tools = doc
        .get_mut("tool")
        .and_then(|item| item.as_array_of_tables_mut())
        .ok_or(DecisionError::NotFound)?;
    let index = tools
        .iter()
        .position(|table| table.get("id").and_then(|id| id.as_str()) == Some(tool_id))
        .ok_or(DecisionError::NotFound)?;
    let table = tools.get_mut(index).expect("position is in range");
    let status = table
        .get("status")
        .and_then(|status| status.as_str())
        .unwrap_or_default()
        .to_string();
    if status != STATUS_PROPOSED {
        return Err(DecisionError::NotProposed(status));
    }

    let applied = match decision {
        Decision::Approve => {
            table["status"] = value(STATUS_BUILDING);
            Applied {
                receipt: receipt(
                    tool_id,
                    "approve",
                    Some(STATUS_BUILDING),
                    Vec::new(),
                    decided_by,
                ),
                rejected_cluster: None,
            }
        }
        Decision::Reject => {
            let cluster = table
                .get("origin_cluster")
                .and_then(|cluster| cluster.as_str())
                .filter(|cluster| !cluster.is_empty())
                .map(str::to_string);
            tools.remove(index);
            let removed = remove_open_tasks(registry_path, tool_id)?;
            Applied {
                receipt: receipt(tool_id, "reject", None, removed, decided_by),
                rejected_cluster: cluster,
            }
        }
    };
    std::fs::write(registry_path, doc.to_string())
        .map_err(|error| DecisionError::Io(error.to_string()))?;
    Ok(applied)
}

fn receipt(
    tool_id: &str,
    decision: &'static str,
    status: Option<&'static str>,
    removed_tasks: Vec<String>,
    decided_by: &str,
) -> DecisionReceipt {
    DecisionReceipt {
        tool_id: tool_id.to_string(),
        decision,
        status,
        removed_tasks,
        decided_by: decided_by.to_string(),
    }
}

/// Delete `tasks/*.toml` files for `tool_id` that were never started.
fn remove_open_tasks(registry_path: &Path, tool_id: &str) -> Result<Vec<String>, DecisionError> {
    let Some(tasks_dir) = registry_path.parent().map(|dir| dir.join("tasks")) else {
        return Ok(Vec::new());
    };
    let Ok(entries) = std::fs::read_dir(&tasks_dir) else {
        return Ok(Vec::new());
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let Ok(task) = std::fs::read_to_string(&path)
            .map_err(drop)
            .and_then(|text| text.parse::<DocumentMut>().map_err(drop))
        else {
            continue;
        };
        let field = |key: &str| task.get(key).and_then(|item| item.as_str());
        if field("tool_id") == Some(tool_id) && field("status") == Some(TASK_STATUS_OPEN) {
            std::fs::remove_file(&path).map_err(|error| DecisionError::Io(error.to_string()))?;
            removed.push(field("id").unwrap_or_default().to_string());
        }
    }
    removed.sort();
    Ok(removed)
}

fn decision_error(
    status: StatusCode,
    code: &'static str,
    reason: &str,
    common_fixes: &'static [&'static str],
) -> AxumResponse {
    typed_error(TypedError {
        status,
        code,
        purpose: "decide a proposed shared tool",
        reason,
        common_fixes,
        docs_url: DOCS,
        repair_hint: "fix the cause, then retry the decision",
        message: reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = r#"schema_version = "1"

# A hand-written published tool; its comment must survive edits.
[[tool]]
id = "kept"
name = "Kept"
kind = "rust-crate"
status = "published"
source = "x"

# Proposed by jeryu-tool-finder from cluster c-1.
[[tool]]
id = "prop"
name = "Prop"
kind = "rust-crate"
status = "proposed"
origin_cluster = "c-1"
"#;

    fn fixture(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jeryu-tool-proposals-{tag}-{}-{}",
            std::process::id(),
            jeryu_runner_core::receipt::now_ms()
        ));
        std::fs::create_dir_all(dir.join("tasks")).unwrap();
        std::fs::write(dir.join("tools-registry.toml"), REGISTRY).unwrap();
        std::fs::write(
            dir.join("tasks/0001-prop.toml"),
            "id = \"0001\"\ntool_id = \"prop\"\nstatus = \"open\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("tasks/0002-kept.toml"),
            "id = \"0002\"\ntool_id = \"kept\"\nstatus = \"open\"\n",
        )
        .unwrap();
        dir.join("tools-registry.toml")
    }

    #[test]
    fn approve_moves_proposed_to_building_and_keeps_the_rest() {
        let path = fixture("approve");
        let applied = apply_decision(&path, "prop", Decision::Approve, "alton").unwrap();
        assert_eq!(applied.receipt.status, Some("building"));
        assert!(applied.rejected_cluster.is_none());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("status = \"building\""));
        assert!(!text.contains("status = \"proposed\""));
        assert!(text.contains("# A hand-written published tool; its comment must survive edits."));
        assert!(path.parent().unwrap().join("tasks/0001-prop.toml").exists());
    }

    #[test]
    fn reject_removes_the_entry_and_its_open_task_only() {
        let path = fixture("reject");
        let applied = apply_decision(&path, "prop", Decision::Reject, "alton").unwrap();
        assert_eq!(applied.receipt.status, None);
        assert_eq!(applied.receipt.removed_tasks, vec!["0001".to_string()]);
        assert_eq!(applied.rejected_cluster.as_deref(), Some("c-1"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("id = \"prop\""));
        assert!(!text.contains("cluster c-1"));
        assert!(text.contains("id = \"kept\""));
        let tasks = path.parent().unwrap().join("tasks");
        assert!(!tasks.join("0001-prop.toml").exists());
        assert!(tasks.join("0002-kept.toml").exists());
    }

    #[test]
    fn only_proposed_tools_can_be_decided() {
        let path = fixture("guard");
        assert!(matches!(
            apply_decision(&path, "kept", Decision::Approve, "alton"),
            Err(DecisionError::NotProposed(status)) if status == "published"
        ));
        assert!(matches!(
            apply_decision(&path, "nope", Decision::Reject, "alton"),
            Err(DecisionError::NotFound)
        ));
    }
}
