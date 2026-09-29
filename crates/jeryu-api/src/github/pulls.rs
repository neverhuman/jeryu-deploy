//! Pull request routes (`/repos/{owner}/{repo}/pulls...`) and their
//! GitHub-shaped renderers.

use jeryu_core::{
    ChangeSet, CreatePullRequestRequest, ForgeError, MergePullRequestRequest, MergeReadiness,
    OpenPr, OverlapConfig, OverlapDecision, PullRequest, PullRequestState,
    UpdatePullRequestRequest, decide,
};
use serde_json::{Value, json};

use crate::routes::Response;

use super::GithubRouter;
use super::support::{
    Pagination, PullStateSelector, actor, docs_url, error_response, json_response, owner_json,
    paginate, parse_body, parse_number, steering,
};

/// The base SHA the forge assigns when a create request omits `base_sha`.
/// Mirrored here so the overlap engine compares the proposed change against
/// existing PRs on the same default base (see `ForgeCore::create_pull_request`).
const DEFAULT_BASE_SHA: &str = "base";

impl GithubRouter {
    pub(super) fn list_pulls(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        page: Pagination,
        pull_state: PullStateSelector,
    ) -> Response {
        // The engine's `state_filter` is an exact match on one of its many
        // internal lifecycle states (Mergeable, BlockedByChecks, ...), so it
        // cannot express GitHub's coarse open/closed/all selector on its own: a
        // healthy PR re-evaluates to a richer state on read and would slip past
        // an exact-`Open` filter. So list everything and keep the PRs whose
        // GitHub-rendered `state` field matches the selector, which guarantees
        // the filter agrees with the `state` value each PR reports. Absent or
        // unrecognized `?state=` defaults to `open` (GitHub's documented
        // default), so a bare list now returns only open PRs.
        match self.core.list_pull_requests(owner, repo, None) {
            Ok(pulls) => {
                let body: Vec<Value> = pulls
                    .iter()
                    .filter(|pr| pull_state.keeps(pr_open_or_closed(&pr.state)))
                    .map(pull_request_json)
                    .collect();
                paginate(path, page, &body, |slice, _total| {
                    Value::Array(slice.to_vec())
                })
            }
            Err(err) => error_response(err),
        }
    }

    pub(super) fn create_pull(&self, owner: &str, repo: &str, body: &str) -> Response {
        let mut req: CreatePullRequestRequest = match parse_body(body) {
            Ok(value) => value,
            Err(response) => return response,
        };
        let author = actor(body);

        // A fork PR names its source repository either explicitly or through
        // GitHub's `head: "fork-owner:branch"` form. Persist it on the record so
        // every later ref resolution (create, merge) knows where the head lives.
        req.source_repository = Some(create_source_repository(owner, repo, &req));

        // With a git backend wired, persist the REAL commit oids of the head and
        // base branch refs (never the literal "base"/"head-<n>" placeholders).
        // A request that already carries explicit shas is honored as-is; an
        // unresolvable branch falls through to the core default, preserving the
        // git-less in-memory path used by unit tests.
        #[cfg(feature = "web")]
        if let Some(rm) = &self.repo_manager {
            self.resolve_create_oids(rm, owner, repo, &mut req);
        }

        // Flagship overlap routing: before opening a fresh PR, see whether the
        // proposed change overlaps an existing OPEN PR enough to hot-fix it.
        // Only runs when the request carries `changed_files`; without them there
        // is nothing to score, so we fall through to a normal create.
        if !req.changed_files.is_empty()
            && let Some(response) = self.maybe_route_overlap(owner, repo, &author, &req)
        {
            return response;
        }

        match self.core.create_pull_request(owner, repo, &author, req) {
            Ok(pr) => {
                #[cfg(feature = "web")]
                if let Some(repo_manager) = &self.repo_manager {
                    // A fork head does not live on a branch of this repository:
                    // fetch it into `refs/pull/<n>/head` first so the commit is
                    // present locally, and seed CI against that ref.
                    let head_ref = match self.fetch_fork_head(repo_manager, owner, repo, &pr) {
                        Some(pull_ref) => pull_ref,
                        None => format!("refs/heads/{}", pr.head.ref_name),
                    };
                    crate::ci_bridge::seed_pull_request_head(
                        &self.core,
                        repo_manager,
                        owner,
                        repo,
                        &head_ref,
                        &pr.head.sha,
                        "",
                    );
                }
                json_response(201, &pull_request_json(&pr))
            }
            Err(err) => error_response(err),
        }
    }

    /// Fill in a create request's `head_sha`/`base_sha` from the real commit
    /// oids of the corresponding branch refs in the bare repo, but only for a
    /// field the caller left unset and a ref that actually resolves. A git error
    /// is non-fatal here: create then falls back to the core default rather than
    /// failing the whole request on a transient resolve hiccup.
    #[cfg(feature = "web")]
    fn resolve_create_oids(
        &self,
        rm: &std::sync::Arc<jeryu_gitd::RepoManager>,
        owner: &str,
        repo: &str,
        req: &mut CreatePullRequestRequest,
    ) {
        use jeryu_gitd::refs::RefService;

        let Ok(resolved) = rm.resolve_parts(owner, repo) else {
            return;
        };
        let refs = RefService::new((**rm).clone());
        // The head branch lives in the PR's SOURCE repository, which is this
        // repository only for a same-repo PR. Resolving a fork head here would
        // silently pick a same-named branch of the destination.
        let head_repo = match fork_source(owner, repo, req.source_repository.as_deref()) {
            Some((fork_owner, fork_repo)) => rm.resolve_parts(&fork_owner, &fork_repo).ok(),
            None => Some(resolved.clone()),
        };
        if req.head_sha.is_none()
            && let Some(head_repo) = &head_repo
            && let Ok(Some(oid)) =
                refs.resolve_commit(head_repo, &format!("refs/heads/{}", head_branch(&req.head)))
        {
            req.head_sha = Some(oid);
        }
        if req.base_sha.is_none()
            && let Ok(Some(oid)) =
                refs.resolve_commit(&resolved, &format!("refs/heads/{}", req.base))
        {
            req.base_sha = Some(oid);
        }
    }

