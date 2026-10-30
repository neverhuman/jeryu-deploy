//! Bounded, read-only commit ancestry for active branches. Edges come only
//! from Git parent objects; a pull request's recorded base is not an edge.
use super::{WebState, api_error, repositories::find_repo};
use axum::{
    Json,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{Duration as ChronoDuration, Utc};
use serde::Serialize;
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;

const MAX_BRANCHES: usize = 24;
const MAX_COMMITS: usize = 300;

#[derive(Serialize)]
struct Commit {
    sha: String,
    parents: Vec<String>,
    summary: String,
    author: String,
    committed_at: String,
}
#[derive(Serialize)]
struct Branch {
    name: String,
    sha: String,
    updated_at: String,
    pull_number: Option<u64>,
    pull_title: Option<String>,
    draft: bool,
    base_ref: Option<String>,
    relation: String,
}
#[derive(Serialize)]
struct Graph {
    repository: String,
    default_branch: String,
    default_sha: String,
    observed_at: String,
    branches: Vec<Branch>,
    commits: Vec<Commit>,
    truncated: bool,
    missing_parents: Vec<String>,
    shallow: bool,
}

async fn git(bin: &str, path: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = tokio::process::Command::new(bin);
    command.arg("-C").arg(path).args(args).kill_on_drop(true);
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().map_err(|_| "Git read failed".to_string())?;
    let stdout = child.stdout.take().ok_or("Git output unavailable")?;
    let read = async {
        let mut output = Vec::new();
        stdout
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut output)
            .await
            .map_err(|_| "Git read failed".to_string())?;
        if output.len() > 2 * 1024 * 1024 {
            return Err("Git read exceeded its output budget".to_string());
        }
        let status = child
            .wait()
            .await
            .map_err(|_| "Git read failed".to_string())?;
        if !status.success() {
            return Err("Git data could not be read".into());
        }
        Ok(String::from_utf8_lossy(&output).trim().to_string())
    };
    tokio::time::timeout(Duration::from_secs(3), read)
        .await
        .map_err(|_| "Git read timed out".to_string())?
}

pub(crate) async fn get(
    State(state): State<Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match tokio::time::timeout(Duration::from_secs(15), read(&state, &id)).await {
        Ok(Ok(graph)) => Json(graph).into_response(),
        Ok(Err((status, message))) => api_error(status, "lineage_unavailable", &message),
        Err(_) => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "lineage_unavailable",
            "Commit ancestry took too long to read; try again",
        ),
    }
}

