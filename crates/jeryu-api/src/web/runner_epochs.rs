//! Work-cell runner lifetimes this daemon issued.
//!
//! `jeryu-runnerd` fences every work-cell claim on a runner lifetime the daemon
//! admitted: `WorkcellManager::sync_runner_fence` is the fence's only writer,
//! and a claim's own epoch is compared against that table but never written
//! into it. The fence fails closed, so a runner with no admitted lifetime
//! claims nothing. In the runner daemon the table is fed from the node
//! registry; the forge keeps its own work-cell ledger and is the daemon for it,
//! so this registry is what issues those epochs here.
//!
//! A runner takes an epoch from `POST /api/v1/workcells/runners` before it may
//! claim, and every claim entry point syncs the ledger from this table first.
//! Issuing again starts a new lifetime: the epoch rises, so a claim still in
//! flight from the runner's previous lifetime is fenced instead of taking a
//! cell. `DELETE /api/v1/workcells/runners/{runner_id}` (the id may contain
//! `/`) drops a lifetime, which fences it and frees its slot. The whole
//! `/api/v1/workcells` surface is admin-only (`web::auth::admin_only_path`).
//!
//! The table lives beside the in-memory ledger it fences and is lost with it on
//! restart, so a restarted forge admits no lifetime and holds no cell: a claim
//! from before the restart is fenced rather than served a freshly minted cell.
//! Epochs are never reused within a process, and each one is at least the wall
//! clock in milliseconds, so across restarts they rise too unless the clock
//! steps back.
//!
//! When the table is full a new runner takes the slot of the oldest lifetime
//! that holds no live cell and was admitted at least [`EVICTION_GRACE`] ago,
//! so runners that went away without being dropped cannot wedge admission.
//!
//! This host's own lifetime ([`HOST_RUNNER_ID`]) is separate: it fences only
//! the warm session pool, which is this process's, and never enters the
//! work-cell ledger's fence, so no claim through `/api/v1/workcells` can use it.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How many reported runner lifetimes the registry holds at once.
pub(crate) const MAX_ADMITTED_RUNNERS: usize = 256;

/// How long a new lifetime is safe from eviction. A runner admitted moments
/// ago holds no cell yet because it has not sent its first claim, which does
/// not make it gone.
pub(crate) const EVICTION_GRACE: Duration = Duration::from_secs(600);

/// Longest runner id the registry admits. The id keys the fence and is copied
/// into every lease it claims, so it stays short and plain.
pub(crate) const MAX_RUNNER_ID_LEN: usize = 128;

/// The in-process runner the forge claims warm session cells as. A new session
/// claims from this host's own warm pool, so the lifetime that owns those cells
/// is this process's, not the `runner` label the request carries. The id is
/// reserved: no reported runner may be admitted, or dropped, under it.
pub(crate) const HOST_RUNNER_ID: &str = "forge-host";

/// `true` when `runner_id` may key the claim fence: a short, non-empty id of
/// ASCII letters, digits and `-`, `_`, `.`, `/` (hosts name slots `node-2/slot0`),
/// which neither starts nor ends at a `/`, has no empty segment, carries no
/// `.` or `..` segment, and is not the reserved [`HOST_RUNNER_ID`]. The id
/// reaches no path, but it is copied into leases, reported, used as an audit
/// subject and named in `DELETE /api/v1/workcells/runners/{runner_id}`, so it
/// stays plain.
pub(crate) fn valid_runner_id(runner_id: &str) -> bool {
    !runner_id.is_empty()
        && runner_id != HOST_RUNNER_ID
        && runner_id.len() <= MAX_RUNNER_ID_LEN
        && runner_id
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
        && runner_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// What [`RunnerEpochRegistry::issue`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Issued {
    /// The epoch the runner's claims must now carry.
    pub(crate) epoch: u64,
    /// The epoch this replaced when the runner already had a lifetime: that
    /// lifetime is fenced from here on.
    pub(crate) replaced_epoch: Option<u64>,
    /// An idle lifetime dropped to make room, when the registry was full.
    pub(crate) evicted: Option<(String, u64)>,
}

#[derive(Debug, Clone, Copy)]
struct Lifetime {
    epoch: u64,
    admitted_at: Instant,
}

#[derive(Debug)]
pub(crate) struct RunnerEpochRegistry {
    issued: BTreeMap<String, Lifetime>,
    /// This process's own lifetime, for the warm session pool only.
    host_epoch: u64,
    /// The lowest epoch the next lifetime may get. Each issue takes the larger
    /// of this and the wall clock in milliseconds, so epochs are never reused
    /// within a process and stay above an earlier process's.
    next_epoch: u64,
}

