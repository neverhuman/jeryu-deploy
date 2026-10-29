//! Per-head gate claims: the forge-held lease that lets several gate hosts
//! run the PR gate at once.
//!
//! Gate hosts used to be partitioned by owner by hand
//! (`GATE_RUNNER_OWNERS` on each host), so a host that hung left its share
//! ungated until an operator moved those owners to another host. The forge
//! now holds the claim on each gated head instead:
//!
//! - a slot asks `POST /api/v1/gate-claims`
//!   (`{repo, number, head_sha|queue_sha, runner_id, host}`) before it posts
//!   `pending`, and gets an exclusive lease for
//!   [`GATE_CLAIM_TTL_SECS`] seconds;
//! - a second host asking for the same head is refused `409 claim_held`, so
//!   only one host ever posts `<repo>/required` for a head;
//! - the holder renews by asking again while the gate runs (the same ask is
//!   the renewal: it moves `expires_at` and keeps `granted_at`), and releases
//!   on publication with `POST /api/v1/gate-claims/release`;
//! - a lease nobody renews (the host died, the slot was killed) expires, and
//!   the head is claimable by any live host within one TTL, which is how a
//!   dead host's work is picked up without an operator.
//!
//! Capability matching replaces the owner partition. A repository declares
//! what a gate of it needs in its own `.jeryu/repo.toml`:
//!
//! ```toml
//! [gate]
//! capabilities = ["host-ci"]
//! ```
//!
//! That file is read at the gated head's base branch (the repository's
//! default branch when the head has no open pull request), never at the pull
//! request's own head: a pull request must not be able to lower, or raise,
//! what its own gate requires. A host advertises what it has in its
//! heartbeat (`capabilities`), and a claim is granted only to a host whose
//! advertised set covers the repository's required set. A file that cannot be
//! parsed, or that names an unknown or repeated capability, grants nothing:
//! the ask is refused with the problem named, because a gate whose
//! requirements cannot be read must not run on a host that may not have them.
//!
//! Who may claim: the logins that may report heartbeats
//! ([`GateRunnerStore::may_report`](super::control_plane::GateRunnerStore::may_report))
//! — a gate claim is a runner acting, and an ordinary account must not be
//! able to park a lease on a head and stop every host from gating it.
//!
//! The leases are in memory, like the heartbeats they are matched against: a
//! forge that restarts loses them, and the first host to ask takes the head
//! again. A head that was being gated across the restart is then gated twice
//! at worst, which is what happened before any of this existed.
//!
//! An expired lease is kept for [`STALE_CLAIM_RETENTION_SECS`] so needs-you
//! can say that a head's gate stopped halfway, and forgotten after that.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use super::*;

/// Seconds a gate claim is held without a renewal. Three heartbeat periods of
/// a gate slot (a slot beats every minute): a host that is merely slow keeps
/// its head, and a host that is gone loses it within one TTL.
pub(crate) const GATE_CLAIM_TTL_SECS: i64 = 180;
/// How long an expired claim is still remembered, so needs-you can report a
/// gate that stopped halfway rather than forget it silently.
pub(crate) const STALE_CLAIM_RETENTION_SECS: i64 = 3600;
/// Claims kept at once (live and recently expired). A gate host runs a handful
/// of slots, so this is far above anything the gated heads can need; the
/// oldest expired claim is forgotten first, and a live claim is never dropped
/// to make room.
const MAX_CLAIMS: usize = 512;
/// Most bytes of `.jeryu/repo.toml` this path reads. A control file is a
/// handful of lines; anything larger is not read at all.
const MAX_REPO_DECLARATION_BYTES: u64 = 64 * 1024;
/// The declaration the capability policy is read from, at the base branch.
pub(crate) const REPO_DECLARATION_PATH: &str = ".jeryu/repo.toml";

/// One head's lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GateClaim {
    /// `owner/name`.
    pub(crate) repo: String,
    /// The pull request the head belongs to, or 0 for a head that has none
    /// (a merge-queue or branch gate).
    pub(crate) number: u64,
    /// The commit being gated: a pull request head or a queue sha.
    pub(crate) head: String,
    pub(crate) runner_id: String,
    pub(crate) host: String,
    pub(crate) granted_at: DateTime<Utc>,
    pub(crate) renewed_at: DateTime<Utc>,
    pub(crate) expires_at: DateTime<Utc>,
    /// What the repository required when the lease was granted, so a stale
    /// lease can be reported without reading git again.
    pub(crate) required_capabilities: Vec<String>,
}

