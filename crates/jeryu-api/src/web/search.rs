//! `GET /api/v1/search`: one text query over the records the forge holds, so
//! an operator can find a piece of work without remembering which page lists
//! it and an agent has an endpoint to ask.
//!
//! What is searchable, and why those five:
//!
//! - `repository` — name, `owner/name` and description.
//! - `pull_request` — title, body, head branch and number, per readable repo.
//! - `issue` — title, body and number, per readable repo.
//! - `todo` — the todoq family queues: title, body, note, id.
//! - `activity` — the pipeline event log: summary, reason, kind, actor.
//!
//! Every one of these is a record the forge already keeps and can read in
//! full without leaving its own stores. Commit messages and file contents are
//! deliberately not here: both mean walking the history or the tree of every
//! repository on every keystroke, which is an index, not a query, and an
//! index is its own change. `kinds` in the answer says what was searched, so
//! a caller never has to assume a silent miss means "no such thing".
//!
//! Authorization is the same as the pages the results link to: repositories,
//! pull requests and issues are searched only where the reader may read them,
//! and `activity` is searched for global admins only, because the event log
//! carries todo titles, notes and log tails from repositories the reader may
//! not have access to (see `auth::admin_only_request`). A non-admin asking
//! for `kind=activity` is refused; one who asks for nothing in particular
//! gets the four kinds they may read and a `kinds` list that says so.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_core::{AccountSummary, Repository, UserRole};
use jeryu_readmodel::contracts::RepositoryId;
use serde::{Deserialize, Serialize};

use super::auth::forbidden;
use super::pipeline::EventsQuery;
use super::repositories::repo_id;
use super::{WebState, api_error, server_time, shift};

/// Hits returned per kind when the caller does not say.
const DEFAULT_LIMIT: usize = 10;
/// Most hits any one kind may return.
const MAX_LIMIT: usize = 100;
/// Longest query we will match; past this it is a paste, not a search.
const MAX_QUERY_CHARS: usize = 200;
/// Newest event rows scanned for a query. The log is append-only and the
/// interesting end is the recent one; a deeper reach belongs to an index.
const ACTIVITY_SCAN: i64 = 500;
/// Longest snippet returned for a body match.
const MAX_SNIPPET_CHARS: usize = 200;

/// A kind of record `/api/v1/search` looks in. Serialized as the `kind` of
/// every hit and as the keys of `counts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SearchKind {
    Repository,
    PullRequest,
    Issue,
    Todo,
    Activity,
}

impl SearchKind {
    /// Display order: where an operator looks first, first.
    const ALL: [SearchKind; 5] = [
        SearchKind::Repository,
        SearchKind::PullRequest,
        SearchKind::Issue,
        SearchKind::Todo,
        SearchKind::Activity,
    ];

    fn as_str(self) -> &'static str {
        match self {
            SearchKind::Repository => "repository",
            SearchKind::PullRequest => "pull_request",
            SearchKind::Issue => "issue",
            SearchKind::Todo => "todo",
            SearchKind::Activity => "activity",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        SearchKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == text)
    }

    /// Whether only a global admin may search this kind.
    fn admin_only(self) -> bool {
        self == SearchKind::Activity
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SearchParams {
    q: Option<String>,
    /// Comma-separated kinds; absent means every kind the reader may search.
    kind: Option<String>,
    /// Hits per kind.
    limit: Option<usize>,
}

/// One record that matched, with the SPA address that opens it.
#[derive(Debug, Serialize)]
pub(super) struct SearchHit {
    pub kind: SearchKind,
    /// Stable within one answer; the caller's list key.
    pub id: String,
    pub title: String,
    /// One line saying where it lives: `owner/name`, a family, a state.
    pub context: String,
    /// The matched line, when what matched was a body rather than a name. A
    /// repository falls back to its description: it is the one line worth
    /// showing either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// The page that opens it.
    pub path: String,
    /// RFC 3339, when the record carries a time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// The repository it belongs to, for the kinds that have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepositoryId>,
}

#[derive(Debug, Serialize)]
pub(super) struct SearchResponse {
    pub generated_at: String,
    /// The query as it was matched: trimmed, but in the caller's spelling.
    pub query: String,
    /// The kinds this answer searched, in display order.
    pub kinds: Vec<SearchKind>,
    /// Matches per kind BEFORE `limit` cut the list, so a caller can say
    /// "7 repositories" while showing three.
    pub counts: BTreeMap<&'static str, usize>,
    /// Hits per kind this answer was cut to.
    pub limit: usize,
    pub results: Vec<SearchHit>,
    /// Sources that could not be read on this query. Empty is the normal case.
    pub problems: Vec<String>,
}

/// Where a query matched one record, best first: the ordering is the ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    /// The name or the number is the query.
    Exact,
    /// The name starts with the query.
    Prefix,
    /// A word of the name starts with the query.
    Word,
    /// The name contains the query somewhere.
    Name,
    /// Only a body, description or note contains it.
    Body,
}

