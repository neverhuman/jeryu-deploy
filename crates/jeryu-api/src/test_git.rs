//! Hermetic `git` for tests.
//!
//! An ambient identity in the environment (`GIT_AUTHOR_NAME` and friends, which
//! unattended workers export) outranks `git -c user.name=...`, and the
//! developer's own global config can add anything else. Tests build their git
//! commands here so a repository they create holds only what they put in it.

use std::process::Command;

/// A `git` command with no ambient identity and no user or system config, so a
/// test's own `-c user.name` / `GIT_AUTHOR_NAME` is what the commit records.
pub(crate) fn git_command() -> Command {
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
