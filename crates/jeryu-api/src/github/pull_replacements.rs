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
        if let Some(refused) = self.refusal(owner, repo, replaced, replacing) {
            return Err(refused);
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

    /// Why [`record`](Self::record) would refuse this link, without writing
    /// anything: a caller that closes the pull request first asks here before
    /// the close, so a refusal never leaves a closed pull request behind.
    pub(crate) fn refusal(
        &self,
        owner: &str,
        repo: &str,
        replaced: u64,
        replacing: u64,
    ) -> Option<LinkRefused> {
        if replaced == replacing {
            return Some(LinkRefused {
                code: "pull_replacement_self",
                message: format!("#{replaced} cannot replace itself"),
            });
        }
        let cycle = self.closes_a_cycle(owner, repo, replaced, replacing)?;
        Some(LinkRefused {
            code: "pull_replacement_cycle",
            message: format!(
                "#{replacing} is already replaced by #{cycle}, so recording it as the \
                 replacement of #{replaced} would make a cycle"
            ),
        })
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

/// The first closing pointer in a text: `Landed via #7`, `Replaced by #7`,
/// `Superseded by #7`, or a `replaces #7` the replacement itself says. Only
/// these forms, so a sentence that merely mentions a number records nothing.
///
/// Returns whether it names the replaced pull request (`replaces #`) and the
/// number it names. Both come from the same match, so `replaces #3 (landed via
/// #9)` reads as "replaces #3" and never as a link to #9.
fn pointer(text: &str) -> Option<(bool, u64)> {
    const LEADS: [(&str, bool); 4] = [
        ("landed via #", false),
        ("replaced by #", false),
        ("superseded by #", false),
        ("replaces #", true),
    ];
    let lowered = text.to_ascii_lowercase();
    LEADS
        .iter()
        .flat_map(|(lead, names_the_replaced)| {
            lowered
                .match_indices(lead)
                .map(move |(at, _)| (at, at + lead.len(), *names_the_replaced))
        })
        .filter_map(|(at, start, names_the_replaced)| {
            let digits: String = lowered[start..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            let number = digits.parse::<u64>().ok().filter(|number| *number > 0)?;
            Some((at, names_the_replaced, number))
        })
        .min_by_key(|(at, _, _)| *at)
        .map(|(_, names_the_replaced, number)| (names_the_replaced, number))
}

/// The branch namespace the GitHub importer writes contributions into
/// (`web/pull_import.rs`). It is reserved: a pull request from it is a
/// contribution whatever its recorded origin says.
const CONTRIBUTION_BRANCH_PREFIX: &str = "contrib/gh-";

/// The facts that decide whether one pull request's own text may name another
/// as replaced. They are the derived `pr_superseded` rule's
/// (`web/pipeline/attention/flow.rs`): the same author, base and source
/// repository, and the claimant opened after the pull request it names.
struct ClaimScope<'a> {
    author: &'a str,
    base_ref: &'a str,
    source_repository: &'a str,
    head_ref: &'a str,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl<'a> ClaimScope<'a> {
    fn of(pr: &'a jeryu_core::PullRequest, repo: &'a str) -> Self {
        let source = pr.source_repository.trim();
        Self {
            author: pr.author.trim(),
            base_ref: &pr.base.ref_name,
            source_repository: if source.is_empty() { repo } else { source },
            head_ref: &pr.head.ref_name,
            created_at: pr.created_at,
        }
    }
}

/// True when `claimant`'s own `replaces #N` may record a link for `replaced`.
/// A pull request's title and body are its author's words, and an imported
/// contribution's are an outsider's: they can only speak for the author's own
/// earlier work on the same base, never for somebody else's pull request.
fn may_claim(claimant: &ClaimScope<'_>, replaced: &ClaimScope<'_>) -> bool {
    !claimant.head_ref.starts_with(CONTRIBUTION_BRANCH_PREFIX)
        && !replaced.head_ref.starts_with(CONTRIBUTION_BRANCH_PREFIX)
        && !claimant.author.is_empty()
        && claimant.author.eq_ignore_ascii_case(replaced.author)
        && claimant.base_ref == replaced.base_ref
        && claimant.source_repository == replaced.source_repository
        && claimant.created_at > replaced.created_at
}

/// True when a closing comment on a pull request may record `replacing` as
/// what replaced it. A comment is whoever wrote it, so without a guard any
/// commenter's "Landed via #9" marks somebody else's pull request replaced.
/// The pointer is believed only from a commenter who could have landed the
/// replacement — write access to the repository — and only within the scope
/// [`may_claim`] allows a pull request's own text: the same base and source
/// repository, neither side an imported contribution, and the replacement
/// opened after the pull request it replaces. The commenter need not be the
/// author: the forge's own close, a reviewer's and a teammate's all count,
/// which is what the closing comments already written look like.
fn may_point(
    commenter_can_write: bool,
    replacing: &ClaimScope<'_>,
    replaced: &ClaimScope<'_>,
) -> bool {
    commenter_can_write
        && !replacing.head_ref.starts_with(CONTRIBUTION_BRANCH_PREFIX)
        && !replaced.head_ref.starts_with(CONTRIBUTION_BRANCH_PREFIX)
        && replacing.base_ref == replaced.base_ref
        && replacing.source_repository == replaced.source_repository
        && replacing.created_at > replaced.created_at
}

/// Links the pull requests whose own text already says what replaced them: a
/// closing comment that points at the replacement (`Landed via #7`), or a
/// replacement whose title or body says `replaces #3`. Only pull requests with
/// no recorded link are read, so a startup after the first does almost no work,
/// and a link a route recorded is never overwritten by a comment.
///
/// Returns how many links it recorded. A refused link (itself, a cycle, a
/// number that is not a pull request of the repository) is skipped: the text is
/// a claim, not an instruction. A `replaces #3` is believed only within the
/// scope [`may_claim`] allows, so no pull request's text can mark somebody
/// else's pull request replaced.
pub(crate) fn backfill(core: &ForgeCore, links: &PullReplacements, now_ms: i64) -> usize {
    let mut recorded = 0;
    for repo in core.list_repositories(None) {
        let Ok(pulls) = core.list_pull_requests(&repo.owner, &repo.name, None) else {
            continue;
        };
        let full_name = format!("{}/{}", repo.owner, repo.name);
        for pr in &pulls {
            // The replacement's own `replaces #3` is read whatever state it is
            // in; a pointer in a comment is read on a pull request that is
            // closed and not merged, which is what a replaced one is.
            let own = format!("{} {}", pr.title, pr.body.clone().unwrap_or_default());
            if let Some((true, replaced)) = pointer(&own)
                && let Some(named) = pulls.iter().find(|candidate| candidate.number == replaced)
                && may_claim(
                    &ClaimScope::of(pr, &full_name),
                    &ClaimScope::of(named, &full_name),
                )
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
            let Some((replacing, commenter)) = closing_pointer(core, pr) else {
                continue;
            };
            let Some(named) = pulls.iter().find(|candidate| candidate.number == replacing) else {
                continue;
            };
            if may_point(
                core.user_can_write_repo(commenter.trim(), &repo.owner, &repo.name),
                &ClaimScope::of(named, &full_name),
                &ClaimScope::of(pr, &full_name),
            ) && links
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

/// The replacement the newest closing pointer on a pull request names, with
/// the login that wrote it for [`may_point`] to weigh. Both
/// places a pointer is written are read — a review comment (the forge's own
/// close writes one) and the pull request's companion issue thread (what the
/// page's close dialog and `gh pr close --comment` write) — newest first, so a
/// pull request closed, reopened and replaced again points at the replacement
/// that is live.
fn closing_pointer(core: &ForgeCore, pr: &jeryu_core::PullRequest) -> Option<(u64, String)> {
    let mut comments: Vec<(chrono::DateTime<chrono::Utc>, String, String)> = core
        .list_reviews(&pr.owner, &pr.repo, pr.number)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|review| {
            review
                .body
                .map(|body| (review.submitted_at, body, review.author))
        })
        .collect();
    comments.extend(
        core.list_issue_comments(&pr.owner, &pr.repo, pr.issue_number)
            .unwrap_or_default()
            .into_iter()
            .map(|comment| (comment.created_at, comment.body, comment.author)),
    );
    comments.sort_by_key(|(at, _, _)| std::cmp::Reverse(*at));
    comments
        .iter()
        .take(BACKFILL_COMMENTS)
        .find_map(|(_, body, author)| match pointer(body) {
            Some((false, number)) => Some((number, author.clone())),
            _ => None,
        })
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
        assert_eq!(
            pointer("Landed via #145, closing this one."),
            Some((false, 145))
        );
        assert_eq!(pointer("Replaced by #9"), Some((false, 9)));
        assert_eq!(pointer("superseded by #12"), Some((false, 12)));
        assert_eq!(pointer("Replaces #3 and #4"), Some((true, 3)));
        assert_eq!(pointer("see the run of #4181 for the failure"), None);
    }

    #[test]
    fn direction_and_number_come_from_the_same_pointer() {
        // The first pointer decides both: a fixed order of forms once read
        // "replaces #3 (landed via #9)" as a link for #9.
        assert_eq!(pointer("replaces #3 (landed via #9)"), Some((true, 3)));
        assert_eq!(pointer("Landed via #9; this replaces #3"), Some((false, 9)));
        // A lead with no number does not hide a later one.
        assert_eq!(pointer("replaces #x, replaced by #5"), Some((false, 5)));
    }

    fn scope<'a>(author: &'a str, base: &'a str, head: &'a str, minute: u32) -> ClaimScope<'a> {
        ClaimScope {
            author,
            base_ref: base,
            source_repository: "jeryu/jeryu-deploy",
            head_ref: head,
            created_at: chrono::TimeZone::with_ymd_and_hms(
                &chrono::Utc,
                2026,
                10,
                19,
                12,
                minute,
                0,
            )
            .single()
            .expect("a valid instant"),
        }
    }

    #[test]
    fn own_text_may_only_replace_the_authors_earlier_pull_on_the_same_base() {
        let old = scope("alton", "main", "nightshift/1", 0);
        assert!(may_claim(
            &scope("Alton", "main", "nightshift/1-r2", 5),
            &old
        ));
        // Another author's text names nothing of yours.
        assert!(!may_claim(&scope("mallory", "main", "feature", 5), &old));
        // Nor does a pull request on another base, or from another repository.
        assert!(!may_claim(
            &scope("alton", "release", "nightshift/1-r2", 5),
            &old
        ));
        let mut forked = scope("alton", "main", "nightshift/1-r2", 5);
        forked.source_repository = "someone/fork";
        assert!(!may_claim(&forked, &old));
        // An earlier pull request cannot claim a later one.
        assert!(!may_claim(
            &scope("alton", "main", "x", 0),
            &scope("alton", "main", "y", 5)
        ));
        // An empty author matches nobody.
        assert!(!may_claim(
            &scope("", "main", "x", 5),
            &scope("", "main", "y", 0)
        ));
    }

    #[test]
    fn an_imported_contribution_is_never_read_as_a_claim() {
        // The importer's namespace is an outsider's words, whatever the author.
        let old = scope("alton", "main", "nightshift/1", 0);
        assert!(!may_claim(
            &scope("alton", "main", "contrib/gh-42", 5),
            &old
        ));
        assert!(!may_claim(
            &scope("alton", "main", "nightshift/1-r2", 5),
            &scope("alton", "main", "contrib/gh-41", 0)
        ));
    }

    #[test]
    fn a_closing_comment_is_read_only_from_a_commenter_who_could_have_landed_it() {
        let replaced = scope("alton", "main", "nightshift/1", 0);
        let replacement = scope("mara", "main", "nightshift/1-r2", 5);
        // Another author's close counts, so long as it is a commenter with
        // write access to the repository.
        assert!(may_point(true, &replacement, &replaced));
        // A commenter who cannot write the repository is a passer-by.
        assert!(!may_point(false, &replacement, &replaced));
        // A replacement on another base, or from another repository, is not
        // this pull request's replacement whoever says it is.
        assert!(!may_point(
            true,
            &scope("mara", "release", "nightshift/1-r2", 5),
            &replaced
        ));
        let mut forked = scope("mara", "main", "nightshift/1-r2", 5);
        forked.source_repository = "someone/fork";
        assert!(!may_point(true, &forked, &replaced));
        // An earlier pull request did not replace a later one.
        assert!(!may_point(
            true,
            &scope("mara", "main", "nightshift/1-r2", 0),
            &scope("alton", "main", "nightshift/1", 5)
        ));
        // Neither side may be an imported contribution.
        assert!(!may_point(
            true,
            &scope("mara", "main", "contrib/gh-42", 5),
            &replaced
        ));
        assert!(!may_point(
            true,
            &replacement,
            &scope("alton", "main", "contrib/gh-41", 0)
        ));
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
