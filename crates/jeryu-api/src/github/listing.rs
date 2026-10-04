//! The ordering and filtering query of the v3 list routes: `?state=`,
//! `?sort=`, `?direction=`, `?head=`, `?base=`.
//!
//! GitHub answers a value none of these accept with a 422 `Validation Failed`
//! naming the field, so a caller learns the vocabulary instead of silently
//! reading an unfiltered, arbitrarily ordered list. An agent asking "does my
//! PR already exist?" with `?head=acme:feature-x` must either get that PR or
//! be told the query was wrong — never the first page of everything.

use serde_json::json;

use crate::routes::Response;

use super::support::{docs_url, json_response, steering};

/// GitHub's `?direction=` sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Asc,
    Desc,
}

impl Direction {
    const ACCEPTED: &'static [&'static str] = &["asc", "desc"];

    fn parse(value: &str) -> Option<Self> {
        match value {
            "asc" => Some(Self::Asc),
            "desc" => Some(Self::Desc),
            _ => None,
        }
    }

    /// The opposite direction. `long-running` is ordered by age, which is the
    /// creation order read backwards, so it sorts by creation time with the
    /// caller's direction turned around.
    pub(super) fn flipped(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }

    /// Applies the direction to a comparison expressed ascending.
    pub(super) fn apply(self, ordering: std::cmp::Ordering) -> std::cmp::Ordering {
        match self {
            Self::Asc => ordering,
            Self::Desc => ordering.reverse(),
        }
    }
}

/// GitHub's `?sort=` for the pulls list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PullSort {
    Created,
    Updated,
    /// Comment count, as GitHub's "popularity".
    Popularity,
    /// How long the pull request has been open.
    LongRunning,
}

impl PullSort {
    const ACCEPTED: &'static [&'static str] = &["created", "updated", "popularity", "long-running"];

    fn parse(value: &str) -> Option<Self> {
        match value {
            "created" => Some(Self::Created),
            "updated" => Some(Self::Updated),
            "popularity" => Some(Self::Popularity),
            "long-running" => Some(Self::LongRunning),
            _ => None,
        }
    }
}

/// GitHub's `?sort=` for the issues list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IssueSort {
    Created,
    Updated,
    Comments,
}

impl IssueSort {
    const ACCEPTED: &'static [&'static str] = &["created", "updated", "comments"];

    fn parse(value: &str) -> Option<Self> {
        match value {
            "created" => Some(Self::Created),
            "updated" => Some(Self::Updated),
            "comments" => Some(Self::Comments),
            _ => None,
        }
    }
}

/// GitHub's `?state=` selector for the pulls and issues lists. Both render an
/// item as either `open` or `closed` (a merged PR is a `closed` sub-state with
/// `merged_at` set), so the three selectors map onto that rendered state:
/// `open` keeps open items, `closed` keeps closed/merged ones, and `all` keeps
/// everything. An absent `?state=` is GitHub's documented `open` default; any
/// other value is a 422 rather than a silent fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StateSelector {
    Open,
    Closed,
    All,
}

impl StateSelector {
    const ACCEPTED: &'static [&'static str] = &["open", "closed", "all"];

    fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "closed" => Some(Self::Closed),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// Whether an item whose GitHub-rendered `state` field is `github_state`
    /// (`open` or `closed`) belongs in the response for this selector.
    pub(super) fn keeps(self, github_state: &str) -> bool {
        match self {
            Self::All => true,
            Self::Open => github_state == "open",
            Self::Closed => github_state == "closed",
        }
    }
}

/// `GET /repos/{owner}/{repo}/pulls` query: which pull requests, in which
/// order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PullListQuery {
    pub(super) state: StateSelector,
    pub(super) sort: PullSort,
    pub(super) direction: Direction,
    /// `owner:branch` (or a bare branch) the pull request's head must name.
    pub(super) head: Option<String>,
    /// The base branch the pull request must target.
    pub(super) base: Option<String>,
}

