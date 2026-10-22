//! Branch-protection routes
//! (`/repos/{owner}/{repo}/branches/{branch}/protection`) and their
//! GitHub-shaped renderer.

use jeryu_core::{BranchProtectionRule, SetBranchProtectionRequest};
use serde_json::{Value, json};

use crate::routes::Response;

use super::GithubRouter;
use super::support::{error_response, json_response, parse_body};

impl GithubRouter {
    pub(super) fn get_protection(&self, owner: &str, repo: &str, branch: &str) -> Response {
        let branch = match decode_branch(branch) {
            Ok(branch) => branch,
            Err(response) => return response,
        };
        match self.core.get_branch_protection(owner, repo, &branch) {
            Ok(rule) => json_response(200, &branch_protection_json(&rule)),
            Err(err) => error_response(err),
        }
    }

    pub(super) fn set_protection(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        body: &str,
    ) -> Response {
        let branch = match decode_branch(branch) {
            Ok(branch) => branch,
            Err(response) => return response,
        };
        let req: SetBranchProtectionRequest = match parse_body(body) {
            Ok(value) => value,
            Err(response) => return response,
        };
        match self.core.set_branch_protection(owner, repo, &branch, req) {
            Ok(rule) => json_response(200, &branch_protection_json(&rule)),
            Err(err) => error_response(err),
        }
    }
}

// Decode once, before applying Git's refs/heads name restrictions. A percent
// sign and a plus are ordinary branch-name characters, not query syntax.
fn decode_branch(segment: &str) -> Result<String, Response> {
    let invalid = || {
        error_response(jeryu_core::ForgeError::Validation(
            "invalid branch ref name".to_string(),
        ))
    };
    let mut bytes = Vec::with_capacity(segment.len());
    let mut input = segment.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let hi = input.next().and_then(|b| (b as char).to_digit(16));
            let lo = input.next().and_then(|b| (b as char).to_digit(16));
            let (Some(hi), Some(lo)) = (hi, lo) else {
                return Err(invalid());
            };
            bytes.push((hi * 16 + lo) as u8);
        } else {
            bytes.push(byte);
        }
    }
    let branch = String::from_utf8(bytes).map_err(|_| invalid())?;
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("@{")
        || branch
            .bytes()
            .any(|b| b <= b' ' || b == 127 || b"~^:?*[\\".contains(&b))
        || branch
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        return Err(invalid());
    }
    Ok(branch)
}

fn branch_protection_json(rule: &BranchProtectionRule) -> Value {
    json!({
        "url": format!(
            "/repos/{}/{}/branches/{}/protection",
            rule.owner, rule.repo, rule.branch
        ),
        "required_status_checks": {
            "strict": rule.strict,
            "contexts": rule.required_status_checks,
        },
        "required_pull_request_reviews": {
            "required_approving_review_count": rule.required_approving_review_count,
        },
        "enforce_admins": { "enabled": rule.enforce_admins },
        "required_linear_history": { "enabled": rule.required_linear_history },
        "allow_force_pushes": { "enabled": rule.allow_force_pushes },
        "allow_deletions": { "enabled": rule.allow_deletions },
        "required_signatures": { "enabled": rule.require_signed_commits },
        "required_jankurai_proof": { "enabled": rule.require_jankurai_proof },
        "updated_at": rule.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::decode_branch;

    #[test]
    fn decoded_branch_names_follow_git_rules() {
        for (encoded, decoded) in [
            ("main", "main"),
            ("release%2Fx", "release/x"),
            ("release%2fx", "release/x"),
            ("a+b", "a+b"),
            ("%252F", "%2F"),
            ("%40", "@"),
            ("caf%C3%A9", "café"),
        ] {
            assert_eq!(decode_branch(encoded).unwrap(), decoded);
            assert!(
                std::process::Command::new("git")
                    .args(["check-ref-format", "--branch", decoded])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        for encoded in [
            "",
            "a%2Fb.lock",
            "a%2F.b",
            "a%2F%2Fb",
            "a.",
            "a%7Eb",
            "a%5Eb",
            "a%3Ab",
            "a%3Fb",
            "a%2Ab",
            "a%5Bb",
            "a%7Fb",
            "%2Fa",
            "%",
            "%GG",
        ] {
            assert!(decode_branch(encoded).is_err(), "{encoded}");
        }
    }
}
