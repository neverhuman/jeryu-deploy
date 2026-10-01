//! `GET /api/v1/repos/:id/pages?ref=`: every Markdown document in a commit,
//! at any depth, for a reader that shows a repository as a set of pages (the
//! wiki). The tree route lists one directory at a time; a wiki's navigation
//! needs the whole set at once.
//!
//! `ref` defaults to the default branch. The list is sorted by path and capped
//! at [`MAX_PAGES`]; `truncated` says when the cap cut it.

use super::source::{git_output, is_markdown_path, normalize_git_path, resolve_commit, source_ref};
use super::*;

const MAX_PAGES: usize = 5000;

#[derive(Debug, Serialize)]
pub(in crate::web) struct MarkdownPage {
    path: String,
    size_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(in crate::web) struct PagesResponse {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    pages: Vec<MarkdownPage>,
    truncated: bool,
}

pub(in crate::web) async fn repo_pages(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<SourceQuery>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    let ref_name = source_ref(&repo, &query).to_string();
    match markdown_pages(&state, &repo, &ref_name) {
        Ok((sha, pages, truncated)) => Json(PagesResponse {
            ref_name,
            sha,
            pages,
            truncated,
        })
        .into_response(),
        Err(response) => *response,
    }
}

fn markdown_pages(
    state: &WebState,
    repo: &Repository,
    ref_name: &str,
) -> SourceResult<(String, Vec<MarkdownPage>, bool)> {
    let bare = state
        .repo_manager
        .open_parts(&repo.owner, &repo.name)
        .map_err(|_| {
            Box::new(api_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "repository storage not found",
            ))
        })?;
    let commit = resolve_commit(state, &bare, ref_name)?;
    let out = git_output(state, &bare.path, &["ls-tree", "-r", "-z", "-l", &commit])?;
    let (pages, truncated) = parse_markdown_pages(&out);
    Ok((commit, pages, truncated))
}

/// Keep the Markdown blobs of an `ls-tree -r -z -l` listing, sorted by path.
fn parse_markdown_pages(out: &[u8]) -> (Vec<MarkdownPage>, bool) {
    let mut pages: Vec<MarkdownPage> = out
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            let text = String::from_utf8_lossy(record);
            let (meta, path) = text.split_once('\t')?;
            let parts: Vec<_> = meta.split_whitespace().collect();
            if parts.len() < 4 || parts[1] != "blob" || parts[0] == "120000" {
                return None;
            }
            if !is_markdown_path(path) || normalize_git_path(Some(path)).is_err() {
                return None;
            }
            Some(MarkdownPage {
                path: path.to_string(),
                size_bytes: parts[3].parse().ok(),
            })
        })
        .collect();
    pages.sort_by(|a, b| a.path.cmp(&b.path));
    let truncated = pages.len() > MAX_PAGES;
    pages.truncate(MAX_PAGES);
    (pages, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_markdown_blobs_at_any_depth_sorted() {
        let listing = [
            "100644 blob aaa      12\tguides/setup.md",
            "100644 blob bbb      30\tREADME.md",
            "100644 blob ccc       9\tscripts/lint.py",
            "120000 blob ddd       7\tlinked.md",
            "160000 commit eee       -\tvendor/sub",
            "100644 blob fff       5\tnotes/Deep/Page.MARKDOWN",
        ]
        .join("\0");
        let (pages, truncated) = parse_markdown_pages(listing.as_bytes());
        let paths: Vec<_> = pages.iter().map(|page| page.path.as_str()).collect();
        assert_eq!(
            paths,
            ["README.md", "guides/setup.md", "notes/Deep/Page.MARKDOWN"]
        );
        assert_eq!(pages[0].size_bytes, Some(30));
        assert!(!truncated);
    }
}