impl PullListQuery {
    /// Parses the pulls-list query, or renders GitHub's 422 for the first
    /// field carrying a value the route does not accept.
    pub(super) fn from_query(query: &str) -> Result<Self, Response> {
        let mut parsed = Self {
            state: StateSelector::Open,
            sort: PullSort::Created,
            direction: Direction::Desc,
            head: None,
            base: None,
        };
        let mut explicit_direction = None;
        for (key, value) in pairs(query) {
            match key.as_str() {
                "state" => {
                    parsed.state = StateSelector::parse(&value).ok_or_else(|| {
                        invalid(PULL_RESOURCE, &key, &value, StateSelector::ACCEPTED)
                    })?;
                }
                "sort" => {
                    parsed.sort = PullSort::parse(&value)
                        .ok_or_else(|| invalid(PULL_RESOURCE, &key, &value, PullSort::ACCEPTED))?;
                }
                "direction" => {
                    explicit_direction = Some(Direction::parse(&value).ok_or_else(|| {
                        invalid(PULL_RESOURCE, &key, &value, Direction::ACCEPTED)
                    })?);
                }
                "head" => parsed.head = non_empty(value),
                "base" => parsed.base = non_empty(value),
                _ => {}
            }
        }
        // GitHub's default direction is `desc` for `created` (and for an absent
        // `?sort=`) and `asc` for every other sort. `long-running` orders by
        // age, the other face of creation time, so it takes the same `desc`
        // default: the pull request open the longest leads, which is the whole
        // point of asking for it.
        parsed.direction = explicit_direction.unwrap_or(match parsed.sort {
            PullSort::Created | PullSort::LongRunning => Direction::Desc,
            PullSort::Updated | PullSort::Popularity => Direction::Asc,
        });
        Ok(parsed)
    }
}

/// `GET /repos/{owner}/{repo}/issues` query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IssueListQuery {
    pub(super) state: StateSelector,
    pub(super) sort: IssueSort,
    pub(super) direction: Direction,
}

impl IssueListQuery {
    pub(super) fn from_query(query: &str) -> Result<Self, Response> {
        // Issues default to `created`/`desc` whatever the sort, as on GitHub.
        let mut parsed = Self {
            state: StateSelector::Open,
            sort: IssueSort::Created,
            direction: Direction::Desc,
        };
        for (key, value) in pairs(query) {
            match key.as_str() {
                "state" => {
                    parsed.state = StateSelector::parse(&value).ok_or_else(|| {
                        invalid(ISSUE_RESOURCE, &key, &value, StateSelector::ACCEPTED)
                    })?;
                }
                "sort" => {
                    parsed.sort = IssueSort::parse(&value).ok_or_else(|| {
                        invalid(ISSUE_RESOURCE, &key, &value, IssueSort::ACCEPTED)
                    })?;
                }
                "direction" => {
                    parsed.direction = Direction::parse(&value).ok_or_else(|| {
                        invalid(ISSUE_RESOURCE, &key, &value, Direction::ACCEPTED)
                    })?;
                }
                _ => {}
            }
        }
        Ok(parsed)
    }
}

/// `GET /repos/{owner}/{repo}/commits` ordering. A commit list has one
/// orderable field — the commit date the history itself is walked by — so
/// `?sort=` accepts only `created`, while `?direction=` flips the walk between
/// newest-first (the default) and oldest-first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CommitListQuery {
    pub(super) direction: Direction,
}

impl CommitListQuery {
    const SORT_ACCEPTED: &'static [&'static str] = &["created"];

    pub(super) fn from_query(query: &str) -> Result<Self, Response> {
        let mut parsed = Self {
            direction: Direction::Desc,
        };
        for (key, value) in pairs(query) {
            match key.as_str() {
                "sort" if value != "created" => {
                    return Err(invalid(COMMIT_RESOURCE, &key, &value, Self::SORT_ACCEPTED));
                }
                "direction" => {
                    parsed.direction = Direction::parse(&value).ok_or_else(|| {
                        invalid(COMMIT_RESOURCE, &key, &value, Direction::ACCEPTED)
                    })?;
                }
                _ => {}
            }
        }
        Ok(parsed)
    }
}

const PULL_RESOURCE: &str = "PullRequest";
const ISSUE_RESOURCE: &str = "Issue";
const COMMIT_RESOURCE: &str = "Commit";

/// GitHub's 422 for a list query field whose value is not one of the accepted
/// ones. The accepted set rides along in the error entry and the steering hint
/// so one reply is enough to fix the call.
fn invalid(resource: &str, field: &str, value: &str, accepted: &[&str]) -> Response {
    let accepted_list = accepted.join(", ");
    json_response(
        422,
        &json!({
            "message": "Validation Failed",
            "errors": [{
                "resource": resource,
                "field": field,
                "code": "invalid",
                "value": value,
                "accepted": accepted,
            }],
            "documentation_url": docs_url(),
            "jeryu_steering": steering(
                "jeryu.get_system_snapshot",
                &format!("?{field}= accepts {accepted_list}; retry with one of those or drop the parameter"),
            ),
        }),
    )
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// The query's `key=value` pairs with the value percent-decoded, so a
/// `?head=acme%3Afeature-x` reads the same as `?head=acme:feature-x`.
fn pairs(query: &str) -> impl Iterator<Item = (String, String)> + '_ {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key.to_owned(), percent_decode(value))
        })
}