/// What separates words inside a name: `owner/name`, `todo.claimed`, `a-b`.
fn is_word_break(c: char) -> bool {
    c.is_whitespace() || matches!(c, '/' | ':' | '#' | '.' | '_' | '-' | ',')
}

/// How `needle` (already lowercase) matches a name-like field.
fn rank_name(text: &str, needle: &str) -> Option<Rank> {
    let text = text.to_lowercase();
    if text == needle {
        return Some(Rank::Exact);
    }
    if text.starts_with(needle) {
        return Some(Rank::Prefix);
    }
    if text
        .split(is_word_break)
        .any(|word| word.starts_with(needle))
    {
        return Some(Rank::Word);
    }
    text.contains(needle).then_some(Rank::Name)
}

/// The best rank over several name-like fields.
fn rank_names<'a>(fields: impl IntoIterator<Item = &'a str>, needle: &str) -> Option<Rank> {
    fields
        .into_iter()
        .filter_map(|field| rank_name(field, needle))
        .min()
}

/// The first line of `body` containing `needle`, trimmed and capped.
fn snippet(body: &str, needle: &str) -> Option<String> {
    let line = body
        .lines()
        .map(str::trim)
        .find(|line| line.to_lowercase().contains(needle))?;
    Some(if line.chars().count() > MAX_SNIPPET_CHARS {
        let cut: String = line.chars().take(MAX_SNIPPET_CHARS).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    })
}

/// A record's rank and, when the body is what matched, the matching line.
struct Match {
    rank: Rank,
    snippet: Option<String>,
}

/// Rank a record by its names first and its bodies only if no name matched, so
/// a repository called `forge` outranks every description mentioning forges.
fn match_record<'a>(
    names: impl IntoIterator<Item = &'a str>,
    bodies: impl IntoIterator<Item = &'a str>,
    needle: &str,
) -> Option<Match> {
    if let Some(rank) = rank_names(names, needle) {
        return Some(Match {
            rank,
            snippet: None,
        });
    }
    bodies
        .into_iter()
        .find_map(|body| snippet(body, needle))
        .map(|snippet| Match {
            rank: Rank::Body,
            snippet: Some(snippet),
        })
}

/// The number a query names, from `12`, `#12` or `owner/name#12`.
fn query_number(query: &str) -> Option<u64> {
    let tail = query.rsplit('#').next()?;
    let digits = if tail == query {
        query.strip_prefix('#').unwrap_or(query)
    } else {
        tail
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|number| *number > 0)
}

/// The repository part of `owner/name#12` or `name#12`, lowercased.
fn query_repo(query: &str) -> Option<String> {
    let (head, tail) = query.split_once('#')?;
    let head = head.trim();
    if head.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(head.to_lowercase())
}

/// Whether a `repo#n` query names this repository (an unqualified `name#n`
/// matches on the bare name, the way the command palette resolves it).
fn repo_named_by(repo: &Repository, wanted: &str) -> bool {
    if wanted.contains('/') {
        format!("{}/{}", repo.owner, repo.name).to_lowercase() == wanted
    } else {
        repo.name.to_lowercase() == wanted
    }
}

