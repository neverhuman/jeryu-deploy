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
//! - a lease is one per `(repo, head sha)`: `number` is carried as metadata
//!   and never keys anything, so the same commit asked for under two pull
//!   request numbers is one lease and cannot be handed to two hosts;
//! - a second host asking for the same head is refused `409 claim_held`, so
//!   only one host ever posts `<repo>/required` for a head;
//! - the holder renews by asking again while the gate runs (the same ask is
//!   the renewal: it moves `expires_at` and keeps `granted_at` and the
//!   requirement the lease was granted under), and releases on publication
//!   with `POST /api/v1/gate-claims/release`;
//! - only the runner that holds a lease may release it, an expired lease
//!   included: the expired record is what needs-you reads, and a stranger
//!   must not be able to erase it;
//! - a lease nobody renews (the host died, the slot was killed) expires, and
//!   the head is claimable by any live host within one TTL, which is how a
//!   dead host's work is picked up without an operator.
//!
//! The lease is advisory: the forge does not refuse a `<repo>/required`
//! status from a host that holds no claim, so an older gate runner keeps
//! working. It is honoured by pr-gate-runner (jeryu-ci-runner `ops/pr-gate`),
//! which takes the lease after admission, renews it from its heartbeat and
//! releases it on publication. Until that runner version is installed on
//! every gate host, two hosts can still both gate one head.
//!
//! Capability matching replaces the owner partition. A repository declares
//! what a gate of it needs in its own `.jeryu/repo.toml`:
//!
//! ```toml
//! [gate]
//! capabilities = ["host-ci"]
//! ```
//!
//! That file is read at the tip of the repository's **default branch**, for
//! every head of that repository, and never at the pull request's own head or
//! at the branch the ask names: a pull request must not be able to lower, or
//! raise, what its own gate requires, and retargeting one at a branch its
//! author can push to must not change the policy either. The `number` an ask
//! carries therefore has no part in choosing the policy. A host advertises
//! what it has in its heartbeat (`capabilities`), and a claim is granted only
//! to a host whose advertised set covers the repository's required set. A
//! declaration that cannot be read -- no git storage, no default branch, a
//! default branch that does not resolve, an unreadable or unparsable file, an
//! unknown or repeated capability name -- grants nothing: the ask is refused
//! with the problem named, because a gate whose requirements cannot be read
//! must not run on a host that may not have them.
//!
//! Who may claim: the logins that may report heartbeats
//! ([`GateRunnerStore::may_report`](super::control_plane::GateRunnerStore::may_report))
//! — a gate claim is a runner acting, and an ordinary account must not be
//! able to park a lease on a head and stop every host from gating it. The
//! `host` an ask names is cross-checked against the heartbeat registered for
//! its `runner_id`, so a slot cannot claim a head as a host it is not on, and
//! a head that requires a capability is leased only to a runner that is
//! itself online on a host advertising it.
//!
//! A repeat carrying an `Idempotency-Key` is **not** replayed on these
//! routes: a renewal is the same request on purpose and must reach the store
//! to move `expires_at`, and a replayed "granted" after a release would hand
//! a head out twice.
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
/// to make room -- an ask that would need one dropped is refused instead.
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
    /// The pull request the lease was taken for, or 0 for a head that has
    /// none (a merge-queue or branch gate). Metadata only, for needs-you to
    /// link the head: the lease is keyed on the commit, not on this.
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
    /// Every slot of the store is a live lease: no head can be taken on
    /// without dropping one, which would hand that head to a second host.
    Full,
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
#[derive(Clone, Copy)]
pub(crate) struct ClaimAsk<'a> {
    /// `owner/name`.
    pub(crate) repo: &'a str,
    /// The pull request the head belongs to, or 0 for a head without one.
    pub(crate) number: u64,
    pub(crate) head: &'a str,
    pub(crate) runner_id: &'a str,
    pub(crate) host: &'a str,
}

/// `(owner/name, head sha)`. The pull request number is deliberately not part
/// of it: one commit is one gate, whatever number the ask carries.
type ClaimKey = (String, String);

