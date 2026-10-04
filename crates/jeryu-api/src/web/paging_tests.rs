//! The paged collections honour `limit` / `per_page` / `page`, say what they
//! applied, and refuse an out-of-range value instead of clamping it.

use std::path::Path;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{
    CreatePullRequestRequest, CreateRepositoryRequest, ForgeCore, MergePullRequestRequest, UserRole,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{WebState, app};

async fn get(router: &axum::Router, uri: &str, token: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::GET)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn big_collections_page_and_refuse_out_of_range_values() {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let mut repo_id = String::new();
    for name in ["one", "two", "three"] {
        let repo = core
            .create_repository(
                "alice",
                CreateRepositoryRequest {
                    name: name.to_string(),
                    private: false,
                    description: None,
                    default_branch: Some("main".to_string()),
                },
            )
            .unwrap();
        repo_id = repo.id.to_string();
    }
    for n in 0..3 {
        core.create_pull_request(
            "alice",
            "three",
            "alice",
            CreatePullRequestRequest {
                title: format!("change {n}"),
                head: format!("feature-{n}"),
                base: "main".to_string(),
                head_sha: Some(format!("{n:040}")),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    }
    let token = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );

    // Repositories: the rows are cut, `total` still counts every match.
    let (status, repos) = get(&router, "/api/v1/repos?limit=2", &token).await;
    assert_eq!(status, StatusCode::OK, "{repos}");
    assert_eq!(repos["repositories"].as_array().unwrap().len(), 2);
    assert_eq!(repos["total"], 3);
    assert_eq!(repos["page"]["limit"], 2);
    assert_eq!(repos["page"]["has_more"], true);
    let (_, rest) = get(&router, "/api/v1/repos?per_page=2&page=2", &token).await;
    assert_eq!(rest["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(rest["page"]["has_more"], false);
    let (_, all) = get(&router, "/api/v1/repos", &token).await;
    assert_eq!(all["page"]["limit"], 100);

    // Pull requests: paged, and `state` filters or is refused.
    let pulls = format!("/api/v1/repos/{repo_id}/pulls");
    let (status, page) = get(&router, &format!("{pulls}?limit=1&page=2"), &token).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["number"], 2);
    assert_eq!(page["total"], 3);
    assert_eq!(page["page"]["limit"], 1);
    let (_, merged) = get(&router, &format!("{pulls}?state=merged"), &token).await;
    assert_eq!(merged["total"], 0);
    let (_, open) = get(&router, &format!("{pulls}?state=open"), &token).await;
    assert_eq!(open["total"], 3);
    let (status, bad) = get(&router, &format!("{pulls}?state=bogus"), &token).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad["code"], "invalid_query");

    // The control-plane snapshot pages each collection it carries.
    let (status, snapshot) = get(&router, "/api/v1/control-plane/status?limit=1", &token).await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    assert_eq!(snapshot["repos"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["page"]["limit"], 1);
    assert_eq!(snapshot["page"]["collections"]["repos"]["total"], 3);
    assert_eq!(snapshot["page"]["collections"]["repos"]["has_more"], true);
    assert!(snapshot["summary"].is_object());

    // Events echo the limit they applied and refuse one out of range.
    let (status, events) = get(&router, "/api/v1/events?limit=7", &token).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(events["limit"], 7);
    let (_, events) = get(&router, "/api/v1/events", &token).await;
    assert_eq!(events["limit"], 100);
    for uri in ["/api/v1/events?limit=0", "/api/v1/events?limit=501"] {
        let (status, body) = get(&router, uri, &token).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
        assert_eq!(body["code"], "events_invalid_query", "{uri}");
    }

    for uri in [
        "/api/v1/repos?limit=0",
        "/api/v1/repos?limit=501",
        "/api/v1/repos?page=0",
        "/api/v1/repos?per_page=x",
        "/api/v1/control-plane/status?limit=1000",
        &format!("{pulls}?limit=-1"),
    ] {
        let (status, body) = get(&router, uri, &token).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
        assert_eq!(body["code"], "invalid_page_parameter", "{uri}");
    }
}

/// A forge with more merged pull requests than a page holds, and the open ones
/// in the repository that sorts last: the default page must still carry every
/// open one, so a reader of page 1 is not told "0 open".
#[tokio::test]
async fn the_first_snapshot_page_carries_every_open_pull_request() {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    for name in ["aaa-done", "zzz-open"] {
        core.create_repository(
            "alice",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }
    // 120 merged pull requests in the repository whose name sorts first.
    for n in 0..120u64 {
        core.create_pull_request(
            "alice",
            "aaa-done",
            "alice",
            CreatePullRequestRequest {
                title: format!("done {n}"),
                head: format!("done-{n}"),
                base: "main".to_string(),
                head_sha: Some(format!("{n:040}")),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
        core.merge_pull_request(
            "alice",
            "aaa-done",
            n + 1,
            MergePullRequestRequest {
                merge_method: "merge".to_string(),
                ..MergePullRequestRequest::default()
            },
        )
        .unwrap();
    }
    for n in 0..7u64 {
        core.create_pull_request(
            "alice",
            "zzz-open",
            "alice",
            CreatePullRequestRequest {
                title: format!("open {n}"),
                head: format!("open-{n}"),
                base: "main".to_string(),
                head_sha: Some(format!("{:040}", n + 1000)),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    }
    let token = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );

    let (status, snapshot) = get(&router, "/api/v1/control-plane/status", &token).await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    assert_eq!(snapshot["summary"]["openPrCount"], 7);
    // The header reads these, so they count pull requests, not check runs.
    assert_eq!(snapshot["summary"]["waitingCheckPrCount"], 7);
    assert_eq!(snapshot["summary"]["failingCheckPrCount"], 0);
    let pulls = snapshot["pullRequests"].as_array().unwrap();
    assert_eq!(pulls.len(), 100, "the page is still 100 rows");
    assert_eq!(
        snapshot["page"]["collections"]["pull_requests"]["total"],
        127
    );
    assert_eq!(
        snapshot["page"]["collections"]["pull_requests"]["has_more"],
        true
    );
    let open: Vec<&Value> = pulls
        .iter()
        .filter(|pr| pr["state"] != "merged" && pr["state"] != "closed")
        .collect();
    assert_eq!(
        open.len(),
        7,
        "every open pull request is on the first page, not only the ones the repository order left room for"
    );
    assert!(
        open.iter().all(|pr| pr["repo"] == "alice/zzz-open"),
        "{open:?}"
    );
    // They lead the page: the finished ones follow.
    assert!(
        pulls[..7]
            .iter()
            .all(|pr| pr["state"] != "merged" && pr["state"] != "closed"),
        "open pull requests sort first"
    );
}

/// One `/api/v1` collection, as the table-driven assertions below read it.
struct PagedRoute {
    /// The route, with `{repo}` standing for the acme repository id.
    uri: &'static str,
    /// The error code an unusable paging parameter answers with.
    code: &'static str,
    /// Where the paging report sits in the body: the dotted path to the object
    /// carrying `limit` and `has_more` (empty for the cursor-walked events).
    report: &'static str,
    /// Rows the acme fixture puts in the collection.
    rows: usize,
    /// Whether the body repeats the pre-paging `total` at the top level. The
    /// control-plane snapshot carries many collections at once, so its totals
    /// live only in the per-collection reports.
    top_total: bool,
}

/// Every paged `/api/v1` collection and what the acme fixture puts in it. A
/// route added to `docs/pagination.md` belongs here: the assertions below are
/// what keeps the one paging rule from holding on some routes only.
/// `/api/v1/repos/{id}/commits` is the one exception: it needs a git-backed
/// repository, so it is proved against one in
/// `crates/jeryu-api/src/web/repositories/commits.rs`.
const PAGED_ROUTES: &[PagedRoute] = &[
    PagedRoute {
        uri: "/api/v1/repos",
        code: "invalid_page_parameter",
        report: "page",
        rows: 3,
        top_total: true,
    },
    PagedRoute {
        uri: "/api/v1/repos/{repo}/pulls",
        code: "invalid_page_parameter",
        report: "page",
        rows: 3,
        top_total: true,
    },
    PagedRoute {
        uri: "/api/v1/control-plane/status",
        code: "invalid_page_parameter",
        report: "page.collections.repos",
        rows: 3,
        top_total: false,
    },
    PagedRoute {
        uri: "/api/v1/merge-queue?state=all",
        code: "invalid_page_parameter",
        report: "page",
        rows: 3,
        top_total: true,
    },
    PagedRoute {
        uri: "/api/v1/repos/{repo}/merge-queue",
        code: "invalid_page_parameter",
        report: "page",
        rows: 2,
        top_total: true,
    },
    PagedRoute {
        uri: "/api/v1/attention",
        code: "invalid_page_parameter",
        report: "page",
        rows: 3,
        top_total: true,
    },
    PagedRoute {
        uri: "/api/v1/agent-runs",
        code: "invalid_page_parameter",
        report: "page",
        rows: 0,
        top_total: true,
    },
    PagedRoute {
        uri: "/api/v1/repos/{repo}/agent-runs",
        code: "invalid_page_parameter",
        report: "page",
        rows: 0,
        top_total: true,
    },
    PagedRoute {
        uri: "/api/v1/audit",
        code: "invalid_page_parameter",
        report: "page",
        rows: 0,
        top_total: true,
    },
    PagedRoute {
        uri: "/api/v1/releases",
        code: "invalid_page_parameter",
        report: "page",
        rows: 3,
        top_total: true,
    },
    PagedRoute {
        uri: "/api/v1/mirrors",
        code: "invalid_page_parameter",
        report: "page",
        rows: 3,
        top_total: true,
    },
    PagedRoute {
        uri: "/api/v1/settings",
        code: "invalid_page_parameter",
        report: "page",
        rows: 3,
        top_total: true,
    },
    PagedRoute {
        uri: "/api/v1/shift/todos",
        code: "invalid_page_parameter",
        report: "page",
        rows: 0,
        top_total: true,
    },
    // The event log is a cursor walk, so it reports `limit`/`has_more` at the
    // top level and keeps its own published refusal code.
    PagedRoute {
        uri: "/api/v1/events",
        code: "events_invalid_query",
        report: "",
        rows: 3,
        top_total: false,
    },
];

/// The paging report of a response: the `page` object, or the body itself for
/// the cursor-walked event log.
fn report<'a>(body: &'a Value, route: &PagedRoute) -> &'a Value {
    let mut value = body;
    for key in route.report.split('.').filter(|key| !key.is_empty()) {
        value = &value[key];
    }
    value
}

/// An `acme` forge: three repositories, three pull requests on the last of
/// them, three queue entries, three stored events. Returns the router, an
/// admin token and the id of the repository the pull requests live in.
async fn acme_fixture() -> (axum::Router, String, String) {
    use crate::web::merge_queue::{QueueEntry, QueueState};

    let core = ForgeCore::new();
    core.create_account("acme-admin", "acme-password", UserRole::Admin)
        .unwrap();
    let mut repo_id = String::new();
    for name in ["gadgets", "sprockets", "widgets"] {
        let repo = core
            .create_repository(
                "acme",
                CreateRepositoryRequest {
                    name: name.to_string(),
                    private: false,
                    description: None,
                    default_branch: Some("main".to_string()),
                },
            )
            .unwrap();
        repo_id = repo.id.to_string();
    }
    for n in 1..=3u64 {
        core.create_pull_request(
            "acme",
            "widgets",
            "acme",
            CreatePullRequestRequest {
                title: format!("change {n}"),
                head: format!("feature-{n}"),
                base: "main".to_string(),
                head_sha: Some(format!("{n:040}")),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    }
    let token = core
        .create_personal_access_token("acme-admin", "t", None)
        .unwrap()
        .secret;
    let state = WebState::new(core).with_auth(true, false, false);
    // Two entries in `acme/widgets` and one elsewhere, so the repository-scoped
    // listing and the family-wide one page different lengths.
    state.merge_queue.seed(
        [("widgets", 1u64), ("widgets", 2), ("gadgets", 3)]
            .into_iter()
            .map(|(name, number)| QueueEntry {
                repo: format!("acme/{name}"),
                base: "main".to_string(),
                number,
                pr_head_sha: format!("{number:040}"),
                base_sha: format!("{:040}", 0),
                queue_ref: format!("refs/queue/main/{number}"),
                queue_sha: format!("{number:040}"),
                state: QueueState::Building,
                enqueued_at: "2026-10-03T00:00:00Z".to_string(),
                enqueued_by: "acme-admin".to_string(),
                approvers: Vec::new(),
                attempts: Vec::new(),
                reason: None,
                refusal_code: None,
                landed_sha: None,
            })
            .collect(),
    );
    let router = app(state, Path::new("/tmp/jeryu-no-spa"));
    for n in 1..=3u64 {
        let posted = post(
            &router,
            "/api/v1/events",
            &token,
            &json!({"events": [{
                "source": "todoq",
                "kind": "todo.claimed",
                "family": "acme",
                "todo_id": format!("todo-{n}"),
                "event_id": format!("event-{n}"),
                "summary": format!("todo-{n} claimed"),
            }]}),
        )
        .await;
        assert!(posted.0.is_success(), "post event {n}: {}", posted.1);
    }
    (router, token, repo_id)
}

async fn post(router: &axum::Router, uri: &str, token: &str, body: &Value) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// `uri` with the paging query `query` appended, honouring a route that already
/// carries a filter of its own.
fn with_query(uri: &str, repo: &str, query: &str) -> String {
    let uri = uri.replace("{repo}", repo);
    let separator = if uri.contains('?') { '&' } else { '?' };
    format!("{uri}{separator}{query}")
}

/// Every paged `/api/v1` collection answers the applied `limit` and an honest
/// `has_more`: a page as long as the limit with rows behind it says so, and the
/// page that empties the collection says it does not.
#[tokio::test]
async fn every_paged_v1_collection_reports_what_it_applied_and_whether_more_is_left() {
    let (router, token, repo) = acme_fixture().await;
    for route in PAGED_ROUTES {
        let uri = with_query(route.uri, &repo, "limit=1");
        let (status, body) = get(&router, &uri, &token).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let applied = report(&body, route);
        assert_eq!(applied["limit"], 1, "{uri} applied limit: {body}");
        assert_eq!(
            applied["has_more"],
            Value::Bool(route.rows > 1),
            "{uri} has_more with {} rows: {body}",
            route.rows
        );
        if !route.report.is_empty() {
            assert_eq!(applied["total"], route.rows, "{uri} total: {body}");
        }
        if route.top_total {
            assert_eq!(body["total"], route.rows, "{uri} top-level total: {body}");
        }

        if route.report.is_empty() {
            // The cursor walk takes no `page=`; the end of its walk is asserted
            // in `the_event_log_answers_a_cursor_and_whether_more_is_left`.
            continue;
        }
        // The page that reaches the end of the collection has nothing behind it.
        let last = with_query(
            route.uri,
            &repo,
            &format!("limit=1&page={}", route.rows.max(1)),
        );
        let (status, body) = get(&router, &last, &token).await;
        assert_eq!(status, StatusCode::OK, "{last}: {body}");
        assert_eq!(
            report(&body, route)["has_more"],
            Value::Bool(false),
            "{last} has_more on the last page: {body}"
        );
    }
}

/// The one paging rule, on every paged collection: a `per_page` (or `limit`, or
/// `page`) outside the accepted range is a 422 naming the route's code, never a
/// clamp. A clamped request cannot be told apart from the end of a collection.
#[tokio::test]
async fn every_paged_v1_collection_refuses_an_out_of_range_parameter() {
    let (router, token, repo) = acme_fixture().await;
    for route in PAGED_ROUTES {
        for query in ["limit=0", "limit=501", "per_page=0", "per_page=x", "page=0"] {
            let uri = with_query(route.uri, &repo, query);
            let (status, body) = get(&router, &uri, &token).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {body}");
            assert_eq!(body["code"], route.code, "{uri}: {body}");
        }
        // The limit at the ceiling is accepted: the bound is inclusive.
        let uri = with_query(route.uri, &repo, "per_page=500");
        let (status, body) = get(&router, &uri, &token).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
}

/// The event log walks by cursor, so it reports `has_more` and the `next_cursor`
/// the following page starts from rather than an offset page.
#[tokio::test]
async fn the_event_log_answers_a_cursor_and_whether_more_is_left() {
    let (router, token, _) = acme_fixture().await;
    let (status, page) = get(&router, "/api/v1/events?limit=2", &token).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["limit"], 2);
    assert_eq!(page["has_more"], true, "a third event is behind: {page}");
    let cursor = page["next_cursor"].as_i64().expect("a cursor");
    assert_eq!(
        cursor,
        page["events"][1]["seq"].as_i64().expect("last seq"),
        "the cursor is the last event on the page: {page}"
    );

    // The newest-first read continues with `before_seq`; the last page of the
    // walk has nothing behind it.
    let (status, rest) = get(
        &router,
        &format!("/api/v1/events?limit=2&before_seq={cursor}"),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rest}");
    assert_eq!(rest["events"].as_array().unwrap().len(), 1);
    assert_eq!(rest["has_more"], false, "the walk is done: {rest}");

    // A page exactly as long as the limit is not a page with more behind it.
    let (_, exact) = get(&router, "/api/v1/events?limit=3", &token).await;
    assert_eq!(exact["events"].as_array().unwrap().len(), 3);
    assert_eq!(exact["has_more"], false, "{exact}");
}

/// The inbox's `counts` stay over every item the filter kept, not over the page
/// in hand: a reader of page 2 still sees how much is waiting.
#[tokio::test]
async fn the_attention_inbox_counts_every_item_it_kept_not_just_the_page() {
    let (router, token, _) = acme_fixture().await;
    let (status, whole) = get(&router, "/api/v1/attention", &token).await;
    assert_eq!(status, StatusCode::OK, "{whole}");
    let items = whole["items"].as_array().expect("items").len();
    assert!(items > 1, "the fixture fills the inbox: {whole}");

    let (status, page) = get(&router, "/api/v1/attention?limit=1", &token).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["counts"], whole["counts"], "counts: {page}");
    assert_eq!(page["total"], items, "total: {page}");
    assert_eq!(page["page"]["has_more"], true, "{page}");
}
