//! Owner/name addressing for `/api/v1/repos/...`.
//!
//! Every repo-scoped v1 route is keyed by `:id`, which the handlers resolve
//! through the repository UUID. Agents and links usually hold the
//! `owner/name` pair instead, so before routing this rewrites
//! `/api/v1/repos/{owner}/{name}/rest` (and the encoded single-segment
//! `/api/v1/repos/{owner}%2F{name}/rest`) onto the UUID path. A path whose
//! first segment is already a UUID, or that names no known repository, is
//! left untouched and routes exactly as before.

use axum::extract::Request;
use axum::http::Uri;

use super::WebState;

const PREFIX: &str = "/api/v1/repos/";

/// Rewrite an owner/name repo path onto its UUID form, in place.
pub(super) fn rewrite(state: &WebState, mut request: Request) -> Request {
    if let Some(uri) = rewritten_uri(state, request.uri()) {
        *request.uri_mut() = uri;
    }
    request
}

fn rewritten_uri(state: &WebState, uri: &Uri) -> Option<Uri> {
    let rest = uri.path().strip_prefix(PREFIX)?;
    let (first, after_first) = split_segment(rest);
    if first.is_empty() || uuid::Uuid::parse_str(first).is_ok() {
        return None;
    }
    let (full_name, tail) = if let Some((owner, name)) = split_encoded(first) {
        (format!("{owner}/{name}"), after_first)
    } else {
        let (second, tail) = split_segment(after_first.strip_prefix('/')?);
        if second.is_empty() {
            return None;
        }
        (format!("{first}/{second}"), tail)
    };
    let id = repository_uuid(state, &full_name)?;
    let path_and_query = match uri.query() {
        Some(query) => format!("{PREFIX}{id}{tail}?{query}"),
        None => format!("{PREFIX}{id}{tail}"),
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(path_and_query.parse().ok()?);
    Uri::from_parts(parts).ok()
}

/// `("a", "/b/c")` for `"a/b/c"`; the tail keeps its leading slash.
fn split_segment(path: &str) -> (&str, &str) {
    match path.find('/') {
        Some(at) => (&path[..at], &path[at..]),
        None => (path, ""),
    }
}

fn split_encoded(segment: &str) -> Option<(&str, &str)> {
    let at = segment.find("%2F").or_else(|| segment.find("%2f"))?;
    let (owner, name) = (&segment[..at], &segment[at + 3..]);
    (!owner.is_empty() && !name.is_empty()).then_some((owner, name))
}

fn repository_uuid(state: &WebState, full_name: &str) -> Option<String> {
    state
        .github
        .core()
        .list_repositories(None)
        .into_iter()
        .find(|repo| repo.full_name.eq_ignore_ascii_case(full_name))
        .map(|repo| repo.id.to_string())
}