impl GateClaim {
    fn live(&self, now: DateTime<Utc>) -> bool {
        self.expires_at > now
    }
}

/// What asking for a lease did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Granted {
    /// A lease this runner did not hold before.
    Fresh(GateClaim),
    /// The holder asked again: the same lease, held longer.
    Renewed(GateClaim),
    /// Another runner holds a live lease on this head.
    Held(GateClaim),
}

/// What releasing did. Releasing a lease that is not there is not an error: a
/// runner that publishes twice (a retry, a recovery) must not be refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseOutcome {
    Released,
    Absent,
    HeldByOther(GateClaim),
}

/// One head, as a runner names it when it asks for or gives back a lease.
pub(crate) struct ClaimAsk<'a> {
    /// `owner/name`.
    pub(crate) repo: &'a str,
    /// The pull request the head belongs to, or 0 for a head without one.
    pub(crate) number: u64,
    pub(crate) head: &'a str,
    pub(crate) runner_id: &'a str,
    pub(crate) host: &'a str,
}

type ClaimKey = (String, u64, String);

#[derive(Debug, Clone, Default)]
pub(crate) struct GateClaimStore {
    claims: Arc<Mutex<BTreeMap<ClaimKey, GateClaim>>>,
}

impl GateClaimStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<ClaimKey, GateClaim>> {
        self.claims
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take or renew the lease on one head.
    ///
    /// Idempotent for the holder: the same ask repeated is the renewal, and
    /// answers the same lease with a later `expires_at`. A lease that has
    /// expired belongs to whoever asks next, including its previous holder.
    pub(crate) fn acquire(
        &self,
        ask: &ClaimAsk<'_>,
        required_capabilities: Vec<String>,
        now: DateTime<Utc>,
    ) -> Granted {
        let mut claims = self.lock();
        forget_old(&mut claims, now);
        let key = (ask.repo.to_string(), ask.number, ask.head.to_string());
        if let Some(held) = claims.get(&key)
            && held.live(now)
            && held.runner_id != ask.runner_id
        {
            return Granted::Held(held.clone());
        }
        let renewed = claims
            .get(&key)
            .filter(|held| held.live(now) && held.runner_id == ask.runner_id)
            .cloned();
        let claim = GateClaim {
            repo: ask.repo.to_string(),
            number: ask.number,
            head: ask.head.to_string(),
            runner_id: ask.runner_id.to_string(),
            host: ask.host.to_string(),
            granted_at: renewed.as_ref().map_or(now, |held| held.granted_at),
            renewed_at: now,
            expires_at: now + chrono::Duration::seconds(GATE_CLAIM_TTL_SECS),
            required_capabilities,
        };
        claims.insert(key, claim.clone());
        if renewed.is_some() {
            Granted::Renewed(claim)
        } else {
            Granted::Fresh(claim)
        }
    }

    /// Give the head back, so the next host can gate it without waiting out
    /// the TTL. Only the holder may: a host must not be able to clear another
    /// host's lease while that host is gating the head.
    pub(crate) fn release(
        &self,
        repo: &str,
        number: u64,
        head: &str,
        runner_id: &str,
        now: DateTime<Utc>,
    ) -> ReleaseOutcome {
        let mut claims = self.lock();
        forget_old(&mut claims, now);
        let key = (repo.to_string(), number, head.to_string());
        match claims.get(&key) {
            None => ReleaseOutcome::Absent,
            Some(held) if held.runner_id == runner_id || !held.live(now) => {
                claims.remove(&key);
                ReleaseOutcome::Released
            }
            Some(held) => ReleaseOutcome::HeldByOther(held.clone()),
        }
    }

    /// The lease on one head, live or expired.
    #[cfg(test)]
    pub(crate) fn claim(&self, repo: &str, number: u64, head: &str) -> Option<GateClaim> {
        self.lock()
            .get(&(repo.to_string(), number, head.to_string()))
            .cloned()
    }

    /// Leases nobody renewed: the gate that held them stopped without
    /// publishing or releasing. What needs-you reports.
    pub(crate) fn stale(&self, now: DateTime<Utc>) -> Vec<GateClaim> {
        self.lock()
            .values()
            .filter(|claim| !claim.live(now))
            .cloned()
            .collect()
    }
}

