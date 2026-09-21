//! Paging for the large `/api/v1` collections.
//!
//! Every paged route accepts `limit` (or its alias `per_page`) and a 1-based
//! `page`, bounds its collection with [`DEFAULT_LIMIT`] when no limit is sent,
//! and echoes what it applied in a `page` object so a caller can tell its
//! request was honoured. A value outside the accepted range is refused with
//! `invalid_page_parameter` rather than clamped.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use serde::{Deserialize, Serialize};

use super::workcells_support::{TypedError, typed_error};

/// Rows a paged collection returns when the request names no limit.
pub(crate) const DEFAULT_LIMIT: usize = 100;
/// The largest `limit` / `per_page` a request may ask for.
pub(crate) const MAX_LIMIT: usize = 500;

/// The paging query parameters. Read as text so a malformed value answers
/// with the paging error instead of a generic query rejection.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct PageParams {
    pub limit: Option<String>,
    pub per_page: Option<String>,
    pub page: Option<String>,
}

/// A validated page request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Page {
    pub limit: usize,
    pub page: usize,
}

/// What a paged response applied, returned alongside the rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PageInfo {
    pub limit: usize,
    pub page: usize,
    /// Rows matching the request before paging.
    pub total: usize,
    pub has_more: bool,
}

fn number(name: &str, raw: &str, max: usize) -> Result<usize, String> {
    let value: usize = raw
        .trim()
        .parse()
        .map_err(|_| format!("{name} must be a whole number from 1 to {max}, got {raw:?}"))?;
    if value == 0 || value > max {
        return Err(format!(
            "{name} must be from 1 to {max}, got {value}; it is not clamped"
        ));
    }
    Ok(value)
}

impl PageParams {
    pub(crate) fn resolve(&self) -> Result<Page, String> {
        let limit = self
            .limit
            .as_deref()
            .map(|raw| number("limit", raw, MAX_LIMIT))
            .transpose()?;
        let per_page = self
            .per_page
            .as_deref()
            .map(|raw| number("per_page", raw, MAX_LIMIT))
            .transpose()?;
        let limit = match (limit, per_page) {
            (Some(a), Some(b)) if a != b => {
                return Err(format!(
                    "limit ({a}) and per_page ({b}) name the same thing; send one"
                ));
            }
            (a, b) => a.or(b).unwrap_or(DEFAULT_LIMIT),
        };
        let page = self
            .page
            .as_deref()
            .map(|raw| number("page", raw, usize::MAX))
            .transpose()?
            .unwrap_or(1);
        Ok(Page { limit, page })
    }

    /// [`Self::resolve`], answering the paging error on failure.
    pub(crate) fn page(&self) -> Result<Page, PageRejection> {
        self.resolve().map_err(PageRejection)
    }
}

impl Page {
    /// Cut `items` down to this page.
    pub(crate) fn apply<T>(self, items: Vec<T>) -> (Vec<T>, PageInfo) {
        let total = items.len();
        let start = (self.page - 1).saturating_mul(self.limit).min(total);
        let rows: Vec<T> = items.into_iter().skip(start).take(self.limit).collect();
        let info = PageInfo {
            limit: self.limit,
            page: self.page,
            total,
            has_more: start + rows.len() < total,
        };
        (rows, info)
    }
}

/// A refused paging request; answers `invalid_page_parameter`.
#[derive(Debug)]
pub(crate) struct PageRejection(pub String);

impl IntoResponse for PageRejection {
    fn into_response(self) -> AxumResponse {
        invalid_page(&self.0)
    }
}

pub(crate) fn invalid_page(reason: &str) -> AxumResponse {
    typed_error(TypedError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: "invalid_page_parameter",
        purpose: "page through a collection",
        reason,
        common_fixes: &[
            "send limit (or per_page) from 1 to 500 and page from 1",
            "read the page object of the previous response for the applied values",
        ],
        docs_url: "docs/errors.md#invalid-page-parameter",
        repair_hint: "retry with an in-range limit and page",
        message: reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(limit: Option<&str>, per_page: Option<&str>, page: Option<&str>) -> PageParams {
        PageParams {
            limit: limit.map(str::to_string),
            per_page: per_page.map(str::to_string),
            page: page.map(str::to_string),
        }
    }

    #[test]
    fn defaults_bound_the_collection() {
        let page = params(None, None, None).resolve().unwrap();
        assert_eq!(
            page,
            Page {
                limit: DEFAULT_LIMIT,
                page: 1
            }
        );
        let (rows, info) = page.apply((0..250).collect());
        assert_eq!(rows.len(), DEFAULT_LIMIT);
        assert_eq!(info.total, 250);
        assert!(info.has_more);
    }

    #[test]
    fn per_page_is_an_alias_and_pages_walk_the_rows() {
        let page = params(None, Some("10"), Some("3")).resolve().unwrap();
        let (rows, info) = page.apply((0..25).collect::<Vec<_>>());
        assert_eq!(rows, (20..25).collect::<Vec<_>>());
        assert!(!info.has_more);
        let (rows, info) = Page { limit: 10, page: 9 }.apply((0..25).collect::<Vec<_>>());
        assert!(rows.is_empty());
        assert!(!info.has_more);
        assert!(params(Some("10"), Some("10"), None).resolve().is_ok());
    }

    #[test]
    fn out_of_range_values_are_refused_not_clamped() {
        for (limit, per_page, page) in [
            (Some("0"), None, None),
            (Some("501"), None, None),
            (Some("ten"), None, None),
            (None, Some("-1"), None),
            (None, None, Some("0")),
            (Some("10"), Some("20"), None),
        ] {
            assert!(
                params(limit, per_page, page).resolve().is_err(),
                "{limit:?} {per_page:?} {page:?}"
            );
        }
    }
}
