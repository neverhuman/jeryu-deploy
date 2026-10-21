//! What a person who is not an administrator reads from the attention inbox.
//!
//! The whole inbox names pull requests, hosts and todos of every repository,
//! so it stays an administrator's. A person with write access to a family's
//! code reads that family's blocked and handed-off todos, scoped the way
//! `shift/access.rs` scopes who may work on a todo; no other row, and no
//! family they cannot write to, reaches them.

use std::collections::BTreeSet;

use super::{AttentionResponse, Counts, Item, Severity};
use crate::web::WebState;
use crate::web::shift::access;

/// The kinds a person with write access may read: the todos waiting on them.
const PERSON_KINDS: &[&str] = &["todo_blocked", "todo_handoff"];

/// `whole` cut down to what `login` may read, counts recounted for it.
pub(super) fn for_person(
    state: &WebState,
    whole: AttentionResponse,
    login: &str,
) -> AttentionResponse {
    let candidates = |item: &Item| PERSON_KINDS.contains(&item.kind);
    let wanted: BTreeSet<(String, String)> = whole
        .items
        .iter()
        .filter(|item| candidates(item))
        .filter_map(|item| Some((item.family.clone()?, item.todo_id.clone()?)))
        .collect();
    let allowed = access::workable_todos(state, login, &wanted);
    let items: Vec<Item> = whole
        .items
        .into_iter()
        .filter(|item| candidates(item))
        .filter(|item| match (&item.family, &item.todo_id) {
            (Some(family), Some(id)) => allowed.contains(&(family.clone(), id.clone())),
            _ => false,
        })
        .collect();
    let count = |severity| {
        items
            .iter()
            .filter(|item| item.severity == severity)
            .count()
    };
    AttentionResponse {
        counts: Counts {
            critical: count(Severity::Critical),
            action: count(Severity::Action),
            watch: count(Severity::Watch),
        },
        items,
        ..whole
    }
}