/// Forget expired claims past the retention window, and -- only if the store
/// is still full -- the oldest expired ones. A live lease is never dropped:
/// dropping one would hand the same head to a second host.
fn forget_old(claims: &mut BTreeMap<ClaimKey, GateClaim>, now: DateTime<Utc>) {
    claims.retain(|_, claim| {
        claim.live(now) || (now - claim.expires_at).num_seconds() <= STALE_CLAIM_RETENTION_SECS
    });
    while claims.len() >= MAX_CLAIMS {
        let Some(oldest) = claims
            .iter()
            .filter(|(_, claim)| !claim.live(now))
            .min_by_key(|(_, claim)| claim.expires_at)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        claims.remove(&oldest);
    }
}

/// The ask, as the runner writes it. `head_sha` for a pull request head,
/// `queue_sha` for a merge-queue entry: exactly one.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GateClaimRequest {
    /// `owner/name`.
    pub(crate) repo: String,
    /// The pull request number, or 0 for a head without one.
    #[serde(default)]
    pub(crate) number: u64,
    #[serde(default, alias = "headSha")]
    pub(crate) head_sha: Option<String>,
    #[serde(default, alias = "queueSha")]
    pub(crate) queue_sha: Option<String>,
    #[serde(alias = "runnerId")]
    pub(crate) runner_id: String,
    pub(crate) host: String,
}

/// The head one ask names, validated.
#[derive(Debug)]
struct Asked {
    owner: String,
    name: String,
    head: String,
}

