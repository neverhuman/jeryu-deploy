//! Who is working on one repository: reported sessions plus the agents the
//! forge already knows about (its own agent runs, gate runners and reviewers,
//! queue worker slots), merged into one list of [`PresenceItem`]s.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use jeryu_core::Repository;
use sha2::{Digest, Sha256};

use super::store::{PresenceRow, effective_state};
use super::types::{Links, PresenceItem, Source, Via};
use crate::web::WebState;
use crate::web::control_plane::{holds_gate_slot, is_online, is_reviewer, offline_after_secs};
use crate::web::shift::HEALTHY_MS;

/// An opaque, stable id for a forge-side agent. Runner ids and worker slots
/// name hosts, which only admins may see, so the id is a digest of them.
fn opaque(parts: &[&str]) -> String {
    let digest = Sha256::digest(parts.join("\u{1f}").as_bytes());
    hex::encode(&digest[..8])
}

/// A reported session as an item. `via` is `None` on the overview list.
pub(crate) fn external_item(
    row: &PresenceRow,
    via: Option<Via>,
    family: Option<String>,
    admin: bool,
    now_ms: i64,
) -> PresenceItem {
    PresenceItem {
        id: format!("{}:{}", Source::External.prefix(), row.session_id),
        source: Source::External,
        session_id: row.session_id.clone(),
        reporter: Some(row.reporter.clone()),
        agent: row.agent.clone(),
        model: row.model.clone(),
        operator: row.operator.clone(),
        lane: row.lane.clone(),
        branch: row.branch.clone(),
        title: row.title.clone(),
        state: effective_state(row, now_ms),
        via,
        family,
        started_at: Some(row.started_ms),
        last_active_at: row.last_active_ms,
        expires_at: Some(row.expires_ms),
        cost_usd: row.cost_usd,
        tokens: row.tokens.clone(),
        links: row.links.clone().unwrap_or_default(),
        host: if admin { row.host.clone() } else { None },
        collides_with: Vec::new(),
        repos: None,
        families: None,
    }
}

/// Reported sessions that name `repo` (`via: repo`) or its family
/// (`via: family`). A session naming both is `repo`.
fn external(
    rows: &[PresenceRow],
    repo: &Repository,
    family: Option<&str>,
    admin: bool,
    now_ms: i64,
) -> Vec<PresenceItem> {
    rows.iter()
        .filter_map(|row| {
            let by_repo = row.repos.contains(&repo.full_name);
            let by_family = family.is_some_and(|family| row.families.iter().any(|f| f == family));
            let via = if by_repo {
                Via::Repo
            } else if by_family {
                Via::Family
            } else {
                return None;
            };
            Some(external_item(
                row,
                Some(via),
                family.map(str::to_string),
                admin,
                now_ms,
            ))
        })
        .collect()
}

/// jeryu's own running agent runs on `repo`. A finished run is left out: it
/// keeps no end time, and the agent-runs list already shows its outcome.
fn agent_runs(
    state: &WebState,
    repo: &Repository,
    family: Option<&str>,
    now_ms: i64,
) -> Vec<PresenceItem> {
    state
        .agent_runs
        .rows_for_repo(&repo.full_name)
        .into_iter()
        .filter(|row| row.status == "running")
        .map(|row| PresenceItem {
            id: format!("{}:{}", Source::AgentRun.prefix(), row.run_id),
            source: Source::AgentRun,
            session_id: row.run_id.clone(),
            reporter: None,
            agent: row.agent.clone().unwrap_or_else(|| "agent".to_string()),
            model: None,
            operator: None,
            lane: None,
            branch: Some(row.branch.clone()).filter(|branch| !branch.is_empty()),
            title: None,
            state: "active",
            via: Some(Via::Repo),
            family: family.map(str::to_string),
            started_at: None,
            last_active_at: now_ms,
            expires_at: None,
            cost_usd: None,
            tokens: None,
            links: Links {
                agent_run: Some(row.run_id),
                ..Links::default()
            },
            host: None,
            collides_with: Vec::new(),
            repos: None,
            families: None,
        })
        .collect()
}

