//! How much is waiting in each family, counted the way `/needs-you` draws it.
//!
//! The inbox is paged, so a client that only reads page 1 cannot count a
//! family's rows from the items in hand: on 2026-10-19 the jeryu family had
//! 50 rows waiting while the page, holding the first 100 of 169 items across
//! every family, showed 17. `families` answers the count in the same response,
//! over the whole inbox, whatever the filter or page.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{Counts, Item, Severity};

/// One family's waiting rows, by their worst severity.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct FamilyCounts {
    /// Canonical family key; null for the forge's own rows (a runner, a
    /// mirror) that belong to no family.
    pub family: Option<String>,
    pub family_label: Option<String>,
    /// Subjects, not items: every item about one pull request, todo or shift
    /// is one row on the page and one here (see [`subject_key`]).
    pub subjects: Counts,
}

/// What a row is about, so that every item raised by one cause is counted
/// once: a pull request, else the todo, else the shift, else the item itself.
/// The same rule as jeryu-web's `subjectKey`, which draws one row per key.
pub(super) fn subject_key(item: &Item) -> String {
    let present = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let family = present(&item.family).unwrap_or_default();
    if let (Some(repo), Some(pr)) = (present(&item.repo), item.pr) {
        return format!("pr:{repo}#{pr}");
    }
    if let Some(todo) = present(&item.todo_id) {
        return format!("todo:{family}:{todo}");
    }
    if let Some(shift) = present(&item.shift) {
        return format!("shift:{family}:{shift}");
    }
    format!("item:{}", item.id)
}

/// Every family with something waiting, named families by key and the
/// forge's own rows last. A subject takes the worst severity of its items.
pub(crate) fn family_summary(items: &[Item]) -> Vec<FamilyCounts> {
    let mut subjects: BTreeMap<String, (Option<String>, Severity)> = BTreeMap::new();
    for item in items {
        subjects
            .entry(subject_key(item))
            .and_modify(|(_, worst)| *worst = (*worst).min(item.severity))
            .or_insert_with(|| (item.family.clone(), item.severity));
    }
    let mut families: BTreeMap<Option<String>, Counts> = BTreeMap::new();
    for (family, severity) in subjects.into_values() {
        let counts = families.entry(family).or_default();
        match severity {
            Severity::Critical => counts.critical += 1,
            Severity::Action => counts.action += 1,
            Severity::Watch => counts.watch += 1,
        }
    }
    let (forge, named): (Vec<_>, Vec<_>) = families
        .into_iter()
        .partition(|(family, _)| family.is_none());
    named
        .into_iter()
        .chain(forge)
        .map(|(family, subjects)| FamilyCounts {
            family_label: crate::web::family::optional_label(family.as_ref()),
            family,
            subjects,
        })
        .collect()
}