    /// Runs the PR-overlap engine for a proposed change. Returns:
    /// * `Some(422)` when the change belongs on an existing open PR: Jeryu does
    ///   not apply the proposed head onto that PR, so it answers with GitHub's
    ///   own "a pull request already exists" shape naming the existing PR
    ///   rather than claiming a success it did not perform,
    /// * `Some(409)` when the best candidate overlaps but coalescing is unsafe
    ///   (stale base / unproven head),
    /// * `None` when a fresh PR should be created (caller proceeds as normal).
    ///
    /// Any failure to list the repo's open PRs is treated as "no candidates"
    /// (returns `None`) so overlap routing can never block a legitimate create.
    fn maybe_route_overlap(
        &self,
        owner: &str,
        repo: &str,
        author: &str,
        req: &CreatePullRequestRequest,
    ) -> Option<Response> {
        // List every PR and keep the ones GitHub would render as `open`. We do
        // NOT filter by `PullRequestState::Open` at the engine: a healthy PR is
        // re-evaluated to a richer lifecycle state (e.g. `Mergeable`) on read,
        // so an exact-`Open` filter would miss live candidates. Only terminal
        // Merged/Closed PRs are excluded.
        let open_prs = self.core.list_pull_requests(owner, repo, None).ok()?;

        let open: Vec<OpenPr> = open_prs
            .iter()
            .filter(|pr| {
                !matches!(
                    pr.state,
                    PullRequestState::Merged | PullRequestState::Closed
                ) && !pr.merged
            })
            .map(|pr| {
                OpenPr::new(
                    pr.number,
                    pr.changed_files.clone(),
                    pr.base.sha.clone(),
                    // A PR is only safe to coalesce onto if its head currently
                    // evaluates as mergeable (checks/protection green).
                    pr.mergeable,
                )
            })
            .collect();

        if open.is_empty() {
            return None;
        }

        let base_sha = req
            .base_sha
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_SHA.to_string());
        let change = ChangeSet::new(
            req.changed_files.clone(),
            base_sha,
            Some(author.to_string()),
        );