async fn read(state: &WebState, id: &str) -> Result<Graph, (StatusCode, String)> {
    let repo =
        find_repo(state, id).ok_or((StatusCode::NOT_FOUND, "Repository not found".into()))?;
    let resolved = state
        .repo_manager
        .resolve_parts(&repo.owner, &repo.name)
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                "Repository has no Git data here".into(),
            )
        })?;
    let bin = &state.repo_manager.config().git_bin;
    let path = &resolved.path;
    let failure = |message| (StatusCode::SERVICE_UNAVAILABLE, message);
    let default_sha = git(
        bin,
        path,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("refs/heads/{}^{{commit}}", repo.default_branch),
        ],
    )
    .await
    .map_err(failure)?;
    let refs = git(
        bin,
        path,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--count=201",
            "--format=%(refname:lstrip=2)%1f%(objectname)%1f%(committerdate:iso-strict)",
            "refs/heads/",
        ],
    )
    .await
    .map_err(failure)?;
    let pulls = state
        .github
        .core()
        .list_pull_requests(
            &repo.owner,
            &repo.name,
            Some(jeryu_core::PullRequestState::Open),
        )
        .map_err(|_| failure("Pull requests could not be read".into()))?;
    let cutoff = Utc::now() - ChronoDuration::days(14);
    let mut candidates = refs
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split('\x1f').collect();
            if fields.len() != 3 || fields[0] == repo.default_branch {
                return None;
            }
            let pull = pulls.iter().find(|p| {
                p.head.ref_name == fields[0]
                    && (p.source_repository.is_empty()
                        || p.source_repository == format!("{}/{}", repo.owner, repo.name))
            });
            let recent =
                chrono::DateTime::parse_from_rfc3339(fields[2]).is_ok_and(|date| date >= cutoff);
            (pull.is_some() || recent).then_some((fields, pull))
        })
        .collect::<Vec<_>>();
    // Open proposed work takes precedence over recently touched unproposed branches.
    candidates.sort_by_key(|(_, pull)| pull.is_none());
    let mut truncated = refs.lines().count() > 200 || candidates.len() > MAX_BRANCHES;
    let mut branches = Vec::new();
    for (fields, pull) in candidates.into_iter().take(MAX_BRANCHES) {
        // Reachability is meaningful only when Git can establish it. A missing
        // parent or a shallow clone stays unknown rather than implying merged.
        let relation = match git(bin, path, &["merge-base", fields[1], &default_sha]).await {
            Ok(base) if base == fields[1] && pull.is_none() => continue,
            Ok(base) if base == fields[1] => "contained",
            Ok(base) if base == default_sha => "ahead",
            Ok(_) => "diverged",
            Err(_) => "unknown",
        };
        branches.push(Branch {
            name: fields[0].into(),
            sha: fields[1].into(),
            updated_at: fields[2].into(),
            pull_number: pull.map(|p| p.number),
            pull_title: pull.map(|p| p.title.clone()),
            draft: pull.is_some_and(|p| p.draft),
            base_ref: pull.map(|p| p.base.ref_name.clone()),
            relation: relation.into(),
        });
    }
    let limit = format!("--max-count={}", MAX_COMMITS + 1);
    let mut args = vec![
        "log",
        "--topo-order",
        &limit,
        "--format=%H%x1f%P%x1f%s%x1f%an%x1f%cI",
        "--end-of-options",
        &default_sha,
    ];
    args.extend(branches.iter().map(|branch| branch.sha.as_str()));
    args.push("--");
    let log = git(bin, path, &args).await.map_err(failure)?;
    let mut commits = parse(&log);
    truncated |= commits.len() > MAX_COMMITS;
    commits.truncate(MAX_COMMITS);
    let loaded: BTreeSet<_> = commits.iter().map(|c| c.sha.as_str()).collect();
    let missing_parents = commits
        .iter()
        .flat_map(|c| c.parents.iter())
        .filter(|p| !loaded.contains(p.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let shallow = git(bin, path, &["rev-parse", "--is-shallow-repository"])
        .await
        .map_err(failure)?
        == "true";
    Ok(Graph {
        repository: format!("{}/{}", repo.owner, repo.name),
        default_branch: repo.default_branch,
        default_sha,
        observed_at: Utc::now().to_rfc3339(),
        branches,
        commits,
        truncated,
        missing_parents,
        shallow,
    })
}

fn parse(log: &str) -> Vec<Commit> {
    log.lines()
        .filter_map(|line| {
            let f: Vec<_> = line.split('\x1f').collect();
            (f.len() == 5).then(|| Commit {
                sha: f[0].into(),
                parents: f[1].split_whitespace().map(str::to_string).collect(),
                summary: f[2].into(),
                author: f[3].into(),
                committed_at: f[4].into(),
            })
        })
        .collect()
}

#[cfg(test)]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parent_objects_preserve_merge_and_root() {
        let commits = parse(
            "merge\x1fleft right\x1fmerge branches\x1fAda\x1f2026-10-01T00:00:00Z\nroot\x1f\x1ffirst\x1fAda\x1f2026-09-01T00:00:00Z",
        );
        assert_eq!(commits[0].parents, ["left", "right"]);
        assert!(commits[1].parents.is_empty());
        assert!(parse("incomplete").is_empty());
    }
}
