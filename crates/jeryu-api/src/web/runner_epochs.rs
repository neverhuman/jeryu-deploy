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
//! cell. `DELETE /api/v1/workcells/runners/{runner_id}` (the id may contain `/`) drops a lifetime, which
//! fences it and frees its slot. The whole `/api/v1/workcells` surface is
//! admin-only (`web::auth::admin_only_path`), this pair included.
//!
//! The table lives beside the in-memory ledger it fences and is lost with it on
//! restart, so a restarted forge admits no lifetime and holds no cell: a claim
//! from before the restart is fenced rather than served a freshly minted cell.
//! Epochs only rise, across restarts too (they start from the wall clock), so
//! a runner admitted again never gets an epoch an earlier lifetime used.
//!
//! When the table is full a new runner takes the slot of the oldest lifetime
//! that holds no live cell, so runners that went away without being dropped
//! cannot wedge admission. `GET /api/v1/workcells/runners` lists what is
//! admitted, and every admission, reissue, eviction and drop is written to the
//! forge audit trail with the admin who made it.

use std::collections::BTreeMap;

/// How many runner lifetimes the registry holds at once. When it is full, a
/// new runner takes the slot of the longest-admitted lifetime that holds no
/// live cell; only when every admitted runner holds one is the admission
/// refused, since evicting it would fence a runner with work in flight.
pub(crate) const MAX_ADMITTED_RUNNERS: usize = 256;

/// Longest runner id the registry admits. The id keys the fence and is copied
/// into every lease it claims, so it stays short and plain.
pub(crate) const MAX_RUNNER_ID_LEN: usize = 128;

/// The in-process runner the forge claims warm session cells as. A new session
/// claims from this host's own warm pool, so the lifetime that owns those cells
/// is this process's, not the `runner` label the request carries. It is seeded
/// at construction and cannot be dropped, so the host always has an epoch.
pub(crate) const HOST_RUNNER_ID: &str = "forge-host";

/// `true` when `runner_id` may key the claim fence: a short, non-empty id of
/// ASCII letters, digits and `-`, `_`, `.`, `/` (hosts name slots `node-2/slot0`),
/// which neither starts nor ends at a `/`, has no empty segment and carries no
/// `.` or `..` segment. The id reaches no path, but it is copied into leases,
/// reported, and named in `DELETE /api/v1/workcells/runners/{runner_id}`, so it
/// stays plain.
pub(crate) fn valid_runner_id(runner_id: &str) -> bool {
    !runner_id.is_empty()
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

#[derive(Debug)]
pub(crate) struct RunnerEpochRegistry {
    issued: BTreeMap<String, u64>,
    /// The epoch the next lifetime gets. Epochs only rise: never reused for a
    /// runner id that was dropped or evicted and admitted again, and seeded
    /// from the wall clock so a forge restart does not hand out an epoch a
    /// runner's previous lifetime already used. A claim from any earlier
    /// lifetime therefore stays fenced.
    next_epoch: u64,
}

impl Default for RunnerEpochRegistry {
    /// Seed [`HOST_RUNNER_ID`]'s lifetime for this process, so the warm session
    /// pool has an admitted epoch from the first request.
    fn default() -> Self {
        Self {
            issued: BTreeMap::from([(HOST_RUNNER_ID.to_string(), 1)]),
            next_epoch: first_runner_epoch(),
        }
    }
}

/// The first epoch this process issues a reported runner: milliseconds since
/// the Unix epoch, so it is above anything an earlier process issued (which
/// counted up from its own, earlier start, one per admission).
fn first_runner_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
        .max(2)
}

impl RunnerEpochRegistry {
    /// Start a new lifetime for `runner_id`. `holds_cells` says whether a
    /// runner still holds a live cell in the ledger; when the registry is full
    /// the oldest lifetime for which it is `false` is dropped to make room.
    /// `None` when the registry is full and every lifetime holds cells.
    pub(crate) fn issue(
        &mut self,
        runner_id: &str,
        holds_cells: impl Fn(&str) -> bool,
    ) -> Option<Issued> {
        let replaced_epoch = self.issued.get(runner_id).copied();
        let mut evicted = None;
        if replaced_epoch.is_none() && self.issued.len() >= MAX_ADMITTED_RUNNERS {
            let idle = self
                .issued
                .iter()
                .filter(|(id, _)| id.as_str() != HOST_RUNNER_ID && !holds_cells(id))
                .min_by_key(|(_, epoch)| **epoch)
                .map(|(id, epoch)| (id.clone(), *epoch))?;
            self.issued.remove(&idle.0);
            evicted = Some(idle);
        }
        let epoch = self.next_epoch;
        self.next_epoch = self.next_epoch.saturating_add(1);
        self.issued.insert(runner_id.to_string(), epoch);
        Some(Issued {
            epoch,
            replaced_epoch,
            evicted,
        })
    }

