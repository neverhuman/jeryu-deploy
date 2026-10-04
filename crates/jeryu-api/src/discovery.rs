//! Agent discovery that resolves: where the capability manifest lives, which
//! documentation pages are served, and what the URLs in an error body mean.
//!
//! Every error body hands a confused agent two URLs: a capability manifest and
//! a `docs_url`. Both used to miss — the manifest sat on `/.jeryu/capabilities`,
//! a path the edge does not route, and the docs pointed at repository-relative
//! markdown (`docs/errors.md`) or at `/docs/rest`, which the web app answered
//! with HTML and a 200. Both now live under the `/api/v1` prefix the edge
//! already routes:
//!
//! - `GET /api/v1/capabilities` — the manifest (`/.jeryu/capabilities` stays
//!   mounted for clients that already learned it);
//! - `GET /api/v1/docs` — what pages are served, and `GET /api/v1/docs/<page>`
//!   the repository markdown itself, embedded in the binary at build time;
//! - `GET /api/v1/docs/rest` — the GitHub-compatible edge's own document;
//! - `GET /api/v1/openapi.json` — the OpenAPI document built from the routers.
//!
//! The web handlers are in `web::discovery`; what a URL means is here, so the
//! GitHub edge (which compiles without the `web` feature) shares it.

use serde_json::{Value, json};

/// Where the capability manifest is served, under the API prefix the edge
/// routes. Error bodies point here.
pub const CAPABILITIES_PATH: &str = "/api/v1/capabilities";
/// The manifest's first path, still mounted for clients that learned it.
pub const FIRST_CAPABILITIES_PATH: &str = "/.jeryu/capabilities";
/// The typed route index for the primary API.
pub const TYPED_INDEX_PATH: &str = "/api/v1";
pub const DOCS_PATH: &str = "/api/v1/docs";
/// The GitHub-compatible edge's document; the edge's `documentation_url`.
pub const REST_DOC_PATH: &str = "/api/v1/docs/rest";
pub const OPENAPI_PATH: &str = "/api/v1/openapi.json";
/// The page [`REST_DOC_PATH`] ends in, a document this module renders rather
/// than one of the embedded markdown pages.
pub(crate) const REST_DOC_PAGE: &str = "rest";
/// The anchor in the errors page that explains the gh-auth steering refusal.
pub(crate) const GH_AUTH_DOCS: &str = "/api/v1/docs/errors.md#github-cli-auth-steering";

pub(crate) struct EmbeddedDoc {
    pub(crate) path: &'static str,
    pub(crate) markdown: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/embedded_docs.rs"));

/// The embedded page a doc reference names, by its repository-relative path
/// with or without the `.md` suffix.
pub(crate) fn page(name: &str) -> Option<&'static EmbeddedDoc> {
    let name = name.trim_matches('/');
    DOCS.iter().find(|doc| {
        doc.path == name
            || doc
                .path
                .strip_suffix(".md")
                .is_some_and(|stem| stem == name)
    })
}

/// Every page this build serves, with the URL it is served at.
pub(crate) fn pages() -> Vec<Value> {
    DOCS.iter()
        .map(|doc| json!({ "page": doc.path, "url": format!("{DOCS_PATH}/{}", doc.path) }))
        .collect()
}

/// A URL that resolves for whatever an error body advertises as its
/// `docs_url`.
///
/// A repository-relative markdown reference (`docs/errors.md#not-found`) and
/// the edge's `/docs/rest` both become their served `/api/v1/docs/...` URL,
/// anchor kept. A reference naming no page this build serves becomes the docs
/// index, which lists the pages it does: an advertised URL that answers 404 is
/// no better than one that answers the web app's HTML. Anything that is not a
/// docs reference (the route index at `/api/v1`, say) is left alone.
pub fn docs_url(raw: &str) -> String {
    let raw = raw.trim();
    let (reference, anchor) = raw.split_once('#').unwrap_or((raw, ""));
    let with_anchor = |url: String| {
        if anchor.is_empty() {
            url
        } else {
            format!("{url}#{anchor}")
        }
    };
    let Some(name) = ["/api/v1/docs/", "/docs/", "docs/"]
        .iter()
        .find_map(|prefix| reference.strip_prefix(*prefix))
    else {
        return raw.to_string();
    };
    if name == REST_DOC_PAGE {
        return with_anchor(REST_DOC_PATH.to_string());
    }
    match page(name) {
        Some(doc) => with_anchor(format!("{DOCS_PATH}/{}", doc.path)),
        None => DOCS_PATH.to_string(),
    }
}

/// The GitHub-compatible edge's document: what it serves, how to authenticate,
/// and the typed Jeryu paths that are faster. Built from the same route list
/// the edge's index and its 404 answer with, so the three cannot disagree.
pub(crate) fn rest_document() -> Value {
    json!({
        "schema": "jeryu.api.docs.rest.v1",
        "title": "Jeryu GitHub-compatible REST edge",
        "summary": "A guided subset of the GitHub REST API, served under /api/v3 \
                    (and unprefixed) by the in-process Jeryu forge, so the real gh \
                    CLI and GitHub clients work against it.",
        "index": "/api/v3",
        "capabilities": CAPABILITIES_PATH,
        "openapi": OPENAPI_PATH,
        "jeryu_api_routes": crate::github::V3_ROUTES,
        "auth": {
            "schemes": ["Bearer <token>", "Basic <login>:<token>"],
            "host_setup": crate::github::GH_SETUP_COMMAND,
            "boundary": crate::github::GH_AUTH_BOUNDARY,
        },
        "faster_paths": {
            "typed_api": TYPED_INDEX_PATH,
            "mcp": "/mcp",
            "first_contact": "/.jeryu/agents/first-contact",
        },
        "errors": "/api/v1/errors",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_are_embedded_and_addressable_with_or_without_the_suffix() {
        assert!(!DOCS.is_empty(), "the build embeds the repository docs");
        assert!(page("errors.md").is_some());
        assert_eq!(page("errors").map(|doc| doc.path), Some("errors.md"));
        assert!(page("no-such-page").is_none());
    }

    #[test]
    fn docs_url_resolves_every_shape_an_error_body_carries() {
        assert_eq!(
            docs_url("docs/errors.md#not-found"),
            "/api/v1/docs/errors.md#not-found"
        );
        assert_eq!(
            docs_url("docs/pipeline-events.md"),
            "/api/v1/docs/pipeline-events.md"
        );
        assert_eq!(docs_url("/docs/rest"), REST_DOC_PATH);
        assert_eq!(docs_url("/api/v1/docs/errors.md"), "/api/v1/docs/errors.md");
        // A page no build serves resolves to the index that lists the ones it does.
        assert_eq!(docs_url("/docs/api/ci-run-evidence"), DOCS_PATH);
        // Not a docs reference: left alone.
        assert_eq!(docs_url(TYPED_INDEX_PATH), TYPED_INDEX_PATH);
    }

    #[test]
    fn the_rest_document_names_the_served_routes_and_the_faster_paths() {
        let document = rest_document();
        assert_eq!(document["capabilities"], CAPABILITIES_PATH);
        let routes = document["jeryu_api_routes"].as_array().expect("routes");
        assert!(
            routes
                .iter()
                .any(|route| route.as_str() == Some("GET /user")),
            "{routes:?}"
        );
    }
}
