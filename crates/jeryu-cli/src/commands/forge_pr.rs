//! `jeryu forge pr {view,checks,comment,close,approve,regate}` and
//! `jeryu auth doctor`: live-only commands authenticated by a token file.
//!
//! The forge is the address `--api-url` / `JERYU_API_URL` names, and only
//! that: the bearer token is never sent to a host read from the clone's git
//! config. A clone's `origin` may be a mirror or anyone's server, so it only
//! supplies the default `owner/repo` the command acts on. Every write first asks the forge who the token is; a token the
//! forge refuses stops the command before any write, so a dead credential is
//! tried once instead of once per write (the forge locks an address out after
//! repeated bad credentials). The token is read from its file by
//! [`ApiClient`] and never printed.

use std::{io::Write, process::Command};

use serde_json::{Value, json};

use crate::cli::PrCommands;
use crate::client::{ClientError, ClientResult};
use crate::commands::api::{ApiClient, api_token};
use crate::commands::render;

/// Commands that exist only against a live forge.
pub(crate) fn is_remote_command(cmd: &PrCommands) -> bool {
    !matches!(
        cmd,
        PrCommands::Open { .. }
            | PrCommands::List { .. }
            | PrCommands::Status { .. }
            | PrCommands::Merge { .. }
    )
}

/// A forge address read from a git remote URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteForge {
    pub base_url: String,
    pub owner: String,
    pub repo: String,
}

/// Parse a git remote URL into the forge base URL and `owner/repo`.
///
/// Userinfo is dropped, so a credential embedded in a remote never reaches a
/// request or the output. ssh remotes name a forge reachable over HTTPS.
pub(crate) fn parse_remote(url: &str) -> Option<RemoteForge> {
    let url = url.trim();
    let (scheme, authority, path) = if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        (scheme, authority.to_string(), path.to_string())
    } else {
        // scp-like: [user@]host:owner/repo.git
        let (host, path) = url.split_once(':')?;
        (
            "ssh",
            host.to_string(),
            path.trim_start_matches('/').to_string(),
        )
    };
    let authority = authority.rsplit('@').next()?;
    let (scheme, host) = match scheme {
        "http" | "https" => (scheme, authority.to_string()),
        "ssh" | "git" | "git+ssh" | "ssh+git" => {
            // The port of an ssh remote is the ssh port, not the web port.
            let host = authority.split(':').next()?;
            ("https", host.to_string())
        }
        _ => return None,
    };
    if host.is_empty() || host.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut segments = path.rsplit('/');
    let repo = segments.next().filter(|s| !s.is_empty())?;
    let owner = segments.next().filter(|s| !s.is_empty())?;
    Some(RemoteForge {
        base_url: format!("{scheme}://{host}"),
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

fn origin_remote() -> Option<RemoteForge> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_remote(String::from_utf8_lossy(&output.stdout).trim())
}

fn no_forge() -> ClientError {
    ClientError::NotWired(
        "no forge address: pass --api-url or set JERYU_API_URL (the token is never sent to \
         the host a clone's remote origin names)"
            .to_string(),
    )
}

/// A client for a live forge, refusing to run without a token.
fn client_for(api_url: &str) -> ClientResult<ApiClient> {
    if api_token()?.is_none() {
        return Err(ClientError::Invalid(
            "no token: pass --token-file or set JERYU_TOKEN_FILE".to_string(),
        ));
    }
    ApiClient::new(api_url)
}

/// Ask the forge who the token is. The one call every write waits on.
fn whoami(api: &ApiClient) -> ClientResult<String> {
    let user = api.get("/api/v3/user")?;
    Ok(user
        .get("login")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string())
}

/// `jeryu auth doctor`.
pub(crate) fn run_auth_doctor(
    api_url: Option<&str>,
    json: bool,
    out: &mut dyn Write,
) -> ClientResult<()> {
    let base = api_url.ok_or_else(no_forge)?.to_string();
    let api = client_for(&base)?;
    let login = whoami(&api)?;
    render(
        out,
        json,
        &json!({"authenticated": true, "login": login, "forge": base}),
        &format!("authenticated as {login} on {base}"),
    )
}

/// `owner/name` addressing of the repository a command acts on.
fn resolve_repo(
    flag: Option<&str>,
    default_owner: &str,
    remote: Option<&RemoteForge>,
) -> ClientResult<(String, String)> {
    let valid = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let (owner, repo) = match flag {
        Some(flag) => match flag.split_once('/') {
            Some((owner, repo)) => (owner.to_string(), repo.to_string()),
            None => (default_owner.to_string(), flag.to_string()),
        },
        None => {
            let remote = remote.ok_or_else(|| {
                ClientError::Invalid(
                    "no repository: pass --repo or run inside a clone of the forge repository"
                        .to_string(),
                )
            })?;
            (remote.owner.clone(), remote.repo.clone())
        }
    };
    if !valid(&owner) || !valid(&repo) {
        return Err(ClientError::Invalid(
            "repository must be `name` or `owner/name` of letters, digits, `-`, `_`, `.`"
                .to_string(),
        ));
    }
    Ok((owner, repo))
}

fn repo_flag(cmd: &PrCommands) -> Option<&str> {
    match cmd {
        PrCommands::View { repo, .. }
        | PrCommands::Checks { repo, .. }
        | PrCommands::Comment { repo, .. }
        | PrCommands::Close { repo, .. }
        | PrCommands::Approve { repo, .. }
        | PrCommands::Regate { repo, .. } => repo.as_deref(),
        _ => None,
    }
}

pub(crate) fn run(
    api_url: Option<&str>,
    default_owner: &str,
    json: bool,
    cmd: PrCommands,
    out: &mut dyn Write,
) -> ClientResult<()> {
    // The address comes only from the operator; `origin` names the repository.
    let base = api_url.ok_or_else(no_forge)?.to_string();
    let remote = origin_remote();
    let (owner, repo) = resolve_repo(repo_flag(&cmd), default_owner, remote.as_ref())?;
    let api = client_for(&base)?;
    execute(&api, &owner, &repo, json, cmd, out)
}

fn execute(
    api: &ApiClient,
    owner: &str,
    repo: &str,
    json: bool,
    cmd: PrCommands,
    out: &mut dyn Write,
) -> ClientResult<()> {
    // `owner%2Frepo` addresses a repository on the /api/v1 routes.
    let v1 = format!("/api/v1/repos/{owner}%2F{repo}");
    let v3 = format!("/api/v3/repos/{owner}/{repo}");
    match cmd {
        PrCommands::View { pr, .. } => {
            let value = api.get(&format!("{v3}/pulls/{pr}"))?;
            let human = format!(
                "#{} {} [{}] {} -> {}",
                pr,
                text(&value, &["title"]),
                text(&value, &["state"]),
                text(&value, &["head", "ref"]),
                text(&value, &["base", "ref"]),
            );
            render(out, json, &value, &human)
        }
        PrCommands::Checks { pr, .. } => {
            let value = api.get(&format!("{v1}/pulls/{pr}/checks"))?;
            let count = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
            let human = format!(
                "{} checks: {} passing, {} failing, {} pending, {} skipped",
                count("total"),
                count("passing"),
                count("failing"),
                count("pending"),
                count("skipped"),
            );
            render(out, json, &value, &human)
        }
        PrCommands::Comment { pr, body, .. } => {
            if body.trim().is_empty() {
                return Err(ClientError::Invalid("comment --body is empty".to_string()));
            }
            whoami(api)?;
            let value = api.post(
                &format!("{v1}/pulls/{pr}/comments"),
                json!({"body_markdown": body}),
            )?;
            render(out, json, &value, &format!("commented on #{pr}"))
        }
        PrCommands::Close {
            pr, superseded_by, ..
        } => {
            whoami(api)?;
            let body = match superseded_by {
                Some(number) => json!({"superseded_by": number}),
                None => json!({}),
            };
            let value = api.post(&format!("{v1}/pulls/{pr}/close"), body)?;
            render(out, json, &value, &format!("closed #{pr}"))
        }
        PrCommands::Approve {
            pr,
            expected_head_sha,
            ..
        } => {
            whoami(api)?;
            let sha = match expected_head_sha {
                Some(sha) => sha,
                None => {
                    let detail = api.get(&format!("{v3}/pulls/{pr}"))?;
                    text(&detail, &["head", "sha"])
                }
            };
            if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(ClientError::Invalid(
                    "expected head sha must be a hex commit id".to_string(),
                ));
            }
            let value = api.post(
                &format!("{v1}/pulls/{pr}/approve"),
                json!({"expected_head_sha": sha}),
            )?;
            render(out, json, &value, &format!("approved #{pr} at {sha}"))
        }
        PrCommands::Regate { pr, .. } => {
            whoami(api)?;
            let value = api.post(&format!("{v1}/pulls/{pr}/regate"), json!({}))?;
            render(out, json, &value, &format!("re-gate requested for #{pr}"))
        }
        _ => Err(ClientError::NotWired(
            "not a remote-only pull request command".to_string(),
        )),
    }
}

