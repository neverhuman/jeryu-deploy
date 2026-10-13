//! The GitHub side of importing a mirror pull request to the forge.
//!
//! The GitHub repositories are read-only copies of the forge ([ADR
//! 0001](../../../docs/adr/0001-forge-is-origin-github-is-a-downstream-mirror.md)),
//! and the owner-run ruleset lets only the mirror identity move a branch
//! there, so nothing merges on GitHub. An outside contributor still opens a
//! pull request on a mirror; the importer brings its commits here as
//! `contrib/gh-<number>`, opens a forge pull request marked `contributed`, and
//! reports the forge's answer back to the GitHub pull request.
//!
//! This module holds what talks to GitHub — the API seam ([`GithubApi`], with
//! the curl transport that speaks to `api.github.com`), the enrolment read
//! from the split manifests, and the fetch that materializes a contribution as
//! a forge branch. What it means for the forge (opening the pull request,
//! marking it, reporting back) lives in `web::pull_import`.
//!
//! Everything here treats a pull request's title, body and author as data: the
//! only values that reach a command are the number the forge resolved and the
//! destination it configured.
//!
//! The importer is off until a site switches it on per repository AND arms it
//! for the whole forge; see [`PullImport::load`] for what must exist first.

#![cfg(feature = "web")]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Branch a contribution lands on in the forge repository. The number is the
/// GitHub pull request's, so a re-import of the same contribution reuses the
/// branch and a force-push on GitHub updates it in place.
pub const CONTRIB_BRANCH_PREFIX: &str = "contrib/gh-";

/// Commit status the forge writes on the GitHub head to report its gate.
pub const GATE_STATUS_CONTEXT: &str = "jeryu/forge-gate";

/// Hard wall-clock bound for one GitHub call or one fetch.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Arms the importer for the whole forge. Unset or anything but `1` leaves
/// every enrolled repository switched off.
const ARM_ENV: &str = "JERYU_GITHUB_PULL_IMPORT";

/// Where the API token is read from, in this order. Nothing secret is ever
/// passed on a command line: the token reaches curl through its stdin config.
const TOKEN_ENV: &str = "JERYU_GITHUB_API_TOKEN";
const TOKEN_FILE_ENV: &str = "JERYU_GITHUB_API_TOKEN_FILE";

/// The branch a contribution lands on for GitHub pull request `number`.
pub fn contrib_branch(number: u64) -> String {
    format!("{CONTRIB_BRANCH_PREFIX}{number}")
}

/// The GitHub pull request number a contribution branch carries, if it is one.
pub fn contrib_number(branch: &str) -> Option<u64> {
    branch.strip_prefix(CONTRIB_BRANCH_PREFIX)?.parse().ok()
}

/// Whether a GitHub pull request is still open there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GithubPullState {
    Open,
    Closed,
}

/// One GitHub pull request, as much of it as the importer uses.
///
/// The strings are the contributor's words and stay data: they are copied into
/// the forge pull request and never interpreted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubPull {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    /// GitHub login that opened it; `dependabot[bot]` is just another one.
    pub author: String,
    /// Branch on the mirror the contributor targeted.
    pub base_ref: String,
    pub head_sha: String,
    pub state: GithubPullState,
}

/// What a commit status says about the forge gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GithubStatusState {
    Pending,
    Success,
    Failure,
}

impl GithubStatusState {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

/// One commit status the forge posts on a GitHub head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubStatus {
    pub state: GithubStatusState,
    pub context: String,
    pub description: String,
}

/// What the importer needs from GitHub. A test stands a fake in its place; the
/// forge never reaches the network through any other path.
pub trait GithubApi: Send + Sync + std::fmt::Debug {
    /// Every pull request still open on the mirror (the polling fallback).
    fn list_open_pulls(&self, slug: &str) -> Result<Vec<GithubPull>, String>;

    /// One pull request, read fresh. A webhook delivery is only a nudge: what
    /// the importer acts on is what this returns.
    fn get_pull(&self, slug: &str, number: u64) -> Result<GithubPull, String>;

    /// The commit a branch points at on GitHub, or `None` when there is no
    /// such branch.
    fn branch_head(&self, slug: &str, branch: &str) -> Result<Option<String>, String>;

