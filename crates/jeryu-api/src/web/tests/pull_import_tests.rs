//! The mirror pull request importer, driven against a fake GitHub API: a
//! contribution opened there becomes a contributed forge pull request, follows
//! a force-push, and has the forge's answer reported back and its GitHub pull
//! request closed when the forge settles it.

use super::*;

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::github_pull_import::{
    GithubApi, GithubPull, GithubPullState, GithubStatus, PullImport, PullImportTarget,
};
use crate::web::pull_import::{ImportAction, ImportOutcome};

const SLUG: &str = "acme-oss/widget";

/// The GitHub side, in memory: the pull requests a mirror holds, the branch it
/// publishes, and every call the importer made.
#[derive(Debug, Default)]
struct FakeGithub {
    pulls: Mutex<Vec<GithubPull>>,
    branch_head: Mutex<Option<String>>,
    statuses: Mutex<Vec<(String, GithubStatus)>>,
    comments: Mutex<Vec<(u64, String)>>,
    closed: Mutex<Vec<u64>>,
}

impl FakeGithub {
    fn with_pull(pull: GithubPull) -> Self {
        Self {
            pulls: Mutex::new(vec![pull]),
            ..Self::default()
        }
    }

    fn set_pull(&self, pull: GithubPull) {
        let mut pulls = self.pulls.lock().expect("fake github pulls");
        pulls.retain(|held| held.number != pull.number);
        pulls.push(pull);
    }

    fn comments(&self) -> Vec<(u64, String)> {
        self.comments.lock().expect("fake github comments").clone()
    }

    fn statuses(&self) -> Vec<(String, GithubStatus)> {
        self.statuses.lock().expect("fake github statuses").clone()
    }

    fn closed(&self) -> Vec<u64> {
        self.closed.lock().expect("fake github closed").clone()
    }
}

impl GithubApi for FakeGithub {
    fn list_open_pulls(&self, slug: &str) -> Result<Vec<GithubPull>, String> {
        assert_eq!(slug, SLUG, "the importer asked a mirror it was not given");
        Ok(self
            .pulls
            .lock()
            .expect("fake github pulls")
            .iter()
            .filter(|pull| pull.state == GithubPullState::Open)
            .cloned()
            .collect())
    }

    fn get_pull(&self, _slug: &str, number: u64) -> Result<GithubPull, String> {
        self.pulls
            .lock()
            .expect("fake github pulls")
            .iter()
            .find(|pull| pull.number == number)
            .cloned()
            .ok_or_else(|| format!("no pull request {number}"))
    }

    fn branch_head(&self, _slug: &str, _branch: &str) -> Result<Option<String>, String> {
        Ok(self.branch_head.lock().expect("fake github branch").clone())
    }

    fn set_commit_status(
        &self,
        _slug: &str,
        sha: &str,
        status: &GithubStatus,
    ) -> Result<(), String> {
        self.statuses
            .lock()
            .expect("fake github statuses")
            .push((sha.to_string(), status.clone()));
        Ok(())
    }

    fn comment(&self, _slug: &str, number: u64, body: &str) -> Result<(), String> {
        self.comments
            .lock()
            .expect("fake github comments")
            .push((number, body.to_string()));
        Ok(())
    }

    fn close_pull(&self, _slug: &str, number: u64) -> Result<(), String> {
        self.closed.lock().expect("fake github closed").push(number);
        self.pulls
            .lock()
            .expect("fake github pulls")
            .iter_mut()
            .filter(|pull| pull.number == number)
            .for_each(|pull| pull.state = GithubPullState::Closed);
        Ok(())
    }
}

