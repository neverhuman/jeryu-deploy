//! What a contributed pull request needs before its head is gated and merged:
//! a maintainer's `ok-to-test` and a DCO sign-off on every commit.
//!
//! A contribution is a stranger's code, so its gate does not start on its own.
//! A login on the site's maintainer list (`JERYU_PULL_MAINTAINERS`) says
//! `ok-to-test` for one exact head. The forge publishes that allowance at
//! `GET /api/v1/gate-allowances?state=active`, the route the gate runner reads
//! each tick (jeryu-ci-runner `ops/pr-gate/README.md`, "Contributions"): the
//! runner executes nothing of a pull request labelled `origin:contributed`
//! unless an allowance from one of its own configured maintainers names the
//! head it is about to run, and it fails closed when the route does not
//! answer. That runner check is the control; the forge also stops waking the
//! runner for the head (`gate_wanted`) and refuses a re-gate until the head is
//! allowed. A new head needs a new `ok-to-test`, so a force-push cannot ride an
//! earlier one.
//!
//! Outside contributors sign the DCO instead of a CLA: every commit of a
//! contributed head must carry a `Signed-off-by: Name <email>` trailer that
//! matches the commit's author. The check reads the commits from the bare
//! repository, so nothing the contributor wrote in the title or description
//! changes it, and a head the forge cannot read fails it.
//!
//! The `ok-to-test` record is in memory, like the re-gate requests: a restart
//! forgets it and a maintainer says it again, which fails closed.
//!
//! - `POST /api/v1/repos/:id/pulls/:number/ok-to-test`: a maintainer allows the
//!   gate on the pull request's current head. Optional body
//!   `{"head_sha": "<40 hex>"}` names the head the maintainer looked at; any
//!   other body is refused rather than read as "no expectation".
//! - `GET /api/v1/gate-allowances?state=active|all`: the allowances, one per
//!   pull request, in the shape the gate runner reads. `active` (the default)
//!   lists only those that still name an open contribution's current head.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use jeryu_core::{AccountSummary, PullRequest};
use serde::{Deserialize, Serialize};

use super::pull_origin;
use super::{WebState, api_error};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};

/// `ok-to-test` records kept at once; the oldest is forgotten first.
const MAX_RECORDS: usize = 1024;
/// DCO verdicts cached by head; the cache is dropped when it fills.
const MAX_CACHED: usize = 256;
/// Most commits one DCO check reads. A contribution longer than this fails the
/// check rather than being read in part.
const MAX_COMMITS: usize = 500;
/// Most offending commits a failure message names.
const MAX_NAMED: usize = 5;

/// The label an allowance carries: the runner's `PR_GATE_OK_TO_TEST_LABEL`
/// default, which it compares with exactly.
pub(crate) const OK_TO_TEST_LABEL: &str = "ok-to-test";

/// Commit status context the DCO result is reported under.
pub(crate) const DCO_CONTEXT: &str = "jeryu/dco";

/// One maintainer's `ok-to-test`, bound to the head they allowed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OkToTest {
    head_sha: String,
    by: String,
    at_ms: i64,
}

type OkRecords = HashMap<(String, String, u64), OkToTest>;
type DcoVerdicts = HashMap<(String, String, String), DcoVerdict>;

#[derive(Clone, Default)]
pub(crate) struct OkToTestStore {
    inner: Arc<Mutex<OkRecords>>,
}

impl OkToTestStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, OkRecords> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn record(&self, pr: &PullRequest, by: &str) {
        let mut records = self.lock();
        let key = (pr.owner.clone(), pr.repo.clone(), pr.number);
        if !records.contains_key(&key)
            && records.len() >= MAX_RECORDS
            && let Some(oldest) = records
                .iter()
                .min_by_key(|(_, record)| record.at_ms)
                .map(|(key, _)| key.clone())
        {
            records.remove(&oldest);
        }
        records.insert(
            key,
            OkToTest {
                head_sha: pr.head.sha.clone(),
                by: by.to_string(),
                at_ms: chrono::Utc::now().timestamp_millis(),
            },
        );
    }

    /// Every record, by (owner, repo, number).
    fn all(&self) -> Vec<((String, String, u64), OkToTest)> {
        self.lock()
            .iter()
            .map(|(key, record)| (key.clone(), record.clone()))
            .collect()
    }

    /// Who allowed this exact head, if anybody has.
    pub(crate) fn allowed_by(&self, pr: &PullRequest) -> Option<String> {
        self.lock()
            .get(&(pr.owner.clone(), pr.repo.clone(), pr.number))
            .filter(|record| record.head_sha == pr.head.sha)
            .map(|record| record.by.clone())
    }
}