impl Default for RunnerEpochRegistry {
    fn default() -> Self {
        let host_epoch = clock_epoch();
        Self {
            issued: BTreeMap::new(),
            host_epoch,
            next_epoch: host_epoch.saturating_add(1),
        }
    }
}

/// The wall clock in milliseconds since the Unix epoch, at least 1. A clock
/// before the Unix epoch reads as 1, and the counter then still only rises.
fn clock_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
        .max(1)
}

impl RunnerEpochRegistry {
    /// Start a new lifetime for `runner_id`. `holds_cells` says whether a
    /// runner still holds a live cell in the ledger; when the registry is full
    /// the oldest lifetime for which it is `false`, admitted at least
    /// [`EVICTION_GRACE`] ago, is dropped to make room. `None` when the
    /// registry is full and no lifetime may be evicted.
    pub(crate) fn issue(
        &mut self,
        runner_id: &str,
        holds_cells: impl Fn(&str) -> bool,
    ) -> Option<Issued> {
        self.issue_at(runner_id, holds_cells, Instant::now())
    }

    fn issue_at(
        &mut self,
        runner_id: &str,
        holds_cells: impl Fn(&str) -> bool,
        now: Instant,
    ) -> Option<Issued> {
        let replaced_epoch = self.issued.get(runner_id).map(|lifetime| lifetime.epoch);
        let mut evicted = None;
        if replaced_epoch.is_none() && self.issued.len() >= MAX_ADMITTED_RUNNERS {
            let idle = self
                .issued
                .iter()
                .filter(|(id, lifetime)| {
                    now.saturating_duration_since(lifetime.admitted_at) >= EVICTION_GRACE
                        && !holds_cells(id)
                })
                .min_by_key(|(_, lifetime)| lifetime.epoch)
                .map(|(id, lifetime)| (id.clone(), lifetime.epoch))?;
            self.issued.remove(&idle.0);
            evicted = Some(idle);
        }
        let epoch = self.next_epoch.max(clock_epoch());
        self.next_epoch = epoch.saturating_add(1);
        self.issued.insert(
            runner_id.to_string(),
            Lifetime {
                epoch,
                admitted_at: now,
            },
        );
        Some(Issued {
            epoch,
            replaced_epoch,
            evicted,
        })
    }

    /// Drop `runner_id`'s lifetime, fencing it from any further claim.
    pub(crate) fn retire(&mut self, runner_id: &str) -> bool {
        self.issued.remove(runner_id).is_some()
    }

    /// Test-only: epoch `runner_id`'s claims must carry, if it has an admitted
    /// lifetime. Production reads the whole table through [`Self::admitted`].
    #[cfg(test)]
    pub(crate) fn epoch(&self, runner_id: &str) -> Option<u64> {
        self.issued.get(runner_id).map(|lifetime| lifetime.epoch)
    }

    /// Epoch this host's own warm session claims carry. It is not in
    /// [`Self::admitted`], so it never fences the work-cell ledger.
    pub(crate) fn host_epoch(&self) -> u64 {
        self.host_epoch
    }

    /// Test-only: age every lifetime by `by`, as if it had been admitted that
    /// much earlier, so a handler test can reach the eviction path.
    #[cfg(test)]
    pub(crate) fn backdate_all(&mut self, by: Duration) {
        for lifetime in self.issued.values_mut() {
            lifetime.admitted_at = lifetime
                .admitted_at
                .checked_sub(by)
                .unwrap_or(lifetime.admitted_at);
        }
    }

