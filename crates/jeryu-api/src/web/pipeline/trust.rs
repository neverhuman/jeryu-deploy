//! Who may write the event kinds other projections believe.
//!
//! Most stored events are telemetry: a reader sees them on the Activity page
//! and nothing acts on them. A few are read as facts about the forge itself,
//! so whoever can write one can decide what the forge reports:
//!
//! * `main.status` decides whether a repository's default branch is red
//!   (`super::main_health`, the Repositories listing, the attention inbox).
//!   Only the gate runner's heartbeat produces one, through
//!   [`super::emit`], which stores it as the forge
//!   (`reporter = source = "forge"`).
//! * `incident.caused_by` blames changes for an incident. The fold writes
//!   never-pruned `incident_causes` rows, fails the change for the change
//!   outcomes, and links a `defect.linked` against each pull request the key
//!   resolves to (`crate::web::incidents`, `crate::web::human_time`,
//!   `crate::web::review_catch`).
//!
//! `POST /api/v1/events` stores what any `JERYU_EVENT_REPORTERS` login (or
//! any global admin) sends, with `reporter` set to that login. That is the
//! right rule for telemetry and the wrong one for the two above, so this
//! module holds the narrower rules and the readers filter on `reporter`
//! rather than trusting a row because of its `kind`.
//!
//! Contract: `docs/pipeline-events.md`, "Reserved kinds".

use jeryu_core::{AccountSummary, UserRole};

/// The `reporter` and `source` of an event the forge emitted itself.
pub(crate) const FORGE: &str = "forge";

/// Logins that may blame changes for an incident without being a global
/// admin (`JERYU_INCIDENT_REPORTERS`, comma-separated). No default: unset,
/// only global admins may post `incident.caused_by`.
const INCIDENT_REPORTERS_ENV: &str = "JERYU_INCIDENT_REPORTERS";

/// Kinds only the forge itself may store, sorted.
///
/// Each one is a fact the forge reads back about its own repositories, so a
/// row of this kind is believed because of where it came from: the gate
/// verdicts behind a red main and the escaped-to-main defects charged from
/// them. Other kinds the forge emits are read as telemetry, and a producer
/// may post them (`gate.finished` from a gate runner's own tooling, the agent
/// reviewer's `review.finding`); reserving those would refuse posts the
/// contract allows today.
///
/// `docs/pipeline-events.md`, "Reserved kinds", publishes this list; a test
/// holds the two together.
pub(crate) const FORGE_ONLY: &[&str] = &["main.escaped", "main.status"];

/// Whether `kind` may only be stored by the forge itself.
pub(crate) fn forge_only(kind: &str) -> bool {
    FORGE_ONLY.binary_search(&kind).is_ok()
}

fn reporter_list(configured: &str) -> Vec<String> {
    configured
        .split(',')
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .map(str::to_string)
        .collect()
}

static INCIDENT_REPORTERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

fn incident_reporters() -> &'static [String] {
    INCIDENT_REPORTERS
        .get_or_init(|| reporter_list(&std::env::var(INCIDENT_REPORTERS_ENV).unwrap_or_default()))
}

/// Seed the incident-reporter list without reading the environment, so a test
/// states the site it is testing instead of depending on one.
#[cfg(test)]
pub(crate) fn configure_incident_reporters_for_tests(logins: &[&str]) {
    let _ = INCIDENT_REPORTERS.set(logins.iter().map(|login| (*login).to_owned()).collect());
}

/// Whether `account` may blame a change for an incident: a global admin, or a
/// login the installation named as incident tooling.
pub(crate) fn may_blame(account: &AccountSummary) -> bool {
    account.role == UserRole::Admin
        || incident_reporters()
            .iter()
            .any(|login| login == &account.login)
}

/// Why `account` may not post an event of `kind`, as the error `code`, the
/// `reason` and the `repair_hint` the route answers with.
pub(crate) struct Refusal {
    pub code: &'static str,
    pub reason: String,
    pub hint: &'static str,
}

/// The refusal for an event this account may not write, or `None` when the
/// ordinary reporter rule is enough. Every route that stores an event a
/// reporter described asks this: `POST /api/v1/events`, the incident form and
/// the alert webhook.
pub(crate) fn refusal(account: &AccountSummary, kind: &str) -> Option<Refusal> {
    if forge_only(kind) {
        return Some(Refusal {
            code: "events_kind_reserved",
            reason: format!("{kind} is the forge's own record of what it did and cannot be posted"),
            hint: "report through the route or heartbeat that makes the forge emit this kind",
        });
    }
    if kind == crate::web::incidents::CAUSED_BY && !may_blame(account) {
        return Some(Refusal {
            code: "incident_blame_reserved",
            reason: format!(
                "{kind} charges a defect against the changes it names; this account may not blame changes (JERYU_INCIDENT_REPORTERS)"
            ),
            hint: "post the incident without detail.changes, or blame it from an incident-tooling identity",
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reserved_list_is_sorted() {
        let mut sorted = FORGE_ONLY.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            FORGE_ONLY,
            sorted.as_slice(),
            "FORGE_ONLY must stay sorted: a lookup is a binary search"
        );
    }

    #[test]
    fn the_trusted_kinds_are_reserved_and_an_unknown_kind_is_not() {
        assert!(forge_only("main.status"));
        assert!(forge_only("main.escaped"));
        assert!(!forge_only("gate.log"));
        assert!(!forge_only("gate.finished"));
        assert!(!forge_only("todo.claimed"));
        assert!(!forge_only("incident.caused_by"));
    }

    fn account(login: &str, role: UserRole) -> AccountSummary {
        AccountSummary {
            login: login.to_string(),
            display_name: login.to_string(),
            role,
            status: jeryu_core::AccountStatus::Active,
            auth_epoch: 0,
            must_change_password: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn a_reporter_may_not_post_a_reserved_kind_but_may_post_telemetry() {
        let reporter = account("ci-bot", UserRole::User);
        let refused = refusal(&reporter, "main.status").expect("main.status is reserved");
        assert_eq!(refused.code, "events_kind_reserved");
        assert!(refusal(&reporter, "gate.log").is_none());
        // An admin identity is no gate runner either.
        assert!(refusal(&account("root", UserRole::Admin), "main.status").is_some());
    }
}