pub(super) async fn search(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(params): Query<SearchParams>,
) -> AxumResponse {
    let query = params.q.unwrap_or_default().trim().to_string();
    if query.is_empty() {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "q must not be empty: /api/v1/search?q=<text>",
        );
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            &format!("q must be at most {MAX_QUERY_CHARS} characters"),
        );
    }
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            &format!("limit must be between 1 and {MAX_LIMIT}"),
        );
    }
    let admin = account.role == UserRole::Admin;
    let kinds = match params.kind.as_deref() {
        // No `kind`: every kind this reader may search, rather than a refusal
        // they cannot act on.
        None => SearchKind::ALL
            .into_iter()
            .filter(|kind| admin || !kind.admin_only())
            .collect(),
        Some(list) => {
            let mut kinds = Vec::new();
            for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                let Some(kind) = SearchKind::parse(name) else {
                    return api_error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "invalid_input",
                        &format!(
                            "unknown kind {name:?}; kind is a comma-separated list of {}",
                            SearchKind::ALL.map(SearchKind::as_str).join(", ")
                        ),
                    );
                };
                if kind.admin_only() && !admin {
                    return forbidden(&format!(
                        "searching {} requires an administrator",
                        kind.as_str()
                    ));
                }
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
            if kinds.is_empty() {
                return api_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_input",
                    "kind must name at least one kind",
                );
            }
            kinds.sort();
            kinds
        }
    };

    let needle = query.to_lowercase();
    let mut counts = BTreeMap::new();
    let mut results = Vec::new();
    let mut problems = Vec::new();
    for kind in &kinds {
        let mut hits = match kind {
            SearchKind::Repository => repository_hits(&state, &account, &needle),
            SearchKind::PullRequest => pull_request_hits(&state, &account, &query, &needle),
            SearchKind::Issue => issue_hits(&state, &account, &query, &needle),
            SearchKind::Todo => {
                let (hits, todo_problems) = todo_hits(&state, &needle);
                problems.extend(todo_problems);
                hits
            }
            SearchKind::Activity => {
                let (hits, activity_problems) = activity_hits(&state, &needle);
                problems.extend(activity_problems);
                hits
            }
        };
        counts.insert(kind.as_str(), hits.len());
        hits.truncate(limit);
        results.extend(hits.into_iter().map(|ranked| ranked.hit));
    }

    Json(SearchResponse {
        generated_at: server_time(),
        query,
        kinds,
        counts,
        limit,
        results,
        problems,
    })
    .into_response()
}

/// A hit with the key it is ordered by, before the rank is dropped.
struct Ranked {
    rank: Rank,
    /// Newest first within a rank; empty when the record has no time.
    recency: String,
    hit: SearchHit,
}

/// Best rank first, then newest, then by title so the order never wobbles.
fn ordered(mut hits: Vec<Ranked>) -> Vec<Ranked> {
    hits.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then_with(|| b.recency.cmp(&a.recency))
            .then_with(|| a.hit.title.cmp(&b.hit.title))
            .then_with(|| a.hit.id.cmp(&b.hit.id))
    });
    hits
}

/// Every repository this account may read. The same rule the repository list
/// applies, so search can never name a repository its list would hide.
fn readable_repositories(state: &WebState, account: &AccountSummary) -> Vec<Repository> {
    state
        .core
        .list_repositories(None)
        .into_iter()
        .filter(|repo| {
            account.role == UserRole::Admin
                || !repo.private
                || state
                    .core
                    .user_can_read_repo(&account.login, &repo.owner, &repo.name)
        })
        .collect()
}

fn repo_front_page(repo: &Repository) -> String {
    format!("/repos/jeryu/{}/{}", repo.owner, repo.name)
}