    /// Drop `runner_id`'s lifetime, fencing it from any further claim.
    /// [`HOST_RUNNER_ID`] is this process itself and is never dropped.
    pub(crate) fn retire(&mut self, runner_id: &str) -> bool {
        if runner_id == HOST_RUNNER_ID {
            return false;
        }
        self.issued.remove(runner_id).is_some()
    }

    /// Epoch `runner_id`'s claims must carry, if it has an admitted lifetime.
    pub(crate) fn epoch(&self, runner_id: &str) -> Option<u64> {
        self.issued.get(runner_id).copied()
    }

    /// Epoch this host's own claims carry. Seeded by [`Self::default`] and
    /// never dropped by [`Self::retire`].
    pub(crate) fn host_epoch(&self) -> u64 {
        self.epoch(HOST_RUNNER_ID)
            .expect("the host runner lifetime is seeded and never retired")
    }

    /// Every lifetime this daemon admits, for `sync_runner_epochs` and the
    /// operator listing.
    pub(crate) fn admitted(&self) -> Vec<(String, u64)> {
        self.issued
            .iter()
            .map(|(runner_id, epoch)| (runner_id.clone(), *epoch))
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

    #[test]
    fn a_fresh_registry_admits_only_this_host() {
        let registry = RunnerEpochRegistry::default();
        assert_eq!(registry.host_epoch(), 1);
        assert_eq!(registry.epoch("r1"), None);
        assert_eq!(
            registry.admitted(),
            vec![(HOST_RUNNER_ID.to_string(), 1)],
            "no reported runner is admitted until the daemon issues it an epoch"
        );
    }

    #[test]
    fn issuing_again_starts_a_new_lifetime_and_fences_the_previous_epoch() {
        let mut registry = RunnerEpochRegistry::default();
        let first = registry.issue("r1", none_hold).unwrap();
        assert_eq!(first.replaced_epoch, None);
        let second = registry.issue("r1", none_hold).unwrap();
        assert!(second.epoch > first.epoch);
        assert_eq!(second.replaced_epoch, Some(first.epoch));
        assert_eq!(registry.epoch("r1"), Some(second.epoch));
        assert_eq!(registry.admitted().len(), 2);
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
        std::thread::sleep(std::time::Duration::from_millis(2));
        let mut restarted = RunnerEpochRegistry::default();
        let after = restarted.issue("r1", none_hold).unwrap().epoch;
        assert!(after > before, "{after} must be above {before}");
    }

    #[test]
    fn a_retired_runner_loses_its_lifetime_and_its_slot() {
        let mut registry = RunnerEpochRegistry::default();
        registry.issue("r1", none_hold);
        assert!(registry.retire("r1"));
        assert_eq!(registry.epoch("r1"), None);
        assert!(!registry.retire("r1"), "retiring twice reports no lifetime");
        assert!(
            !registry.retire(HOST_RUNNER_ID),
            "this process's own lifetime is not a reported runner's to drop"
        );
        assert_eq!(registry.host_epoch(), 1);
    }

    #[test]
    fn a_full_table_evicts_the_oldest_idle_lifetime() {
        let mut registry = RunnerEpochRegistry::default();
        // The seeded host lifetime already occupies one slot.
        for slot in 0..MAX_ADMITTED_RUNNERS - 1 {
            assert!(registry.issue(&format!("r{slot}"), none_hold).is_some());
        }
        // r0 holds a cell, so the oldest idle lifetime is r1.
        let issued = registry
            .issue("newcomer", |id| id == "r0")
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
        assert!(
            registry.epoch(HOST_RUNNER_ID).is_some(),
            "the host is never evicted"
        );
    }

    #[test]
    fn a_full_table_of_busy_runners_refuses_rather_than_evicting() {
        let mut registry = RunnerEpochRegistry::default();
        for slot in 0..MAX_ADMITTED_RUNNERS - 1 {
            registry.issue(&format!("r{slot}"), none_hold);
        }
        assert_eq!(registry.issue("one-too-many", all_hold), None);
        assert_eq!(registry.admitted().len(), MAX_ADMITTED_RUNNERS);
        assert!(
            registry.issue("r0", all_hold).is_some(),
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
        ] {
            assert!(!valid_runner_id(refused), "{refused:?}");
        }
        assert!(!valid_runner_id(&"n".repeat(MAX_RUNNER_ID_LEN + 1)));
    }
}
