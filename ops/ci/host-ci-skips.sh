# shellcheck shell=bash
# The single list of tests host-ci does not run, sourced by pr-ci.sh, web.sh and
# dependency-sources.sh. Every entry here runs on the dedicated GitHub-mirror
# runners (full caps, unmanaged cgroup, unloaded); add a reason with each entry.

# jeryu-sandbox-linux (35 tests) spawns REAL sandboxes (user/mount/pid namespaces
# + cgroup-v2 + landlock/seccomp) and fails under host-ci's systemd-managed poll
# cgroup (cgroup_create EEXIST / clone EOPNOTSUPP).
HOST_CI_EXCLUDE_PACKAGES=(--exclude jeryu-sandbox-linux)

HOST_CI_SKIPPED_TESTS=(
  # agentbridge sandbox-runtime tests: need a real kernel sandbox, same reason as above.
  same_write_path_succeeds_inside_and_is_blocked_outside
  unsandboxed_control_can_write_outside_proving_landlock_is_the_blocker
  budget_kill_is_live_and_truncates
  watchdog_kill_is_live
  require_cgroup_driver_fails_closed_without_delegated_subtree
  opt_out_driver_runs_on_this_no_delegation_host
  editbot_writes_inside_the_cell
  editbot_writing_outside_the_cell_is_denied_by_landlock
  watchdog_kills_a_runaway_editbot
  output_budget_exceeded_kills_the_child
  # agent-stream live tests: an oversubscribed host starves their 30s await_tty deadline.
  streams_terminal_output_to_the_sink
  control_input_reaches_the_agent_stdin
  terminate_stops_a_runaway_agent
  # web::sessions live-stream proofs: spawn a real PTY and poll await_tty, same class.
  create_session_spawns_agent_and_streams_its_tty_output
  create_session_agent_runs_in_workspace_with_branch_env
  create_session_docker_runtime_streams_live_and_carries_hardened_flags
  create_session_native_runtime_uses_native_path
)

# libtest arguments (after `--`) that skip every HOST_CI_SKIPPED_TESTS entry.
host_ci_skip_args() {
  local name
  for name in "${HOST_CI_SKIPPED_TESTS[@]}"; do
    printf -- '--skip\n%s\n' "$name"
  done
}