    fn set_commit_status(&self, slug: &str, sha: &str, status: &GithubStatus)
    -> Result<(), String>;

    fn comment(&self, slug: &str, number: u64, body: &str) -> Result<(), String>;

    fn close_pull(&self, slug: &str, number: u64) -> Result<(), String>;
}

/// One repository that imports its mirror's pull requests.
#[derive(Clone, Debug)]
pub struct PullImportTarget {
    pub github_slug: String,
    /// The forge branch a contribution is proposed against.
    pub branch: String,
    /// Test seam: a local path that replaces the GitHub clone URL the
    /// contribution is fetched from.
    pub fetch_override: Option<String>,
}

/// Which repositories import, keyed by lowercased forge slug `owner/name`.
#[derive(Clone, Debug, Default)]
pub struct PullImport {
    enabled: bool,
    targets: BTreeMap<String, PullImportTarget>,
}

#[derive(Debug, serde::Deserialize)]
struct ImportManifest {
    repo: Option<Vec<ImportManifestRepo>>,
}

#[derive(Debug, serde::Deserialize)]
struct ImportManifestRepo {
    github_slug: Option<String>,
    jeryu_slug: Option<String>,
    default_branch: Option<String>,
    #[serde(default)]
    mirror_github_main: bool,
    /// The per-repository switch. Off by default, and off is the only safe
    /// value until the preconditions in [`PullImport::load`] hold.
    #[serde(default)]
    import_github_pulls: bool,
}

impl PullImport {
    /// Load the enrolled repositories from the split manifests.
    ///
    /// A repository imports only when its `[[repo]]` entry carries
    /// `github_slug`, `jeryu_slug`, `mirror_github_main = true` AND
    /// `import_github_pulls = true`, and only when the forge is armed with
    /// `JERYU_GITHUB_PULL_IMPORT=1`.
    ///
    /// Arming is deliberate, and a site may not do it before all of these
    /// exist, because a contribution is untrusted code the forge runs:
    ///
    /// - the untrusted-pull-request flag and the sandboxed gate, so a
    ///   contribution's gate run cannot reach the forge or the network, and
    /// - a live `pr-redteam` running the `jeryu-ci-runner` main code, because
    ///   the older copy approves by default and would wave a contribution
    ///   through.
    ///
    /// The contributed-origin hold (`docs/github-mirror.md`) is what stops an
    /// imported pull request from merging without a maintainer; arming without
    /// the two above still runs a stranger's code on the gate.
    pub fn load(manifests: &[PathBuf]) -> Self {
        if std::env::var(ARM_ENV).as_deref() != Ok("1") {
            return Self::default();
        }
        let mut targets = BTreeMap::new();
        for path in manifests {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let Ok(parsed) = toml::from_str::<ImportManifest>(&text) else {
                continue;
            };
            for repo in parsed.repo.unwrap_or_default() {
                if !repo.mirror_github_main || !repo.import_github_pulls {
                    continue;
                }
                let (Some(github_slug), Some(jeryu_slug)) = (repo.github_slug, repo.jeryu_slug)
                else {
                    continue;
                };
                targets.insert(
                    jeryu_slug.to_ascii_lowercase(),
                    PullImportTarget {
                        github_slug,
                        branch: repo.default_branch.unwrap_or_else(|| "main".to_string()),
                        fetch_override: None,
                    },
                );
            }
        }
        Self {
            enabled: !targets.is_empty(),
            targets,
        }
    }