        match decide(&change, &open, OverlapConfig::default()) {
            OverlapDecision::RouteToExisting { pr, reason } => {
                // The overlap engine only decides WHERE the change belongs; no
                // coalescing is performed here, so the proposed head is not in
                // review anywhere. Answering 200 would tell the caller its
                // change had landed on #pr when nothing was applied. Use
                // GitHub's 422 for a duplicate create instead, naming the
                // existing PR so the caller can push onto it itself.
                let existing = open_prs.iter().find(|candidate| candidate.number == pr);
                let head_label = existing
                    .map(|candidate| candidate.head.label.clone())
                    .unwrap_or_else(|| format!("{owner}:{}", req.head));
                let payload = json!({
                    "message": format!(
                        "A pull request already exists for {head_label}."
                    ),
                    "errors": [{
                        "resource": "PullRequest",
                        "code": "custom",
                        "field": "base",
                        "message": format!(
                            "A pull request already exists for {head_label}."
                        ),
                    }],
                    "existing_pull_request": {
                        "number": pr,
                        "html_url": existing.map_or(Value::Null, |candidate| json!(
                            super::support::web_url(&pull_request_web_path(
                                &candidate.owner,
                                &candidate.repo,
                                candidate.number,
                            ))
                        )),
                        "url": format!("/repos/{owner}/{repo}/pulls/{pr}"),
                        "reason": reason,
                    },
                    "documentation_url": docs_url(),
                    "jeryu_steering": steering(
                        "jeryu.propose_patch",
                        "this change overlaps an open pull request; push it onto that PR instead of opening a duplicate",
                    ),
                });
                Some(json_response(422, &payload))
            }
            OverlapDecision::RefuseCoalesce { pr, reason } => {
                // GitHub returns 409 Conflict when a change cannot be applied
                // cleanly onto its target; the overlap engine refuses to clobber
                // a stale base or stack work on an unproven head.
                let payload = json!({
                    "message": reason,
                    "refuse_coalesce": { "pr": pr },
                    "documentation_url": docs_url(),
                });
                Some(json_response(409, &payload))
            }
            // CreateNew: nothing safe to coalesce onto; proceed with a fresh PR.
            OverlapDecision::CreateNew { .. } => None,
        }
    }

    pub(super) fn get_pull(&self, owner: &str, repo: &str, number: &str) -> Response {
        let number = match parse_number(number) {
            Ok(value) => value,
            Err(response) => return response,
        };
        match self.core.get_pull_request(owner, repo, number) {
            Ok(pr) => json_response(200, &pull_request_json(&pr)),
            Err(err) => error_response(err),
        }
    }

    pub(super) fn update_pull(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
        body: &str,
    ) -> Response {
        let number = match parse_number(number) {
            Ok(value) => value,
            Err(response) => return response,
        };
        let req: UpdatePullRequestRequest = match parse_body(body) {
            Ok(value) => value,
            Err(response) => return response,
        };
        match self.core.update_pull_request(owner, repo, number, req) {
            Ok(pr) => json_response(200, &pull_request_json(&pr)),
            Err(err) => error_response(err),
        }
    }

    pub(super) fn merge_pull(&self, owner: &str, repo: &str, number: &str, body: &str) -> Response {
        let number = match parse_number(number) {
            Ok(value) => value,
            Err(response) => return response,
        };
        let req: MergePullRequestRequest = if body.trim().is_empty() {
            MergePullRequestRequest::default()
        } else {
            match parse_body(body) {
                Ok(value) => value,
                Err(response) => return response,
            }
        };

        // GATE FIRST (no git yet). A blocked PR returns Err(BranchProtection)
        // here and the handler short-circuits BEFORE any git ref is touched.
        let readiness =
            match self
                .core
                .evaluate_merge_readiness(owner, repo, number, req.sha.as_deref())
            {
                Ok(readiness) => readiness,
                // GitHub returns 405 "Method Not Allowed" when a PR is not
                // mergeable (failing checks / protection), distinct from a 404.
                Err(ForgeError::BranchProtection(reason)) => {
                    return json_response(
                        405,
                        &json!({ "message": reason, "documentation_url": docs_url() }),
                    );
                }
                Err(err) => return error_response(err),
            };

        match readiness {
            // Idempotent: an already-merged PR returns its recorded merge sha.
            MergeReadiness::AlreadyMerged { sha } => json_response(
                200,
                &json!({
                    "sha": sha,
                    "merged": true,
                    "message": "Pull Request already merged",
                }),
            ),
            // `base_sha` is intentionally ignored here: the git merge path
            // resolves the LIVE base tip rather than trusting a possibly-stale
            // stored sha. It remains part of the readiness disclosure for
            // callers/inspection.
            MergeReadiness::Ready {
                base_ref,
                head_ref,
                head_sha,
                require_linear_history,
                ..
            } => self.merge_ready_pull(MergeReady {
                owner,
                repo,
                number,
                req: &req,
                base_ref,
                head_ref,
                source_repository: self.pull_source_repository(owner, repo, number),
                head_sha,
                require_linear_history,
            }),
        }
    }

    /// The repository full name a PR's head lives in, as recorded on the PR.
    /// Falls back to this repository so a lookup failure keeps the same-repo
    /// resolution path rather than inventing a fork.
    fn pull_source_repository(&self, owner: &str, repo: &str, number: u64) -> String {
        self.core
            .get_pull_request(owner, repo, number)
            .map(|pr| pr.source_repository)
            .unwrap_or_else(|_| format!("{owner}/{repo}"))
    }

    /// Finalize a PR that has already passed the merge gate. With a git
    /// [`RepoManager`](jeryu_gitd::RepoManager) wired this advances the real
    /// base ref; without one the merge fails closed, unless a test opted into
    /// the synthetic-sha path via `with_in_memory_merge`.
    fn merge_ready_pull(&self, ready: MergeReady<'_>) -> Response {
        #[cfg(feature = "web")]
        {
            if let Some(rm) = &self.repo_manager {
                return self.merge_ready_pull_git(rm, ready);
            }
            if !self.in_memory_merge {
                // No git backend wired: refuse rather than record a merge sha
                // that exists in no repository. 503 marks it as a server
                // misconfiguration (a missing `.with_repo_manager(...)` in
                // web.rs), not something the caller can fix by retrying.
                return json_response(
                    503,
                    &json!({
                        "message": "merge unavailable: this server has no git backend attached, \
                                    so no merge commit can be produced",
                        "errors": [{
                            "resource": "PullRequest",
                            "field": "merge",
                            "code": "git_backend_unavailable",
                        }],
                        "documentation_url": docs_url(),
                        "jeryu_steering": steering(
                            "jeryu.request_merge",
                            "the server is missing its git repository manager; an operator must \
                             wire one (`GithubRouter::with_repo_manager`) before merges can land",
                        ),
                    }),
                );
            }
        }
        self.merge_ready_pull_in_memory(ready)
    }

    /// Git-less finalize: synthesize a merge sha in core. Used by tests and, off
    /// the `web` feature, by the git-less build.
    fn merge_ready_pull_in_memory(&self, ready: MergeReady<'_>) -> Response {
        let merge_sha = format!("merge-{}-{}", ready.head_sha, ready.number);
        match self.core.finalize_merge(
            ready.owner,
            ready.repo,
            ready.number,
            merge_sha,
            ready.req.sha.as_deref(),
        ) {
            Ok(result) => merge_success_response(&result),
            Err(ForgeError::BranchProtection(reason)) => json_response(
                405,
                &json!({ "message": reason, "documentation_url": docs_url() }),
            ),
            Err(err) => error_response(err),
        }
    }

    /// Real, gated git merge: advance `refs/heads/{base_ref}` in the bare repo
    /// to the produced merge oid, then reconcile that real sha into the PR
    /// record via `finalize_merge`.
    ///
    /// The merge does NOT trust the PR's stored base/head shas (which may be
    /// stale placeholders): it resolves `base_ref` and the PR's head branch ref
    /// live against the bare repo. When the head is already an ANCESTOR of the
    /// base (its code is in main), it finalizes the record as merged
    /// idempotently without moving any ref. An unresolvable base/head yields a
    /// clean 4xx, never a 500.
    #[cfg(feature = "web")]
    fn merge_ready_pull_git(
        &self,
        rm: &std::sync::Arc<jeryu_gitd::RepoManager>,
        ready: MergeReady<'_>,
    ) -> Response {
        use jeryu_gitd::GitdError;
        use jeryu_gitd::object_fsck::ObjectFsck;
        use jeryu_gitd::refs::RefService;

        let resolved = match rm.resolve_parts(ready.owner, ready.repo) {
            Ok(resolved) => resolved,
            Err(err) => {
                return json_response(
                    500,
                    &json!({
                        "message": format!("could not resolve repository: {err}"),
                        "documentation_url": docs_url(),
                    }),
                );
            }
        };

        let refs = RefService::new((**rm).clone());

        // Resolve the LIVE base tip, never the stored base sha. A base branch
        // that does not exist yet (an empty repository) is created at the head
        // below instead of failing the merge.
        let base_oid =
            match refs.resolve_commit(&resolved, &format!("refs/heads/{}", ready.base_ref)) {
                Ok(base_oid) => base_oid,
                Err(err) => {
                    return json_response(
                        500,
                        &json!({ "message": err.to_string(), "documentation_url": docs_url() }),
                    );
                }
            };

        // Resolve the LIVE head: prefer the PR's head branch ref, then fall back
        // to the stored head sha ONLY if it is itself a real commit. A head that
        // resolves by neither route is unprocessable (422), not a 500.
        let head_oid = match self.resolve_pull_head(rm, &refs, &resolved, &ready) {
            Ok(Some(oid)) => oid,
            Ok(None) => {
                return json_response(
                    422,
                    &json!({
                        "message": format!(
                            "head ref {} does not resolve to a commit",
                            ready.head_locator()
                        ),
                        "documentation_url": docs_url(),
                    }),
                );
            }
            Err(err) => {
                return json_response(
                    500,
                    &json!({ "message": err.to_string(), "documentation_url": docs_url() }),
                );
            }
        };

        // No base branch yet: the gate already passed, so seed the base at the
        // PR head, a fast-forward from nothing. Direct pushes to main stay
        // blocked; this merge is the sanctioned path to create it.
        let Some(base_oid) = base_oid else {
            return self.create_base_at_head(rm, &refs, &resolved, &ready, &head_oid);
        };

        // If the head is already contained in the base history, the code has
        // already landed (e.g. a server-side fast-forward that never flipped the
        // record). Mark the PR merged idempotently against the real base oid
        // WITHOUT moving any ref.
        let fsck = ObjectFsck::new(rm.config().git_bin.clone());
        match fsck.is_ancestor(&resolved, &head_oid, &base_oid) {
            Ok(true) => {
                return match self.core.finalize_merge(
                    ready.owner,
                    ready.repo,
                    ready.number,
                    base_oid,
                    ready.req.sha.as_deref(),
                ) {
                    Ok(result) => merge_success_response(&result),
                    Err(ForgeError::BranchProtection(reason)) => json_response(
                        405,
                        &json!({ "message": reason, "documentation_url": docs_url() }),
                    ),
                    Err(err) => error_response(err),
                };
            }
            Ok(false) => {}
            Err(err) => {
                return json_response(
                    500,
                    &json!({ "message": err.to_string(), "documentation_url": docs_url() }),
                );
            }
        }

        // A linear-history base only accepts fast-forwards. Replay the PR onto
        // the live base tip first (as the merge queue does) so a clean PR that
        // readiness reports as mergeable actually lands; a replay that cannot
        // be built is refused with its reason instead of a bare non-ff error.
        let head_oid = if ready.require_linear_history {
            match crate::web::rebase_onto(
                &rm.config().git_bin,
                &resolved.path,
                &base_oid,
                &head_oid,
            ) {
                // A replay onto a moved base is a NEW sha that no runner has
                // gated: the gate ran on the PR head. Landing it would leave
                // the base tip without a required status of its own, which is
                // what auto-pin and the tag cutter read. Refuse, and let the
                // merge queue build and gate the replay (docs/merge-queue.md).
                Ok(rebased) if rebased != head_oid => {
                    match self.replay_gate_blocker(
                        ready.owner,
                        ready.repo,
                        &ready.base_ref,
                        &rebased,
                    ) {
                        None => rebased,
                        Some(reason) => {
                            return json_response(
                                409,
                                &json!({
                                    "message": format!(
                                        "{} requires linear history, and the replay of the pull \
                                         request onto it ({rebased}) {reason}; queue the pull \
                                         request so the forge gates the commit it lands",
                                        ready.base_ref
                                    ),
                                    "documentation_url": docs_url(),
                                }),
                            );
                        }
                    }
                }
                Ok(rebased) => rebased,
                Err(reason) => {
                    return json_response(
                        409,
                        &json!({
                            "message": format!(
                                "{} requires linear history and the pull request could not be \
                                 rebased onto it: {reason}",
                                ready.base_ref
                            ),
                            "documentation_url": docs_url(),
                        }),
                    );
                }
            }
        } else {
            head_oid
        };

        let message = merge_message(ready.number, ready.req);
        let outcome = match refs.merge_pull(
            &resolved,
            "system:pr-merge",
            &format!("refs/heads/{}", ready.base_ref),
            &base_oid,
            &head_oid,
            &message,
            ready.require_linear_history,
        ) {
            Ok(outcome) => outcome,
            // A conflicting tree is a 409, as is a refused non-fast-forward
            // merge on a linear-history-protected base.
            Err(GitdError::MergeConflict(detail)) => {
                return json_response(
                    409,
                    &json!({
                        "message": format!("merge conflict: {detail}"),
                        "documentation_url": docs_url(),
                    }),
                );
            }
            Err(err @ GitdError::NonFastForwardRequired) => {
                return json_response(
                    409,
                    &json!({ "message": err.to_string(), "documentation_url": docs_url() }),
                );
            }
            Err(err) => {
                return json_response(
                    500,
                    &json!({ "message": err.to_string(), "documentation_url": docs_url() }),
                );
            }
        };

        // The ref moved server-side, so run the same post-update bridge that a
        // receive-pack push would have triggered. This keeps declared internal
        // main automation (not client pushes) on the protected CAS/update-ref path.
        crate::ci_bridge::on_push(
            &self.core,
            rm,
            ready.owner,
            ready.repo,
            &[crate::ci_bridge::RefUpdate {
                ref_name: format!("refs/heads/{}", ready.base_ref),
                old_oid: base_oid,
                new_oid: outcome.merge_oid.clone(),
            }],
            &crate::ci_bridge::default_origin_base_url(),
        );

        // Reconcile the REAL git oid back into the PR record (handler never
        // mutates the model directly).
        match self.core.finalize_merge(
            ready.owner,
            ready.repo,
            ready.number,
            outcome.merge_oid.clone(),
            ready.req.sha.as_deref(),
        ) {
            Ok(result) => {
                // Merge landed on the real ref: mirror the (possibly
                // autoversion-advanced) live main tip to GitHub. Advisory by
                // construction — the merge response is already decided.
                self.mirror_merged_main(rm, &resolved, &ready, &outcome.merge_oid);
                merge_success_response(&result)
            }
            // The ref already moved; we do NOT roll it back. A reconciler can
            // detect the divergence by comparing merge_commit_sha vs the ref.
            Err(ForgeError::BranchProtection(reason)) => json_response(
                405,
                &json!({ "message": reason, "documentation_url": docs_url() }),
            ),
            Err(err) => error_response(err),
        }
    }

    /// Why `sha` may not land on `base_ref`, or `None` when it carries its own
    /// green gate. A base that declares no required contexts has no gate to
    /// wait for, so nothing blocks there.
    #[cfg(feature = "web")]
    fn replay_gate_blocker(
        &self,
        owner: &str,
        repo: &str,
        base_ref: &str,
        sha: &str,
    ) -> Option<String> {
        let required: Vec<String> = self
            .core
            .get_branch_protection(owner, repo, base_ref)
            .ok()?
            .required_status_checks;
        if required.is_empty() {
            return None;
        }
        match crate::web::gate_verdict(&self.core, owner, repo, sha, &required) {
            Some(true) => None,
            Some(false) => Some(format!("failed {}", required.join(", "))),
            None => Some(format!("has no result for {}", required.join(", "))),
        }
    }

    /// Create a missing `refs/heads/{base_ref}` at `head_oid` for an approved,
    /// mergeable PR. The ref update is a compare-and-swap against the zero oid,
    /// so a base that appeared concurrently is refused rather than overwritten.
    #[cfg(feature = "web")]
    fn create_base_at_head(
        &self,
        rm: &std::sync::Arc<jeryu_gitd::RepoManager>,
        refs: &jeryu_gitd::refs::RefService,
        resolved: &jeryu_gitd::repo::Repository,
        ready: &MergeReady<'_>,
        head_oid: &str,
    ) -> Response {
        use jeryu_gitd::refs::ZERO_OID;

        let base_name = format!("refs/heads/{}", ready.base_ref);
        if let Err(err) = refs.update_ref(
            resolved,
            "system:pr-merge",
            &base_name,
            head_oid,
            Some(ZERO_OID),
        ) {
            return json_response(
                409,
                &json!({
                    "message": format!("could not create {base_name}: {err}"),
                    "documentation_url": docs_url(),
                }),
            );
        }
        crate::ci_bridge::on_push(
            &self.core,
            rm,
            ready.owner,
            ready.repo,
            &[crate::ci_bridge::RefUpdate {
                ref_name: base_name,
                old_oid: ZERO_OID.to_string(),
                new_oid: head_oid.to_string(),
            }],
            &crate::ci_bridge::default_origin_base_url(),
        );
        match self.core.finalize_merge(
            ready.owner,
            ready.repo,
            ready.number,
            head_oid.to_string(),
            ready.req.sha.as_deref(),
        ) {
            Ok(result) => {
                self.mirror_merged_main(rm, resolved, ready, head_oid);
                merge_success_response(&result)
            }
            Err(ForgeError::BranchProtection(reason)) => json_response(
                405,
                &json!({ "message": reason, "documentation_url": docs_url() }),
            ),
            Err(err) => error_response(err),
        }
    }

    /// Land a merge-queue commit (docs/merge-queue.md): fast-forward
    /// `refs/heads/<base>` from `expected_base` to `queue_sha`, the gated
    /// replay of the PR onto that base, and record the PR as merged at it.
    ///
    /// Nothing here is queue-specific policy. The PR must still pass the same
    /// merge gate at its own head (`evaluate_merge_readiness`, re-checked by
    /// `finalize_merge`); the ref moves only by compare-and-swap from
    /// `expected_base` and only as a fast-forward; the push bridge and the
    /// GitHub mirror run exactly as for a direct merge. Returns the new base
    /// oid, or why it did not land (the queue decides whether to rebuild).
    #[cfg(feature = "web")]
    pub(crate) fn land_queued(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        pr_head: &str,
        queue_sha: &str,
        expected_base: &str,
    ) -> std::result::Result<String, LandRefusal> {
        use jeryu_gitd::refs::RefService;

        let rm = self
            .repo_manager
            .as_ref()
            .ok_or_else(|| LandRefusal::Blocked("no git backend is wired".to_string()))?;
        let (base_ref, head_ref, head_sha, require_linear_history) = match self
            .core
            .evaluate_merge_readiness(owner, repo, number, Some(pr_head))
        {
            Ok(MergeReadiness::Ready {
                base_ref,
                head_ref,
                head_sha,
                require_linear_history,
                ..
            }) => (base_ref, head_ref, head_sha, require_linear_history),
            Ok(MergeReadiness::AlreadyMerged { sha }) => {
                return Err(LandRefusal::Blocked(format!("already merged at {sha}")));
            }
            Err(err) => return Err(LandRefusal::Blocked(err.to_string())),
        };
        let resolved = rm
            .resolve_parts(owner, repo)
            .map_err(|err| LandRefusal::Blocked(err.to_string()))?;
        let refs = RefService::new((**rm).clone());
        let base_name = format!("refs/heads/{base_ref}");
        let base_oid = refs
            .resolve_commit(&resolved, &base_name)
            .map_err(|err| LandRefusal::Blocked(err.to_string()))?
            .ok_or_else(|| LandRefusal::Blocked(format!("{base_name} does not resolve")))?;
        if base_oid != expected_base {
            return Err(LandRefusal::BaseMoved(base_oid));
        }
        let req = MergePullRequestRequest {
            sha: Some(pr_head.to_string()),
            ..MergePullRequestRequest::default()
        };
        let message = merge_message(number, &req);
        // Always a fast-forward: the queue commit was built on `expected_base`.
        let outcome = refs
            .merge_pull(
                &resolved,
                "system:merge-queue",
                &base_name,
                &base_oid,
                queue_sha,
                &message,
                true,
            )
            .map_err(|err| LandRefusal::Blocked(err.to_string()))?;
        crate::ci_bridge::on_push(
            &self.core,
            rm,
            owner,
            repo,
            &[crate::ci_bridge::RefUpdate {
                ref_name: base_name,
                old_oid: base_oid,
                new_oid: outcome.merge_oid.clone(),
            }],
            &crate::ci_bridge::default_origin_base_url(),
        );
        self.core
            .finalize_merge(
                owner,
                repo,
                number,
                outcome.merge_oid.clone(),
                Some(pr_head),
            )
            .map_err(|err| LandRefusal::Blocked(format!("ref moved but finalize failed: {err}")))?;
        let ready = MergeReady {
            owner,
            repo,
            number,
            req: &req,
            base_ref,
            head_ref,
            source_repository: self.pull_source_repository(owner, repo, number),
            head_sha,
            require_linear_history,
        };
        self.mirror_merged_main(rm, &resolved, &ready, &outcome.merge_oid);
        Ok(outcome.merge_oid)
    }

    /// Push the merged default branch to its configured GitHub mirror and
    /// record the outcome as the `jeryu/github-mirror` check-run on the merge
    /// sha. Strictly advisory: every failure path is swallowed after being
    /// recorded — a GitHub outage must never fail or delay a local merge
    /// beyond the bounded push timeout.
    #[cfg(feature = "web")]
    fn mirror_merged_main(
        &self,
        rm: &std::sync::Arc<jeryu_gitd::RepoManager>,
        resolved: &jeryu_gitd::repo::Repository,
        ready: &MergeReady<'_>,
        merge_oid: &str,
    ) {
        use crate::github_mirror::MirrorPushOutcome;
        use jeryu_core::{CheckConclusion, CheckRunOutput, CheckRunStatus, CreateCheckRunRequest};

        let Some(mirror) = self.github_mirror.as_ref() else {
            return;
        };
        // Only pushes of the repo's default branch mirror to GitHub main.
        let default_branch = match self.core.get_repository(ready.owner, ready.repo) {
            Ok(repo) => repo.default_branch,
            Err(_) => return,
        };
        if ready.base_ref != default_branch {
            return;
        }
        let outcome = mirror.push_branch(
            &rm.config().git_bin,
            &resolved.path,
            ready.owner,
            ready.repo,
        );
        let (conclusion, summary, tip) = match outcome {
            // Unconfigured repo: no push attempted, no check-run noise.
            MirrorPushOutcome::Skipped(_) => return,
            MirrorPushOutcome::Pushed { tip } => (
                CheckConclusion::Success,
                format!("pushed {tip} to GitHub main"),
                tip,
            ),
            MirrorPushOutcome::Failed(detail) => {
                (CheckConclusion::Failure, detail, merge_oid.to_string())
            }
        };
        let _ = self.core.create_check_run(
            ready.owner,
            ready.repo,
            CreateCheckRunRequest {
                name: crate::github_mirror::MIRROR_CHECK_NAME.to_string(),
                head_sha: tip,
                status: Some(CheckRunStatus::Completed),
                conclusion: Some(conclusion),
                details_url: None,
                output: Some(CheckRunOutput {
                    title: "merge-to-GitHub mirror".to_string(),
                    summary,
                    text: None,
                }),
            },
        );
    }

    /// Mirror a fork PR's head branch into this repository as
    /// `refs/pull/<number>/head` and return that ref name, so the destination
    /// holds the fork's commit without the fork's branch names ever leaking
    /// into `refs/heads/`. Returns `None` for a same-repo PR or when the fetch
    /// does not succeed (callers then fall back to their own resolution).
    #[cfg(feature = "web")]
    fn fetch_pull_head(
        &self,
        rm: &std::sync::Arc<jeryu_gitd::RepoManager>,
        owner: &str,
        repo: &str,
        number: u64,
        head_ref: &str,
        source_repository: &str,
    ) -> Option<String> {
        let (fork_owner, fork_repo) = fork_source(owner, repo, Some(source_repository))?;
        let source = rm.resolve_parts(&fork_owner, &fork_repo).ok()?;
        let destination = rm.resolve_parts(owner, repo).ok()?;
        let pull_ref = format!("refs/pull/{number}/head");
        let spec = format!("+refs/heads/{}:{pull_ref}", head_branch(head_ref));
        let status = std::process::Command::new(&rm.config().git_bin)
            .args(["fetch", "--no-tags", &source.path.to_string_lossy(), &spec])
            .current_dir(&destination.path)
            .status()
            .ok()?;
        status.success().then_some(pull_ref)
    }

    /// Fetch a freshly created fork PR's head into `refs/pull/<n>/head`.
    #[cfg(feature = "web")]
    fn fetch_fork_head(
        &self,
        rm: &std::sync::Arc<jeryu_gitd::RepoManager>,
        owner: &str,
        repo: &str,
        pr: &PullRequest,
    ) -> Option<String> {
        self.fetch_pull_head(
            rm,
            owner,
            repo,
            pr.number,
            &pr.head.ref_name,
            &pr.source_repository,
        )
    }

    /// Resolve a PR's head to a real commit oid for merging.
    ///
    /// For a FORK PR the head branch does not live here: re-fetch it from the
    /// source repository into `refs/pull/<n>/head` and resolve that, never
    /// `refs/heads/<head_ref>` of the destination (which may be an unrelated
    /// same-named branch). For a same-repo PR the live head branch ref is tried
    /// first. Both then fall back to the stored head sha ONLY when it is itself
    /// a real commit in the repo. Returns `Ok(None)` when nothing resolves, so
    /// the caller renders a 4xx rather than feeding a placeholder into the merge.
    #[cfg(feature = "web")]
    fn resolve_pull_head(
        &self,
        rm: &std::sync::Arc<jeryu_gitd::RepoManager>,
        refs: &jeryu_gitd::refs::RefService,
        repo: &jeryu_gitd::repo::Repository,
        ready: &MergeReady<'_>,
    ) -> jeryu_gitd::Result<Option<String>> {
        if fork_source(ready.owner, ready.repo, Some(&ready.source_repository)).is_some() {
            let _ = self.fetch_pull_head(
                rm,
                ready.owner,
                ready.repo,
                ready.number,
                &ready.head_ref,
                &ready.source_repository,
            );
            if let Some(oid) =
                refs.resolve_commit(repo, &format!("refs/pull/{}/head", ready.number))?
            {
                return Ok(Some(oid));
            }
            return refs.resolve_commit(repo, &ready.head_sha);
        }
        if let Some(oid) = refs.resolve_commit(repo, &format!("refs/heads/{}", ready.head_ref))? {
            return Ok(Some(oid));
        }
        refs.resolve_commit(repo, &ready.head_sha)
    }
}