fn repository_hits(state: &WebState, account: &AccountSummary, needle: &str) -> Vec<Ranked> {
    let hits = readable_repositories(state, account)
        .into_iter()
        .filter_map(|repo| {
            let full_name = format!("{}/{}", repo.owner, repo.name);
            let description = repo.description.clone().unwrap_or_default();
            let found = match_record(
                [repo.name.as_str(), full_name.as_str()],
                [description.as_str()],
                needle,
            )?;
            Some(Ranked {
                rank: found.rank,
                recency: repo.updated_at.to_rfc3339(),
                hit: SearchHit {
                    kind: SearchKind::Repository,
                    id: format!("repository:{full_name}"),
                    title: full_name,
                    context: if repo.private { "private" } else { "public" }.to_string(),
                    snippet: found
                        .snippet
                        .or_else(|| (!description.is_empty()).then(|| description.clone())),
                    path: repo_front_page(&repo),
                    updated_at: Some(repo.updated_at.to_rfc3339()),
                    repo: Some(repo_id(&repo)),
                },
            })
        })
        .collect();
    ordered(hits)
}

fn pull_request_hits(
    state: &WebState,
    account: &AccountSummary,
    query: &str,
    needle: &str,
) -> Vec<Ranked> {
    let number = query_number(query);
    let wanted_repo = query_repo(query);
    let mut hits = Vec::new();
    for repo in readable_repositories(state, account) {
        // `name#12` names one repository; do not offer #12 of every other.
        if let Some(wanted) = &wanted_repo
            && !repo_named_by(&repo, wanted)
        {
            continue;
        }
        let Ok(pulls) = state.core.list_pull_requests(&repo.owner, &repo.name, None) else {
            continue;
        };
        for pull in pulls {
            let body = pull.body.clone().unwrap_or_default();
            let found = if number == Some(pull.number) {
                Some(Match {
                    rank: Rank::Exact,
                    snippet: None,
                })
            } else {
                match_record(
                    [pull.title.as_str(), pull.head.ref_name.as_str()],
                    [body.as_str()],
                    needle,
                )
            };
            let Some(found) = found else { continue };
            hits.push(Ranked {
                rank: found.rank,
                recency: pull.updated_at.to_rfc3339(),
                hit: SearchHit {
                    kind: SearchKind::PullRequest,
                    id: format!("pull_request:{}/{}#{}", repo.owner, repo.name, pull.number),
                    title: format!("#{} {}", pull.number, pull.title),
                    context: format!("{}/{} · {:?}", repo.owner, repo.name, pull.state)
                        .to_lowercase(),
                    snippet: found.snippet,
                    path: format!("{}/pulls/{}", repo_front_page(&repo), pull.number),
                    updated_at: Some(pull.updated_at.to_rfc3339()),
                    repo: Some(repo_id(&repo)),
                },
            });
        }
    }
    ordered(hits)
}

fn issue_hits(
    state: &WebState,
    account: &AccountSummary,
    query: &str,
    needle: &str,
) -> Vec<Ranked> {
    let number = query_number(query);
    let wanted_repo = query_repo(query);
    let mut hits = Vec::new();
    for repo in readable_repositories(state, account) {
        if let Some(wanted) = &wanted_repo
            && !repo_named_by(&repo, wanted)
        {
            continue;
        }
        let Ok(issues) = state.core.list_issues(&repo.owner, &repo.name, None) else {
            continue;
        };
        for issue in issues {
            // An issue that carries a pull request is that pull request; the
            // pull_request kind already offers it.
            if issue.pull_request.is_some() {
                continue;
            }
            let body = issue.body.clone().unwrap_or_default();
            let labels = issue.labels.join(" ");
            let found = if number == Some(issue.number) {
                Some(Match {
                    rank: Rank::Exact,
                    snippet: None,
                })
            } else {
                match_record(
                    [issue.title.as_str(), labels.as_str()],
                    [body.as_str()],
                    needle,
                )
            };
            let Some(found) = found else { continue };
            hits.push(Ranked {
                rank: found.rank,
                recency: issue.updated_at.to_rfc3339(),
                hit: SearchHit {
                    kind: SearchKind::Issue,
                    id: format!("issue:{}/{}#{}", repo.owner, repo.name, issue.number),
                    title: format!("#{} {}", issue.number, issue.title),
                    context: format!("{}/{} · {:?}", repo.owner, repo.name, issue.state)
                        .to_lowercase(),
                    snippet: found.snippet,
                    path: format!("{}/issues#{}", repo_front_page(&repo), issue.number),
                    updated_at: Some(issue.updated_at.to_rfc3339()),
                    repo: Some(repo_id(&repo)),
                },
            });
        }
    }
    ordered(hits)
}

