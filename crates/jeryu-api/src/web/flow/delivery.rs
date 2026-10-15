//! Correlate releases by immutable Git ancestry, never by merge timestamps.
use super::project::EventView;
use crate::web::WebState;
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::BTreeSet;
use std::process::Command;

pub(crate) fn valid_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}
#[derive(Clone)]
pub(crate) struct Delivery {
    pub seq: i64,
    pub ts: i64,
    pub kind: String,
    pub outcome: Option<String>,
    pub family: Option<String>,
    pub repo: String,
    pub sha: String,
    pub detail: Option<Value>,
}
#[derive(Clone)]
pub(crate) struct Revision {
    pub repo: String,
    pub pr: i64,
    pub sha: String,
    pub family: Option<String>,
}

pub(crate) fn pending(conn: &Connection) -> Result<Vec<Delivery>, String> {
    let mut stmt=conn.prepare("SELECT seq,ts_ms,kind,outcome,family,repo,sha,detail_json FROM pipeline_events
        WHERE (kind IN ('release.staged','deploy.created') OR (kind='deploy.status' AND outcome IN ('success','failure','error')))
        AND (kind='release.staged' OR json_extract(detail_json,'$.environment')='production')
        AND repo IS NOT NULL AND length(sha)=40 AND seq NOT IN (SELECT event_seq FROM flow_delivery_reads)
        ORDER BY seq LIMIT 128").map_err(|e|e.to_string())?;
    stmt.query_map([], |r| {
        let detail: Option<String> = r.get(7)?;
        Ok(Delivery {
            seq: r.get(0)?,
            ts: r.get(1)?,
            kind: r.get(2)?,
            outcome: r.get(3)?,
            family: r.get(4)?,
            repo: r.get(5)?,
            sha: r.get(6)?,
            detail: detail.and_then(|s| serde_json::from_str(&s).ok()),
        })
    })
    .map_err(|e| e.to_string())?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| e.to_string())
}
pub(crate) fn revisions(conn: &Connection) -> Result<Vec<Revision>, String> {
    let mut stmt = conn
        .prepare("SELECT repo,pr,sha,family FROM flow_merge_revisions")
        .map_err(|e| e.to_string())?;
    stmt.query_map([], |r| {
        Ok(Revision {
            repo: r.get(0)?,
            pr: r.get(1)?,
            sha: r.get(2)?,
            family: r.get(3)?,
        })
    })
    .map_err(|e| e.to_string())?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| e.to_string())
}
fn git(state: &WebState, repo: &str, args: &[&str]) -> Option<String> {
    let (owner, name) = repo.split_once('/')?;
    let resolved = state.repo_manager.open_parts(owner, name).ok()?;
    let output = Command::new(&state.repo_manager.config().git_bin)
        .arg("-C")
        .arg(resolved.path)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}
fn contains(state: &WebState, repo: &str, sha: &str) -> Option<BTreeSet<String>> {
    if !valid_sha(sha) {
        return None;
    }
    // Bounded walk. Revisions beyond this bound stay uncorrelated, never guessed.
    git(
        state,
        repo,
        &["rev-list", "--max-count=100000", "--end-of-options", sha],
    )
    .map(|s| s.lines().map(str::to_string).collect())
}
fn shipped(state: &WebState, event: &Delivery) -> Vec<(String, String)> {
    let mut tips = vec![(event.repo.clone(), event.sha.clone())];
    if !event.repo.ends_with("/jeryu-deploy") {
        return tips;
    }
    // The web bundle is built from this lock at the exact deployed revision.
    // Other lock entries are not assumed to be release artifacts.
    if let Some(lock) = git(
        state,
        &event.repo,
        &["show", &format!("{}:jeryu-split.lock.toml", event.sha)],
    )
    .and_then(|s| s.parse::<toml::Value>().ok())
    {
        for row in lock
            .get("repo")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
        {
            if row.get("name").and_then(toml::Value::as_str) == Some("jeryu-web")
                && let Some(sha) = row
                    .get("commit")
                    .and_then(toml::Value::as_str)
                    .filter(|s| valid_sha(s))
                && let Some((owner, _)) = event.repo.split_once('/')
            {
                tips.push((format!("{owner}/jeryu-web"), sha.to_string()));
            }
        }
    }
    tips
}
/// Runs outside the event writer mutex. Successful reads are cached durably.
pub(crate) fn reconcile(state: &WebState) -> Result<(), String> {
    let (events, revisions) = state.events.flow_delivery_inputs()?;
    for event in events {
        let mut matches = Vec::new();
        let mut readable = true;
        for (repo, sha) in shipped(state, &event) {
            let Some(ancestors) = contains(state, &repo, &sha) else {
                readable = false;
                continue;
            };
            for revision in &revisions {
                if revision.repo == repo
                    && ancestors.contains(&revision.sha)
                    && (event.family.is_none() || event.family == revision.family)
                {
                    matches.push(revision.clone());
                }
            }
        }
        if readable {
            state.events.flow_apply_delivery(&event, &matches)?;
        }
    }
    Ok(())
}
pub(crate) fn apply(
    conn: &Connection,
    event: &Delivery,
    revisions: &[Revision],
) -> Result<(), String> {
    let detail = event.detail.as_ref();
    for revision in revisions {
        let view = EventView {
            seq: event.seq,
            reporter: "flow-delivery",
            event_id: None,
            ts_ms: event.ts,
            kind: &event.kind,
            outcome: event.outcome.as_deref(),
            family: revision.family.as_deref(),
            repo: Some(&revision.repo),
            pr: Some(revision.pr),
            todo_id: None,
            shift: None,
            sha: Some(&revision.sha),
            summary: Some("Release membership verified against immutable Git revision"),
            detail,
        };
        super::project::deliver(conn, &view, &event.repo, &event.sha)?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO flow_delivery_reads VALUES (?1)",
        params![event.seq],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::shift::tests::run_git;
    #[test]
    fn immutable_tip_excludes_a_later_merge_and_a_divergent_branch() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        run_git(&work, &["init", "-q", "-b", "main"]);
        let commit = |name: &str| {
            std::fs::write(work.join(name), name).unwrap();
            run_git(&work, &["add", "."]);
            run_git(&work, &["commit", "-qm", name]);
            run_git(&work, &["rev-parse", "HEAD"])
        };
        let base = commit("base");
        let released = commit("released");
        let later = commit("later");
        run_git(&work, &["checkout", "-qb", "other", &base]);
        let divergent = commit("divergent");
        let owner = root.path().join("acme");
        std::fs::create_dir_all(&owner).unwrap();
        run_git(
            &work,
            &[
                "clone",
                "-q",
                "--bare",
                ".",
                owner.join("widget.git").to_str().unwrap(),
            ],
        );
        let state =
            WebState::new_with_git_storage(jeryu_core::ForgeCore::new(), root.path().to_path_buf());
        let members = contains(&state, "acme/widget", &released).unwrap();
        assert!(members.contains(&base));
        assert!(members.contains(&released));
        assert!(!members.contains(&later));
        assert!(!members.contains(&divergent));
        assert!(contains(&state, "acme/widget", "--all").is_none());
        assert!(contains(&state, "other/widget", &released).is_none());
    }
}