fn validate(request: &GateClaimRequest) -> Result<Asked, String> {
    let (owner, name) = request
        .repo
        .split_once('/')
        .ok_or_else(|| "repo must be owner/name".to_string())?;
    for (field, value, extra) in [
        ("repo owner", owner, "-_."),
        ("repo name", name, "-_."),
        ("runner_id", request.runner_id.as_str(), "-_./"),
        ("host", request.host.as_str(), "-."),
    ] {
        if value.is_empty() || value.len() > 128 {
            return Err(format!("{field} must be 1 to 128 characters"));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
        {
            return Err(format!("{field} has a character it may not"));
        }
    }
    let head = match (&request.head_sha, &request.queue_sha) {
        (Some(head), None) | (None, Some(head)) => head,
        _ => return Err("exactly one of head_sha or queue_sha is required".to_string()),
    };
    // A sha reaches git's argv on the paths this claim gates, so only a
    // commit id shape is accepted here.
    if !(7..=64).contains(&head.len())
        || !head
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    {
        return Err("head_sha/queue_sha must be a lowercase hex commit id".to_string());
    }
    Ok(Asked {
        owner: owner.to_string(),
        name: name.to_string(),
        head: head.clone(),
    })
}

/// What the repository requires of a gate host, from `[gate] capabilities` in
/// its own `.jeryu/repo.toml`, read at the base branch of the gated head.
///
/// `Ok(set)` is the policy (empty: no special capability). `Err(reason)` is a
/// file that could not be read as a policy; nothing is claimable then.
fn required_capabilities(
    state: &WebState,
    owner: &str,
    name: &str,
    number: u64,
) -> Result<Vec<String>, String> {
    let Ok(opened) = state.repo_manager.open_parts(owner, name) else {
        // No bare storage: a metadata-only repository declares nothing.
        return Ok(Vec::new());
    };
    let dir = opened.path.as_path();
    let Some(base) = base_commit(state, dir, owner, name, number) else {
        return Ok(Vec::new());
    };
    let Some(text) = read_declaration(state, dir, &base)? else {
        return Ok(Vec::new());
    };
    parse_capabilities(&text)
}

/// The commit the policy is read at: the tip of the gated head's base branch
/// when the head belongs to an open pull request, else the repository's
/// default branch. Never the pull request's head.
fn base_commit(
    state: &WebState,
    dir: &std::path::Path,
    owner: &str,
    name: &str,
    number: u64,
) -> Option<String> {
    let mut branch = super::repositories::find_repo(state, &format!("{owner}/{name}"))
        .map(|repo| repo.default_branch)
        .filter(|branch| !branch.is_empty())?;
    if number > 0
        && let Ok(pr) = state.core.get_pull_request(owner, name, number)
        && !pr.base.ref_name.is_empty()
    {
        branch = pr.base.ref_name.clone();
    }
    let tip = format!("refs/heads/{branch}^{{commit}}");
    let resolved = crate::web::shift::queue::git(
        &state.repo_manager.config().git_bin,
        dir,
        &["rev-parse", "--verify", "--quiet", "--end-of-options", &tip],
        &[],
        None,
    )
    .ok()
    .map(|out| String::from_utf8_lossy(&out).trim().to_string())?;
    (resolved.len() == 40 && resolved.chars().all(|c| c.is_ascii_hexdigit())).then_some(resolved)
}

/// `<base>:.jeryu/repo.toml`, when it is there and small enough to read.
///
/// `Ok(None)`: the repository declares nothing. `Err`: git could not be asked,
/// or the file is too large to read -- either way its policy is unknown.
fn read_declaration(
    state: &WebState,
    dir: &std::path::Path,
    base: &str,
) -> Result<Option<String>, String> {
    let git_bin = &state.repo_manager.config().git_bin;
    let listed = crate::web::shift::queue::git(
        git_bin,
        dir,
        &[
            "ls-tree",
            "--end-of-options",
            base,
            "--",
            REPO_DECLARATION_PATH,
        ],
        &[],
        None,
    )
    .map_err(|error| format!("{REPO_DECLARATION_PATH} could not be listed: {error}"))?;
    if listed.is_empty() {
        return Ok(None);
    }
    let object = format!("{base}:{REPO_DECLARATION_PATH}");
    let size = crate::web::shift::queue::git(
        git_bin,
        dir,
        &["cat-file", "-s", "--end-of-options", &object],
        &[],
        None,
    )
    .ok()
    .and_then(|out| String::from_utf8_lossy(&out).trim().parse::<u64>().ok())
    .ok_or_else(|| format!("{REPO_DECLARATION_PATH} could not be read"))?;
    if size > MAX_REPO_DECLARATION_BYTES {
        return Err(format!(
            "{REPO_DECLARATION_PATH} is larger than {MAX_REPO_DECLARATION_BYTES} bytes"
        ));
    }
    crate::web::shift::queue::git(
        git_bin,
        dir,
        &["cat-file", "blob", "--end-of-options", &object],
        &[],
        None,
    )
    .map(|out| Some(String::from_utf8_lossy(&out).into_owned()))
    .map_err(|error| format!("{REPO_DECLARATION_PATH} could not be read: {error}"))
}

/// `[gate] capabilities = ["host-ci", ...]`, validated against the closed
/// vocabulary every heartbeat is validated against. Anything the policy
/// cannot be read from is an error naming the file: a typo must neither
/// require a capability nobody has nor quietly require nothing.
pub(crate) fn parse_capabilities(text: &str) -> Result<Vec<String>, String> {
    let table = text
        .parse::<toml::Table>()
        .map_err(|error| format!("{REPO_DECLARATION_PATH} is not valid TOML: {error}"))?;
    let Some(gate) = table.get("gate") else {
        return Ok(Vec::new());
    };
    let gate = gate
        .as_table()
        .ok_or_else(|| format!("{REPO_DECLARATION_PATH}: [gate] must be a table"))?;
    let Some(declared) = gate.get("capabilities") else {
        return Ok(Vec::new());
    };
    let declared = declared
        .as_array()
        .ok_or_else(|| format!("{REPO_DECLARATION_PATH}: gate.capabilities must be an array"))?;
    let mut capabilities = Vec::new();
    for value in declared {
        let name = value.as_str().ok_or_else(|| {
            format!("{REPO_DECLARATION_PATH}: gate.capabilities must hold strings")
        })?;
        capabilities.push(name.to_string());
    }
    super::control_plane::validate_capabilities(&capabilities)
        .map_err(|reason| format!("{REPO_DECLARATION_PATH}: {reason}"))?;
    Ok(capabilities)
}

/// What the live slots on one host advertise, as the union of their
/// heartbeats. A host with no live slot advertises nothing.
pub(crate) fn host_capabilities(
    runners: &[super::control_plane::GateRunnerRecord],
    host: &str,
    now: DateTime<Utc>,
) -> BTreeSet<String> {
    runners
        .iter()
        .filter(|record| record.heartbeat.host == host)
        .filter(|record| super::control_plane::holds_gate_slot(&record.heartbeat))
        .filter(|record| super::control_plane::is_online(record, now))
        .flat_map(|record| record.heartbeat.capabilities.iter().cloned())
        .collect()
}

/// Is a gate host alive that has every capability this head needs?
pub(crate) fn capable_host_alive(
    runners: &[super::control_plane::GateRunnerRecord],
    required: &[String],
    now: DateTime<Utc>,
) -> bool {
    let hosts: BTreeSet<&str> = runners
        .iter()
        .filter(|record| super::control_plane::holds_gate_slot(&record.heartbeat))
        .filter(|record| super::control_plane::is_online(record, now))
        .map(|record| record.heartbeat.host.as_str())
        .collect();
    hosts.into_iter().any(|host| {
        let advertised = host_capabilities(runners, host, now);
        required.iter().all(|name| advertised.contains(name))
    })
}

fn claim_json(claim: &GateClaim, state_word: &str) -> Value {
    json!({
        "state": state_word,
        "repo": claim.repo,
        "number": claim.number,
        "head": claim.head,
        "runner_id": claim.runner_id,
        "host": claim.host,
        "granted_at": claim.granted_at.to_rfc3339(),
        "renewed_at": claim.renewed_at.to_rfc3339(),
        "expires_at": claim.expires_at.to_rfc3339(),
        "ttl_seconds": GATE_CLAIM_TTL_SECS,
        "required_capabilities": claim.required_capabilities,
    })
}

fn may_claim(state: &WebState, account: &AccountSummary) -> bool {
    state
        .gate_runners
        .may_report(&account.login, account.role == UserRole::Admin)
}

fn not_a_runner() -> AxumResponse {
    api_error(
        StatusCode::FORBIDDEN,
        "permission_denied",
        "this account may not claim gated heads: claim as a forge admin or a JERYU_RUNNER_REPORTERS login",
    )
}

/// `POST /api/v1/gate-claims`: take the lease on one head, or renew the lease
/// this runner already holds.
pub(super) async fn acquire(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Json(request): Json<GateClaimRequest>,
) -> AxumResponse {
    if !may_claim(&state, &account) {
        return not_a_runner();
    }
    let asked = match validate(&request) {
        Ok(asked) => asked,
        Err(reason) => {
            return api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", &reason);
        }
    };
    if super::repositories::find_repo(&state, &format!("{}/{}", asked.owner, asked.name)).is_none()
    {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    }
    let required = match required_capabilities(&state, &asked.owner, &asked.name, request.number) {
        Ok(required) => required,
        // Fail closed: a policy that cannot be read is not "no policy".
        Err(reason) => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_repo_config",
                &reason,
            );
        }
    };
    let now = Utc::now();
    if !required.is_empty() {
        let runners = state.gate_runners.snapshot();
        let advertised = host_capabilities(&runners, &request.host, now);
        let missing: Vec<&String> = required
            .iter()
            .filter(|name| !advertised.contains(*name))
            .collect();
        if !missing.is_empty() {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "code": "capability_missing",
                    "message": format!(
                        "{}/{} requires {} in {REPO_DECLARATION_PATH}; {} advertises {}",
                        asked.owner,
                        asked.name,
                        required.join(", "),
                        request.host,
                        if advertised.is_empty() {
                            "none".to_string()
                        } else {
                            advertised.iter().cloned().collect::<Vec<_>>().join(", ")
                        },
                    ),
                    "required_capabilities": required,
                    "missing_capabilities": missing,
                })),
            )
                .into_response();
        }
    }
    let ask = ClaimAsk {
        repo: &request.repo,
        number: request.number,
        head: &asked.head,
        runner_id: &request.runner_id,
        host: &request.host,
    };
    match state.gate_claims.acquire(&ask, required, now) {
        Granted::Fresh(claim) => {
            (StatusCode::CREATED, Json(claim_json(&claim, "granted"))).into_response()
        }
        Granted::Renewed(claim) => {
            (StatusCode::OK, Json(claim_json(&claim, "renewed"))).into_response()
        }
        Granted::Held(claim) => (
            StatusCode::CONFLICT,
            Json(json!({
                "code": "claim_held",
                "message": format!(
                    "{} on {} holds the gate claim on {}@{} until {}",
                    claim.runner_id,
                    claim.host,
                    claim.repo,
                    claim.head,
                    claim.expires_at.to_rfc3339(),
                ),
                "holder": claim_json(&claim, "held"),
            })),
        )
            .into_response(),
    }
}