fn text(value: &Value, path: &[&str]) -> String {
    let mut current = value;
    for key in path {
        match current.get(key) {
            Some(next) => current = next,
            None => return String::new(),
        }
    }
    current.as_str().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no address named, the token goes nowhere: the command refuses
    /// before any request, even inside a clone whose `origin` is a remote
    /// host (this test runs in the repository's own checkout).
    #[test]
    fn no_named_forge_address_means_no_request() {
        let mut out = Vec::new();
        let error = run_auth_doctor(None, false, &mut out).unwrap_err();
        assert!(
            matches!(&error, ClientError::NotWired(message) if message.contains("JERYU_API_URL")),
            "{error:?}"
        );
        assert!(out.is_empty());
    }

    #[test]
    fn parses_remote_forms_and_drops_credentials() {
        let want = |base: &str| RemoteForge {
            base_url: base.to_string(),
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
        };
        assert_eq!(
            parse_remote("https://forge.example/acme/widgets.git"),
            Some(want("https://forge.example"))
        );
        assert_eq!(
            parse_remote("https://bot:secret@forge.example:8443/acme/widgets"),
            Some(want("https://forge.example:8443"))
        );
        assert_eq!(
            parse_remote("git@forge.example:acme/widgets.git"),
            Some(want("https://forge.example"))
        );
        assert_eq!(
            parse_remote("ssh://git@forge.example:2222/acme/widgets.git"),
            Some(want("https://forge.example"))
        );
        assert_eq!(
            parse_remote("http://127.0.0.1:8787/acme/widgets"),
            Some(want("http://127.0.0.1:8787"))
        );
        assert_eq!(parse_remote("/srv/git/widgets.git"), None);
        assert_eq!(parse_remote("file:///srv/git/widgets.git"), None);
    }

    #[test]
    fn repository_names_are_validated_before_they_reach_a_path() {
        assert!(resolve_repo(Some("a/b"), "jeryu", None).is_ok());
        assert_eq!(
            resolve_repo(Some("b"), "jeryu", None).unwrap(),
            ("jeryu".to_string(), "b".to_string())
        );
        assert!(resolve_repo(Some("a/../b"), "jeryu", None).is_err());
        assert!(resolve_repo(Some("a/b?x=1"), "jeryu", None).is_err());
        assert!(resolve_repo(None, "jeryu", None).is_err());
    }
}