    /// Every reported runner lifetime this daemon admits, for the work-cell
    /// ledger's fence and the operator listing.
    pub(crate) fn admitted(&self) -> Vec<(String, u64)> {
        self.issued
            .iter()
            .map(|(runner_id, lifetime)| (runner_id.clone(), lifetime.epoch))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none_hold(_: &str) -> bool {
        false
    }

    fn all_hold(_: &str) -> bool {
        true
    }

    /// A registry with every slot taken, admitted at `at`.
    fn full_registry(at: Instant) -> RunnerEpochRegistry {
        let mut registry = RunnerEpochRegistry::default();
        for slot in 0..MAX_ADMITTED_RUNNERS {
            assert!(
                registry
                    .issue_at(&format!("r{slot}"), none_hold, at)
                    .is_some()
            );
        }
        registry
    }

    #[test]
    fn a_fresh_registry_admits_no_reported_runner_and_keeps_the_host_apart() {
        let registry = RunnerEpochRegistry::default();
        assert_eq!(registry.epoch("r1"), None);
        assert!(
            registry.admitted().is_empty(),
            "the host lifetime never enters the work-cell ledger's fence"
        );
        assert!(
            registry.host_epoch() > 1,
            "the host epoch comes from the clock"
        );
    }

    #[test]
    fn issuing_again_starts_a_new_lifetime_and_fences_the_previous_epoch() {
        let mut registry = RunnerEpochRegistry::default();
        let first = registry.issue("r1", none_hold).unwrap();
        assert_eq!(first.replaced_epoch, None);
        assert!(first.epoch > registry.host_epoch());
        let second = registry.issue("r1", none_hold).unwrap();
        assert!(second.epoch > first.epoch);
        assert_eq!(second.replaced_epoch, Some(first.epoch));
        assert_eq!(registry.epoch("r1"), Some(second.epoch));
        assert_eq!(registry.admitted().len(), 1);
    }

    #[test]
    fn an_epoch_is_never_reissued_after_a_lifetime_is_dropped() {
        let mut registry = RunnerEpochRegistry::default();
        let first = registry.issue("r1", none_hold).unwrap().epoch;
        assert!(registry.retire("r1"));
        let again = registry.issue("r1", none_hold).unwrap().epoch;
        assert!(
            again > first,
            "a claim from the dropped lifetime ({first}) must stay fenced"
        );
    }

    #[test]
    fn a_new_process_issues_epochs_above_an_earlier_one() {
        let mut earlier = RunnerEpochRegistry::default();
        let before = earlier.issue("r1", none_hold).unwrap().epoch;
        std::thread::sleep(Duration::from_millis(2));
        let restarted = RunnerEpochRegistry::default();
        assert!(
            restarted.host_epoch() > before,
            "the host lifetime rises too"
        );
    }

    #[test]
    fn a_retired_runner_loses_its_lifetime_and_its_slot() {
        let mut registry = RunnerEpochRegistry::default();
        registry.issue("r1", none_hold);
        assert!(registry.retire("r1"));
        assert_eq!(registry.epoch("r1"), None);
        assert!(!registry.retire("r1"), "retiring twice reports no lifetime");
    }

    #[test]
    fn a_full_table_evicts_the_oldest_idle_lifetime_past_its_grace() {
        let then = Instant::now();
        let mut registry = full_registry(then);
        let later = then + EVICTION_GRACE;
        // r0 holds a cell, so the oldest idle lifetime is r1.
        let issued = registry
            .issue_at("newcomer", |id| id == "r0", later)
            .expect("an idle lifetime makes room");
        assert_eq!(
            issued.evicted.as_ref().map(|(id, _)| id.as_str()),
            Some("r1")
        );
        assert_eq!(registry.epoch("r1"), None, "the evicted runner is fenced");
        assert!(
            registry.epoch("r0").is_some(),
            "a runner holding cells stays"
        );
        assert_eq!(registry.admitted().len(), MAX_ADMITTED_RUNNERS);
    }

    #[test]
    fn a_runner_admitted_within_the_grace_is_not_evicted_before_its_first_claim() {
        let then = Instant::now();
        let mut registry = full_registry(then);
        assert_eq!(
            registry.issue_at("newcomer", none_hold, then + EVICTION_GRACE / 2),
            None,
            "idle but just admitted is not gone"
        );
        assert_eq!(registry.admitted().len(), MAX_ADMITTED_RUNNERS);
    }

    #[test]
    fn a_full_table_of_busy_runners_refuses_rather_than_evicting() {
        let then = Instant::now();
        let mut registry = full_registry(then);
        let later = then + EVICTION_GRACE;
        assert_eq!(registry.issue_at("one-too-many", all_hold, later), None);
        assert_eq!(registry.admitted().len(), MAX_ADMITTED_RUNNERS);
        assert!(
            registry.issue_at("r0", all_hold, later).is_some(),
            "a runner already admitted still renews at the cap"
        );
    }

    #[test]
    fn runner_ids_are_short_plain_and_non_empty() {
        assert!(valid_runner_id("node-2/slot0"));
        assert!(valid_runner_id("r1"));
        assert!(valid_runner_id("node-2.local"));
        for refused in [
            "",
            "node 2",
            "../etc/passwd",
            "/node-2",
            "node-2/",
            "node-2//slot0",
            ".",
            "..",
            "node-2/./slot0",
            HOST_RUNNER_ID,
        ] {
            assert!(!valid_runner_id(refused), "{refused:?}");
        }
        assert!(!valid_runner_id(&"n".repeat(MAX_RUNNER_ID_LEN + 1)));
    }
}
