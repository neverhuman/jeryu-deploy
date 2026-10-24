//! Raw file bytes, by query (`/raw?ref=&path=`) or by path
//! (`/raw/{ref}/{path}`), and the headers that keep repository content inert
//! on the forge's own origin.
//!
//! The path form exists so a file can reach its siblings: an `index.html` at
//! `/api/v1/repos/{id}/raw/main/docs/index.html` that says `<img src="a.png">`
//! loads `/api/v1/repos/{id}/raw/main/docs/a.png`. The query form has no
//! directory for a relative link to resolve against.
//!
//! Raw bytes are whatever a pusher committed, served from the origin that
//! holds the session cookie. Every raw response therefore carries
//! [`RAW_CONTENT_SECURITY_POLICY`]: a page renders with its images, styles and
//! fonts, but runs no script, submits no form and cannot be framed. The
//! sandbox keeps `allow-same-origin` so a private repository's images still
//! load with the viewer's cookie; it never grants `allow-scripts`.

use super::source::{git_blob_bytes, normalize_git_path};
use super::*;

/// The policy on every raw response, whichever form asked for it.
pub(in crate::web) const RAW_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; \
     img-src 'self' data:; style-src 'self' 'unsafe-inline'; font-src 'self' data:; \
     media-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; \
     sandbox allow-same-origin";

/// A path-form ref is at most this many `/`-separated segments; the longest
/// candidate is tried first, so this bounds the ref lookups per request.
const MAX_REF_SEGMENTS: usize = 8;

/// Add the raw headers to any response, an error included: an error body is
/// JSON, but the policy costs nothing there and nothing slips through.
pub(super) fn with_raw_headers(mut response: AxumResponse, path: Option<&str>) -> AxumResponse {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(RAW_CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    if response.status().is_success()
        && let Some(disposition) = path.and_then(inline_disposition)
    {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, disposition);
    }
    response
}

/// `inline` with the file's own name, so a save keeps it. The quoted name is
/// ASCII with anything awkward replaced; `filename*` carries the exact name.
fn inline_disposition(path: &str) -> Option<header::HeaderValue> {
    let name = path.rsplit('/').next().filter(|name| !name.is_empty())?;
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    header::HeaderValue::from_str(&format!(
        "inline; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    ))
    .ok()
}

/// `GET /api/v1/repos/:id/raw/*rest`: `rest` is `{ref}/{path}`.
///
/// A ref may contain `/` (`release/v1.2`), so the split is found, not
/// assumed: the longest leading run of segments that names a branch, tag or
/// commit wins, and the rest is the file path. A ref may also arrive as one
/// segment with its slashes percent-encoded (`release%2Fv1.2`); that decodes
/// to the same string. Only plain ref names and object ids are accepted here,
/// never revision expressions such as `main~1` or `main:file`.
pub(in crate::web) async fn repo_raw_path(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath((id, rest)): AxumPath<(String, String)>,
) -> AxumResponse {
    let response = raw_path_response(&state, &id, &rest);
    with_raw_headers(response.0, response.1.as_deref())
}

fn raw_path_response(state: &WebState, id: &str, rest: &str) -> (AxumResponse, Option<String>) {
    let Some(repo) = find_repo(state, id) else {
        return (
            api_error(StatusCode::NOT_FOUND, "not_found", "repository not found"),
            None,
        );
    };
    let segments: Vec<&str> = rest.split('/').collect();
    if rest.contains('\0') || segments.iter().any(|part| matches!(*part, "." | "..")) {
        return (
            api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                "path may not contain . or .. segments or NUL bytes",
            ),
            None,
        );
    }
    if segments.len() < 2 || segments.iter().any(|part| part.is_empty()) {
        return (
            api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                "raw path is /raw/{ref}/{path}: both a ref and a file path are required",
            ),
            None,
        );
    }
    let bare = match state.repo_manager.open_parts(&repo.owner, &repo.name) {
        Ok(bare) => bare,
        Err(_) => {
            return (
                api_error(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "repository storage not found",
                ),
                None,
            );
        }
    };
    let refs = RefService::new((*state.repo_manager).clone());
    let longest = (segments.len() - 1).min(MAX_REF_SEGMENTS);
    for split in (1..=longest).rev() {
        let ref_name = segments[..split].join("/");
        if !plain_ref_name(&ref_name) {
            continue;
        }
        let commit = match refs.resolve_commit(&bare, &ref_name) {
            Ok(Some(commit)) => commit,
            Ok(None) => continue,
            Err(err) => {
                return (
                    api_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "git_source_failed",
                        &format!("resolve ref: {err}"),
                    ),
                    None,
                );
            }
        };
        let path = segments[split..].join("/");
        if let Err(response) = normalize_git_path(Some(&path)) {
            return (*response, None);
        }
        return match git_blob_bytes(state, &repo, &commit, &path) {
            Ok((bytes, mime)) => (
                ([(header::CONTENT_TYPE, mime)], bytes).into_response(),
                Some(path),
            ),
            Err(response) => (*response, None),
        };
    }
    (
        api_error(StatusCode::NOT_FOUND, "not_found", "ref not found"),
        None,
    )
}

/// A branch or tag name, or an object id: what a URL segment may name. Git's
/// own rules forbid these characters in ref names; refusing them here also
/// keeps revision syntax (`~`, `^`, `:`, `@{`) and option-like `-x` away
/// from `rev-parse`.
fn plain_ref_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with(['.', '-'])
                && !part.ends_with(".lock")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ref_names_exclude_revision_syntax() {
        for good in ["main", "release/v10.1.6-jope", "v1.2.3", "a1b2c3d", "x+y"] {
            assert!(plain_ref_name(good), "{good}");
        }
        for bad in [
            "",
            "-p",
            "main~1",
            "main^",
            "main:x",
            "HEAD@{1}",
            ".hidden",
            "a//b",
            "a.lock",
            "main/../x",
            "white space",
        ] {
            assert!(!plain_ref_name(bad), "{bad}");
        }
    }

    #[test]
    fn disposition_keeps_the_name_and_quotes_safely() {
        let value = inline_disposition("docs/a \"b\".png").unwrap();
        assert_eq!(
            value.to_str().unwrap(),
            "inline; filename=\"a__b_.png\"; filename*=UTF-8''a%20%22b%22.png"
        );
        assert!(inline_disposition("docs/").is_none());
    }
}
