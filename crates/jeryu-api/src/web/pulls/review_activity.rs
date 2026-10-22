//! What the automated reviewer is doing about this pull request right now.
//!
//! A review verdict is about the head it was taken on, so a page that shows
//! verdicts without saying which head is current reads a withdrawn hold as a
//! live one (live on 2026-10-22: jeryu/jeryu-ci-runner#109 was held at
//! 91beebd, the author pushed edd08c5, and the reviewer was already reading
//! edd08c5 while the page still showed the hold as the verdict).
//!
//! The reviews themselves are in the detail payload and carry their head, so
//! the one fact the page cannot derive is the pass that has not finished yet.
//! That is this block: the head a reviewer is reading and when it started.

use chrono::Utc;
use serde::Serialize;

use jeryu_core::PullRequest;

use crate::web::WebState;
use crate::web::auth::ANONYMOUS_LOGIN;
use crate::web::control_plane::{is_online, is_reviewer};

/// A review pass a reviewer reports as running on this pull request.
#[derive(Debug, Clone, Serialize)]
pub(super) struct ReviewActivity {
    /// The head being read. Compared with the pull request's own head by the
    /// reader, so a pass on a head that has itself been replaced is visible
    /// as such rather than silently taken for the current one.
    pub(super) sha: String,
    /// When the pass started (RFC3339).
    pub(super) started_at: String,
    /// Which reviewer is reading it. Absent for an anonymous reader: a
    /// runner's host is site topology, and this route answers readers of
    /// public repositories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) runner_id: Option<String>,
}

/// The review pass running on this pull request, if a reviewer reports one.
///
/// Only a reviewer that is still beating counts: a pass claimed by a runner
/// that has gone quiet is not something to wait for, and saying "reviewing
/// since" about it would be the same mistake as a hold on a head that is
/// gone. `viewer` is the reading account's login, as on the checks tab.
pub(super) fn review_activity(
    state: &WebState,
    pr: &PullRequest,
    viewer: Option<&str>,
) -> Option<ReviewActivity> {
    let repo = format!("{}/{}", pr.owner, pr.repo);
    let now = Utc::now();
    let named = viewer.is_some_and(|login| !login.is_empty() && login != ANONYMOUS_LOGIN);
    state
        .gate_runners
        .snapshot()
        .iter()
        .filter(|record| is_reviewer(&record.heartbeat) && is_online(record, now))
        .find_map(|record| {
            let task = record
                .heartbeat
                .current
                .as_ref()
                .filter(|task| task.repo == repo && task.pr == Some(pr.number))?;
            Some(ReviewActivity {
                sha: task.sha.clone(),
                started_at: task.started_at.to_rfc3339(),
                runner_id: named.then(|| record.heartbeat.runner_id.clone()),
            })
        })
}