/// Why a merge-queue commit did not land.
#[cfg(feature = "web")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LandRefusal {
    /// The base tip is no longer the one the queue commit was built on; the
    /// queue rebuilds on this new tip.
    BaseMoved(String),
    /// The PR no longer passes the merge gate, or git refused the update.
    Blocked(String),
}

/// Parameters describing a PR that has already passed the merge gate.
///
/// `base_ref`/`head_ref`/`require_linear_history` are only consumed by the real
/// git-merge path (`web` feature); the in-memory fallback uses only
/// `head_sha`/`number`.
struct MergeReady<'a> {
    owner: &'a str,
    repo: &'a str,
    number: u64,
    req: &'a MergePullRequestRequest,
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    base_ref: String,
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    head_ref: String,
    /// Repository full name the head lives in; a FORK when it differs from
    /// `owner/repo` (see `fork_source`).
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    source_repository: String,
    head_sha: String,
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    require_linear_history: bool,
}

#[cfg(feature = "web")]
impl MergeReady<'_> {
    /// Where the merge looks for this PR's head, for error messages.
    fn head_locator(&self) -> String {
        if fork_source(self.owner, self.repo, Some(&self.source_repository)).is_some() {
            format!("refs/pull/{}/head", self.number)
        } else {
            format!("refs/heads/{}", self.head_ref)
        }
    }
}

