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
//! cell. `DELETE /api/v1/workcells/runners/:runner_id` drops a lifetime, which
//! fences it and frees its slot. The whole `/api/v1/workcells` surface is
//! admin-only (`web::auth::admin_only_path`), this pair included.
//!
//! The table lives beside the in-memory ledger it fences and is lost with it on
//! restart, so a restarted forge admits no lifetime and holds no cell: a claim
//! from before the restart is fenced rather than served a freshly minted cell.

use std::collections::BTreeMap;

/// How many runner lifetimes the registry holds at once. A runner that has
/// gone takes its slot with it only when it is dropped, so the cap is what
/// bounds the table; admitting past it is refused rather than evicting a
/// lifetime that may still be holding cells.
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
/// which neither starts at the root nor carries a `..` segment. The id reaches
/// no path, but it is copied into leases and reported, so it stays plain.
pub(crate) fn valid_runner_id(runner_id: &str) -> bool {
    !runner_id.is_empty()
        && runner_id.len() <= MAX_RUNNER_ID_LEN
        && !runner_id.starts_with('/')
        && !runner_id.split('/').any(|segment| segment == "..")
        && runner_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

#[derive(Debug)]
pub(crate) struct RunnerEpochRegistry {
    issued: BTreeMap<String, u64>,
}

impl Default for RunnerEpochRegistry {
    /// Seed [`HOST_RUNNER_ID`]'s lifetime for this process, so the warm session
    /// pool has an admitted epoch from the first request.
    fn default() -> Self {
        Self {
            issued: BTreeMap::from([(HOST_RUNNER_ID.to_string(), 1)]),
        }
    }
}

impl RunnerEpochRegistry {
    /// Start a new lifetime for `runner_id` and return the epoch its claims
    /// must carry. `None` when the registry is full and `runner_id` is not
    /// already in it.
    pub(crate) fn issue(&mut self, runner_id: &str) -> Option<u64> {
        if let Some(epoch) = self.issued.get_mut(runner_id) {
            *epoch = epoch.saturating_add(1);
            return Some(*epoch);
        }
        if self.issued.len() >= MAX_ADMITTED_RUNNERS {
            return None;
        }
        self.issued.insert(runner_id.to_string(), 1);
        Some(1)
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

    /// Every lifetime this daemon admits, for `sync_runner_epochs`.
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
        assert_eq!(registry.issue("r1"), Some(1));
        assert_eq!(registry.issue("r1"), Some(2));
        assert_eq!(registry.epoch("r1"), Some(2));
        assert_eq!(registry.admitted().len(), 2);
    }

    #[test]
    fn a_retired_runner_loses_its_lifetime_and_its_slot() {
        let mut registry = RunnerEpochRegistry::default();
        registry.issue("r1");
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
    fn the_table_is_capped_and_refuses_rather_than_evicting() {
        let mut registry = RunnerEpochRegistry::default();
        // The seeded host lifetime already occupies one slot.
        for slot in 0..MAX_ADMITTED_RUNNERS - 1 {
            assert_eq!(registry.issue(&format!("r{slot}")), Some(1));
        }
        assert_eq!(registry.issue("one-too-many"), None);
        assert_eq!(registry.admitted().len(), MAX_ADMITTED_RUNNERS);
        assert_eq!(
            registry.issue("r0"),
            Some(2),
            "a runner already admitted still renews at the cap"
        );
    }

    #[test]
    fn runner_ids_are_short_plain_and_non_empty() {
        assert!(valid_runner_id("node-2/slot0"));
        assert!(valid_runner_id("r1"));
        assert!(!valid_runner_id(""));
        assert!(!valid_runner_id("node 2"));
        assert!(!valid_runner_id("../etc/passwd"));
        assert!(!valid_runner_id("/node-2"));
        assert!(!valid_runner_id(&"n".repeat(MAX_RUNNER_ID_LEN + 1)));
    }
}
