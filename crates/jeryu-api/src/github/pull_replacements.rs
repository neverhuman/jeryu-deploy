//! What replaced a pull request, as data.
//!
//! An agent that cannot push to a frozen or contested branch lands the work as
//! a second pull request cherry-picked onto the base, and closes the first one.
//! Without a recorded link the only trace is the closing comment, so the pull
//! request page, the In flight graph and Flow all read the closed one as
//! abandoned work. This module holds the link itself: replaced pull request ->
//! the pull request that replaced it, one row per replaced pull request, so a
//! replacement may carry several.
//!
//! Both pull requests are in the same repository: the key carries the owner and
//! the name, and the route refuses a link that names another repository. A link
//! to itself and a link that closes a cycle are refused too, so following the
//! chain always ends.
//!
//! With a store attached (the web server attaches `<data_dir>/shift.sqlite`,
//! schema from `db/migrations/0026_pull_request_replacements.sql`) the links
//! survive a restart; without one — unit tests, embedding callers — they live
//! in this process.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use jeryu_core::ForgeCore;

/// How far a chain of replacements is followed before it is called broken.
/// Recording refuses a cycle, so a chain this long is a store written by
/// something else; following stops rather than spinning.
const MAX_CHAIN: usize = 32;

/// How many of a pull request's newest review comments the backfill reads when
/// looking for a closing pointer. The pointer is the closing comment, so it is
/// at the end; the bound keeps a long review history from being re-read in full
/// at every startup.
const BACKFILL_COMMENTS: usize = 20;

/// One recorded link and who recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReplacementLink {
    /// The pull request that replaced this one, in the same repository.
    pub number: u64,
    pub set_by: String,
    pub set_ms: i64,
}

/// Why a link was refused. The code is an error-envelope code: the route
/// answers it as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkRefused {
    pub code: &'static str,
    pub message: String,
}

type Key = (String, String, u64);

/// Replaced pull request -> the pull request that replaced it.
#[derive(Clone, Debug, Default)]
pub(crate) struct PullReplacements {
    links: Arc<Mutex<BTreeMap<Key, ReplacementLink>>>,
    #[cfg(feature = "web")]
    store: Option<store::PullReplacementStore>,
}

impl PullReplacements {
    /// Attaches the durable store at `path` and loads what it holds. Errors
    /// carry the sqlite failure so the caller can refuse to start rather than
    /// run with links that silently disappear.
    #[cfg(feature = "web")]
    pub(crate) fn with_store(path: &std::path::Path) -> Result<Self, String> {
        let store = store::PullReplacementStore::open(path)?;
        let loaded = store.load()?;
        Ok(Self {
            links: Arc::new(Mutex::new(loaded)),
            store: Some(store),
        })
    }