/// The DCO check's answer for one head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DcoVerdict {
    Pass,
    /// Every commit named lacks a matching sign-off; the text is ready to show.
    Fail(String),
}

impl DcoVerdict {
    pub(crate) fn passed(&self) -> bool {
        matches!(self, Self::Pass)
    }
}

#[derive(Clone, Default)]
pub(crate) struct DcoCache {
    inner: Arc<Mutex<DcoVerdicts>>,
}

impl DcoCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, DcoVerdicts> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }
}

/// The DCO verdict for a pull request's head. A head is immutable, so a
/// verdict about its commits is cached by (repository, base, head). A head the
/// forge could not read fails closed but is not cached: that is this host's
/// problem, not the contributor's, and the next read tries again.
pub(crate) fn dco_verdict(state: &WebState, pr: &PullRequest) -> DcoVerdict {
    let key = (
        format!("{}/{}", pr.owner, pr.repo),
        pr.base.sha.clone(),
        pr.head.sha.clone(),
    );
    if let Some(known) = state.dco_cache.lock().get(&key) {
        return known.clone();
    }
    let verdict = match compute_dco(state, pr) {
        Ok(verdict) => verdict,
        Err(unreadable) => return DcoVerdict::Fail(unreadable),
    };
    let mut cache = state.dco_cache.lock();
    if cache.len() >= MAX_CACHED {
        cache.clear();
    }
    cache.insert(key, verdict.clone());
    verdict
}

/// The verdict on the head's commits, or why they could not be read.
fn compute_dco(state: &WebState, pr: &PullRequest) -> Result<DcoVerdict, String> {
    let unreadable = |why: &str| {
        Err(format!(
            "the forge could not read this head's commits to check their sign-offs ({why})"
        ))
    };
    if !crate::github::pulls::is_hex40(&pr.head.sha)
        || !crate::github::pulls::is_hex40(&pr.base.sha)
    {
        return unreadable("the head or base is not a full commit id");
    }
    let Ok(resolved) = state.repo_manager.resolve_parts(&pr.owner, &pr.repo) else {
        return unreadable("the repository has no bare copy on this host");
    };
    let git_bin = state.repo_manager.config().git_bin.clone();
    match commits_without_sign_off(&git_bin, &resolved.path, &pr.base.sha, &pr.head.sha) {
        Ok(missing) if missing.is_empty() => Ok(DcoVerdict::Pass),
        Ok(missing) => Ok(DcoVerdict::Fail(dco_failure_message(&missing))),
        Err(why) => unreadable(&why),
    }
}

/// The failure text a contributor reads: what is wrong, which commits, and the
/// command that fixes it.
fn dco_failure_message(missing: &[String]) -> String {
    let named: Vec<&str> = missing.iter().take(MAX_NAMED).map(String::as_str).collect();
    let more = missing.len().saturating_sub(MAX_NAMED);
    let tail = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    format!(
        "DCO sign-off missing: every commit needs a `Signed-off-by:` line matching its author. \
         Not signed: {}{tail}. Add it with `git commit -s --amend` (or \
         `git rebase --signoff <base>` for several commits) and push again.",
        named.join(", ")
    )
}