/// Online gate runners and reviewers whose current pass is on `repo`.
fn gate_runners(
    state: &WebState,
    repo: &Repository,
    family: Option<&str>,
    admin: bool,
    now: DateTime<Utc>,
) -> Vec<PresenceItem> {
    state
        .gate_runners
        .snapshot()
        .into_iter()
        .filter(|record| is_online(record, now))
        .filter(|record| holds_gate_slot(&record.heartbeat) || is_reviewer(&record.heartbeat))
        .filter_map(|record| {
            let current = record.heartbeat.current.as_ref()?;
            if current.repo != repo.full_name {
                return None;
            }
            let (source, agent) = if is_reviewer(&record.heartbeat) {
                (Source::Reviewer, "reviewer")
            } else {
                (Source::Gate, "gate")
            };
            let session_id = opaque(&[&record.heartbeat.runner_id]);
            let received = record.received_at.timestamp_millis();
            let title = match current.pr {
                Some(pr) => format!("{} on #{pr}", current.recipe),
                None => format!("{} on {}", current.recipe, short_sha(&current.sha)),
            };
            Some(PresenceItem {
                id: format!("{}:{session_id}", source.prefix()),
                source,
                session_id,
                reporter: None,
                agent: agent.to_string(),
                model: None,
                operator: admin.then(|| record.reporter.clone()),
                lane: None,
                branch: None,
                title: Some(title),
                state: "active",
                via: Some(Via::Repo),
                family: family.map(str::to_string),
                started_at: Some(current.started_at.timestamp_millis()),
                last_active_at: received,
                expires_at: Some(received + offline_after_secs(&record.heartbeat) * 1000),
                cost_usd: None,
                tokens: None,
                links: Links {
                    pr: current.pr.map(|pr| format!("{}#{pr}", repo.full_name)),
                    ..Links::default()
                },
                host: admin.then(|| record.heartbeat.host.clone()),
                collides_with: Vec::new(),
                repos: None,
                families: None,
            })
        })
        .collect()
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Healthy queue worker slots of `repo`'s family that are working. A slot
/// whose todo the family queue can name is listed only when that todo lists
/// this repository (`via: repo`); a slot whose todo cannot be read is listed
/// for the whole family (`via: family`). The queue is read only when a slot
/// of the family is working, and once per request.
fn queue_workers(
    state: &WebState,
    repo: &Repository,
    family: Option<&str>,
    admin: bool,
    now_ms: i64,
) -> Vec<PresenceItem> {
    let Some(family) = family else {
        return Vec::new();
    };
    let Ok(slots) = state.shift.heartbeats.latest(now_ms - HEALTHY_MS) else {
        return Vec::new();
    };
    let working: Vec<_> = slots
        .into_iter()
        .filter(|slot| {
            slot.heartbeat.state == "working"
                && crate::web::family::canonical(&slot.heartbeat.family) == family
        })
        .collect();
    if working.is_empty() {
        return Vec::new();
    }
    let todos = family_todos(state, family);
    working
        .into_iter()
        .filter_map(|slot| {
            let beat = &slot.heartbeat;
            let todo = beat.todo_id.as_ref().and_then(|id| todos.get(id));
            let via = match todo {
                Some(todo) if todo.repos.contains(&repo.name) => Via::Repo,
                Some(_) => return None,
                None => Via::Family,
            };
            let session_id = opaque(&[&beat.operator, &beat.host, &beat.slot, &beat.family]);
            let title = match (todo, beat.stage.as_deref()) {
                (Some(todo), Some(stage)) => Some(format!("{stage}: {}", todo.title)),
                (Some(todo), None) => Some(todo.title.clone()),
                (None, Some(stage)) => Some(stage.to_string()),
                (None, None) => None,
            };
            Some(PresenceItem {
                id: format!("{}:{session_id}", Source::QueueWorker.prefix()),
                source: Source::QueueWorker,
                session_id,
                reporter: None,
                agent: beat.kind.clone(),
                model: None,
                operator: Some(beat.operator.clone()),
                lane: None,
                branch: beat.shift.clone(),
                title,
                state: "active",
                via: Some(via),
                family: Some(family.to_string()),
                started_at: None,
                last_active_at: slot.received_ms,
                expires_at: Some(slot.received_ms + HEALTHY_MS),
                cost_usd: None,
                tokens: None,
                links: Links {
                    todo: beat.todo_id.clone(),
                    ..Links::default()
                },
                host: admin.then(|| beat.host.clone()),
                collides_with: Vec::new(),
                repos: None,
                families: None,
            })
        })
        .collect()
}

/// The todos of `family`'s queue by id, or none when it hosts no queue.
fn family_todos(
    state: &WebState,
    family: &str,
) -> BTreeMap<String, crate::web::shift::todo_file::TodoFile> {
    crate::web::shift::queue::discover(&state.repo_manager)
        .into_iter()
        .filter(|queue| crate::web::family::canonical(&queue.family.name) == family)
        .filter_map(|queue| crate::web::shift::queue_todos(state, &queue).ok())
        .flatten()
        .map(|queued| (queued.todo.id.clone(), queued.todo))
        .collect()
}

/// Every agent on `repo`, collisions marked, live first and newest first.
pub(crate) fn repo_items(
    state: &WebState,
    rows: &[PresenceRow],
    repo: &Repository,
    admin: bool,
    now: DateTime<Utc>,
) -> Vec<PresenceItem> {
    let now_ms = now.timestamp_millis();
    let family = crate::web::repositories::effective_family(state, repo);
    let family = family.as_deref();
    let mut items = external(rows, repo, family, admin, now_ms);
    items.extend(agent_runs(state, repo, family, now_ms));
    items.extend(gate_runners(state, repo, family, admin, now));
    items.extend(queue_workers(state, repo, family, admin, now_ms));
    mark_collisions(
        &mut items,
        |item| (item.via == Some(Via::Repo)).then(|| vec![repo.full_name.clone()]),
        |_| Some(repo.default_branch.clone()),
    );
    sort(&mut items);
    items
}

/// For every live item, the other live items that share a repository (as
/// `repos_of` names them) and the same branch. An item without a branch
/// collides with nothing. Nothing collides on a repository's default branch
/// (`default_branch_of`): it moves only by merge, so sessions there are
/// reading or reviewing, not writing the same commits. Two queue workers never
/// collide with each other: a queue lands its slots on one shared branch by design.
pub(crate) fn mark_collisions(
    items: &mut [PresenceItem],
    repos_of: impl Fn(&PresenceItem) -> Option<Vec<String>>,
    default_branch_of: impl Fn(&str) -> Option<String>,
) {
    // Index by repository and branch; bound each payload even when many sessions collide.
    const MAX_COLLISIONS: usize = 64;
    let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    let keys: Vec<_> = items
        .iter()
        .map(|item| {
            let branch = item.branch.as_ref().filter(|_| item.live())?;
            let repos: Vec<String> = repos_of(item)?
                .into_iter()
                .filter(|repo| default_branch_of(repo).as_ref() != Some(branch))
                .collect();
            Some((repos, branch.clone()))
        })
        .collect();
    let sources: Vec<Source> = items.iter().map(|item| item.source).collect();
    for (index, key) in keys.iter().enumerate() {
        if let Some((repos, branch)) = key {
            for repo in repos {
                groups
                    .entry((repo.clone(), branch.clone()))
                    .or_default()
                    .push(index);
            }
        }
    }
    let ids: Vec<_> = items.iter().map(|item| item.id.clone()).collect();
    for (index, item) in items.iter_mut().enumerate() {
        let mut others = BTreeSet::new();
        if let Some((repos, branch)) = &keys[index] {
            for repo in repos {
                if let Some(group) = groups.get(&(repo.clone(), branch.clone())) {
                    for other in group
                        .iter()
                        .copied()
                        .filter(|other| *other != index)
                        .filter(|other| {
                            !(sources[index] == Source::QueueWorker
                                && sources[*other] == Source::QueueWorker)
                        })
                        .take(MAX_COLLISIONS)
                    {
                        others.insert(other);
                        if others.len() >= MAX_COLLISIONS {
                            break;
                        }
                    }
                }
                if others.len() >= MAX_COLLISIONS {
                    break;
                }
            }
        }
        item.collides_with = others.into_iter().map(|other| ids[other].clone()).collect();
    }
}

/// Live first (active, idle), then stale, then ended; newest activity first.
pub(crate) fn sort(items: &mut [PresenceItem]) {
    items.sort_by(|a, b| {
        a.rank()
            .cmp(&b.rank())
            .then(b.last_active_at.cmp(&a.last_active_at))
            .then(a.id.cmp(&b.id))
    });
}