fn git(args: &[&str], cwd: &Path) -> String {
    let output = crate::test_git::git_command()
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Octo Dev")
        .env("GIT_AUTHOR_EMAIL", "octo@example.com")
        .env("GIT_COMMITTER_NAME", "Octo Dev")
        .env("GIT_COMMITTER_EMAIL", "octo@example.com")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A forge repository with a real bare store, plus the bare repository that
/// stands in for its GitHub mirror, publishing pull request heads under
/// `refs/pull/<number>/head` exactly as GitHub does.
struct Fixture {
    core: ForgeCore,
    repo: jeryu_core::Repository,
    mirror: PathBuf,
    work: PathBuf,
    storage: PathBuf,
}

fn fixture(storage: &Path) -> Fixture {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "acme",
            CreateRepositoryRequest {
                name: "widget".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .expect("create the forge repository");
    build_bare_repo_with_main_and_feature(
        storage,
        "acme",
        "widget",
        "src/widget.rs",
        "pub fn widget() -> u8 { 1 }\n",
    );
    let forge_bare = storage.join("acme").join("widget.git");
    let mirror = storage.join("widget-mirror.git");
    git(
        &[
            "clone",
            "--quiet",
            "--bare",
            forge_bare.to_str().expect("bare utf8"),
            mirror.to_str().expect("mirror utf8"),
        ],
        storage,
    );
    let work = storage.join("contributor");
    git(
        &[
            "clone",
            "--quiet",
            mirror.to_str().expect("mirror utf8"),
            work.to_str().expect("work utf8"),
        ],
        storage,
    );
    Fixture {
        core,
        repo,
        mirror,
        work,
        storage: storage.to_path_buf(),
    }
}

impl Fixture {
    /// Write a contribution in the contributor's clone and publish it on the
    /// mirror as `refs/pull/<number>/head`, the ref GitHub exposes whether the
    /// pull request came from a fork or a branch.
    fn publish_contribution(&self, number: u64, contents: &str) -> String {
        git(
            &["checkout", "--quiet", "-B", "contribution", "main"],
            &self.work,
        );
        std::fs::write(self.work.join("CONTRIBUTION.txt"), contents).expect("write contribution");
        git(&["add", "CONTRIBUTION.txt"], &self.work);
        git(
            &["commit", "--quiet", "-m", "add a contribution"],
            &self.work,
        );
        let sha = git(&["rev-parse", "HEAD"], &self.work);
        git(
            &[
                "push",
                "--quiet",
                "--force",
                self.mirror.to_str().expect("mirror utf8"),
                &format!("HEAD:refs/pull/{number}/head"),
            ],
            &self.work,
        );
        sha
    }

    fn publish_two_commit_contribution(&self, number: u64) -> String {
        git(
            &["checkout", "--quiet", "-B", "contribution", "main"],
            &self.work,
        );
        std::fs::write(self.work.join("CONTRIBUTION.txt"), "first change\n")
            .expect("write first contribution");
        git(&["add", "CONTRIBUTION.txt"], &self.work);
        git(
            &["commit", "--quiet", "-m", "add first contribution"],
            &self.work,
        );
        let second = self.work.join("src/second.rs");
        std::fs::create_dir_all(second.parent().expect("second parent"))
            .expect("create second parent");
        std::fs::write(second, "pub fn second() -> u8 { 2 }\n").expect("write second contribution");
        git(&["add", "src/second.rs"], &self.work);
        git(
            &["commit", "--quiet", "-m", "add second contribution"],
            &self.work,
        );
        let sha = git(&["rev-parse", "HEAD"], &self.work);
        git(
            &[
                "push",
                "--quiet",
                "--force",
                self.mirror.to_str().expect("mirror utf8"),
                &format!("HEAD:refs/pull/{number}/head"),
            ],
            &self.work,
        );
        sha
    }

    fn state(&self, github: Arc<FakeGithub>) -> Arc<WebState> {
        let import = PullImport::with_targets(BTreeMap::from([(
            "acme/widget".to_string(),
            PullImportTarget {
                github_slug: SLUG.to_string(),
                branch: "main".to_string(),
                fetch_override: Some(self.mirror.to_string_lossy().to_string()),
            },
        )]));
        Arc::new(
            WebState::new_with_git_storage(self.core.clone(), self.storage.clone())
                .with_pull_trust(&["importbot"], &["maintainer"])
                .with_test_pull_import(import, github),
        )
    }
}

fn open_pull(number: u64, head_sha: &str, author: &str, title: &str) -> GithubPull {
    GithubPull {
        number,
        title: title.to_string(),
        body: Some("Please take this.".to_string()),
        author: author.to_string(),
        base_ref: "main".to_string(),
        head_sha: head_sha.to_string(),
        state: GithubPullState::Open,
    }
}

/// The last thing a pass had to say about one GitHub pull request: a settled
/// contribution is first seen by the import step and then by the settle step.
fn imported(outcomes: &[ImportOutcome], number: u64) -> &ImportOutcome {
    outcomes
        .iter()
        .rev()
        .find(|outcome| outcome.github_number == number)
        .unwrap_or_else(|| panic!("no outcome for GitHub pull request {number}: {outcomes:?}"))
}

/// A pull request opened on a mirror becomes a forge pull request on
/// `contrib/gh-<number>`, marked contributed, with the contributor's commit
/// and author intact and the forge's gate reported back.
#[tokio::test]
async fn a_mirror_pull_request_is_imported_as_a_contributed_forge_pull_request() {
    let storage = tempdir().expect("git storage dir");
    let fixture = fixture(storage.path());
    let head = fixture.publish_contribution(7, "a contribution\n");
    let github = Arc::new(FakeGithub::with_pull(open_pull(
        7,
        &head,
        "octo-dev",
        "add a contribution",
    )));
    let state = fixture.state(github.clone());

    let outcomes = crate::web::pull_import::import_once(&state);
    let outcome = imported(&outcomes, 7);
    assert_eq!(outcome.action, ImportAction::Opened);
    assert_eq!(outcome.detail, None);

    let number = outcome.forge_number.expect("a forge pull request number");
    let pr = fixture
        .core
        .get_pull_request("acme", "widget", number)
        .expect("the imported pull request");
    assert_eq!(pr.head.ref_name, "contrib/gh-7");
    assert_eq!(pr.head.sha, head);
    assert_eq!(pr.base.ref_name, "main");
    assert_eq!(pr.author, "gh-octo-dev");
    assert_eq!(pr.title, "add a contribution");
    assert!(
        pr.body
            .as_deref()
            .unwrap_or_default()
            .contains(&format!("https://github.com/{SLUG}/pull/7")),
        "the forge pull request links the GitHub one: {:?}",
        pr.body
    );
    assert_eq!(pr.changed_files, vec!["CONTRIBUTION.txt".to_string()]);

    // The commits are fetched, never rewritten, so the contributor is still
    // the author of the commit the forge now holds.
    let bare = storage.path().join("acme").join("widget.git");
    assert_eq!(
        git(
            &["log", "-1", "--format=%an <%ae>", "refs/heads/contrib/gh-7"],
            &bare
        ),
        "Octo Dev <octo@example.com>"
    );

    // It is held for a maintainer, by the marking the importer wrote.
    let detail = response_json(
        crate::web::pulls::detail(
            State(state.clone()),
            authenticated_account("bob"),
            AxumPath((fixture.repo.id.to_string(), number)),
        )
        .await,
    )
    .await;
    assert_eq!(detail["merge_passport"]["status"], "blocked");
    assert_eq!(detail["summary"]["labels"][0], "origin:contributed");

    // GitHub hears the gate's answer and the review posture.
    let statuses = github.statuses();
    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[0].0, head);
    assert_eq!(statuses[0].1.context, "jeryu/forge-gate");
    assert_eq!(statuses[0].1.state.wire_name(), "pending");
    assert_eq!(
        statuses[0].1.description,
        "waiting for a maintainer to say ok-to-test"
    );
    assert_eq!(statuses[1].1.context, "jeryu/dco");
    let comments = github.comments();
    assert_eq!(comments.len(), 1);
    assert!(
        comments[0].1.contains("held for a maintainer"),
        "the comment states the hold: {}",
        comments[0].1
    );

    // A second pass with nothing new changes nothing and says nothing twice.
    let again = crate::web::pull_import::import_once(&state);
    assert_eq!(imported(&again, 7).action, ImportAction::Unchanged);
    assert_eq!(github.statuses().len(), 2);
    assert_eq!(github.comments().len(), 1);
}

/// A mirror PR carries only branch names from GitHub. The imported forge PR
/// stores the resolved base commit so diff and commit reads stay on the pull
/// request range instead of expanding to the head branch's whole history.
#[tokio::test]
async fn an_imported_pull_request_records_a_real_base_for_diff_and_commits() {
    let storage = tempdir().expect("git storage dir");
    let fixture = fixture(storage.path());
    let base = bare_ref(storage.path(), "acme", "widget", "refs/heads/main");
    let head = fixture.publish_two_commit_contribution(7);
    let github = Arc::new(FakeGithub::with_pull(open_pull(
        7,
        &head,
        "octo-dev",
        "add two contribution commits",
    )));
    let state = fixture.state(github);

    let outcomes = crate::web::pull_import::import_once(&state);
    let number = imported(&outcomes, 7)
        .forge_number
        .expect("a forge pull request number");
    let pr = fixture
        .core
        .get_pull_request("acme", "widget", number)
        .expect("the imported pull request");
    assert_eq!(pr.base.ref_name, "main");
    assert_eq!(pr.base.sha, base);
    assert!(
        crate::github::pulls::is_hex40(&pr.base.sha),
        "base sha must be a 40-hex oid, got {}",
        pr.base.sha
    );

    let bare = storage.path().join("acme").join("widget.git");
    let expected_files: Vec<String> =
        git(&["diff", "--name-only", &format!("{base}...{head}")], &bare)
            .lines()
            .map(str::to_string)
            .collect();
    assert_eq!(pr.changed_files, expected_files);

    let diff = response_json(
        crate::web::pulls::diff(
            State(state.clone()),
            authenticated_account("bob"),
            AxumPath((fixture.repo.id.to_string(), number)),
        )
        .await,
    )
    .await;
    let diff_files: Vec<String> = diff["files"]
        .as_array()
        .expect("diff files")
        .iter()
        .map(|file| file["path"].as_str().expect("diff path").to_string())
        .collect();
    assert_eq!(diff["base_sha"], base);
    assert_eq!(diff_files, expected_files);

    let expected_count = git(&["rev-list", "--count", &format!("{base}..{head}")], &bare)
        .parse::<u64>()
        .expect("git count");
    let pulled = state
        .github
        .get(&format!("/repos/acme/widget/pulls/{number}"));
    assert_eq!(pulled.status, 200, "pull read: {}", pulled.body);
    let pulled: serde_json::Value = serde_json::from_str(&pulled.body).expect("pull read body");
    assert_eq!(pulled["base"]["sha"], base);
    assert_eq!(pulled["commits"], expected_count);
}

/// A contributor who force-pushes on GitHub moves the forge pull request's
/// head to the new commits instead of leaving it on the old ones.
#[tokio::test]
async fn a_force_push_on_github_moves_the_forge_pull_request_head() {
    let storage = tempdir().expect("git storage dir");
    let fixture = fixture(storage.path());
    let first = fixture.publish_contribution(7, "first try\n");
    let github = Arc::new(FakeGithub::with_pull(open_pull(
        7,
        &first,
        "octo-dev",
        "add a contribution",
    )));
    let state = fixture.state(github.clone());
    let opened = crate::web::pull_import::import_once(&state);
    let number = imported(&opened, 7)
        .forge_number
        .expect("a forge pull request number");

    // The contributor rewrites the branch and force-pushes it.
    git(&["reset", "--quiet", "--hard", "main"], &fixture.work);
    let second = fixture.publish_contribution(7, "second try, rewritten\n");
    assert_ne!(first, second);
    github.set_pull(open_pull(
        7,
        &second,
        "octo-dev",
        "add a contribution, again",
    ));

    let outcomes = crate::web::pull_import::import_once(&state);
    assert_eq!(imported(&outcomes, 7).action, ImportAction::Updated);
    assert_eq!(imported(&outcomes, 7).forge_number, Some(number));
    let pr = fixture
        .core
        .get_pull_request("acme", "widget", number)
        .expect("the imported pull request");
    assert_eq!(pr.head.sha, second);
    assert_eq!(pr.title, "add a contribution, again");
    assert_eq!(
        bare_ref(storage.path(), "acme", "widget", "refs/heads/contrib/gh-7"),
        second,
        "the contribution branch follows the force-push"
    );
}

/// When the forge merges the contribution and the mirror push carries the
/// commits to GitHub, the GitHub pull request is told the sha and closed.
#[tokio::test]
async fn a_merged_contribution_closes_its_github_pull_request_with_the_sha() {
    let storage = tempdir().expect("git storage dir");
    let fixture = fixture(storage.path());
    let head = fixture.publish_contribution(7, "a contribution\n");
    let github = Arc::new(FakeGithub::with_pull(open_pull(
        7,
        &head,
        "octo-dev",
        "add a contribution",
    )));
    let state = fixture.state(github.clone());
    let number = imported(&crate::web::pull_import::import_once(&state), 7)
        .forge_number
        .expect("a forge pull request number");

    // The forge merges it: the merge commit is a descendant of the head, which
    // is what a fast-forward of the contributor's commits looks like.
    fixture
        .core
        .finalize_merge("acme", "widget", number, head.clone(), None)
        .expect("merge the forge pull request");

    // Before the mirror push, GitHub is not told anything: the commits are not
    // there yet, so there is no sha to point at.
    let waiting = crate::web::pull_import::import_once(&state);
    assert!(
        waiting
            .iter()
            .all(|outcome| outcome.action != ImportAction::ClosedAsMerged),
        "nothing is closed before the mirror push lands: {waiting:?}"
    );
    assert!(github.closed().is_empty());

    // The mirror push lands the merge on the GitHub branch.
    *github.branch_head.lock().expect("fake github branch") = Some(head.clone());
    let outcomes = crate::web::pull_import::import_once(&state);
    let outcome = imported(&outcomes, 7);
    assert_eq!(outcome.action, ImportAction::ClosedAsMerged);
    assert_eq!(outcome.detail, None);
    assert_eq!(github.closed(), vec![7]);
    let last = github.comments().pop().expect("a closing comment").1;
    assert!(
        last.contains(&format!("merged as {head}")),
        "the closing comment names the sha: {last}"
    );

    // Settled once: a later pass has nothing left to say.
    let after = crate::web::pull_import::import_once(&state);
    assert!(after.iter().all(|outcome| outcome.github_number != 7));
    assert_eq!(github.closed(), vec![7]);
}

/// A contribution the forge closes without merging has its GitHub pull request
/// closed with the reason.
#[tokio::test]
async fn a_contribution_closed_on_the_forge_closes_on_github_with_the_reason() {
    let storage = tempdir().expect("git storage dir");
    let fixture = fixture(storage.path());
    let head = fixture.publish_contribution(7, "a contribution\n");
    let github = Arc::new(FakeGithub::with_pull(open_pull(
        7,
        &head,
        "octo-dev",
        "add a contribution",
    )));
    let state = fixture.state(github.clone());
    let number = imported(&crate::web::pull_import::import_once(&state), 7)
        .forge_number
        .expect("a forge pull request number");

    fixture
        .core
        .create_review(
            "acme",
            "widget",
            number,
            "maintainer",
            CreateReviewRequest {
                body: Some("this belongs in another repository".to_string()),
                event: ReviewState::ChangesRequested,
                comments: Vec::new(),
                expected_head_sha: Some(head.clone()),
            },
        )
        .expect("request changes");
    fixture
        .core
        .update_pull_request(
            "acme",
            "widget",
            number,
            jeryu_core::UpdatePullRequestRequest {
                state: Some(jeryu_core::PullRequestState::Closed),
                ..jeryu_core::UpdatePullRequestRequest::default()
            },
        )
        .expect("close the forge pull request");

    let outcomes = crate::web::pull_import::import_once(&state);
    assert_eq!(imported(&outcomes, 7).action, ImportAction::ClosedOnForge);
    assert_eq!(github.closed(), vec![7]);
    let last = github.comments().pop().expect("a closing comment").1;
    assert!(
        last.contains("closed on the forge without merging")
            && last.contains("maintainer asked for changes"),
        "the closing comment carries the reason: {last}"
    );
}

/// Dependabot takes the same path as anybody else: its pull request is
/// imported, marked contributed and held for a maintainer.
#[tokio::test]
async fn a_dependabot_pull_request_is_imported_like_any_other_contribution() {
    let storage = tempdir().expect("git storage dir");
    let fixture = fixture(storage.path());
    let head = fixture.publish_contribution(12, "bump the pinned tool\n");
    let github = Arc::new(FakeGithub::with_pull(open_pull(
        12,
        &head,
        "dependabot[bot]",
        "build(deps): bump widget from 1.0 to 1.1",
    )));
    let state = fixture.state(github.clone());

    let outcomes = crate::web::pull_import::import_once(&state);
    let outcome = imported(&outcomes, 12);
    assert_eq!(outcome.action, ImportAction::Opened);
    let number = outcome.forge_number.expect("a forge pull request number");
    let pr = fixture
        .core
        .get_pull_request("acme", "widget", number)
        .expect("the imported pull request");
    assert_eq!(pr.head.ref_name, "contrib/gh-12");
    assert_eq!(pr.author, "gh-dependabot-bot");
    let origin = response_json(
        crate::web::pull_origin::show(
            State(state.clone()),
            AxumPath((fixture.repo.id.to_string(), number)),
        )
        .await,
    )
    .await;
    assert_eq!(origin["origin"], "contributed");
    assert_eq!(origin["maintainer_approved"], false);
    assert_eq!(
        origin["merge_hold"],
        "contributed pull request without a maintainer's ok-to-test on this head"
    );
}

/// A pull request on a repository nobody enrolled is ignored: nothing is
/// fetched, no forge pull request is opened, and GitHub is not written to.
#[tokio::test]
async fn a_pull_request_on_an_unconfigured_repository_is_ignored() {
    let storage = tempdir().expect("git storage dir");
    let fixture = fixture(storage.path());
    let head = fixture.publish_contribution(3, "a contribution\n");
    let github = Arc::new(FakeGithub::with_pull(open_pull(
        3,
        &head,
        "octo-dev",
        "add a contribution",
    )));
    // The forge imports acme/widget from acme-oss/widget; the event names a
    // mirror of a repository that is not enrolled at all.
    let state = fixture.state(github.clone());
    let event = crate::web::pull_import::events(
        State(state.clone()),
        authenticated_account("importbot"),
        axum::body::Bytes::from(
            serde_json::json!({
                "action": "opened",
                "number": 3,
                "repository": { "full_name": "globex/gadget" },
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(event.status(), StatusCode::OK);
    let body = response_json(event).await;
    assert_eq!(body["status"], "ignored");
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("globex/gadget"),
        "the answer names the repository it ignored: {body}"
    );
    assert!(
        fixture
            .core
            .list_pull_requests("acme", "widget", None)
            .expect("list")
            .is_empty()
    );
    assert!(github.statuses().is_empty() && github.comments().is_empty());

    // The enrolled mirror's own event is imported through the same route.
    let event = crate::web::pull_import::events(
        State(state.clone()),
        authenticated_account("importbot"),
        axum::body::Bytes::from(
            serde_json::json!({
                "action": "opened",
                "number": 3,
                "repository": { "full_name": SLUG },
            })
            .to_string(),
        ),
    )
    .await;
    let body = response_json(event).await;
    assert_eq!(body["status"], "imported");
    assert_eq!(body["outcome"]["action"], "opened");
}

/// Only an importer identity may deliver an event; a contributor who finds the
/// route cannot make the forge import on their say-so.
#[tokio::test]
async fn an_event_from_anybody_else_is_refused() {
    let storage = tempdir().expect("git storage dir");
    let fixture = fixture(storage.path());
    let github = Arc::new(FakeGithub::default());
    let state = fixture.state(github.clone());
    let refused = crate::web::pull_import::events(
        State(state.clone()),
        authenticated_account("octo-dev"),
        axum::body::Bytes::from(
            serde_json::json!({ "number": 1, "repository": { "full_name": SLUG } }).to_string(),
        ),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let body = response_json(refused).await;
    assert_eq!(body["code"], "pull_import_importer_required");
}
