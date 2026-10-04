//! The web handlers for agent discovery: the capability manifest's prefixed
//! path, the documentation pages the error bodies advertise, and the OpenAPI
//! document. What the advertised URLs mean lives in [`crate::discovery`].

use axum::Json;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response as AxumResponse};
use serde_json::{Value, json};

pub(crate) use crate::discovery::{
    CAPABILITIES_PATH, DOCS_PATH, FIRST_CAPABILITIES_PATH, OPENAPI_PATH, REST_DOC_PATH,
};
use crate::discovery::{REST_DOC_PAGE, page, pages, rest_document};

/// `GET /api/v1/docs`: the pages this build serves.
pub(crate) async fn index() -> Json<Value> {
    Json(json!({
        "schema": "jeryu.api.docs.v1",
        "capabilities": CAPABILITIES_PATH,
        "openapi": OPENAPI_PATH,
        "routes": super::route_index::INDEX_PATH,
        "errors": "/api/v1/errors",
        "rest_edge": REST_DOC_PATH,
        "pages": pages(),
    }))
}

/// `GET /api/v1/docs/{page}`: one embedded markdown page as `text/markdown`,
/// or the REST edge's own document as JSON.
pub(crate) async fn doc_page(Path(requested): Path<String>) -> AxumResponse {
    if requested.trim_matches('/') == REST_DOC_PAGE {
        return Json(rest_document()).into_response();
    }
    match page(&requested) {
        Some(doc) => (
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            doc.markdown,
        )
            .into_response(),
        None => super::workcells_support::typed_error(super::workcells_support::TypedError {
            status: StatusCode::NOT_FOUND,
            code: "api_route_not_found",
            purpose: "read a jeryu documentation page",
            reason: &format!("no documentation page is served at {DOCS_PATH}/{requested}"),
            common_fixes: &[
                "list the served pages with GET /api/v1/docs",
                "name the page by its repository path, for example errors.md",
            ],
            docs_url: DOCS_PATH,
            repair_hint: "list the pages with GET /api/v1/docs and retry with one of them",
            message: "documentation page not found",
        }),
    }
}
