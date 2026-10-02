//! Shared test helpers for the integration tests.

use std::process::Command;

/// A `git` command with no ambient identity and no user or system config, so a
/// test's own `-c user.name` / `GIT_AUTHOR_NAME` is what the commit records.
/// Unattended workers export `GIT_AUTHOR_NAME` and friends, and those outrank
/// `git -c user.name=...`; a test repository must hold only what the test put
/// in it.
pub fn git_command() -> Command {
    let mut command = Command::new("git");
    for key in [
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_COMMITTER_DATE",
    ] {
        command.env_remove(key);
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}
