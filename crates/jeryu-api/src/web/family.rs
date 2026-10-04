//! One canonical family key for the whole jeryu API.
//!
//! A product family has exactly one key on the wire: a lowercase slug with no
//! suffix (`veox`, `jekko`). The split manifest of a family spells its own
//! name with a `-split` suffix (`veox-split`) because that is the name of the
//! directory the repos are split into, so the same family used to reach the
//! API under two spellings depending on which endpoint answered. Every
//! endpoint that takes a family now accepts either spelling on input
//! ([`canonical`]) and answers with the canonical key plus [`label`], the
//! string a reader is shown. A client renders `label` and filters by the key;
//! it never has to strip a suffix or guess which spelling an endpoint wants.
//!
//! The endpoints that speak family: `GET /api/v1/repos` (and its `families`
//! facet, whose entries are canonical keys), `GET /api/v1/attention`,
//! `GET /api/v1/events`, `GET /api/v1/shift/todos`, `GET /api/v1/shift/shifts`,
//! `GET /api/v1/shift/families`, and `/api/v1/release-board[/:family]`.
//!
//! A family nobody hosts is a typed client error ([`UNKNOWN_CODE`]), never an
//! empty 200: a typo'd filter must not read as "nothing to do". The status is
//! the one this API gives every unreadable request: `error_envelope` rewrites
//! a 400 to 422, so the code is what a client matches on.

use std::collections::BTreeSet;

use axum::http::StatusCode;
use axum::response::Response as AxumResponse;

use super::WebState;
use super::workcells_support::{TypedError, typed_error};

/// The suffix a split manifest adds to its family's name. Accepted on input,
/// never returned.
pub(crate) const ALIAS_SUFFIX: &str = "-split";
pub(crate) const UNKNOWN_CODE: &str = "family_unknown";
const DOCS: &str = "docs/family-key.md";

/// The canonical key of a family, whichever spelling the caller used.
pub(crate) fn canonical(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    match trimmed.strip_suffix(ALIAS_SUFFIX) {
        Some(stem) if !stem.is_empty() => stem.to_string(),
        _ => trimmed,
    }
}

/// What a reader is shown for a family. Today a family's key is also its
/// name, so the label is the key; clients render this field so that a future
/// family whose name is not its key needs no client change.
pub(crate) fn label(key: &str) -> String {
    canonical(key)
}

/// The same, for a field that may be absent.
pub(crate) fn optional_label(key: Option<&String>) -> Option<String> {
    key.map(|key| label(key))
}

/// Every family key the forge knows: the repositories it hosts, the todo
/// queues it serves, the release boards collectors posted, and the families
/// the pipeline log has events for.
pub(crate) fn known(state: &WebState) -> BTreeSet<String> {
    let mut families = BTreeSet::new();
    for repo in state.github.core().list_repositories(None) {
        if let Some(family) = super::repositories::effective_family(state, &repo) {
            families.insert(family);
        }
    }
    for queue in super::shift::queue::discover(&state.repo_manager) {
        families.insert(canonical(&queue.family.name));
    }
    for board in state.release_boards.list() {
        families.insert(canonical(&board.family));
    }
    families.extend(
        state
            .events
            .families()
            .unwrap_or_default()
            .iter()
            .map(|family| canonical(family)),
    );
    families
}

/// The typed 400 for a family the forge does not know.
pub(crate) fn unknown(state: &WebState, raw: &str) -> AxumResponse {
    let known: Vec<String> = known(state).into_iter().collect();
    let key = canonical(raw);
    let reason = format!("no family {raw:?} ({key:?}): the forge knows {known:?}");
    typed_error(TypedError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: UNKNOWN_CODE,
        purpose: "filter a list by product family",
        reason: &reason,
        common_fixes: &[
            "list the families with GET /api/v1/shift/families",
            "read the family keys off the repositories facet (GET /api/v1/repos)",
        ],
        docs_url: DOCS,
        repair_hint: "send one of the families named in the message, or drop the filter",
        message: &reason,
    })
}

/// The canonical key `raw` names, or a typed 400 when the forge hosts no such
/// family. Use for a family the caller must name (a path segment, a body).
pub(crate) fn require(state: &WebState, raw: &str) -> Result<String, Box<AxumResponse>> {
    let key = canonical(raw);
    if key.is_empty() || !known(state).contains(&key) {
        return Err(Box::new(unknown(state, raw)));
    }
    Ok(key)
}

/// The canonical key of an optional `?family=` filter: `None` when the filter
/// is absent or empty, a typed 400 when it names no family.
pub(crate) fn filter(
    state: &WebState,
    raw: Option<&str>,
) -> Result<Option<String>, Box<AxumResponse>> {
    match raw.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => Ok(None),
        Some(raw) => require(state, raw).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_split_suffix_is_an_alias_of_the_same_key() {
        assert_eq!(canonical("veox-split"), "veox");
        assert_eq!(canonical("veox"), "veox");
        assert_eq!(canonical("  Veox-Split "), "veox");
        assert_eq!(label("veox-split"), "veox");
    }

    #[test]
    fn a_family_actually_named_split_keeps_its_name() {
        assert_eq!(canonical("-split"), "-split");
        assert_eq!(canonical("split"), "split");
    }
}