fn merge_success_response(result: &jeryu_core::MergeResult) -> Response {
    json_response(
        200,
        &json!({
            "sha": result.sha,
            "merged": result.merged,
            "message": result.message,
        }),
    )
}

/// Build the merge commit message: a default GitHub-shaped title, optionally
/// followed by a request-provided title/body.
#[cfg(feature = "web")]
fn merge_message(number: u64, req: &MergePullRequestRequest) -> String {
    let mut message = format!("Merge pull request #{number}");
    if let Some(title) = req
        .commit_title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        message.push_str("\n\n");
        message.push_str(title);
    }
    if let Some(body) = req
        .commit_message
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        message.push_str("\n\n");
        message.push_str(body);
    }
    message
}

/// The repository full name a create request's head lives in: an explicit
/// `source_repository`, else the `fork-owner:branch` prefix of `head` (GitHub's
/// cross-repository form, whose repo name matches the destination), else this
/// repository.
fn create_source_repository(owner: &str, repo: &str, req: &CreatePullRequestRequest) -> String {
    req.source_repository
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            req.head
                .split_once(':')
                .map(|(head_owner, _)| head_owner.trim())
                .filter(|head_owner| !head_owner.is_empty())
                .map(|head_owner| format!("{head_owner}/{repo}"))
        })
        .unwrap_or_else(|| format!("{owner}/{repo}"))
}