fn key(repo: &str, head: &str) -> ClaimKey {
    (repo.to_string(), head.to_string())
}

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

    /// Renew the live lease this runner already holds on the head, if it
    /// holds one.
    ///
    /// The renewal keeps `granted_at` and the requirement the lease was
    /// granted under, so a repository that changed its `[gate] capabilities`
    /// while the gate runs does not drop a running gate's lease: a head is
    /// judged by the policy it was handed out under, and the next head of
    /// that repository is judged by the new one.
    pub(crate) fn renew(&self, ask: &ClaimAsk<'_>, now: DateTime<Utc>) -> Option<GateClaim> {
        let mut claims = self.lock();
        let held = claims.get_mut(&key(ask.repo, ask.head))?;
        if !held.live(now) || held.runner_id != ask.runner_id {
            return None;
        }
        held.host = ask.host.to_string();
        held.renewed_at = now;
        held.expires_at = now + chrono::Duration::seconds(GATE_CLAIM_TTL_SECS);
        Some(held.clone())
    }

    /// Take the lease on one head, or renew the one this runner holds.
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
        let key = key(ask.repo, ask.head);
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
        if renewed.is_none() && !claims.contains_key(&key) && !make_room(&mut claims, now) {
            return Granted::Full;
        }
        let was_renewal = renewed.is_some();
        let claim = GateClaim {
            repo: ask.repo.to_string(),
            number: renewed.as_ref().map_or(ask.number, |held| held.number),
            head: ask.head.to_string(),
            runner_id: ask.runner_id.to_string(),
            host: ask.host.to_string(),
            granted_at: renewed.as_ref().map_or(now, |held| held.granted_at),
            renewed_at: now,
            expires_at: now + chrono::Duration::seconds(GATE_CLAIM_TTL_SECS),
            required_capabilities: renewed
                .map_or(required_capabilities, |held| held.required_capabilities),
        };
        claims.insert(key, claim.clone());
        if was_renewal {
            Granted::Renewed(claim)
        } else {
            Granted::Fresh(claim)
        }
    }

    /// Give the head back, so the next host can gate it without waiting out
    /// the TTL. Only the runner that holds the lease may, an expired lease
    /// included: a host must not be able to clear another host's lease while
    /// that host is gating the head, and the expired record is the
    /// `gate_claim_stale` row needs-you shows, which a stranger must not be
    /// able to erase.
    pub(crate) fn release(
        &self,
        repo: &str,
        head: &str,
        runner_id: &str,
        now: DateTime<Utc>,
    ) -> ReleaseOutcome {
        let mut claims = self.lock();
        forget_old(&mut claims, now);
        let key = key(repo, head);
        match claims.get(&key) {
            None => ReleaseOutcome::Absent,
            Some(held) if held.runner_id == runner_id => {
                claims.remove(&key);
                ReleaseOutcome::Released
            }
            Some(held) => ReleaseOutcome::HeldByOther(held.clone()),
        }
    }

    /// The lease on one head, live or expired.
    #[cfg(test)]
    pub(crate) fn claim(&self, repo: &str, head: &str) -> Option<GateClaim> {
        self.lock().get(&key(repo, head)).cloned()
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

/// Forget expired claims past the retention window.
fn forget_old(claims: &mut BTreeMap<ClaimKey, GateClaim>, now: DateTime<Utc>) {
    claims.retain(|_, claim| {
        claim.live(now) || (now - claim.expires_at).num_seconds() <= STALE_CLAIM_RETENTION_SECS
    });
}

/// Make room for one more lease by forgetting the oldest expired ones, and
/// say whether there is room. A live lease is never dropped: dropping one
/// would hand the same head to a second host, so a store of
/// [`MAX_CLAIMS`] live leases refuses the next ask instead.
fn make_room(claims: &mut BTreeMap<ClaimKey, GateClaim>, now: DateTime<Utc>) -> bool {
    while claims.len() >= MAX_CLAIMS {
        let Some(oldest) = claims
            .iter()
            .filter(|(_, claim)| !claim.live(now))
            .min_by_key(|(_, claim)| claim.expires_at)
            .map(|(key, _)| key.clone())
        else {
            return false;
        };
        claims.remove(&oldest);
    }
    true
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
/// its own `.jeryu/repo.toml`, read at the tip of its default branch.
///
/// `Ok(set)` is the policy (empty: the file, or the table, is absent and no
/// special capability is needed). `Err(reason)` is a policy that could not be
/// read at all -- no git storage, no default branch, a default branch that
/// does not resolve, an unreadable or unparsable file -- and nothing of the
/// repository is claimable then. It must never answer "requires nothing"
/// because something could not be read: that would hand a head that needs
/// the host-ci sandbox to a host without it.
///
/// Blocking: it runs git. Call it from `spawn_blocking`.
fn required_capabilities(state: &WebState, owner: &str, name: &str) -> Result<Vec<String>, String> {
    let opened = state
        .repo_manager
        .open_parts(owner, name)
        .map_err(|error| {
            format!(
                "the git storage of {owner}/{name} could not be opened, so {REPO_DECLARATION_PATH} \
             could not be read: {error}"
            )
        })?;
    let dir = opened.path.as_path();
    let base = default_branch_commit(state, dir, owner, name)?;
    let Some(text) = read_declaration(state, dir, &base)? else {
        return Ok(Vec::new());
    };
    parse_capabilities(&text)
}

/// The commit the policy is read at: the tip of the repository's default
/// branch. Never the pull request's head, and never the base branch the ask
/// or the pull request names -- an author who can retarget a pull request at
/// a branch they push to must not be able to choose its gate policy.
fn default_branch_commit(
    state: &WebState,
    dir: &std::path::Path,
    owner: &str,
    name: &str,
) -> Result<String, String> {
    let repo = format!("{owner}/{name}");
    let branch = super::repositories::find_repo(state, &repo)
        .map(|found| found.default_branch)
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| {
            format!(
                "{repo} has no default branch recorded, so the {REPO_DECLARATION_PATH} its gate \
                 needs could not be read"
            )
        })?;
    let tip = format!("refs/heads/{branch}^{{commit}}");
    let resolved = crate::web::shift::queue::git(
        &state.repo_manager.config().git_bin,
        dir,
        &["rev-parse", "--verify", "--quiet", "--end-of-options", &tip],
        &[],
        None,
    )
    .map(|out| String::from_utf8_lossy(&out).trim().to_string())
    .unwrap_or_default();
    if resolved.len() == 40 && resolved.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(resolved);
    }
    Err(format!(
        "the default branch of {repo} ({branch}) does not resolve to a commit, so the \
         {REPO_DECLARATION_PATH} its gate needs could not be read"
    ))
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

/// The ask must name the host the runner last reported from: `runner_id` is
/// the slot's identity, and the capability match is made on what its host
/// advertises, so a slot that names another host would borrow that host's
/// capabilities. A runner that has never reported since this forge started is
/// not refused here -- it beats before it asks, and a forge that restarted
/// has no beat to compare against -- but it cannot borrow a capability
/// either: [`capable_claimant`] needs a live beat of its own.
fn host_mismatch(state: &WebState, request: &GateClaimRequest) -> Option<AxumResponse> {
    let reported = state.gate_runners.previous(&request.runner_id)?;
    (reported.host != request.host).then(|| {
        api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "host_mismatch",
            &format!(
                "{} last reported from {}, not {}: a gate claim names the host the runner beats from",
                request.runner_id, reported.host, request.host,
            ),
        )
    })
}