/// Short ids of the non-merge commits in `base..head` whose message has no
/// `Signed-off-by` matching the commit's author.
fn commits_without_sign_off(
    git_bin: &str,
    bare: &Path,
    base: &str,
    head: &str,
) -> Result<Vec<String>, String> {
    let output = Command::new(git_bin)
        .current_dir(bare)
        .args([
            "log",
            "--no-merges",
            "-n",
            &(MAX_COMMITS + 1).to_string(),
            "--format=%H%x1f%an%x1f%ae%x1f%B%x1e",
            &format!("{base}..{head}"),
            "--",
        ])
        .output()
        .map_err(|err| format!("git did not start: {err}"))?;
    if !output.status.success() {
        return Err("git log failed".to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let records: Vec<&str> = text
        .split('\x1e')
        .map(str::trim)
        .filter(|record| !record.is_empty())
        .collect();
    if records.len() > MAX_COMMITS {
        return Err(format!("more than {MAX_COMMITS} commits"));
    }
    let mut missing = Vec::new();
    for record in records {
        let mut fields = record.splitn(4, '\x1f');
        let (Some(sha), Some(name), Some(email), Some(message)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err("unexpected git log output".to_string());
        };
        if !has_matching_sign_off(message, name, email) {
            missing.push(sha.chars().take(12).collect());
        }
    }
    Ok(missing)
}

/// Whether some line of `message` is `Signed-off-by: <name> <<email>>` for the
/// author. The name matches exactly and the address ignoring case.
fn has_matching_sign_off(message: &str, name: &str, email: &str) -> bool {
    message.lines().any(|line| {
        let line = line.trim();
        let Some(prefix) = line.get(..14) else {
            return false;
        };
        if !prefix.eq_ignore_ascii_case("signed-off-by:") {
            return false;
        }
        let signer = line[14..].trim();
        let Some((signed_name, rest)) = signer.split_once('<') else {
            return false;
        };
        let Some(signed_email) = rest.strip_suffix('>') else {
            return false;
        };
        !name.trim().is_empty()
            && signed_name.trim() == name.trim()
            && signed_email.trim().eq_ignore_ascii_case(email.trim())
    })
}

/// Whether the gate may start on this pull request's head: always for an
/// internal pull request, and for a contribution once a maintainer has said
/// `ok-to-test` for this exact head.
pub(crate) fn gate_may_start(state: &WebState, pr: &PullRequest) -> bool {
    !pull_origin::is_contributed(state, pr) || state.ok_to_test.allowed_by(pr).is_some()
}

#[derive(Debug, Serialize)]
struct OkToTestResponse {
    head_sha: String,
    ok_to_test_by: String,
    dco: &'static str,
    dco_detail: Option<String>,
}

/// `POST /api/v1/repos/:id/pulls/:number/ok-to-test`.
pub(super) async fn ok_to_test(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
    body: Bytes,
) -> AxumResponse {
    if !state
        .pull_maintainers
        .iter()
        .any(|login| login == &account.login)
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "pull_maintainer_required",
            "only a login on the JERYU_PULL_MAINTAINERS list may say ok-to-test",
        );
    }
    let Some(repo) = super::repositories::find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "pull request not found");
    };
    let Ok(pr) = state
        .github
        .core()
        .get_pull_request(&repo.owner, &repo.name, number)
    else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "pull request not found");
    };
    if !super::merge_queue::is_open_pull(&pr) {
        return api_error(
            StatusCode::CONFLICT,
            "not_open",
            "only an open pull request has a head to test",
        );
    }
    if !pull_origin::is_contributed(&state, &pr) {
        return api_error(
            StatusCode::CONFLICT,
            "not_contributed",
            "only a contributed pull request waits for ok-to-test",
        );
    }
    let expected = match expected_head(&body) {
        Ok(expected) => expected,
        Err(message) => return api_error(StatusCode::BAD_REQUEST, "invalid_request", message),
    };
    if let Some(expected) = expected
        && expected != pr.head.sha
    {
        return api_error(
            StatusCode::CONFLICT,
            "head_moved",
            "the pull request head is not the one you looked at; read it again first",
        );
    }
    state.ok_to_test.record(&pr, &account.login);
    super::gate_wanted::record_ok_to_test(
        &state,
        &format!("{}/{}", pr.owner, pr.repo),
        number,
        &pr.head.sha,
    );
    let dco = dco_verdict(&state, &pr);
    Json(OkToTestResponse {
        head_sha: pr.head.sha.clone(),
        ok_to_test_by: account.login,
        dco: if dco.passed() { "pass" } else { "fail" },
        dco_detail: match dco {
            DcoVerdict::Pass => None,
            DcoVerdict::Fail(message) => Some(message),
        },
    })
    .into_response()
}

