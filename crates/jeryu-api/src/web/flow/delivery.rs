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

pub(crate) fn pending(conn: &Connection, now: i64) -> Result<Vec<Delivery>, String> {
    let mut stmt=conn.prepare("SELECT seq,ts_ms,kind,outcome,family,repo,sha,detail_json FROM pipeline_events
        WHERE (kind IN ('release.staged','deploy.created') OR (kind='deploy.status' AND outcome IN ('success','failure','error')))
        AND (kind='release.staged' OR json_extract(detail_json,'$.environment')='production')
        AND repo IS NOT NULL AND length(sha)=40 AND seq NOT IN (SELECT event_seq FROM flow_delivery_reads WHERE retry_after_ms=0 OR retry_after_ms>?1)
        ORDER BY seq LIMIT 32").map_err(|e|e.to_string())?;
    stmt.query_map(params![now], |r| {
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
fn shipped(state: &WebState, event: &Delivery) -> Option<Vec<(String, String)>> {
    if !valid_sha(&event.sha) {
        return None;
    }
    let mut tips = vec![(event.repo.clone(), event.sha.clone())];
    if !event.repo.ends_with("/jeryu-deploy") {
        return Some(tips);
    }
    // The web bundle is built from this lock at the exact deployed revision.
    // Other lock entries are not assumed to be release artifacts.
    // An existing revision with no lock predates split packaging. It still
    // carries its own deploy changes; an inaccessible revision remains unknown.
    let paths = git(
        state,
        &event.repo,
        &[
            "ls-tree",
            "--name-only",
            "--end-of-options",
            &event.sha,
            "--",
            "jeryu-split.lock.toml",
        ],
    )?;
    if paths.trim().is_empty() {
        return Some(tips);
    }
    let document = git(
        state,
        &event.repo,
        &[
            "show",
            "--end-of-options",
            &format!("{}:jeryu-split.lock.toml", event.sha),
        ],
    )?;
    // Value::from_str parses one TOML value, not a complete document in toml 1.
    let lock: toml::Value = toml::from_str(&document).ok()?;
    let Some(web) = lock
        .get("repo")
        .and_then(toml::Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.get("name").and_then(toml::Value::as_str) == Some("jeryu-web"))
        })
    else {
        return Some(tips);
    };
    let sha = web.get("commit")?.as_str().filter(|s| valid_sha(s))?;
    let (owner, _) = event.repo.split_once('/')?;
    tips.push((format!("{owner}/jeryu-web"), sha.to_string()));
    Some(tips)
}
/// Runs outside the event writer mutex. Successful reads are cached durably.
pub(crate) fn reconcile(state: &WebState) -> Result<(), String> {
    let Some(_guard) = state.events.flow_delivery_guard() else {
        return Ok(());
    };
    let now = chrono::Utc::now().timestamp_millis();
    let (events, revisions) = state.events.flow_delivery_inputs(now)?;
    for event in events {
        let mut matches = Vec::new();
        let mut readable = true;
        let Some(tips) = shipped(state, &event) else {
            state.events.flow_defer_delivery(event.seq, now + 300_000)?;
            continue;
        };
        for (repo, sha) in tips {
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
        if readable && !matches.is_empty() {
            state.events.flow_apply_delivery(&event, &matches)?;
        } else {
            state.events.flow_defer_delivery(event.seq, now + 300_000)?;
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
            actor: None,
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
        "INSERT OR REPLACE INTO flow_delivery_reads(event_seq,retry_after_ms) VALUES (?1,0)",
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
    fn lock_lookup_rejects_option_shaped_and_non_hex_revisions() {
        let root = tempfile::tempdir().unwrap();
        let state =
            WebState::new_with_git_storage(jeryu_core::ForgeCore::new(), root.path().to_path_buf());
        for sha in [format!("--output={:0<31}", "owned"), "z".repeat(40)] {
            assert_eq!(sha.len(), 40);
            let event = Delivery {
                seq: 1,
                ts: 1,
                kind: "release.staged".into(),
                outcome: None,
                family: None,
                repo: "acme/jeryu-deploy".into(),
                sha,
                detail: None,
            };
            assert!(shipped(&state, &event).is_none());
        }
    }
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
    #[test]
    fn real_split_lock_reconciles_pinned_web_and_defers_an_invalid_document() {
        use crate::web::pipeline::NewEvent;
        for case in ["valid", "invalid", "no_pin", "absent"] {
            let root = tempfile::tempdir().unwrap();
            let owner = root.path().join("acme");
            std::fs::create_dir_all(&owner).unwrap();
            let make_repo = |name: &str, contents: &str, file: &str| {
                let work = root.path().join(format!("work-{name}"));
                std::fs::create_dir_all(&work).unwrap();
                run_git(&work, &["init", "-q", "-b", "main"]);
                std::fs::write(work.join(file), contents).unwrap();
                run_git(&work, &["add", "."]);
                run_git(&work, &["commit", "-qm", "fixture"]);
                let sha = run_git(&work, &["rev-parse", "HEAD"]);
                run_git(
                    &work,
                    &[
                        "clone",
                        "-q",
                        "--bare",
                        ".",
                        owner.join(format!("{name}.git")).to_str().unwrap(),
                    ],
                );
                sha
            };
            let web_sha = make_repo("jeryu-web", "shipped", "README");
            let lock = format!(
                "schema = 1\n[[repo]]\nname = 'jeryu-web'\ncommit = '{web_sha}'\n[[repo]]\nname = 'other'\ncommit = '{}'\n",
                "b".repeat(40)
            );
            let document = if case == "valid" {
                format!("# Full split lock document\n{}", lock)
            } else if case == "no_pin" {
                "schema = 1".into()
            } else {
                "not a TOML document".into()
            };
            let deploy_sha = make_repo(
                "jeryu-deploy",
                &document,
                if case == "absent" {
                    "README"
                } else {
                    "jeryu-split.lock.toml"
                },
            );
            let state = WebState::new_with_git_storage(
                jeryu_core::ForgeCore::new(),
                root.path().to_path_buf(),
            );
            let now = chrono::Utc::now().timestamp_millis();
            let post = |repo: &str, pr, sha: &str, kind: &str, outcome: Option<&str>, offset| {
                state.events.insert("operator", &NewEvent {
                    source: "fixture".into(), kind: kind.into(), summary: kind.into(),
                    family: Some("acme".into()), repo: Some(repo.into()), pr,
                    sha: Some(sha.into()), outcome: outcome.map(str::to_string),
                    detail: Some(serde_json::json!({"environment":"production","release":"fixture-release"})),
                    ..Default::default()
                }, now + offset).unwrap();
            };
            post(
                "acme/jeryu-web",
                Some(1),
                &web_sha,
                "pr.opened",
                None,
                -60_000,
            );
            post(
                "acme/jeryu-web",
                Some(1),
                &web_sha,
                "pr.merged",
                None,
                -50_000,
            );
            post(
                "acme/jeryu-deploy",
                Some(2),
                &deploy_sha,
                "pr.merged",
                None,
                -40_000,
            );
            post(
                "acme/jeryu-deploy",
                None,
                &deploy_sha,
                "release.staged",
                None,
                -30_000,
            );
            post(
                "acme/jeryu-deploy",
                None,
                &deploy_sha,
                "deploy.created",
                None,
                -20_000,
            );
            post(
                "acme/jeryu-deploy",
                None,
                &deploy_sha,
                "deploy.status",
                Some("success"),
                -10_000,
            );
            reconcile(&state).unwrap();
            let query = super::super::history::Query {
                family: Some("acme".into()),
                lot: Some("acme/jeryu-web#1".into()),
                days: Some(7),
                ..Default::default()
            };
            let history = state.events.flow_history(&query, now).unwrap();
            if case == "valid" {
                assert_eq!(history.deliveries.len(), 3);
                assert!(
                    history
                        .deliveries
                        .iter()
                        .any(|d| d["kind"] == "deploy.status" && d["revision"] == deploy_sha)
                );
                assert!(
                    state
                        .events
                        .flow_visits(Some("acme"), 0)
                        .unwrap()
                        .iter()
                        .filter(|v| matches!(
                            v.station,
                            super::super::Station::Stage | super::super::Station::Deploy
                        ))
                        .all(|v| v.left_ms.is_some())
                );
                assert!(
                    state
                        .events
                        .flow_delivery_inputs(now + 360_000)
                        .unwrap()
                        .0
                        .is_empty()
                );
            } else if case != "invalid" {
                assert!(history.deliveries.is_empty());
                let direct = state
                    .events
                    .flow_history(
                        &super::super::history::Query {
                            lot: Some("acme/jeryu-deploy#2".into()),
                            family: Some("acme".into()),
                            days: Some(7),
                            ..Default::default()
                        },
                        now,
                    )
                    .unwrap();
                assert_eq!(direct.deliveries.len(), 3);
                assert!(
                    state
                        .events
                        .flow_delivery_inputs(now + 360_000)
                        .unwrap()
                        .0
                        .is_empty()
                );
            } else {
                assert!(history.deliveries.is_empty());
                assert!(
                    state.events.flow_delivery_inputs(now).unwrap().0.is_empty(),
                    "invalid document backs off"
                );
                assert_eq!(
                    state
                        .events
                        .flow_delivery_inputs(now + 360_000)
                        .unwrap()
                        .0
                        .len(),
                    3,
                    "partial deploy membership must not consume a release"
                );
            }
        }
    }
}