/// Is this runner itself a live gate slot of the host it names, advertising
/// every capability the repository requires? What a head with a requirement
/// needs, rather than "some slot of that host is online": the lease is handed
/// to this runner, so this runner must be the one with the capability.
fn capable_claimant(
    runners: &[super::control_plane::GateRunnerRecord],
    request: &GateClaimRequest,
    required: &[String],
    now: DateTime<Utc>,
) -> bool {
    runners
        .iter()
        .filter(|record| record.heartbeat.runner_id == request.runner_id)
        .filter(|record| record.heartbeat.host == request.host)
        .filter(|record| super::control_plane::holds_gate_slot(&record.heartbeat))
        .filter(|record| super::control_plane::is_online(record, now))
        .any(|record| {
            required
                .iter()
                .all(|name| record.heartbeat.capabilities.contains(name))
        })
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
    if let Some(refusal) = host_mismatch(&state, &request) {
        return refusal;
    }
    let ask = ClaimAsk {
        repo: &request.repo,
        number: request.number,
        head: &asked.head,
        runner_id: &request.runner_id,
        host: &request.host,
    };
    let now = Utc::now();
    // The holder's own renewal is answered from the lease: it keeps the
    // requirement the head was handed out under, so a declaration edited
    // mid-gate does not take a running gate's lease away, and a renewal
    // every minute does not run git.
    if let Some(claim) = state.gate_claims.renew(&ask, now) {
        return (StatusCode::OK, Json(claim_json(&claim, "renewed"))).into_response();
    }
    let git_state = Arc::clone(&state);
    let (owner, name) = (asked.owner.clone(), asked.name.clone());
    // git is blocking, and this runs on the request's async worker.
    let read =
        tokio::task::spawn_blocking(move || required_capabilities(&git_state, &owner, &name)).await;
    let required = match read {
        Ok(Ok(required)) => required,
        // Fail closed: a policy that cannot be read is not "no policy".
        Ok(Err(reason)) => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_repo_config",
                &reason,
            );
        }
        Err(error) => {
            return api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "invalid_repo_config",
                &format!(
                    "{REPO_DECLARATION_PATH} of {}/{} could not be read: {error}",
                    asked.owner, asked.name
                ),
            );
        }
    };
    if !required.is_empty() {
        let runners = state.gate_runners.snapshot();
        if !capable_claimant(&runners, &request, &required, now) {
            let advertised = host_capabilities(&runners, &request.host, now);
            let missing: Vec<&String> = required
                .iter()
                .filter(|name| !advertised.contains(*name))
                .collect();
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "code": "capability_missing",
                    "message": format!(
                        "{}/{} requires {} in {REPO_DECLARATION_PATH}; {} on {} advertises {}",
                        asked.owner,
                        asked.name,
                        required.join(", "),
                        request.runner_id,
                        request.host,
                        if advertised.is_empty() {
                            "none".to_string()
                        } else {
                            advertised.iter().cloned().collect::<Vec<_>>().join(", ")
                        },
                    ),
                    "required_capabilities": required,
                    // Empty when the host advertises everything but this
                    // runner has no live gate beat of its own to prove it.
                    "missing_capabilities": missing,
                })),
            )
                .into_response();
        }
    }
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
        // Never by dropping a live lease: a head that is being gated stays
        // with its host, and this host asks again on its next tick.
        Granted::Full => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "claim_capacity",
            &format!("the forge holds its {MAX_CLAIMS} gate claims; retry on the next tick"),
        ),
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
    match state
        .gate_claims
        .release(&request.repo, &asked.head, &request.runner_id, Utc::now())
    {
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
                    "{} on {} holds the gate claim on {}@{}; only its holder may release it, \
                     an expired lease included",
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

    const FIXTURE_HEAD: &str = "abcdef1234567890abcdef1234567890abcdef12";

    fn ask_for(
        store: &GateClaimStore,
        number: u64,
        head: &str,
        runner: &str,
        host: &str,
        at: i64,
    ) -> Granted {
        store.acquire(
            &ClaimAsk {
                repo: "veox/jain-deploy",
                number,
                head,
                runner_id: runner,
                host,
            },
            Vec::new(),
            time(at),
        )
    }

    fn ask(store: &GateClaimStore, runner: &str, host: &str, at: i64) -> Granted {
        ask_for(store, 7, FIXTURE_HEAD, runner, host, at)
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

    /// The lease is on the commit. One sha asked for under another pull
    /// request number -- a head pushed to two pull requests, a number the
    /// caller got wrong -- is the same lease, so it cannot be gated twice.
    #[test]
    fn one_commit_is_one_lease_whatever_number_the_ask_carries() {
        let store = GateClaimStore::default();
        let Granted::Fresh(first) = ask_for(&store, 7, FIXTURE_HEAD, "gate-a/slot0", "gate-a", 0)
        else {
            panic!("the first ask should be granted");
        };
        let Granted::Held(holder) = ask_for(&store, 8, FIXTURE_HEAD, "gate-b/slot0", "gate-b", 1)
        else {
            panic!("the same sha under another number must be refused");
        };
        assert_eq!(holder.runner_id, "gate-a/slot0");
        assert_eq!(
            holder.number, 7,
            "the lease keeps the number it was taken for"
        );
        // And the holder renews it whatever number it sends.
        let Granted::Renewed(renewed) =
            ask_for(&store, 9, FIXTURE_HEAD, "gate-a/slot0", "gate-a", 2)
        else {
            panic!("the holder renews its own lease on this sha");
        };
        assert_eq!(renewed.granted_at, first.granted_at);
        assert_eq!(renewed.number, 7);
        // A different sha of the same repository is a lease of its own.
        assert!(matches!(
            ask_for(&store, 7, "1234567", "gate-b/slot0", "gate-b", 3),
            Granted::Fresh(_)
        ));
    }

    /// A renewal is answered from the lease, so a repository that changes
    /// `[gate] capabilities` mid-gate does not take a running gate's lease.
    #[test]
    fn a_renewal_keeps_the_requirement_its_lease_was_granted_under() {
        let store = GateClaimStore::default();
        let ask = ClaimAsk {
            repo: "veox/jain",
            number: 7,
            head: FIXTURE_HEAD,
            runner_id: "gate-b/slot0",
            host: "gate-b",
        };
        store.acquire(&ask, vec!["host-ci".to_string()], time(0));
        let renewed = store.renew(&ask, time(60)).expect("the holder renews");
        assert_eq!(renewed.required_capabilities, vec!["host-ci".to_string()]);
        assert_eq!(renewed.granted_at, time(0));
        assert_eq!(renewed.expires_at, time(60 + GATE_CLAIM_TTL_SECS));
        // Nobody else renews it, and an expired lease is not renewed either:
        // whoever takes it over is judged by the policy as it is then.
        assert!(
            store
                .renew(
                    &ClaimAsk {
                        runner_id: "gate-a/slot0",
                        host: "gate-a",
                        ..ask
                    },
                    time(61)
                )
                .is_none()
        );
        assert!(
            store
                .renew(&ask, time(60 + GATE_CLAIM_TTL_SECS + 1))
                .is_none()
        );
    }

    #[test]
    fn only_the_holder_releases_and_releasing_twice_is_not_an_error() {
        let store = GateClaimStore::default();
        ask(&store, "gate-a/slot0", "gate-a", 0);
        assert!(matches!(
            store.release("veox/jain-deploy", FIXTURE_HEAD, "gate-b/slot0", time(1)),
            ReleaseOutcome::HeldByOther(_)
        ));
        assert_eq!(
            store.release("veox/jain-deploy", FIXTURE_HEAD, "gate-a/slot0", time(2)),
            ReleaseOutcome::Released
        );
        assert_eq!(
            store.release("veox/jain-deploy", FIXTURE_HEAD, "gate-a/slot0", time(3)),
            ReleaseOutcome::Absent
        );
        // Released, so the next host takes it at once rather than at the TTL.
        assert!(matches!(
            ask(&store, "gate-b/slot0", "gate-b", 4),
            Granted::Fresh(_)
        ));
    }

    /// The expired record is the `gate_claim_stale` row needs-you shows, so
    /// only its own holder may clear it.
    #[test]
    fn a_stranger_may_not_release_an_expired_lease() {
        let store = GateClaimStore::default();
        ask(&store, "gate-a/slot0", "gate-a", 0);
        let later = time(GATE_CLAIM_TTL_SECS + 1);
        assert!(matches!(
            store.release("veox/jain-deploy", FIXTURE_HEAD, "gate-b/slot0", later),
            ReleaseOutcome::HeldByOther(_)
        ));
        assert_eq!(store.stale(later).len(), 1, "the stale record survives");
        assert_eq!(
            store.release("veox/jain-deploy", FIXTURE_HEAD, "gate-a/slot0", later),
            ReleaseOutcome::Released
        );
        assert!(store.stale(later).is_empty());
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
        assert!(store.claim("veox/jain-deploy", FIXTURE_HEAD).is_none());
    }

    /// A full store refuses the next head rather than dropping a live lease,
    /// which would hand a head that is being gated to a second host.
    #[test]
    fn a_full_store_refuses_a_new_head_and_keeps_every_live_lease() {
        let store = GateClaimStore::default();
        ask(&store, "gate-a/slot0", "gate-a", 0);
        for index in 1..MAX_CLAIMS {
            let head = format!("{index:040x}");
            assert!(
                matches!(
                    ask_for(&store, 0, &head, "gate-b/slot0", "gate-b", 1),
                    Granted::Fresh(_)
                ),
                "ask {index} fills the store"
            );
        }
        let head = format!("{:040x}", MAX_CLAIMS);
        assert_eq!(
            ask_for(&store, 0, &head, "gate-b/slot0", "gate-b", 1),
            Granted::Full
        );
        // Every live lease is still where it was, the first one included, and
        // its holder still renews it.
        assert!(
            store
                .claim("veox/jain-deploy", FIXTURE_HEAD)
                .is_some_and(|claim| claim.runner_id == "gate-a/slot0")
        );
        assert!(matches!(
            ask(&store, "gate-a/slot0", "gate-a", 2),
            Granted::Renewed(_)
        ));
        // Once they expire, the head that was refused is claimable.
        assert!(matches!(
            ask_for(
                &store,
                0,
                &head,
                "gate-b/slot0",
                "gate-b",
                2 + GATE_CLAIM_TTL_SECS + 1
            ),
            Granted::Fresh(_)
        ));
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
