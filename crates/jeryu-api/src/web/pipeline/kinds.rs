//! The closed set of event kinds the forge itself emits, and where each one
//! is emitted from. Nothing on a request path reads this: it is the contract
//! the sources and the docs are checked against. `docs/pipeline-events.md` publishes the same set under
//! "Emitted by the forge"; [`kinds_tests`](super::kinds_tests) holds the two
//! together, so a kind cannot be documented with no emit point behind it, or
//! emitted without the contract saying when.

/// One kind the forge emits, and the source that emits it.
pub(super) struct ForgeKind {
    /// The `kind` as it lands in the event log and in the docs table.
    pub kind: &'static str,
    /// The emitting file, relative to `crates/jeryu-api/src`.
    pub source: &'static str,
    /// Text that must still be in `source` for the emit point to exist: the
    /// kind's own literal, or the function that builds the kind when it is
    /// built rather than written out (`gate.*`, `review.*`).
    pub needle: &'static str,
}

const fn kind(kind: &'static str, source: &'static str, needle: &'static str) -> ForgeKind {
    ForgeKind {
        kind,
        source,
        needle,
    }
}

/// Every kind with `source = "forge"`, sorted by kind.
pub(super) const FORGE_KINDS: &[ForgeKind] = &[
    kind("defect.linked", "web/review_catch.rs", "DEFECT_KIND"),
    kind(
        "deploy.created",
        "web/pipeline/emit.rs",
        "\"deploy.created\"",
    ),
    kind("deploy.status", "web/pipeline/emit.rs", "\"deploy.status\""),
    // `gate.*` and `review.*` are built from the runner's label, so the emit
    // point is the heartbeat handler rather than a literal.
    kind(
        "gate.finished",
        "web/pipeline/emit.rs",
        "fn runner_heartbeat",
    ),
    kind(
        "gate.started",
        "web/pipeline/emit.rs",
        "fn runner_heartbeat",
    ),
    kind("main.escaped", "web/pipeline/emit.rs", "\"main.escaped\""),
    kind("main.status", "web/pipeline/emit.rs", "\"main.status\""),
    kind("pr.approved", "web/pipeline/emit.rs", "\"pr.approved\""),
    kind("pr.draft", "web/pipeline/emit.rs", "\"pr.draft\""),
    kind("pr.merged", "web/pipeline/emit.rs", "\"pr.merged\""),
    kind("pr.opened", "web/pipeline/emit.rs", "\"pr.opened\""),
    kind(
        "pr.ready_for_review",
        "web/pipeline/emit.rs",
        "\"pr.ready_for_review\"",
    ),
    kind("pr.review", "web/pipeline/emit.rs", "\"pr.review\""),
    kind("pr.skipped", "web/pipeline/emit.rs", "\"pr.skipped\""),
    kind("queue.building", "web/merge_queue.rs", "\"queue.building\""),
    kind("queue.dequeued", "web/merge_queue.rs", "\"queue.dequeued\""),
    kind("queue.enqueued", "web/merge_queue.rs", "\"queue.enqueued\""),
    kind("queue.failed", "web/merge_queue.rs", "\"queue.failed\""),
    kind("queue.landed", "web/merge_queue.rs", "\"queue.landed\""),
    kind("queue.refused", "web/merge_queue.rs", "\"queue.refused\""),
    kind(
        "release.promote_requested",
        "web/release_board/promote.rs",
        "\"release.promote_requested\"",
    ),
    kind("repo.archived", "web/pipeline/emit.rs", "\"repo.archived\""),
    kind("repo.renamed", "web/pipeline/emit.rs", "\"repo.renamed\""),
    kind(
        "repo.transferred",
        "web/pipeline/emit.rs",
        "\"repo.transferred\"",
    ),
    kind(
        "repo.unarchived",
        "web/pipeline/emit.rs",
        "\"repo.unarchived\"",
    ),
    kind("review.finding", "web/review_catch.rs", "FINDING_KIND"),
    kind(
        "review.finished",
        "web/pipeline/emit.rs",
        "fn runner_heartbeat",
    ),
    kind("review.resolved", "web/review_catch.rs", "RESOLVED_KIND"),
    kind(
        "review.started",
        "web/pipeline/emit.rs",
        "fn runner_heartbeat",
    ),
    kind(
        "scorecard.digest",
        "web/scorecard/weekly.rs",
        "\"scorecard.digest\"",
    ),
    kind(
        "scorecard.digest_undelivered",
        "web/scorecard/weekly.rs",
        "\"scorecard.digest_undelivered\"",
    ),
    kind("shift.pr_opened", "web/shift.rs", "\"shift.pr_opened\""),
    kind("todo.action", "web/shift.rs", "\"todo.action\""),
    kind("todo.commented", "web/shift.rs", "\"todo.commented\""),
    kind("todo.filed", "web/shift.rs", "\"todo.filed\""),
    kind(
        "worker.stage",
        "web/shift/visibility.rs",
        "\"worker.stage\"",
    ),
];