fn todo_hits(state: &WebState, needle: &str) -> (Vec<Ranked>, Vec<String>) {
    let (todos, problems) = shift::all_todos(state);
    let hits = todos
        .into_iter()
        .filter_map(|todo| {
            let found = match_record(
                [todo.title.as_str(), todo.id.as_str()],
                [todo.body.as_str(), todo.note.as_str()],
                needle,
            )?;
            Some(Ranked {
                rank: found.rank,
                recency: todo.filed_at.clone(),
                hit: SearchHit {
                    kind: SearchKind::Todo,
                    id: format!("todo:{}/{}", todo.family, todo.id),
                    title: todo.title.clone(),
                    context: format!("{} · {}", todo.family, todo.status.as_str()),
                    snippet: found.snippet,
                    // The todo's own page, not the queue it sits in.
                    path: format!(
                        "/work/{}?family={}",
                        urlencoding(&todo.id),
                        urlencoding(&todo.family)
                    ),
                    updated_at: Some(todo.filed_at),
                    repo: None,
                },
            })
        })
        .collect();
    (ordered(hits), problems)
}

fn activity_hits(state: &WebState, needle: &str) -> (Vec<Ranked>, Vec<String>) {
    let events = match state.events.query(&EventsQuery {
        limit: Some(ACTIVITY_SCAN),
        ..EventsQuery::default()
    }) {
        Ok(events) => events,
        Err(err) => return (Vec::new(), vec![format!("activity log: {err}")]),
    };
    let hits = events
        .into_iter()
        .filter_map(|event| {
            let reason = event.reason.clone().unwrap_or_default();
            let names = [
                event.summary.as_str(),
                event.kind.as_str(),
                event.actor.as_deref().unwrap_or_default(),
                event.repo.as_deref().unwrap_or_default(),
                event.todo_id.as_deref().unwrap_or_default(),
            ];
            let found = match_record(names, [reason.as_str()], needle)?;
            Some(Ranked {
                rank: found.rank,
                recency: event.ts.clone(),
                hit: SearchHit {
                    kind: SearchKind::Activity,
                    id: format!("activity:{}", event.seq),
                    title: event.summary.clone(),
                    context: [
                        Some(event.kind.clone()),
                        event.repo.clone(),
                        event.actor.clone(),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · "),
                    snippet: found.snippet,
                    path: activity_path(&event),
                    updated_at: Some(event.ts),
                    repo: None,
                },
            })
        })
        .collect();
    (ordered(hits), Vec::new())
}

/// The Activity page narrowed to the event: its todo when it has one, else its
/// repository, else its kind. The feed has no per-event address.
fn activity_path(event: &super::pipeline::Event) -> String {
    let filter = if let Some(todo) = event.todo_id.as_deref().filter(|id| !id.is_empty()) {
        format!("todo_id={}", urlencoding(todo))
    } else if let Some(repo) = event.repo.as_deref().filter(|repo| !repo.is_empty()) {
        format!("repo={}", urlencoding(repo))
    } else {
        format!("kind={}", urlencoding(&event.kind))
    };
    format!("/activity?{filter}")
}

/// Percent-encode one query-string value. The values here are ids, family and
/// repository names and event kinds, so this only has to be correct, not fast.
fn urlencoding(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests;