/// The branch name of a head that may carry GitHub's `fork-owner:branch` prefix.
#[cfg(feature = "web")]
fn head_branch(head: &str) -> &str {
    head.split_once(':').map_or(head, |(_, branch)| branch)
}

/// Split a PR's source repository into `(owner, repo)` when it is a FORK of the
/// destination. Returns `None` for a same-repo PR (and for an unparsable name),
/// so callers keep resolving the head in this repository as before.
#[cfg(feature = "web")]
fn fork_source(
    owner: &str,
    repo: &str,
    source_repository: Option<&str>,
) -> Option<(String, String)> {
    let (source_owner, source_repo) = source_repository?.trim().split_once('/')?;
    let source_repo = source_repo.trim_end_matches(".git");
    if source_owner.is_empty() || source_repo.is_empty() {
        return None;
    }
    if source_owner == owner && source_repo == repo.trim_end_matches(".git") {
        return None;
    }
    Some((source_owner.to_string(), source_repo.to_string()))
}

/// Web UI route for a pull request: `/repos/<host>/<owner>/<repo>/pulls/<n>`.
/// Every forge repository is served under the `jeryu` host segment.
pub(crate) fn pull_request_web_path(owner: &str, repo: &str, number: u64) -> String {
    format!("/repos/jeryu/{owner}/{repo}/pulls/{number}")
}