/// `POST /api/v1/gate-claims/release`: the holder publishes and gives the head
/// back. Repeating it is not an error.
pub(super) async fn release(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Json(request): Json<GateClaimRequest>,
) -> AxumResponse {
    if !may_claim(&state, &account) {
        return not_a_runner();
    }
    let asked = match validate(&request) {
        Ok(asked) => asked,
        Err(reason) => {
            return api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", &reason);
        }
    };
    match state.gate_claims.release(
        &request.repo,
        request.number,
        &asked.head,
        &request.runner_id,
        Utc::now(),
    ) {
        ReleaseOutcome::Released => (
            StatusCode::OK,
            Json(json!({"state": "released", "repo": request.repo, "head": asked.head})),
        )
            .into_response(),
        ReleaseOutcome::Absent => (
            StatusCode::OK,
            Json(json!({"state": "absent", "repo": request.repo, "head": asked.head})),
        )
            .into_response(),
        ReleaseOutcome::HeldByOther(claim) => (
            StatusCode::CONFLICT,
            Json(json!({
                "code": "claim_held",
                "message": format!(
                    "{} on {} holds the gate claim on {}@{}; only its holder may release it",
                    claim.runner_id, claim.host, claim.repo, claim.head,
                ),
                "holder": claim_json(&claim, "held"),
            })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + seconds, 0).expect("fixture time")
    }

    fn ask(store: &GateClaimStore, runner: &str, host: &str, at: i64) -> Granted {
        store.acquire(
            &ClaimAsk {
                repo: "veox/jain-deploy",
                number: 7,
                head: "abcdef1234567890abcdef1234567890abcdef12",
                runner_id: runner,
                host,
            },
            Vec::new(),
            time(at),
        )
    }

    #[test]
    fn one_head_is_leased_to_one_runner_until_it_expires() {
        let store = GateClaimStore::default();
        let Granted::Fresh(first) = ask(&store, "gate-a/slot0", "gate-a", 0) else {
            panic!("the first ask should be granted");
        };
        assert_eq!(first.expires_at, time(GATE_CLAIM_TTL_SECS));
        let Granted::Held(holder) = ask(&store, "gate-b/slot0", "gate-b", 1) else {
            panic!("a second host must be refused while the lease is live");
        };
        assert_eq!(holder.runner_id, "gate-a/slot0");
        // The holder asking again is the renewal: one lease, held longer.
        let Granted::Renewed(renewed) = ask(&store, "gate-a/slot0", "gate-a", 60) else {
            panic!("the holder's own ask should renew");
        };
        assert_eq!(renewed.granted_at, first.granted_at);
        assert_eq!(renewed.expires_at, time(60 + GATE_CLAIM_TTL_SECS));
        // Nobody renews it: within one TTL the other host takes the head.
        assert!(matches!(
            ask(
                &store,
                "gate-b/slot0",
                "gate-b",
                60 + GATE_CLAIM_TTL_SECS + 1
            ),
            Granted::Fresh(_)
        ));
    }

    #[test]
    fn only_the_holder_releases_and_releasing_twice_is_not_an_error() {
        let store = GateClaimStore::default();
        let head = "abcdef1234567890abcdef1234567890abcdef12";
        ask(&store, "gate-a/slot0", "gate-a", 0);
        assert!(matches!(
            store.release("veox/jain-deploy", 7, head, "gate-b/slot0", time(1)),
            ReleaseOutcome::HeldByOther(_)
        ));
        assert_eq!(
            store.release("veox/jain-deploy", 7, head, "gate-a/slot0", time(2)),
            ReleaseOutcome::Released
        );
        assert_eq!(
            store.release("veox/jain-deploy", 7, head, "gate-a/slot0", time(3)),
            ReleaseOutcome::Absent
        );
        // Released, so the next host takes it at once rather than at the TTL.
        assert!(matches!(
            ask(&store, "gate-b/slot0", "gate-b", 4),
            Granted::Fresh(_)
        ));
    }

    #[test]
    fn an_unrenewed_lease_is_stale_then_forgotten() {
        let store = GateClaimStore::default();
        ask(&store, "gate-a/slot0", "gate-a", 0);
        assert!(store.stale(time(10)).is_empty());
        let stale = store.stale(time(GATE_CLAIM_TTL_SECS + 1));
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].host, "gate-a");
        // Asking again past the retention window forgets it.
        store.acquire(
            &ClaimAsk {
                repo: "veox/jain-web",
                number: 1,
                head: "1234567",
                runner_id: "gate-b/slot0",
                host: "gate-b",
            },
            Vec::new(),
            time(GATE_CLAIM_TTL_SECS + STALE_CLAIM_RETENTION_SECS + 2),
        );
        assert!(
            store
                .claim(
                    "veox/jain-deploy",
                    7,
                    "abcdef1234567890abcdef1234567890abcdef12"
                )
                .is_none()
        );
    }

    #[test]
    fn a_live_lease_survives_a_full_store() {
        let store = GateClaimStore::default();
        ask(&store, "gate-a/slot0", "gate-a", 0);
        for index in 0..MAX_CLAIMS + 8 {
            store.acquire(
                &ClaimAsk {
                    repo: "veox/jain-web",
                    number: u64::try_from(index).expect("fixture index"),
                    head: "1234567",
                    runner_id: "gate-b/slot0",
                    host: "gate-b",
                },
                Vec::new(),
                time(1),
            );
        }
        assert!(
            store
                .claim(
                    "veox/jain-deploy",
                    7,
                    "abcdef1234567890abcdef1234567890abcdef12"
                )
                .is_some_and(|claim| claim.runner_id == "gate-a/slot0")
        );
    }

    #[test]
    fn the_repository_declares_its_gate_capabilities() {
        assert_eq!(parse_capabilities("").expect("empty"), Vec::<String>::new());
        assert_eq!(
            parse_capabilities("[gate]\ncapabilities = [\"host-ci\"]\n").expect("declared"),
            vec!["host-ci".to_string()]
        );
        assert_eq!(
            parse_capabilities("[gate]\ncapabilities = []\n").expect("empty list"),
            Vec::<String>::new()
        );
        for (text, reason) in [
            (
                "[gate]\ncapabilities = [\"host_ci\"]\n",
                "unknown capability",
            ),
            (
                "[gate]\ncapabilities = [\"host-ci\", \"host-ci\"]\n",
                "duplicate capability",
            ),
            ("[gate]\ncapabilities = \"host-ci\"\n", "must be an array"),
            ("[gate]\ncapabilities = [1]\n", "must hold strings"),
            ("gate = 1\n", "must be a table"),
            ("[gate\n", "not valid TOML"),
        ] {
            let error = parse_capabilities(text).expect_err(text);
            assert!(error.contains(reason), "{text}: {error}");
            assert!(error.contains(REPO_DECLARATION_PATH), "{text}: {error}");
        }
    }

    #[test]
    fn an_ask_names_one_head_of_one_repository() {
        let good = GateClaimRequest {
            repo: "veox/jain-deploy".to_string(),
            number: 7,
            head_sha: Some("abcdef1234567890abcdef1234567890abcdef12".to_string()),
            queue_sha: None,
            runner_id: "gate-a/slot0".to_string(),
            host: "gate-a".to_string(),
        };
        assert_eq!(validate(&good).expect("valid").head.len(), 40);
        let queue = GateClaimRequest {
            head_sha: None,
            queue_sha: Some("1234567".to_string()),
            number: 0,
            ..good.clone()
        };
        assert_eq!(validate(&queue).expect("queue head").head, "1234567");
        for (request, reason) in [
            (
                GateClaimRequest {
                    repo: "jain-deploy".to_string(),
                    ..good.clone()
                },
                "owner/name",
            ),
            (
                GateClaimRequest {
                    queue_sha: Some("1234567".to_string()),
                    ..good.clone()
                },
                "exactly one",
            ),
            (
                GateClaimRequest {
                    head_sha: None,
                    queue_sha: None,
                    ..good.clone()
                },
                "exactly one",
            ),
            (
                GateClaimRequest {
                    head_sha: Some("ABCDEF1234567890abcdef1234567890abcdef12".to_string()),
                    ..good.clone()
                },
                "lowercase hex",
            ),
            (
                GateClaimRequest {
                    head_sha: Some("main".to_string()),
                    ..good.clone()
                },
                "lowercase hex",
            ),
            (
                GateClaimRequest {
                    repo: "veox/../etc".to_string(),
                    ..good.clone()
                },
                "may not",
            ),
            (
                GateClaimRequest {
                    host: "gate a".to_string(),
                    ..good.clone()
                },
                "may not",
            ),
        ] {
            let error = validate(&request).expect_err(reason);
            assert!(error.contains(reason), "{reason}: {error}");
        }
    }
}
