//! Where an inbox command is run. Every `action.command` comes with a
//! `run_in` phrase naming the machine and directory, and the machine names
//! live here and nowhere else: three roles, each overridable by environment.

const RELEASE_ENV: &str = "JERYU_RELEASE_HOST";
const GATE_ENV: &str = "JERYU_GATE_HOST";
const FORGE_ENV: &str = "JERYU_FORGE_HOST";

/// The machines an operator is sent to, by role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hosts {
    /// Runs auto-pin, auto-stage, `deploy-release.sh` and the todoq workers.
    pub release: String,
    /// Runs the `pr-gate-runner@` slots.
    pub gate: String,
    /// Runs the forge itself; mirror pushes leave from here.
    pub forge: String,
}

impl Default for Hosts {
    fn default() -> Self {
        Self::from_lookup(|_| None)
    }
}

impl Hosts {
    /// Read once per inbox computation.
    pub(crate) fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// A blank value counts as unset, so an empty line in an env file cannot
    /// produce "Run on , any directory".
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let role = |name: &str, default: &str| {
            lookup(name)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Self {
            release: role(RELEASE_ENV, "xbabe0"),
            gate: role(GATE_ENV, "xbabe2"),
            forge: role(FORGE_ENV, "atomicsoul"),
        }
    }

    /// A command that works from whatever directory the shell is in.
    pub(crate) fn anywhere(host: &str) -> String {
        format!("{host}, any directory")
    }

    /// A command that is a path inside `repo` (`owner/name`).
    pub(crate) fn checkout(host: &str, repo: &str) -> String {
        format!("{host}, in a {repo} checkout")
    }
}
