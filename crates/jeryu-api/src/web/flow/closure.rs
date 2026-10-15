//! Current lifecycle overlay. Never rewrite event history or infer delivery from closure.
use super::report::Visit;
use crate::web::{WebState, shift};
use jeryu_core::{PullRequest, PullRequestState};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
pub(crate) struct OpenVisit {
    pub lot: String,
    pub station: String,
    pub entered: i64,
    pub repo: Option<String>,
    pub pr: Option<u64>,
    pub family: Option<String>,
    pub started: bool,
    pub held: bool,
}
impl OpenVisit {
    fn key(&self) -> (String, String, i64) {
        (self.lot.clone(), self.station.clone(), self.entered)
    }
}
#[derive(Clone, Serialize)]
pub(crate) struct Replacement {
    pub href: String,
    pub label: String,
    pub state: &'static str,
}
#[derive(Clone, Serialize)]
pub(crate) struct Closure {
    pub lot_id: String,
    pub repo: Option<String>,
    pub station: String,
    pub href: String,
    pub title: String,
    pub kind: &'static str,
    pub disposition: &'static str,
    pub explanation: String,
    pub replacement: Option<Replacement>,
    pub needs_attention: bool,
}
pub(crate) struct Snapshot {
    pub excluded: BTreeSet<(String, String, i64)>,
    pub items: Vec<Closure>,
    pub repositories: serde_json::Value,
    pub problems: Vec<String>,
}
fn field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let mut value = None;
    let mut fence = None;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let mark = &trimmed[..3];
            if fence == Some(mark) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(mark);
            }
            continue;
        }
        if fence.is_some() || line.starts_with("    ") || line.starts_with('\t') {
            continue;
        }
        if let Some((_, v)) = line
            .split_once(':')
            .filter(|(name, _)| name.trim().eq_ignore_ascii_case(key))
        {
            if value.is_some() || v.trim().is_empty() {
                return None;
            }
            value = Some(v.trim());
        }
    }
    value
}
/// Only an explicit, whole-line relation. Similar titles/branches are not evidence.
fn target(body: &str, repo: &str) -> Option<(String, u64)> {
    let value = field(body, "Superseded-by")?;
    let (name, number) = value.rsplit_once('#')?;
    let name = if name.is_empty() { repo } else { name };
    if name.split('/').count() != 2
        || name.split('/').any(|s| {
            s.is_empty()
                || !s
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    {
        return None;
    }
    let number = number.parse::<u64>().ok().filter(|n| *n > 0)?;
    Some((name.into(), number))
}
fn replacement(
    body: &str,
    repo: &str,
    pulls: &BTreeMap<(String, u64), PullRequest>,
) -> Option<Replacement> {
    let (repo, number) = target(body, repo)?;
    let pr = pulls.get(&(repo.clone(), number));
    Some(Replacement {
        href: format!("/{repo}/pull/{number}"),
        label: format!("{repo}#{number}"),
        state: match pr {
            Some(p) if p.merged => "merged",
            Some(p) if p.state == PullRequestState::Closed => "closed",
            Some(_) => "open",
            None => "unknown",
        },
    })
}
fn disposition(body: &str, replacement: Option<&Replacement>) -> (&'static str, bool) {
    if let Some(r) = replacement {
        return (
            "replacement_recorded",
            !matches!(r.state, "open" | "merged"),
        );
    }
    if field(body, "Flow-disposition").is_some_and(|v| v.eq_ignore_ascii_case("abandoned"))
        && field(body, "Flow-reason").is_some()
    {
        return ("abandoned_recorded", false);
    }
    ("needs_disposition", true)
}
pub(crate) fn snapshot(
    state: &WebState,
    open: &[OpenVisit],
    repo_filter: Option<&str>,
) -> Snapshot {
    let mut problems = Vec::new();
    let mut pulls = BTreeMap::new();
    // Local forge reads, once per repository; no per-row network or Git ancestry work.
    let repos: BTreeSet<_> = open.iter().filter_map(|v| v.repo.as_deref()).collect();
    for repo in repos {
        let Some((owner, name)) = repo.split_once('/') else {
            continue;
        };
        match state.core.list_pull_requests(owner, name, None) {
            Ok(rows) => {
                for pr in rows {
                    pulls.insert((repo.to_string(), pr.number), pr);
                }
            }
            Err(_) => problems.push(format!("PR lifecycle unavailable for {repo}")),
        }
    }
    let families: BTreeSet<_> = open
        .iter()
        .filter(|v| v.pr.is_none())
        .filter_map(|v| v.family.as_deref())
        .collect();
    let mut todos = BTreeMap::new();
    for queue in shift::queue::discover(&state.repo_manager) {
        if !families.contains(queue.family.name.as_str()) {
            continue;
        }
        match shift::queue_todos(state, &queue) {
            Ok(rows) => {
                for row in rows {
                    let todo = row.todo.to_api(chrono::Utc::now());
                    todos.insert((todo.family.clone(), todo.id.clone()), todo);
                }
            }
            Err(_) => problems.push(format!(
                "Todo lifecycle unavailable for {}",
                queue.family.name
            )),
        }
    }
    let mut result = Snapshot {
        excluded: BTreeSet::new(),
        items: Vec::new(),
        repositories: serde_json::json!([]),
        problems,
    };
    let mut counts: BTreeMap<(String, String), [u64; 3]> = BTreeMap::new();
    let mut missing = BTreeSet::new();
    for v in open {
        let pr = v
            .repo
            .as_ref()
            .zip(v.pr)
            .and_then(|(repo, n)| pulls.get(&(repo.clone(), n)));
        if (v.pr.is_some() && pr.is_none())
            || (v.pr.is_none()
                && v.family
                    .as_ref()
                    .is_none_or(|f| !todos.contains_key(&(f.clone(), v.lot.clone()))))
        {
            missing.insert(v.lot.clone());
        }
        // A merge is authoritative only for this visit's exact repository/PR.
        // Keep release work and visits belonging to another PR of the same todo.
        if pr.is_some_and(|p| p.merged || p.state == PullRequestState::Merged)
            && matches!(
                v.station.as_str(),
                "queue" | "work" | "shift" | "review" | "gate" | "merge"
            )
        {
            result.excluded.insert(v.key());
            continue;
        }
        let closed =
            if let Some(pr) = pr.filter(|p| !p.merged && p.state == PullRequestState::Closed) {
                Some((
                    "pull_request",
                    pr.title.clone(),
                    pr.body.clone().unwrap_or_default(),
                    format!("/{}/{}/pull/{}", pr.owner, pr.repo, pr.number),
                ))
            } else if v.pr.is_none() {
                v.family
                    .as_ref()
                    .and_then(|f| todos.get(&(f.clone(), v.lot.clone())))
                    .filter(|t| t.status == shift::TodoStatus::Closed && !t.merged)
                    .map(|t| {
                        (
                            "todo",
                            t.title.clone(),
                            format!("{}\n{}", t.note, t.body),
                            format!("/work/{}", t.id),
                        )
                    })
            } else {
                None
            };
        if let Some((kind, title, body, href)) = closed {
            result.excluded.insert(v.key());
            if repo_filter.is_none_or(|r| v.repo.as_deref() == Some(r)) {
                let replacement = replacement(&body, v.repo.as_deref().unwrap_or(""), &pulls);
                let (disposition, needs_attention) = disposition(&body, replacement.as_ref());
                result.items.push(Closure {
                    lot_id: v.lot.clone(),
                    repo: v.repo.clone(),
                    station: v.station.clone(),
                    href,
                    title,
                    kind,
                    disposition,
                    explanation: body.chars().take(2000).collect(),
                    replacement,
                    needs_attention,
                });
            }
        } else if let Some(repo) = &v.repo {
            let row = counts.entry((repo.clone(), v.station.clone())).or_default();
            row[if v.held {
                2
            } else if v.started {
                1
            } else {
                0
            }] += 1;
        }
    }
    if !missing.is_empty() {
        result.problems.push(format!(
            "{} recorded items have no readable current lifecycle record",
            missing.len()
        ));
    }
    result.repositories = serde_json::json!(counts.into_iter().map(|((repo,station),[waiting,active,held])| serde_json::json!({"repo":repo,"station":station,"waiting":waiting,"active":active,"held":held})).collect::<Vec<_>>());
    result
}
impl Snapshot {
    pub fn filter(&self, visits: &mut Vec<Visit>) {
        visits.retain(|v| {
            v.left_ms.is_some()
                || !self
                    .excluded
                    .contains(&(v.lot_id.clone(), v.station.id().into(), v.entered_ms))
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disposition_needs_explicit_evidence_and_valid_links() {
        assert_eq!(
            disposition("closed because probably superseded", None),
            ("needs_disposition", true)
        );
        assert_eq!(
            disposition("Flow-disposition: abandoned", None),
            ("needs_disposition", true)
        );
        assert_eq!(
            disposition(
                "Flow-disposition: abandoned\nFlow-reason: owner cancelled",
                None
            ),
            ("abandoned_recorded", false)
        );
        assert_eq!(
            target("Superseded-by: #12", "a/b"),
            Some(("a/b".into(), 12))
        );
        assert!(target("Superseded-by: https://bad/#12", "a/b").is_none());
        assert!(target("not superseded by #12", "a/b").is_none());
        assert!(target("```text\nSuperseded-by: #12\n```", "a/b").is_none());
        assert!(target("Superseded-by: #12\nSuperseded-by: #13", "a/b").is_none());

        let r = Replacement {
            href: "/a/b/pull/2".into(),
            label: "a/b#2".into(),
            state: "closed",
        };
        assert_eq!(disposition("", Some(&r)), ("replacement_recorded", true));
    }
    #[test]
    fn current_closure_changes_wip_without_changing_history_and_reopen_restores_it() {
        use crate::web::pipeline::NewEvent;
        use jeryu_core::{
            CreatePullRequestRequest, CreateRepositoryRequest, ForgeCore, UpdatePullRequestRequest,
        };
        let root = tempfile::tempdir().unwrap();
        let core = ForgeCore::new();
        core.create_repository(
            "acme",
            CreateRepositoryRequest {
                name: "widget".into(),
                private: false,
                description: None,
                default_branch: Some("main".into()),
            },
        )
        .unwrap();
        let pr = core
            .create_pull_request(
                "acme",
                "widget",
                "author",
                CreatePullRequestRequest {
                    title: "work".into(),
                    head: "feature".into(),
                    base: "main".into(),
                    head_sha: Some("a".repeat(40)),
                    ..Default::default()
                },
            )
            .unwrap();
        let state = WebState::new_with_git_storage(core.clone(), root.path().to_path_buf());
        state
            .events
            .insert(
                "fixture",
                &NewEvent {
                    source: "fixture".into(),
                    kind: "pr.opened".into(),
                    summary: "opened".into(),
                    family: Some("acme".into()),
                    repo: Some("acme/widget".into()),
                    pr: Some(pr.number as i64),
                    ..Default::default()
                },
                1000,
            )
            .unwrap();
        let open = state.events.flow_open_lifecycle(Some("acme")).unwrap();
        assert!(!open.is_empty());
        let original = state.events.flow_visits(Some("acme"), 0).unwrap();
        assert!(snapshot(&state, &open, None).items.is_empty());
        core.update_pull_request(
            "acme",
            "widget",
            pr.number,
            UpdatePullRequestRequest {
                state: Some(PullRequestState::Closed),
                ..Default::default()
            },
        )
        .unwrap();
        let closed = snapshot(&state, &open, None);
        assert_eq!(closed.items.len(), open.len());
        assert!(closed.items.iter().all(|i| i.needs_attention));
        let mut visible = original.clone();
        closed.filter(&mut visible);
        assert!(visible.iter().all(|v| v.left_ms.is_some()));
        assert_eq!(state.events.flow_visits(Some("acme"), 0).unwrap(), original);
        assert!(closed.repositories.as_array().unwrap().is_empty());
        core.update_pull_request(
            "acme",
            "widget",
            pr.number,
            UpdatePullRequestRequest {
                state: Some(PullRequestState::Open),
                ..Default::default()
            },
        )
        .unwrap();
        let reopened = snapshot(&state, &open, None);
        assert!(reopened.items.is_empty());
        let mut visible = original.clone();
        reopened.filter(&mut visible);
        assert_eq!(visible, original);
        core.update_pull_request(
            "acme",
            "widget",
            pr.number,
            UpdatePullRequestRequest {
                state: Some(PullRequestState::Merged),
                ..Default::default()
            },
        )
        .unwrap();
        let mut merged_visits = open.clone();
        let mut release = open[0].clone();
        release.station = "stage".into();
        merged_visits.push(release);
        let merged = snapshot(&state, &merged_visits, None);
        assert!(merged.items.is_empty(), "merged is not unresolved closure");
        assert_eq!(merged.excluded.len(), open.len());
        assert_eq!(merged.repositories[0]["station"], "stage");
        assert_eq!(merged.repositories[0]["waiting"], 1);
        let later = core
            .create_pull_request(
                "acme",
                "widget",
                "author",
                CreatePullRequestRequest {
                    title: "follow-up".into(),
                    head: "later".into(),
                    base: "main".into(),
                    head_sha: Some("b".repeat(40)),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut followup = open[0].clone();
        followup.pr = Some(later.number);
        followup.entered += 10;
        followup.held = true;
        merged_visits.push(followup.clone());
        let mixed = snapshot(&state, &merged_visits, None);
        assert!(
            !mixed.excluded.contains(&followup.key()),
            "merging an older pull cannot hide a later hold sharing its todo"
        );
    }
}