pub(super) fn pull_request_json(pr: &PullRequest) -> Value {
    json!({
        "id": pr.id,
        // GitHub-compatible: per-repo `number`, never an internal/global id.
        "number": pr.number,
        "state": pr_open_or_closed(&pr.state),
        "draft": pr.draft,
        "title": pr.title,
        "body": pr.body,
        "user": owner_json(&pr.author),
        "head": git_ref_json(pr, &pr.head),
        "base": git_ref_json(pr, &pr.base),
        "mergeable": pr.mergeable,
        "mergeable_state": pr.mergeable_state,
        "merged": pr.merged,
        "merged_at": pr.merged_at,
        "merge_commit_sha": pr.merge_commit_sha,
        "source_repository": pr.source_repository,
        "html_url": super::support::web_url(&pull_request_web_path(&pr.owner, &pr.repo, pr.number)),
        "url": format!("/repos/{}/{}/pulls/{}", pr.owner, pr.repo, pr.number),
        "created_at": pr.created_at,
        "updated_at": pr.updated_at,
    })
}

fn git_ref_json(pr: &PullRequest, git_ref: &jeryu_core::GitBranchRef) -> Value {
    json!({
        "label": git_ref.label,
        "ref": git_ref.ref_name,
        "sha": git_ref.sha,
        "repo": { "full_name": format!("{}/{}", pr.owner, pr.repo) },
    })
}

