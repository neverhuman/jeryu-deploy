//! Current lifecycle overlay. Never rewrite event history or infer delivery from closure.
use super::identity;
use super::report::Visit;
use crate::web::family::canonical;
use crate::web::{WebState, shift};
use jeryu_core::{PullRequest, PullRequestState};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// The ledger names a todo for the lot (not a bare pull request).
    pub todo: bool,
    /// A landing entered the lot at Stage after this visit entered.
    pub landed: bool,
    /// For a Work visit: the station whose rework arc sent the lot there.
    pub back_from: Option<String>,
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
    /// Open visits the todo's recorded state moves ([`super::work`]); a
    /// visit that leaves is in `excluded` instead.
    pub readings: BTreeMap<(String, String, i64), super::work::Reading>,
    /// What [`super::work`] changed, scoped to the repository filter.
    pub work: super::work::Reconciled,
    pub items: Vec<Closure>,
    pub repositories: serde_json::Value,
    pub problems: Vec<String>,
    pub total_records: usize,
    pub merged_upstream_records: usize,
}
const MAX_CLOSURES: usize = 1000;
#[derive(Default)]
struct TodoFacts {
    todos: BTreeMap<(String, String), shift::ShiftTodo>,
    problems: Vec<String>,
}
struct CachedTodos {
    read: Instant,
    families: BTreeSet<String>,
    facts: Arc<TodoFacts>,
}
/// Shared by cloned event stores. Only filesystem queue reads are cached;
/// current pull states and current ledger rows are read on every request.
#[derive(Default)]
pub(crate) struct Cache(Mutex<Vec<CachedTodos>>);
impl Cache {
    fn get(&self, state: &WebState, families: &BTreeSet<String>) -> Arc<TodoFacts> {
        if families.is_empty() {
            return Arc::new(TodoFacts::default());
        }
        let mut cache = self.0.lock().unwrap_or_else(|poisoned| {
            let mut cache = poisoned.into_inner();
            cache.clear();
            self.0.clear_poison();
            cache
        });
        if let Some(cached) = cache
            .iter()
            .find(|c| c.families == *families && c.read.elapsed() < Duration::from_secs(30))
        {
            return cached.facts.clone();
        }
        let mut facts = TodoFacts::default();
        for queue in shift::queue::discover(&state.repo_manager) {
            if !families.contains(&canonical(&queue.family.name)) {
                continue;
            }
            match shift::queue_todos(state, &queue) {
                Ok(rows) => {
                    for row in rows {
                        let todo = row.todo.to_api(chrono::Utc::now());
                        facts
                            .todos
                            .insert((todo.family.clone(), todo.id.clone()), todo);
                    }
                }
                Err(_) => facts.problems.push(format!(
                    "Todo lifecycle unavailable for {}",
                    queue.family.name
                )),
            }
        }
        let facts = Arc::new(facts);
        cache.retain(|c| c.read.elapsed() < Duration::from_secs(30));
        if cache.len() >= 8 {
            cache.remove(0);
        }
        cache.push(CachedTodos {
            read: Instant::now(),
            families: families.clone(),
            facts: facts.clone(),
        });
        facts
    }
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
                || matches!(s, "." | "..")
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
/// The pull request that replaced this one, if the forge recorded a
/// replacement chain or the body names one. `recorded` is the slug the ledger
/// kept, `current` the slug the repository is hosted under now; chain links
/// are slug-keyed and not retargeted when a repository moves, so a chain
/// recorded before the move is still read under the recorded slug.
fn replacement(
    state: &WebState,
    body: &str,
    recorded: &str,
    current: &str,
    pr_number: Option<u64>,
    pulls: &BTreeMap<(String, u64), PullRequest>,
) -> Option<Replacement> {
    let (repo, number) = pr_number
        .and_then(|number| {
            let links = state.github.pull_replacements();
            let end = [current, recorded].into_iter().find_map(|slug| {
                let (owner, name) = slug.split_once('/')?;
                links.chain_end(owner, name, number)
            })?;
            Some((current.to_string(), end))
        })
        .or_else(|| {
            let (named, number) = target(body, current)?;
            // The body may name a slug that has since moved as well.
            let repo = identity::resolve(state, &named)
                .map(|(owner, name)| format!("{owner}/{name}"))
                .unwrap_or(named);
            Some((repo, number))
        })?;
    let pr = pulls.get(&(repo.clone(), number));
    Some(Replacement {
        href: format!(
            "/repos/{}/{repo}/pulls/{number}",
            crate::links::NATIVE_PROVIDER
        ),
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
    // A ledger row keeps the slug the repository had when the event was
    // recorded; current state is read under the slug it is hosted under now.
    let recorded: BTreeSet<String> = open.iter().filter_map(|v| v.repo.clone()).collect();
    let canonical_repo: BTreeMap<String, String> = recorded
        .into_iter()
        .filter_map(|repo| {
            let (owner, name) = identity::resolve(state, &repo)?;
            Some((repo, format!("{owner}/{name}")))
        })
        .collect();
    let slug = |repo: &str| {
        canonical_repo
            .get(repo)
            .cloned()
            .unwrap_or_else(|| repo.to_string())
    };
    let keys: BTreeSet<_> = open
        .iter()
        .filter_map(|v| v.repo.as_deref().map(&slug).zip(v.pr))
        .collect();
    let mut pulls = BTreeMap::new();
    for (repo, number) in keys {
        if let Some((owner, name)) = repo.split_once('/')
            && let Ok(pr) = state.core.get_pull_request(owner, name, number)
        {
            pulls.insert((repo, number), pr);
        }
    }
    // The same reading `replacement` makes, so the pull a chain or a body
    // names is loaded before its state is reported.
    let mut aliases: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (recorded, canonical) in &canonical_repo {
        aliases
            .entry(canonical.as_str())
            .or_default()
            .insert(recorded.as_str());
    }
    let replacements: BTreeSet<_> = pulls
        .iter()
        .filter_map(|((repo, number), pr)| {
            let links = state.github.pull_replacements();
            let slugs = std::iter::once(repo.as_str())
                .chain(aliases.get(repo.as_str()).into_iter().flatten().copied());
            slugs
                .filter_map(|slug| {
                    let (owner, name) = slug.split_once('/')?;
                    links.chain_end(owner, name, *number)
                })
                .next()
                .map(|end| (repo.clone(), end))
                .or_else(|| {
                    let (named, number) = target(pr.body.as_deref().unwrap_or(""), repo)?;
                    let named = identity::resolve(state, &named)
                        .map(|(owner, name)| format!("{owner}/{name}"))
                        .unwrap_or(named);
                    Some((named, number))
                })
        })
        .collect();
    for (repo, number) in replacements {
        if pulls.contains_key(&(repo.clone(), number)) {
            continue;
        }
        if let Some((owner, name)) = repo.split_once('/')
            && let Ok(pr) = state.core.get_pull_request(owner, name, number)
        {
            pulls.insert((repo, number), pr);
        }
    }
    let families: BTreeSet<_> = open
        .iter()
        .filter(|v| v.pr.is_none() || (v.todo && v.station == "work"))
        .filter_map(|v| v.family.as_deref().map(canonical))
        .collect();
    let facts = state.events.flow_lifecycle_cache().get(state, &families);
    let todos = &facts.todos;
    let problems = facts.problems.clone();
    let mut open_per_lot: BTreeMap<&str, usize> = BTreeMap::new();
    for v in open {
        *open_per_lot.entry(v.lot.as_str()).or_default() += 1;
    }
    let mut result = Snapshot {
        excluded: BTreeSet::new(),
        readings: BTreeMap::new(),
        work: super::work::Reconciled::default(),
        items: Vec::new(),
        repositories: serde_json::json!([]),
        problems,
        total_records: 0,
        merged_upstream_records: 0,
    };
    let mut counts: BTreeMap<(String, String), [u64; 3]> = BTreeMap::new();
    let mut missing = BTreeSet::new();
    for v in open {
        let pr = v
            .repo
            .as_deref()
            .zip(v.pr)
            .and_then(|(repo, n)| pulls.get(&(slug(repo), n)));
        if (v.pr.is_some() && pr.is_none())
            || (v.pr.is_none()
                && v.family
                    .as_ref()
                    .is_none_or(|f| !todos.contains_key(&(canonical(f), v.lot.clone()))))
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
            if repo_filter.is_none_or(|r| v.repo.as_deref() == Some(r)) {
                result.merged_upstream_records += 1;
            }
            continue;
        }
        let closed =
            if let Some(pr) = pr.filter(|p| !p.merged && p.state == PullRequestState::Closed) {
                Some((
                    "pull_request",
                    pr.title.clone(),
                    pr.body.clone().unwrap_or_default(),
                    crate::links::native_pull_app_path(&pr.owner, &pr.repo, pr.number),
                ))
            } else if v.pr.is_none() {
                v.family
                    .as_ref()
                    .and_then(|f| todos.get(&(canonical(f), v.lot.clone())))
                    .filter(|t| t.status == shift::TodoStatus::Closed && !t.merged)
                    .map(|t| {
                        (
                            "todo",
                            t.title.clone(),
                            format!("{}\n{}", t.note, t.body),
                            format!(
                                "/work/{}?family={}",
                                super::report::path_segment(&t.id),
                                super::report::path_segment(&t.family)
                            ),
                        )
                    })
            } else {
                None
            };
        if let Some((kind, title, body, href)) = closed {
            result.excluded.insert(v.key());
            if repo_filter.is_none_or(|r| v.repo.as_deref() == Some(r)) {
                result.total_records += 1;
                if result.items.len() == MAX_CLOSURES {
                    continue;
                }
                let recorded = v.repo.as_deref().unwrap_or("");
                let replacement =
                    replacement(state, &body, recorded, &slug(recorded), v.pr, &pulls);
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
        } else {
            use super::work::Reading;
            let todo = v
                .family
                .as_ref()
                .filter(|_| v.todo)
                .and_then(|f| todos.get(&(canonical(f), v.lot.clone())));
            let elsewhere = open_per_lot.get(v.lot.as_str()).is_some_and(|n| *n > 1);
            let reading = super::work::read(v, todo, elsewhere);
            if repo_filter.is_none_or(|r| v.repo.as_deref() == Some(r)) {
                result.work.count(reading);
            }
            let (station, held, started) = match reading {
                Reading::Leaves(_) => {
                    result.excluded.insert(v.key());
                    continue;
                }
                Reading::Current => (v.station.clone(), v.held, v.started),
                Reading::Held => (v.station.clone(), true, v.started),
                Reading::Waiting => (v.station.clone(), false, false),
                Reading::HeldAt(at) => (at.id().to_string(), true, false),
            };
            if reading != Reading::Current {
                result.readings.insert(v.key(), reading);
            }
            if let Some(repo) = &v.repo {
                let row = counts.entry((repo.clone(), station)).or_default();
                row[if held {
                    2
                } else if started {
                    1
                } else {
                    0
                }] += 1;
            }
        }
    }
    if !missing.is_empty() {
        result.problems.push(format!(
            "{} recorded items have a missing or unreadable current lifecycle record; they remain in recorded work",
            missing.len()
        ));
    }
    if result.total_records > result.items.len() {
        result.problems.push(format!("Showing the first {} of {} closed station records. Narrow the repository filter; additional records remain available in repository pull requests and work history.", result.items.len(), result.total_records));
    }
    result.repositories = serde_json::json!(counts.into_iter().map(|((repo,station),[waiting,active,held])| serde_json::json!({"repo":repo,"station":station,"waiting":waiting,"active":active,"held":held})).collect::<Vec<_>>());
    result
}
impl Snapshot {
    /// Whether an open ledger visit is still current work, by the same rule
    /// [`Snapshot::is_current`] applies to a report visit.
    pub fn is_current_open(&self, v: &OpenVisit) -> bool {
        !self.excluded.contains(&v.key())
    }
    /// Apply [`Self::readings`] to the report's visits: a held todo is held,
    /// an unclaimed one waits, and a pull request no todo carries is held at
    /// the station that stopped it. A recorded hold reason is kept; a visit
    /// already held for a reason (`superseded`) is not made to wait.
    pub fn settle(&self, visits: &mut [Visit]) {
        use super::work::Reading;
        for v in visits.iter_mut().filter(|v| v.left_ms.is_none()) {
            let key = (v.lot_id.clone(), v.station.id().to_string(), v.entered_ms);
            match self.readings.get(&key) {
                Some(Reading::Held) => v.direction = "hold".to_string(),
                Some(Reading::Waiting) if v.hold.is_none() => {
                    v.direction = "open".to_string();
                    v.started_ms = None;
                }
                Some(Reading::HeldAt(station)) => {
                    v.station = *station;
                    v.direction = "hold".to_string();
                    v.started_ms = None;
                }
                _ => {}
            }
        }
    }
    pub fn is_current(&self, v: &Visit) -> bool {
        !self
            .excluded
            .contains(&(v.lot_id.clone(), v.station.id().into(), v.entered_ms))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_queue_closures_use_canonical_families_and_reopen_after_cache_expiry() {
        use crate::web::shift::{tests::run_git, todo_file::TodoFile};
        let root = tempfile::tempdir().unwrap();
        let queue = root.path().join("acme/acme-todo.git");
        std::fs::create_dir_all(queue.join("todos")).unwrap();
        run_git(&queue, &["init", "-q"]);
        run_git(&queue, &["checkout", "-q", "-b", "queue"]);
        std::fs::write(queue.join("family.toml"), "[family]\nname = 'Acme-split'\n").unwrap();
        let mut todo = TodoFile::new(
            "closed/?#%".into(),
            "Acme-split".into(),
            "unfinished work".into(),
        );
        todo.status = shift::TodoStatus::Closed;
        std::fs::write(queue.join("todos/closed.md"), todo.dump()).unwrap();
        let mut merged = todo.clone();
        merged.id = "merged".into();
        merged.merged = true;
        std::fs::write(queue.join("todos/merged.md"), merged.dump()).unwrap();
        run_git(&queue, &["add", "."]);
        run_git(&queue, &["commit", "-q", "-m", "queue fixture"]);
        let state =
            WebState::new_with_git_storage(jeryu_core::ForgeCore::new(), root.path().to_path_buf());
        let mut open = OpenVisit {
            lot: todo.id.clone(),
            station: "work".into(),
            entered: 1000,
            repo: Some("acme/widget".into()),
            pr: None,
            family: Some("acme".into()),
            started: true,
            held: false,
            todo: true,
            landed: false,
            back_from: None,
        };
        for family in ["acme", "Acme-split"] {
            open.family = Some(family.into());
            let closed = snapshot(&state, &[open.clone()], None);
            assert_eq!(closed.items.len(), 1, "{family}");
            assert!(closed.problems.is_empty(), "{:?}", closed.problems);
            assert_eq!(closed.items[0].href, "/work/closed%2F%3F%23%25?family=acme");
            assert_eq!(closed.items[0].kind, "todo");
            assert_eq!(closed.items[0].disposition, "needs_disposition");
        }
        let mut merged_visit = open.clone();
        merged_visit.lot = "merged".into();
        assert!(
            snapshot(&state, &[merged_visit], None).excluded.is_empty(),
            "raw merged flag is not proof to suppress remaining work"
        );
        let mut bound = open.clone();
        bound.pr = Some(123);
        let bound_snapshot = snapshot(&state, &[bound], None);
        assert!(
            bound_snapshot.excluded.is_empty(),
            "todo closure must not erase PR-bound work"
        );
        assert_eq!(bound_snapshot.problems.len(), 1);

        todo.status = shift::TodoStatus::Open;
        std::fs::write(queue.join("todos/closed.md"), todo.dump()).unwrap();
        run_git(&queue, &["commit", "-q", "-am", "reopen"]);
        assert_eq!(
            snapshot(&state, &[open.clone()], None).items.len(),
            1,
            "cache is bounded-stale"
        );
        for cached in state
            .events
            .flow_lifecycle_cache()
            .0
            .lock()
            .unwrap()
            .iter_mut()
        {
            cached.read = Instant::now() - Duration::from_secs(31);
        }
        let reopened = snapshot(&state, &[open], None);
        assert!(reopened.excluded.is_empty());
        assert!(reopened.problems.is_empty());
        // Reopened and unclaimed: back in the line, waiting for a slot.
        assert_eq!(reopened.repositories[0]["waiting"], 1);
        assert_eq!(reopened.repositories[0]["active"], 0);
        assert_eq!(reopened.work.open_waiting, 1);
    }

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
        assert!(target("Superseded-by: ../..#1", "a/b").is_none());
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
        assert!(
            closed
                .items
                .iter()
                .all(|i| i.href == format!("/repos/jeryu/acme/widget/pulls/{}", pr.number))
        );
        let mut visible = original.clone();
        visible.retain(|v| v.left_ms.is_some() || closed.is_current(v));
        assert!(visible.iter().all(|v| v.left_ms.is_some()));
        assert_eq!(state.events.flow_visits(Some("acme"), 0).unwrap(), original);
        assert!(closed.repositories.as_array().unwrap().is_empty());
        let many: Vec<_> = (0..1001)
            .map(|i| {
                let mut v = open[0].clone();
                v.entered += i;
                v
            })
            .collect();
        let bounded = snapshot(&state, &many, None);
        assert_eq!(bounded.total_records, 1001);
        assert_eq!(bounded.items.len(), 1000);
        assert_eq!(bounded.excluded.len(), 1001);
        assert!(bounded.problems.iter().any(|p| p.contains("1000 of 1001")));

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
        visible.retain(|v| v.left_ms.is_some() || reopened.is_current(v));
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
        assert_eq!(merged.merged_upstream_records, open.len());
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
        state
            .github
            .pull_replacements()
            .record("acme", "widget", pr.number, later.number, "author", 2000)
            .unwrap();
        let replaced = snapshot(&state, &open, None);
        assert_eq!(replaced.items[0].disposition, "replacement_recorded");
        assert!(!replaced.items[0].needs_attention);
        assert_eq!(
            replaced.items[0].replacement.as_ref().unwrap().label,
            format!("acme/widget#{}", later.number)
        );
    }
    /// A lot recorded under a repository's earlier slug is reconciled against
    /// the repository it moved to, and only against that repository: a pull
    /// still open there stays open, and a different repository that later
    /// takes the vacated slug answers for it instead of the moved one.
    #[test]
    fn a_moved_repository_reconciles_recorded_lots_under_its_current_slug() {
        use crate::web::pipeline::NewEvent;
        use jeryu_core::{
            CreateOrganizationRequest, CreatePullRequestRequest, CreateRepositoryRequest,
            ForgeCore, UpdatePullRequestRequest,
        };
        let root = tempfile::tempdir().unwrap();
        let core = ForgeCore::new();
        let repository = |owner: &str, name: &str| {
            core.create_repository(
                owner,
                CreateRepositoryRequest {
                    name: name.into(),
                    private: false,
                    description: None,
                    default_branch: Some("main".into()),
                },
            )
            .unwrap()
        };
        let pull = |owner: &str, name: &str, title: &str, head: &str| {
            core.create_pull_request(
                owner,
                name,
                "author",
                CreatePullRequestRequest {
                    title: title.into(),
                    head: head.into(),
                    base: "main".into(),
                    head_sha: Some("c".repeat(40)),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        repository("nimbus-split", "nimbus-lambda");
        let landed = pull("nimbus-split", "nimbus-lambda", "lock refresh", "lock");
        let still_open = pull("nimbus-split", "nimbus-lambda", "held work", "held");
        core.create_organization(CreateOrganizationRequest {
            login: "cirrus".into(),
            display_name: None,
        })
        .unwrap();
        // The move the ledger never saw: the rows keep `nimbus-split/...`.
        core.rename_repository(
            "operator",
            "nimbus-split",
            "nimbus-lambda",
            "cirrus",
            "nimbus-lambda",
        )
        .unwrap();
        core.update_pull_request(
            "cirrus",
            "nimbus-lambda",
            landed.number,
            UpdatePullRequestRequest {
                state: Some(PullRequestState::Merged),
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
                    family: Some("nimbus".into()),
                    repo: Some("nimbus-split/nimbus-lambda".into()),
                    pr: Some(landed.number as i64),
                    ..Default::default()
                },
                1000,
            )
            .unwrap();
        let recorded = OpenVisit {
            lot: "nimbus-split/nimbus-lambda#2".into(),
            station: "review".into(),
            entered: 1_568_000_000_000,
            repo: Some("nimbus-split/nimbus-lambda".into()),
            pr: Some(landed.number),
            family: Some("nimbus".into()),
            started: true,
            held: false,
            todo: false,
            landed: false,
            back_from: None,
        };
        let mut gate = recorded.clone();
        gate.station = "gate".into();
        let mut held = recorded.clone();
        held.pr = Some(still_open.number);
        held.lot = "nimbus-split/nimbus-lambda#3".into();
        held.held = true;
        let visits = vec![recorded.clone(), gate.clone(), held.clone()];

        let moved = snapshot(&state, &visits, None);
        assert!(
            moved.excluded.contains(&recorded.key()) && moved.excluded.contains(&gate.key()),
            "a merged pull closes its stale station visits under the recorded slug"
        );
        assert_eq!(moved.merged_upstream_records, 2);
        assert!(moved.items.is_empty(), "a merge is not unresolved closure");
        assert!(
            !moved.excluded.contains(&held.key()),
            "a pull still open in the moved repository stays open work"
        );
        assert!(
            moved.problems.is_empty(),
            "the recorded slug resolves, so nothing is unreadable: {:?}",
            moved.problems
        );
        assert_eq!(moved.repositories[0]["repo"], "nimbus-split/nimbus-lambda");
        assert_eq!(moved.repositories[0]["held"], 1);

        // Closed, not merged: the closure links the repository's current page.
        core.update_pull_request(
            "cirrus",
            "nimbus-lambda",
            still_open.number,
            UpdatePullRequestRequest {
                state: Some(PullRequestState::Closed),
                ..Default::default()
            },
        )
        .unwrap();
        let closed = snapshot(&state, &[held.clone()], None);
        assert_eq!(closed.items.len(), 1);
        assert_eq!(
            closed.items[0].href,
            format!(
                "/repos/jeryu/cirrus/nimbus-lambda/pulls/{}",
                still_open.number
            )
        );
        assert_eq!(closed.items[0].disposition, "needs_disposition");

        // A new repository takes the vacated slug: it is a different
        // repository, so the moved one's merged pull says nothing about it.
        repository("nimbus-split", "nimbus-lambda");
        let fresh = pull("nimbus-split", "nimbus-lambda", "fresh work", "fresh");
        assert_eq!(fresh.number, landed.number, "same number, other repository");
        let reoccupied = snapshot(&state, &[recorded.clone(), gate.clone()], None);
        assert!(
            reoccupied.excluded.is_empty(),
            "a live repository at the slug wins over the alias"
        );
        assert_eq!(reoccupied.merged_upstream_records, 0);
    }
    /// A slug no repository hosts and no alias names keeps its recorded
    /// identity: the lot stays in recorded work and is reported unreadable,
    /// never closed on a guess.
    #[test]
    fn an_unknown_recorded_slug_is_reported_not_closed() {
        let root = tempfile::tempdir().unwrap();
        let state =
            WebState::new_with_git_storage(jeryu_core::ForgeCore::new(), root.path().to_path_buf());
        let visit = OpenVisit {
            lot: "nimbus-split/nimbus-lambda#2".into(),
            station: "gate".into(),
            entered: 1_568_000_000_000,
            repo: Some("nimbus-split/nimbus-lambda".into()),
            pr: Some(2),
            family: Some("nimbus".into()),
            started: true,
            held: false,
            todo: false,
            landed: false,
            back_from: None,
        };
        let snapshot = snapshot(&state, &[visit], None);
        assert!(snapshot.excluded.is_empty());
        assert!(snapshot.items.is_empty());
        assert_eq!(snapshot.problems.len(), 1);
        assert!(snapshot.problems[0].contains("remain in recorded work"));
    }
    #[test]
    fn queue_reads_share_a_short_cache_and_closure_output_has_a_limit() {
        let root = tempfile::tempdir().unwrap();
        let state =
            WebState::new_with_git_storage(jeryu_core::ForgeCore::new(), root.path().to_path_buf());
        let families = BTreeSet::from(["acme".into()]);
        let one = state.events.flow_lifecycle_cache().get(&state, &families);
        let cloned = state.events.clone();
        let two = cloned.flow_lifecycle_cache().get(&state, &families);
        assert!(Arc::ptr_eq(&one, &two));
        state
            .events
            .flow_lifecycle_cache()
            .0
            .lock()
            .unwrap()
            .first_mut()
            .unwrap()
            .read = Instant::now() - Duration::from_secs(31);
        let three = cloned.flow_lifecycle_cache().get(&state, &families);
        assert!(!Arc::ptr_eq(&one, &three));
    }
}