    /// The pull request that replaced `number`, if one is recorded.
    pub(crate) fn superseded_by(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Option<ReplacementLink> {
        self.lock().get(&key(owner, repo, number)).cloned()
    }

    /// The pull requests `number` replaced, ascending. One replacement may
    /// carry several, which is what a cherry-pick of two held pull requests
    /// onto one branch leaves.
    pub(crate) fn supersedes(&self, owner: &str, repo: &str, number: u64) -> Vec<u64> {
        let mut replaced: Vec<u64> = self
            .lock()
            .iter()
            .filter(|((link_owner, link_repo, _), link)| {
                link_owner == owner && link_repo == repo && link.number == number
            })
            .map(|((_, _, replaced), _)| *replaced)
            .collect();
        replaced.sort_unstable();
        replaced
    }

    /// The end of the chain from `number`: the pull request that replaced it,
    /// then whatever replaced that, and so on. `None` when nothing replaced
    /// `number` itself. `#141 -> #145 -> #147` answers `#147`.
    pub(crate) fn chain_end(&self, owner: &str, repo: &str, number: u64) -> Option<u64> {
        let mut at = self.superseded_by(owner, repo, number)?.number;
        let mut seen = vec![number, at];
        for _ in 0..MAX_CHAIN {
            let Some(next) = self.superseded_by(owner, repo, at) else {
                return Some(at);
            };
            if seen.contains(&next.number) {
                return Some(at);
            }
            seen.push(next.number);
            at = next.number;
        }
        Some(at)
    }

    /// Records that `replacing` replaced `replaced`, both in `owner/repo`.
    /// Refuses a link to itself and one that would close a cycle; the durable
    /// write comes first, so a store that cannot be written is reported
    /// instead of leaving a link that vanishes on the next restart.
    pub(crate) fn record(
        &self,
        owner: &str,
        repo: &str,
        replaced: u64,
        replacing: u64,
        set_by: &str,
        now_ms: i64,
    ) -> Result<(), LinkRefused> {
        if replaced == replacing {
            return Err(LinkRefused {
                code: "pull_replacement_self",
                message: format!("#{replaced} cannot replace itself"),
            });
        }
        if let Some(cycle) = self.closes_a_cycle(owner, repo, replaced, replacing) {
            return Err(LinkRefused {
                code: "pull_replacement_cycle",
                message: format!(
                    "#{replacing} is already replaced by #{cycle}, so recording it as the \
                     replacement of #{replaced} would make a cycle"
                ),
            });
        }
        let link = ReplacementLink {
            number: replacing,
            set_by: set_by.to_string(),
            set_ms: now_ms,
        };
        #[cfg(feature = "web")]
        if let Some(store) = &self.store {
            store
                .set(owner, repo, replaced, &link)
                .map_err(|error| LinkRefused {
                    code: "storage_failed",
                    message: format!("could not record the replacement link: {error}"),
                })?;
        }
        self.lock().insert(key(owner, repo, replaced), link);
        Ok(())
    }

    /// The pull request on the chain from `replacing` that already names
    /// `replaced`, if any: recording the new link would close a cycle.
    fn closes_a_cycle(
        &self,
        owner: &str,
        repo: &str,
        replaced: u64,
        replacing: u64,
    ) -> Option<u64> {
        let mut at = replacing;
        for _ in 0..MAX_CHAIN {
            let next = self.superseded_by(owner, repo, at)?.number;
            if next == replaced {
                return Some(at);
            }
            if next == at {
                return None;
            }
            at = next;
        }
        None
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<Key, ReplacementLink>> {
        self.links.lock().expect("pull replacement links lock")
    }
}

fn key(owner: &str, repo: &str, number: u64) -> Key {
    (owner.to_string(), repo.to_string(), number)
}

/// The pull request a closing pointer names: `Landed via #7`, `Replaced by
/// #7`, `Superseded by #7`, or a `replaces #7` the replacement itself says.
/// Only these forms, so a sentence that merely mentions a number records
/// nothing.
pub(crate) fn pointer_in(text: &str) -> Option<u64> {
    const LEADS: [&str; 4] = [
        "landed via #",
        "replaced by #",
        "superseded by #",
        "replaces #",
    ];
    let lowered = text.to_ascii_lowercase();
    LEADS.iter().find_map(|lead| {
        let start = lowered.find(lead)? + lead.len();
        let digits: String = lowered[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse::<u64>().ok().filter(|number| *number > 0)
    })
}

/// Which way round a pointer reads: a pull request's own `replaces #7` names
/// the pull request *it* replaced, every other form names its replacement.
fn pointer_names_the_replaced(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    let replaces = lowered.find("replaces #");
    let other = ["landed via #", "replaced by #", "superseded by #"]
        .iter()
        .filter_map(|lead| lowered.find(lead))
        .min();
    match (replaces, other) {
        (Some(replaces), Some(other)) => replaces < other,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Links the pull requests whose own text already says what replaced them: a
/// closing comment that points at the replacement (`Landed via #7`), or a
/// replacement whose title or body says `replaces #3`. Only pull requests with
/// no recorded link are read, so a startup after the first does almost no work,
/// and a link a route recorded is never overwritten by a comment.
///
/// Returns how many links it recorded. A refused link (itself, a cycle, a
/// number that is not a pull request of the repository) is skipped: the text is
/// a claim, not an instruction.
pub(crate) fn backfill(core: &ForgeCore, links: &PullReplacements, now_ms: i64) -> usize {
    let mut recorded = 0;
    for repo in core.list_repositories(None) {
        let Ok(pulls) = core.list_pull_requests(&repo.owner, &repo.name, None) else {
            continue;
        };
        let numbers: Vec<u64> = pulls.iter().map(|pr| pr.number).collect();
        for pr in &pulls {
            // The replacement's own `replaces #3` is read whatever state it is
            // in; a pointer in a comment is read on a pull request that is
            // closed and not merged, which is what a replaced one is.
            let own = format!("{} {}", pr.title, pr.body.clone().unwrap_or_default());
            if pointer_names_the_replaced(&own)
                && let Some(replaced) = pointer_in(&own)
                && numbers.contains(&replaced)
                && links
                    .superseded_by(&repo.owner, &repo.name, replaced)
                    .is_none()
                && links
                    .record(
                        &repo.owner,
                        &repo.name,
                        replaced,
                        pr.number,
                        "backfill",
                        now_ms,
                    )
                    .is_ok()
            {
                recorded += 1;
            }
            if pr.merged || !matches!(pr.state, jeryu_core::PullRequestState::Closed) {
                continue;
            }
            if links
                .superseded_by(&repo.owner, &repo.name, pr.number)
                .is_some()
            {
                continue;
            }
            let Some(replacing) = closing_pointer(core, pr) else {
                continue;
            };
            if numbers.contains(&replacing)
                && links
                    .record(
                        &repo.owner,
                        &repo.name,
                        pr.number,
                        replacing,
                        "backfill",
                        now_ms,
                    )
                    .is_ok()
            {
                recorded += 1;
            }
        }
    }
    recorded
}

/// The replacement the newest closing pointer on a pull request names. Both
/// places a pointer is written are read — a review comment (the forge's own
/// close writes one) and the pull request's companion issue thread (what the
/// page's close dialog and `gh pr close --comment` write) — newest first, so a
/// pull request closed, reopened and replaced again points at the replacement
/// that is live.
fn closing_pointer(core: &ForgeCore, pr: &jeryu_core::PullRequest) -> Option<u64> {
    let mut comments: Vec<(chrono::DateTime<chrono::Utc>, String)> = core
        .list_reviews(&pr.owner, &pr.repo, pr.number)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|review| review.body.map(|body| (review.submitted_at, body)))
        .collect();
    comments.extend(
        core.list_issue_comments(&pr.owner, &pr.repo, pr.issue_number)
            .unwrap_or_default()
            .into_iter()
            .map(|comment| (comment.created_at, comment.body)),
    );
    comments.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    comments
        .iter()
        .take(BACKFILL_COMMENTS)
        .map(|(_, body)| body.as_str())
        .filter(|body| !pointer_names_the_replaced(body))
        .find_map(pointer_in)
}

#[cfg(feature = "web")]
mod store {
    //! The sqlite half, in `<data_dir>/shift.sqlite`.

    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use rusqlite::{Connection, params};

    use super::{Key, ReplacementLink};

    #[derive(Clone)]
    pub(super) struct PullReplacementStore {
        inner: Arc<Mutex<Connection>>,
    }

    impl std::fmt::Debug for PullReplacementStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("PullReplacementStore")
        }
    }

    impl PullReplacementStore {
        pub(super) fn open(path: &Path) -> Result<Self, String> {
            let conn = Connection::open(path).map_err(|err| err.to_string())?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map_err(|err| err.to_string())?;
            crate::web::shift::migrate_shift_store(&conn)?;
            Ok(Self {
                inner: Arc::new(Mutex::new(conn)),
            })
        }

        pub(super) fn load(&self) -> Result<BTreeMap<Key, ReplacementLink>, String> {
            let conn = self.lock();
            let mut statement = conn
                .prepare(
                    "SELECT repo_owner, repo_name, pr_number, replacement_number, set_by, set_ms
                     FROM pull_request_replacements",
                )
                .map_err(|err| err.to_string())?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        (
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)? as u64,
                        ),
                        ReplacementLink {
                            number: row.get::<_, i64>(3)? as u64,
                            set_by: row.get(4)?,
                            set_ms: row.get(5)?,
                        },
                    ))
                })
                .map_err(|err| err.to_string())?;
            rows.collect::<Result<_, _>>()
                .map_err(|err| err.to_string())
        }

        pub(super) fn set(
            &self,
            owner: &str,
            repo: &str,
            replaced: u64,
            link: &ReplacementLink,
        ) -> Result<(), String> {
            self.lock()
                .execute(
                    "INSERT INTO pull_request_replacements
                       (repo_owner, repo_name, pr_number, replacement_number, set_by, set_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(repo_owner, repo_name, pr_number) DO UPDATE SET
                       replacement_number = excluded.replacement_number,
                       set_by = excluded.set_by,
                       set_ms = excluded.set_ms",
                    params![
                        owner,
                        repo,
                        replaced as i64,
                        link.number as i64,
                        link.set_by,
                        link.set_ms
                    ],
                )
                .map(|_| ())
                .map_err(|err| err.to_string())
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
            self.inner
                .lock()
                .expect("pull replacement store mutex poisoned")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links() -> PullReplacements {
        PullReplacements::default()
    }

    #[test]
    fn a_link_reads_both_ways_and_one_replacement_carries_several() {
        let links = links();
        links
            .record("acme", "widgets", 141, 145, "ada", 10)
            .unwrap();
        links
            .record("acme", "widgets", 146, 145, "ada", 11)
            .unwrap();
        assert_eq!(
            links
                .superseded_by("acme", "widgets", 141)
                .map(|l| l.number),
            Some(145)
        );
        assert_eq!(links.supersedes("acme", "widgets", 145), vec![141, 146]);
        // Another repository's numbers are its own.
        assert!(links.superseded_by("globex", "widgets", 141).is_none());
    }

    #[test]
    fn a_chain_is_followed_to_the_pull_request_that_is_live() {
        let links = links();
        links
            .record("acme", "widgets", 141, 145, "ada", 10)
            .unwrap();
        links
            .record("acme", "widgets", 145, 147, "ada", 11)
            .unwrap();
        assert_eq!(links.chain_end("acme", "widgets", 141), Some(147));
        assert_eq!(links.chain_end("acme", "widgets", 147), None);
    }

    #[test]
    fn a_link_to_itself_or_one_that_closes_a_cycle_is_refused() {
        let links = links();
        assert_eq!(
            links
                .record("acme", "widgets", 7, 7, "ada", 1)
                .unwrap_err()
                .code,
            "pull_replacement_self"
        );
        links.record("acme", "widgets", 7, 8, "ada", 1).unwrap();
        links.record("acme", "widgets", 8, 9, "ada", 2).unwrap();
        assert_eq!(
            links
                .record("acme", "widgets", 9, 7, "ada", 3)
                .unwrap_err()
                .code,
            "pull_replacement_cycle"
        );
        assert_eq!(links.chain_end("acme", "widgets", 7), Some(9));
    }

    #[test]
    fn only_a_pointer_form_is_read_as_one() {
        assert_eq!(pointer_in("Landed via #145, closing this one."), Some(145));
        assert_eq!(pointer_in("Replaced by #9"), Some(9));
        assert_eq!(pointer_in("superseded by #12"), Some(12));
        assert_eq!(pointer_in("Replaces #3 and #4"), Some(3));
        assert_eq!(pointer_in("see the run of #4181 for the failure"), None);
        assert!(pointer_names_the_replaced("replaces #3"));
        assert!(!pointer_names_the_replaced("landed via #3"));
    }

    #[cfg(feature = "web")]
    #[test]
    fn a_link_survives_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shift.sqlite");
        PullReplacements::with_store(&path)
            .unwrap()
            .record("acme", "widgets", 141, 145, "ada", 10)
            .unwrap();
        let reopened = PullReplacements::with_store(&path).unwrap();
        assert_eq!(
            reopened
                .superseded_by("acme", "widgets", 141)
                .map(|link| link.number),
            Some(145)
        );
        assert_eq!(reopened.supersedes("acme", "widgets", 145), vec![141]);
    }
}