/// The head an `ok-to-test` body names: none for an empty body, else the one
/// `head_sha` field, a full commit id. Anything else is refused, so a typo'd
/// key or a malformed body never turns a pinned ok-to-test into an unpinned one.
fn expected_head(body: &[u8]) -> Result<Option<String>, &'static str> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_slice(body) else {
        return Err("the body must be empty or {\"head_sha\": \"<40 hex>\"}");
    };
    if fields.keys().any(|key| key != "head_sha") {
        return Err("ok-to-test takes one field, head_sha");
    }
    match fields.get("head_sha") {
        None => Ok(None),
        Some(serde_json::Value::String(sha))
            if crate::github::pulls::is_hex40(&sha.to_ascii_lowercase()) =>
        {
            Ok(Some(sha.to_ascii_lowercase()))
        }
        Some(_) => Err("head_sha must be a full 40-hex commit id"),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct AllowanceQuery {
    state: Option<String>,
}

#[derive(Debug, Serialize)]
struct Allowance {
    repo: String,
    number: u64,
    head_sha: String,
    label: &'static str,
    allowed_by: String,
    allowed_at: String,
}

/// `GET /api/v1/gate-allowances?state=active|all`: the `ok-to-test`
/// allowances in the shape the gate runner reads (jeryu-ci-runner
/// `ops/pr-gate/README.md`), limited to repositories the caller can read.
/// `active` keeps only an allowance that still names an open contribution's
/// current head and came from a login still on the maintainer list.
pub(super) async fn gate_allowances(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<AllowanceQuery>,
) -> AxumResponse {
    let active = match query.state.as_deref() {
        None | Some("active") => true,
        Some("all") => false,
        Some(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_query",
                "state is active or all",
            );
        }
    };
    let mut allowances: Vec<Allowance> = state
        .ok_to_test
        .all()
        .into_iter()
        .filter(|((owner, repo, _), _)| {
            account.role == jeryu_core::UserRole::Admin
                || state.core.user_can_read_repo(&account.login, owner, repo)
        })
        .filter(|((owner, repo, number), record)| {
            !active
                || (state
                    .pull_maintainers
                    .iter()
                    .any(|login| login == &record.by)
                    && state
                        .core
                        .get_pull_request(owner, repo, *number)
                        .is_ok_and(|pr| {
                            super::merge_queue::is_open_pull(&pr)
                                && pr.head.sha == record.head_sha
                                && pull_origin::is_contributed(&state, &pr)
                        }))
        })
        .map(|((owner, repo, number), record)| Allowance {
            repo: format!("{owner}/{repo}"),
            number,
            head_sha: record.head_sha,
            label: OK_TO_TEST_LABEL,
            allowed_by: record.by,
            allowed_at: chrono::DateTime::from_timestamp_millis(record.at_ms)
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .unwrap_or_default(),
        })
        .collect();
    allowances.sort_by(|a, b| (&a.repo, a.number).cmp(&(&b.repo, b.number)));
    Json(serde_json::json!({ "allowances": allowances })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sign_off_must_match_the_author() {
        let message = "add a widget\n\nSigned-off-by: Ada Lovelace <ada@example.test>\n";
        assert!(has_matching_sign_off(
            message,
            "Ada Lovelace",
            "ada@example.test"
        ));
        assert!(has_matching_sign_off(
            message,
            "Ada Lovelace",
            "ADA@example.test"
        ));
        assert!(!has_matching_sign_off(
            message,
            "Grace Hopper",
            "ada@example.test"
        ));
        assert!(!has_matching_sign_off(
            message,
            "Ada Lovelace",
            "grace@example.test"
        ));
        assert!(!has_matching_sign_off(
            "add a widget\n",
            "Ada Lovelace",
            "ada@example.test"
        ));
        assert!(!has_matching_sign_off(
            "Signed-off-by: Ada Lovelace ada@example.test",
            "Ada Lovelace",
            "ada@example.test"
        ));
    }

    #[test]
    fn the_failure_message_names_commits_and_the_fix() {
        let missing: Vec<String> = (0..7).map(|n| format!("abc{n}")).collect();
        let message = dco_failure_message(&missing);
        assert!(message.contains("abc0"));
        assert!(message.contains("and 2 more"));
        assert!(message.contains("git commit -s"));
    }

    #[test]
    fn an_ok_to_test_body_is_empty_or_names_one_full_head() {
        let sha = "ab".repeat(20);
        assert_eq!(expected_head(b""), Ok(None));
        assert_eq!(expected_head(b"  \n"), Ok(None));
        assert_eq!(expected_head(b"{}"), Ok(None));
        assert_eq!(
            expected_head(format!("{{\"head_sha\":\"{}\"}}", sha.to_uppercase()).as_bytes()),
            Ok(Some(sha.clone()))
        );
        for refused in [
            "not json".to_string(),
            "[]".to_string(),
            format!("{{\"headSha\":\"{sha}\"}}"),
            format!("{{\"head_sha\":\"{sha}\",\"extra\":1}}"),
            "{\"head_sha\":\"abc123\"}".to_string(),
            "{\"head_sha\":7}".to_string(),
        ] {
            assert!(expected_head(refused.as_bytes()).is_err(), "{refused}");
        }
    }
}