    /// Embedding/test seam: build an importer from explicit targets.
    pub fn with_targets(targets: BTreeMap<String, PullImportTarget>) -> Self {
        Self {
            enabled: !targets.is_empty(),
            targets,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The target for a forge repository, or `None` when it does not import.
    pub fn target(&self, owner: &str, name: &str) -> Option<&PullImportTarget> {
        if !self.enabled {
            return None;
        }
        self.targets
            .get(&format!("{owner}/{name}").to_ascii_lowercase())
    }

    /// The forge repository a GitHub slug imports into. A pull request on a
    /// repository nobody enrolled resolves to `None` and is ignored.
    pub fn forge_repo_for(&self, github_slug: &str) -> Option<(String, String)> {
        if !self.enabled {
            return None;
        }
        self.targets
            .iter()
            .find(|(_, target)| target.github_slug.eq_ignore_ascii_case(github_slug))
            .and_then(|(slug, _)| slug.split_once('/'))
            .map(|(owner, name)| (owner.to_string(), name.to_string()))
    }
}

/// Where a contribution is fetched from: the mirror itself, which publishes
/// every pull request head as `refs/pull/<number>/head` whether it was opened
/// from a fork or from a branch on the mirror.
fn fetch_source(target: &PullImportTarget) -> Result<String, String> {
    if let Some(seam) = &target.fetch_override {
        return Ok(seam.clone());
    }
    let candidate = format!("https://github.com/{}.git", target.github_slug);
    let allowlist = crate::git_remote::GitRemoteAllowlist::from_env(
        crate::github_mirror::MIRROR_REMOTE_ALLOWLIST_VAR,
    )
    .ok_or_else(|| {
        "JERYU_MIRROR_REMOTE_ALLOWLIST must be configured before importing".to_string()
    })?;
    allowlist.check(&candidate)?;
    Ok(format!(
        "https://x-access-token:jeryussh@github.com/{}.git",
        target.github_slug
    ))
}

/// Bring GitHub pull request `number` into `bare` as `contrib/gh-<number>` and
/// return the commit it now points at.
///
/// The commits are fetched, never rewritten, so every original author and
/// committer is preserved exactly as the contributor wrote them. The branch
/// update is forced: it is the importer's own branch, and a contributor who
/// force-pushes on GitHub expects the forge pull request to follow.
pub fn fetch_contribution(
    git_bin: &str,
    bare: &Path,
    target: &PullImportTarget,
    number: u64,
) -> Result<String, String> {
    let branch = contrib_branch(number);
    let refspec = format!("+refs/pull/{number}/head:refs/heads/{branch}");
    let source = fetch_source(target)?;
    run_bounded(
        git_bin,
        &["fetch", "--no-tags", "--force", &source, &refspec],
        bare,
    )?;
    let head = run_bounded(
        git_bin,
        &["rev-parse", &format!("refs/heads/{branch}")],
        bare,
    )?
    .trim()
    .to_string();
    if head.is_empty() {
        return Err(format!("{branch} did not resolve after the fetch"));
    }
    Ok(head)
}

/// Repository-relative paths a contribution changes against its base, for
/// CODEOWNERS and the gate. An unreadable range yields no paths rather than
/// failing the import: the pull request is still worth opening.
pub fn changed_files(git_bin: &str, bare: &Path, base: &str, head: &str) -> Vec<String> {
    let range = format!("refs/heads/{base}...{head}");
    run_bounded(git_bin, &["diff", "--name-only", &range], bare)
        .map(|out| {
            out.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `sha` is reachable from `tip` in the bare repository. Used to tell
/// whether the mirror push has actually carried a merge back to GitHub.
pub fn contains_commit(git_bin: &str, bare: &Path, tip: &str, sha: &str) -> bool {
    run_bounded(git_bin, &["merge-base", "--is-ancestor", sha, tip], bare).is_ok()
}

/// The GitHub REST transport, over curl.
///
/// The token never appears in a process argument: curl reads its headers and
/// its URL from a config on stdin, so `ps` shows nothing but `curl --config -`.
#[derive(Clone, Debug)]
pub struct CurlGithubApi {
    api_base: String,
    token: String,
}

impl CurlGithubApi {
    /// Build the transport from the environment, or `None` when no token is
    /// configured — without one GitHub refuses every write anyway.
    pub fn from_env() -> Option<Self> {
        let token = std::env::var(TOKEN_ENV)
            .ok()
            .or_else(|| {
                std::env::var(TOKEN_FILE_ENV)
                    .ok()
                    .and_then(|path| std::fs::read_to_string(path).ok())
            })
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())?;
        Some(Self {
            api_base: "https://api.github.com".to_string(),
            token,
        })
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<String, String> {
        let (status, body) = self.call_response(method, path, body)?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(format!(
                "github HTTP {status}: {}",
                crate::github_mirror::redact(&body)
            ))
        }
    }

    fn call_response(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(u16, String), String> {
        let mut config = String::new();
        config.push_str("--silent\n--show-error\nwrite-out = \"\\n%{http_code}\"\n");
        config.push_str(&format!("--max-time {}\n", CALL_TIMEOUT.as_secs()));
        config.push_str(&format!("--request {method}\n"));
        config.push_str(&format!(
            "header = \"Authorization: Bearer {}\"\n",
            self.token
        ));
        config.push_str("header = \"Accept: application/vnd.github+json\"\n");
        config.push_str("header = \"X-GitHub-Api-Version: 2022-11-28\"\n");
        if let Some(body) = body {
            config.push_str("header = \"Content-Type: application/json\"\n");
            config.push_str(&format!(
                "data = {}\n",
                json_config_value(&body.to_string())
            ));
        }
        config.push_str(&format!("url = \"{}{}\"\n", self.api_base, path));
        run_curl(&config)
    }

    fn pull_from_json(value: &serde_json::Value) -> Option<GithubPull> {
        Some(GithubPull {
            number: value.get("number")?.as_u64()?,
            title: value
                .get("title")
                .and_then(|title| title.as_str())
                .unwrap_or_default()
                .to_string(),
            body: value
                .get("body")
                .and_then(|body| body.as_str())
                .map(str::to_string),
            author: value
                .get("user")
                .and_then(|user| user.get("login"))
                .and_then(|login| login.as_str())
                .unwrap_or_default()
                .to_string(),
            base_ref: value
                .get("base")
                .and_then(|base| base.get("ref"))
                .and_then(|name| name.as_str())
                .unwrap_or("main")
                .to_string(),
            head_sha: value
                .get("head")
                .and_then(|head| head.get("sha"))
                .and_then(|sha| sha.as_str())
                .unwrap_or_default()
                .to_string(),
            state: match value.get("state").and_then(|state| state.as_str()) {
                Some("closed") => GithubPullState::Closed,
                _ => GithubPullState::Open,
            },
        })
    }
}

impl GithubApi for CurlGithubApi {
    fn list_open_pulls(&self, slug: &str) -> Result<Vec<GithubPull>, String> {
        let body = self.call(
            "GET",
            &format!("/repos/{slug}/pulls?state=open&per_page=100"),
            None,
        )?;
        let parsed: serde_json::Value =
            serde_json::from_str(&body).map_err(|err| format!("read pull list: {err}"))?;
        Ok(parsed
            .as_array()
            .map(|rows| rows.iter().filter_map(Self::pull_from_json).collect())
            .unwrap_or_default())
    }

    fn get_pull(&self, slug: &str, number: u64) -> Result<GithubPull, String> {
        let body = self.call("GET", &format!("/repos/{slug}/pulls/{number}"), None)?;
        let parsed: serde_json::Value =
            serde_json::from_str(&body).map_err(|err| format!("read pull: {err}"))?;
        Self::pull_from_json(&parsed).ok_or_else(|| format!("{slug}#{number} has no number"))
    }

    fn branch_head(&self, slug: &str, branch: &str) -> Result<Option<String>, String> {
        let (status, body) = self.call_response(
            "GET",
            &format!("/repos/{slug}/git/ref/heads/{branch}"),
            None,
        )?;
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(format!(
                "github HTTP {status}: {}",
                crate::github_mirror::redact(&body)
            ));
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&body).map_err(|err| format!("read branch head: {err}"))?;
        Ok(parsed
            .get("object")
            .and_then(|object| object.get("sha"))
            .and_then(|sha| sha.as_str())
            .map(str::to_string))
    }

    fn set_commit_status(
        &self,
        slug: &str,
        sha: &str,
        status: &GithubStatus,
    ) -> Result<(), String> {
        self.call(
            "POST",
            &format!("/repos/{slug}/statuses/{sha}"),
            Some(&serde_json::json!({
                "state": status.state.wire_name(),
                "context": status.context,
                "description": status.description,
            })),
        )
        .map(|_| ())
    }

    fn comment(&self, slug: &str, number: u64, body: &str) -> Result<(), String> {
        self.call(
            "POST",
            &format!("/repos/{slug}/issues/{number}/comments"),
            Some(&serde_json::json!({ "body": body })),
        )
        .map(|_| ())
    }

    fn close_pull(&self, slug: &str, number: u64) -> Result<(), String> {
        self.call(
            "PATCH",
            &format!("/repos/{slug}/pulls/{number}"),
            Some(&serde_json::json!({ "state": "closed" })),
        )
        .map(|_| ())
    }
}

/// Quote a value for a curl config line: the file format reads a
/// double-quoted C-style string, so a backslash and a quote are escaped.
fn json_config_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Run `curl --config -` with the request on stdin, bounded like every other
/// outbound call the mirror makes.
fn run_curl(config: &str) -> Result<(u16, String), String> {
    let mut child = std::process::Command::new("curl")
        .args(["--config", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| format!("spawn curl: {err}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(config.as_bytes())
            .map_err(|err| format!("write curl config: {err}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("wait for curl: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if output.status.success() {
        return parse_http_output(&stdout);
    }
    Err(format!(
        "github call failed ({}): {}",
        output.status,
        crate::github_mirror::redact(stdout.trim())
    ))
}

fn parse_http_output(stdout: &str) -> Result<(u16, String), String> {
    let (body, status) = stdout
        .rsplit_once('\n')
        .ok_or_else(|| "curl did not report HTTP status".to_string())?;
    let status = status
        .trim()
        .parse::<u16>()
        .map_err(|_| "curl reported an invalid HTTP status".to_string())?;
    if !(100..600).contains(&status) {
        return Err("curl reported no HTTP response".to_string());
    }
    Ok((status, body.to_string()))
}

/// Run git with prompts disabled and a hard timeout; returns stdout on success.
fn run_bounded(git_bin: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let mut child = std::process::Command::new(git_bin)
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=10",
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| format!("spawn {git_bin}: {err}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_string(&mut stdout);
                }
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut stderr);
                }
                if status.success() {
                    return Ok(stdout);
                }
                return Err(crate::github_mirror::redact(&format!(
                    "git {} failed: {}",
                    args.join(" "),
                    stderr.trim()
                )));
            }
            Ok(None) => {
                if start.elapsed() > CALL_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(crate::github_mirror::redact(&format!(
                        "git {} timed out after {}s",
                        args.join(" "),
                        CALL_TIMEOUT.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(format!("wait: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_is_separate_from_response_text() {
        assert_eq!(
            parse_http_output("Not Found\n404").unwrap(),
            (404, "Not Found".to_string())
        );
        assert_eq!(parse_http_output("body mentions 404\n500").unwrap().0, 500);
        assert!(parse_http_output("no status").is_err());
        assert!(parse_http_output("network error\n000").is_err());
        assert!(
            !crate::github_mirror::redact(
                "https://x-access-token:private-test@host.invalid/repo.git"
            )
            .contains("private-test")
        );
    }

    #[test]
    fn a_contribution_branch_names_its_github_pull_request() {
        assert_eq!(contrib_branch(42), "contrib/gh-42");
        assert_eq!(contrib_number("contrib/gh-42"), Some(42));
        assert_eq!(contrib_number("feature"), None);
        assert_eq!(contrib_number("contrib/gh-"), None);
    }

    #[test]
    fn an_unenrolled_repository_resolves_to_nothing() {
        let import = PullImport::with_targets(BTreeMap::from([(
            "jeryu/widget".to_string(),
            PullImportTarget {
                github_slug: "acme-oss/widget".to_string(),
                branch: "main".to_string(),
                fetch_override: None,
            },
        )]));
        assert!(import.target("jeryu", "widget").is_some());
        assert!(import.target("jeryu", "gadget").is_none());
        assert_eq!(
            import.forge_repo_for("ACME-OSS/widget"),
            Some(("jeryu".to_string(), "widget".to_string()))
        );
        assert_eq!(import.forge_repo_for("globex/gadget"), None);
    }

    #[test]
    fn a_curl_config_value_cannot_break_out_of_its_quotes() {
        assert_eq!(
            json_config_value("{\"body\":\"a\\nb\"}"),
            "\"{\\\"body\\\":\\\"a\\\\nb\\\"}\""
        );
    }
}