/// GitHub PRs only ever report `open`, `closed`, or merged-as-closed at the
/// `state` field. Jeryu's richer lifecycle (Mergeable, BlockedByChecks, ...)
/// is surfaced through `mergeable`/`mergeable_state`; `state` is normalized.
fn pr_open_or_closed(state: &PullRequestState) -> &'static str {
    match state {
        PullRequestState::Merged | PullRequestState::Closed => "closed",
        _ => "open",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jeryu_core::{CreateRepositoryRequest, CreateUserRequest, ForgeCore};

    #[test]
    fn pull_request_json_includes_source_repository() {
        let core = ForgeCore::new();
        core.create_user(CreateUserRequest {
            login: "alice".to_string(),
            ..Default::default()
        })
        .unwrap();
        core.create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                default_branch: Some("main".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let default_pr = core
            .create_pull_request(
                "alice",
                "jeryu",
                "alice",
                CreatePullRequestRequest {
                    title: "default source".to_string(),
                    head: "feature".to_string(),
                    base: "main".to_string(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            pull_request_json(&default_pr)["source_repository"],
            "alice/jeryu"
        );

        let explicit_pr = core
            .create_pull_request(
                "alice",
                "jeryu",
                "alice",
                CreatePullRequestRequest {
                    title: "forked source".to_string(),
                    head: "feature-2".to_string(),
                    base: "main".to_string(),
                    source_repository: Some("fork-owner/jeryu".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            pull_request_json(&explicit_pr)["source_repository"],
            "fork-owner/jeryu"
        );
    }
}
