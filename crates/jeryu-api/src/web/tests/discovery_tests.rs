//! Agent discovery: every URL an error body advertises resolves, the REST
//! edge answers its index with a 200, and the route index says what a route
//! takes and who may call it.

use super::*;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

/// A router whose SPA serves a real shell, so a discovery URL that fell
/// through to the web app would be caught answering HTML with a 200 — which is
/// what `docs_url` used to do.
fn app_with_spa(spa: &Path) -> (AxumRouter, PathBuf) {
    write_file(spa, "index.html", "<!doctype html><title>jeryu</title>");
    (app(WebState::new(ForgeCore::new()), spa), spa.to_path_buf())
}

struct Fetched {
    status: StatusCode,
    content_type: String,
    body: String,
}

async fn fetch(app: &AxumRouter, uri: &str) -> Fetched {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::USER_AGENT, "curl/8.5.0")
                .body(Body::empty())
                .expect("discovery request"),
        )
        .await
        .expect("router answered");
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read body");
    Fetched {
        status,
        content_type,
        body: String::from_utf8_lossy(&bytes).to_string(),
    }
}

/// Every URL a body advertises as somewhere to go: the capability manifest and
/// the documentation page, wherever they are nested.
fn advertised_urls(body: &Value, found: &mut BTreeSet<String>) {
    const KEYS: [&str; 6] = [
        "docs_url",
        "documentation_url",
        "capabilities",
        "faster_path",
        "first_contact",
        "start_here",
    ];
    match body {
        Value::Object(fields) => {
            for (key, value) in fields {
                if KEYS.contains(&key.as_str())
                    && let Some(url) = value.as_str()
                    && url.starts_with('/')
                {
                    found.insert(url.to_string());
                }
                advertised_urls(value, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| advertised_urls(item, found)),
        _ => {}
    }
}

/// The acceptance test: fetch every URL any error body advertises and get
/// JSON or markdown with a 200, never the web app's HTML shell.
#[tokio::test]
async fn every_url_an_error_body_advertises_resolves() {
    let spa = tempdir().expect("spa dir");
    let (app, _) = app_with_spa(spa.path());
    // A path that is not routed does fall through to the SPA, so the
    // assertions below are about discovery and not about an empty dist.
    let shell = fetch(&app, "/some/web/page").await;
    assert_eq!(shell.status, StatusCode::OK);
    assert!(shell.body.contains("<!doctype html>"), "{}", shell.body);

    // One request per error-body shape a confused agent runs into.
    let probes = [
        "/api/v1/no-such-route",
        "/api/v3/repos/acme/widgets/no-such-thing",
        "/api/v3/repos/acme/widgets/pulls/not-a-number",
        "/login/device/code",
        "/.jeryu/agents/first-contact",
        "/api/v1/errors",
        crate::discovery::CAPABILITIES_PATH,
        crate::discovery::DOCS_PATH,
    ];
    let mut urls = BTreeSet::new();
    for probe in probes {
        let answered = fetch(&app, probe).await;
        let body: Value = serde_json::from_str(&answered.body)
            .unwrap_or_else(|err| panic!("{probe} answered JSON: {err}: {}", answered.body));
        advertised_urls(&body, &mut urls);
    }
    assert!(
        urls.contains(crate::discovery::CAPABILITIES_PATH),
        "the error bodies advertise the capability manifest: {urls:?}"
    );
    assert!(
        urls.iter().any(|url| url.starts_with("/api/v1/docs")),
        "the error bodies advertise a served documentation page: {urls:?}"
    );
    for url in &urls {
        let answered = fetch(&app, url).await;
        assert_eq!(answered.status, StatusCode::OK, "{url}: {}", answered.body);
        assert!(
            answered.content_type.starts_with("application/json")
                || answered.content_type.starts_with("text/markdown"),
            "{url} answered {}: {}",
            answered.content_type,
            answered.body
        );
        assert!(
            !answered.body.trim_start().starts_with('<'),
            "{url} answered the web app's HTML shell"
        );
    }
}

/// The manifest is served under the prefix the edge routes, and its first
/// path keeps working for clients that already learned it.
#[tokio::test]
async fn the_capability_manifest_is_served_under_the_api_prefix() {
    let spa = tempdir().expect("spa dir");
    let (app, _) = app_with_spa(spa.path());
    for path in [
        crate::discovery::CAPABILITIES_PATH,
        crate::discovery::FIRST_CAPABILITIES_PATH,
    ] {
        let answered = fetch(&app, path).await;
        assert_eq!(answered.status, StatusCode::OK, "{path}");
        let body: Value = serde_json::from_str(&answered.body).expect("manifest JSON");
        assert_eq!(body["server"], "jeryu");
        assert_eq!(body["capabilities"], crate::discovery::CAPABILITIES_PATH);
    }
    // The advisory header steers at the path the edge routes.
    let answered = fetch(&app, "/api/v1/errors").await;
    assert_eq!(answered.status, StatusCode::OK);
}

#[tokio::test]
async fn a_documentation_page_is_served_as_markdown_and_a_missing_one_as_json() {
    let spa = tempdir().expect("spa dir");
    let (app, _) = app_with_spa(spa.path());
    let page = fetch(&app, "/api/v1/docs/errors.md").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.content_type.starts_with("text/markdown"),
        "{}",
        page.content_type
    );
    assert!(
        page.body.contains("# Error Repair Surface"),
        "{}",
        page.body
    );

    let rest = fetch(&app, crate::discovery::REST_DOC_PATH).await;
    assert_eq!(rest.status, StatusCode::OK);
    let rest: Value = serde_json::from_str(&rest.body).expect("REST document JSON");
    assert_eq!(rest["index"], "/api/v3");

    let missing = fetch(&app, "/api/v1/docs/no-such-page.md").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let body: Value = serde_json::from_str(&missing.body).expect("404 JSON");
    assert_eq!(body["code"], "api_route_not_found");
    assert_eq!(body["docs_url"], crate::discovery::DOCS_PATH);
}

/// `GET /api/v3` answered its own index under a 404, which reads as "there is
/// no such edge", and the index left out opening a pull request.
#[tokio::test]
async fn the_rest_edge_answers_its_index_with_a_200_that_lists_pr_create() {
    let spa = tempdir().expect("spa dir");
    let (app, _) = app_with_spa(spa.path());
    let answered = fetch(&app, "/api/v3").await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let body: Value = serde_json::from_str(&answered.body).expect("index JSON");
    let routes: Vec<&str> = body["jeryu_api_routes"]
        .as_array()
        .expect("routes")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        routes
            .iter()
            .any(|route| route.starts_with("POST /repos/{owner}/{repo}/pulls")),
        "the index lists opening a pull request: {routes:?}"
    );
    assert_eq!(
        body["jeryu_connection"]["capabilities"],
        crate::discovery::CAPABILITIES_PATH
    );
}

