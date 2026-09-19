//! Live read-model assembly: turns forge state into the [`TuiReadModel`] the
//! TUI/web panes render. Kept out of `web.rs` so the HTTP/WS edge stays focused
//! on routing rather than rollup logic.
//!
//! Nothing here is invented: job counts come from check runs on open-PR heads,
//! runner capacity from the gate runners that actually report heartbeats (the
//! same fabric `GET /api/v1/control-plane/runners` serves), and components with
//! no real probe report `Unknown` rather than `Healthy`.

use jeryu_readmodel::{
    ComponentHealth, HealthLevel, PoolActivity, PoolRollup, RepoActivity, RunnerHealth,
    SystemHealth, TuiReadModel,
};

/// Name of the single pool the forge's gate runners serve.
const DEFAULT_POOL: &str = "default";

/// Active job counts for one repository: check runs on open-PR heads only, so
/// results recorded against merged or closed PRs never inflate the numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RepoJobs {
    pub repo: String,
    pub queued: u32,
    pub running: u32,
    pub failed: u32,
}

/// Runner capacity as reported by the live runner fabric. The default (all
/// zero) is the honest answer when no runner has reported a heartbeat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FleetCapacity {
    pub online_runners: u32,
    pub busy_runners: u32,
    pub idle_runners: u32,
    /// Runners that reported once but have since gone silent.
    pub stuck_runners: u32,
    /// Slots on online runners.
    pub active_slots: u32,
    /// Slots on every reporting runner, online or not.
    pub total_slots: u32,
}

/// Build a [`TuiReadModel`] from per-repo active job counts and the live runner
/// fabric capacity.
pub(crate) fn assemble_read_model(jobs: &[RepoJobs], fleet: &FleetCapacity) -> TuiReadModel {
    TuiReadModel {
        pool_activity: assemble_pool_activity(jobs, fleet),
        system: system_health(fleet),
        ..TuiReadModel::default()
    }
}

/// Roll up every repo's active jobs into a `default` [`PoolActivity`] whose
/// runner capacity is the live fabric. An empty server surfaces no pool (the
/// "no fabric" contract).
pub(crate) fn assemble_pool_activity(jobs: &[RepoJobs], fleet: &FleetCapacity) -> PoolActivity {
    let mut pool = PoolRollup::new(DEFAULT_POOL);
    let repos: Vec<RepoActivity> = jobs
        .iter()
        .map(|repo| {
            pool.queued_jobs = pool.queued_jobs.saturating_add(repo.queued);
            pool.running_jobs = pool.running_jobs.saturating_add(repo.running);
            pool.failed_jobs = pool.failed_jobs.saturating_add(repo.failed);
            RepoActivity {
                repo: repo.repo.clone(),
                queued_jobs: repo.queued,
                running_jobs: repo.running,
                failed_jobs: repo.failed,
                pools: vec![DEFAULT_POOL.to_string()],
            }
        })
        .collect();
    pool.online_runners = fleet.online_runners;
    pool.active_slots = fleet.active_slots;
    pool.configured_max_slots = fleet.total_slots;
    pool.stuck_runners = fleet.stuck_runners;

    let pools = if repos.is_empty() {
        Vec::new()
    } else {
        vec![pool]
    };
    PoolActivity {
        repos,
        pools,
        ..PoolActivity::default()
    }
}

/// System health with no invented verdicts: components the forge does not
/// probe report `Unknown`; runner counts come from the live fabric.
pub(crate) fn system_health(fleet: &FleetCapacity) -> SystemHealth {
    SystemHealth {
        scm: unprobed("scm"),
        database: unprobed("database"),
        sandbox: unprobed("sandbox"),
        cache: unprobed("cache"),
        vault: unprobed("vault"),
        runners: RunnerHealth {
            online: fleet.online_runners,
            busy: fleet.busy_runners,
            idle: fleet.idle_runners,
            degraded: fleet.stuck_runners,
        },
    }
}

fn unprobed(name: &str) -> ComponentHealth {
    ComponentHealth {
        name: name.to_string(),
        status: HealthLevel::Unknown,
        latency_ms: None,
        detail: Some("no health probe".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_jobs(queued: u32, running: u32, failed: u32) -> RepoJobs {
        RepoJobs {
            repo: "alice/jeryu".to_string(),
            queued,
            running,
            failed,
        }
    }

    #[test]
    fn empty_server_has_no_pool() {
        let activity = assemble_pool_activity(&[], &FleetCapacity::default());
        assert!(activity.repos.is_empty());
        assert!(activity.pools.is_empty());
    }

    #[test]
    fn pool_capacity_is_the_live_fabric() {
        let fleet = FleetCapacity {
            online_runners: 6,
            busy_runners: 2,
            idle_runners: 4,
            stuck_runners: 1,
            active_slots: 6,
            total_slots: 7,
        };
        let activity = assemble_pool_activity(&[repo_jobs(1, 2, 3)], &fleet);
        let pool = &activity.pools[0];
        assert_eq!(pool.online_runners, 6);
        assert_eq!(pool.active_slots, 6);
        assert_eq!(pool.configured_max_slots, 7);
        assert_eq!(pool.stuck_runners, 1);
        assert_eq!(
            (pool.queued_jobs, pool.running_jobs, pool.failed_jobs),
            (1, 2, 3)
        );
    }

    #[test]
    fn no_reporting_runner_means_zero_capacity() {
        let activity = assemble_pool_activity(&[repo_jobs(0, 0, 0)], &FleetCapacity::default());
        let pool = &activity.pools[0];
        assert_eq!(pool.configured_max_slots, 0);
        assert_eq!(pool.online_runners, 0);
    }

    #[test]
    fn unprobed_components_are_unknown_not_healthy() {
        let system = system_health(&FleetCapacity::default());
        for component in [
            &system.scm,
            &system.database,
            &system.sandbox,
            &system.cache,
            &system.vault,
        ] {
            assert!(matches!(component.status, HealthLevel::Unknown));
        }
        assert_eq!(system.runners, RunnerHealth::default());
    }
}