/// Decodes `%XX` escapes and `+` in a query value; an incomplete or
/// non-hexadecimal escape is kept verbatim.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' => {
                let decoded = value
                    .get(index + 1..index + 3)
                    .filter(|hex| hex.chars().all(|c| c.is_ascii_hexdigit()))
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match decoded {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn body(response: &Response) -> Value {
        serde_json::from_str(&response.body).expect("json body")
    }

    #[test]
    fn a_bare_pulls_query_is_open_and_newest_first() {
        let parsed = PullListQuery::from_query("").expect("default query");
        assert_eq!(
            parsed,
            PullListQuery {
                state: StateSelector::Open,
                sort: PullSort::Created,
                direction: Direction::Desc,
                head: None,
                base: None,
            }
        );
    }

    #[test]
    fn pulls_read_state_sort_direction_head_and_base() {
        let parsed = PullListQuery::from_query(
            "state=all&sort=updated&direction=desc&head=acme%3Afeature-x&base=main",
        )
        .expect("query");
        assert_eq!(parsed.state, StateSelector::All);
        assert_eq!(parsed.sort, PullSort::Updated);
        assert_eq!(parsed.direction, Direction::Desc);
        assert_eq!(parsed.head.as_deref(), Some("acme:feature-x"));
        assert_eq!(parsed.base.as_deref(), Some("main"));
    }

    #[test]
    fn only_the_time_ordered_sorts_default_to_descending() {
        for (query, expected) in [
            ("sort=created", Direction::Desc),
            ("sort=long-running", Direction::Desc),
            ("sort=updated", Direction::Asc),
            ("sort=popularity", Direction::Asc),
            ("sort=popularity&direction=desc", Direction::Desc),
        ] {
            let parsed = PullListQuery::from_query(query).expect("query");
            assert_eq!(parsed.direction, expected, "{query}");
        }
    }

    #[test]
    fn an_unaccepted_value_is_a_422_naming_the_field_and_the_accepted_set() {
        for (query, field) in [
            ("sort=bogus", "sort"),
            ("direction=sideways", "direction"),
            ("state=bogus", "state"),
        ] {
            let response = PullListQuery::from_query(query).expect_err("422");
            assert_eq!(response.status, 422, "{query}");
            let body = body(&response);
            assert_eq!(body["message"], "Validation Failed");
            assert_eq!(body["errors"][0]["resource"], "PullRequest");
            assert_eq!(body["errors"][0]["field"], field);
            assert_eq!(body["errors"][0]["code"], "invalid");
            assert!(
                body["errors"][0]["accepted"].is_array(),
                "accepted set for {query}: {}",
                response.body
            );
            assert!(body["jeryu_steering"]["hint"].is_string());
        }
    }

    #[test]
    fn issue_and_commit_queries_reject_their_own_unaccepted_values() {
        let issue = IssueListQuery::from_query("sort=popularity").expect_err("422");
        assert_eq!(issue.status, 422);
        assert_eq!(body(&issue)["errors"][0]["resource"], "Issue");
        assert_eq!(
            IssueListQuery::from_query("sort=comments&direction=asc&state=closed")
                .expect("query")
                .sort,
            IssueSort::Comments
        );

        let commit = CommitListQuery::from_query("sort=popularity").expect_err("422");
        assert_eq!(commit.status, 422);
        assert_eq!(body(&commit)["errors"][0]["resource"], "Commit");
        assert_eq!(
            CommitListQuery::from_query("sha=main&direction=asc")
                .expect("query")
                .direction,
            Direction::Asc
        );
    }

    #[test]
    fn query_values_are_percent_decoded() {
        assert_eq!(percent_decode("acme%3Afeature-x"), "acme:feature-x");
        assert_eq!(percent_decode("feature%2Fx"), "feature/x");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn state_selectors_keep_the_rendered_state_they_name() {
        assert!(StateSelector::Open.keeps("open"));
        assert!(!StateSelector::Open.keeps("closed"));
        assert!(StateSelector::Closed.keeps("closed"));
        assert!(StateSelector::All.keeps("open") && StateSelector::All.keeps("closed"));
    }
}