#[tokio::test]
async fn the_route_index_describes_params_and_who_may_call_each_route() {
    let spa = tempdir().expect("spa dir");
    let (app, _) = app_with_spa(spa.path());
    let answered = fetch(&app, "/api/v1").await;
    assert_eq!(answered.status, StatusCode::OK);
    let body: Value = serde_json::from_str(&answered.body).expect("index JSON");
    let routes = body["routes"].as_array().expect("described routes");
    let attention = routes
        .iter()
        .find(|route| route["path"] == "/api/v1/attention")
        .expect("the index describes /api/v1/attention");
    let params: Vec<&str> = attention["query_params"]
        .as_array()
        .expect("query params")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        params,
        ["family", "severity", "kind", "limit", "per_page", "page"]
    );
    assert_eq!(attention["auth"]["GET"], "admin");

    let pulls = routes
        .iter()
        .find(|route| route["path"] == "/api/v1/repos/{id}/pulls/{number}")
        .expect("the index describes a parameterized route");
    let path_params: Vec<&str> = pulls["path_params"]
        .as_array()
        .expect("path params")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(path_params, ["id", "number"]);
}

#[tokio::test]
async fn the_openapi_document_is_generated_from_the_mounted_routes() {
    let spa = tempdir().expect("spa dir");
    let (app, _) = app_with_spa(spa.path());
    let answered = fetch(&app, crate::discovery::OPENAPI_PATH).await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let document: Value = serde_json::from_str(&answered.body).expect("openapi JSON");
    assert_eq!(document["openapi"], "3.1.0");
    let attention = &document["paths"]["/api/v1/attention"]["get"];
    let names: Vec<&str> = attention["parameters"]
        .as_array()
        .expect("parameters")
        .iter()
        .filter_map(|parameter| parameter["name"].as_str())
        .collect();
    assert_eq!(
        names,
        ["family", "severity", "kind", "limit", "per_page", "page"]
    );
    assert_eq!(attention["x-jeryu-credential"], "admin");
    // Every described path is a path the router mounts.
    let mounted: BTreeSet<String> = api_v1_routes()
        .iter()
        .map(|(path, _)| {
            path.split('/')
                .map(|segment| match segment.strip_prefix([':', '*']) {
                    Some(name) => format!("{{{name}}}"),
                    None => segment.to_string(),
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    for path in document["paths"].as_object().expect("paths").keys() {
        assert!(
            mounted.contains(path),
            "{path} is described but not mounted"
        );
    }
}
