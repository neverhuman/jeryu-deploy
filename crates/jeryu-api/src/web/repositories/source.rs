//! Bounded Git source browsing and README mutation handlers.

use super::*;

pub(super) fn source_ref<'a>(repo: &'a Repository, query: &'a SourceQuery) -> &'a str {
    query
        .ref_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&repo.default_branch)
}

pub(super) fn required_param(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

/// A blob read without a path or ref must not guess: an empty or README
/// answer reads as a real file to a caller that typoed a parameter.
pub(super) fn missing_param_error(name: &str) -> AxumResponse {
    api_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_input",
        &format!("{name} is required"),
    )
}

pub(super) fn normalize_git_path(path: Option<&str>) -> SourceResult<String> {
    let raw = path.unwrap_or("");
    if raw.starts_with('/') || raw.contains('\0') {
        return Err(Box::new(api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "path may not be absolute or contain NUL bytes",
        )));
    }
    let path = raw.trim_matches('/');
    if path.split('/').any(|part| matches!(part, "." | "..")) {
        return Err(Box::new(api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "path may not contain . or .. segments",
        )));
    }
    Ok(path.to_string())
}

fn git_tree(
    state: &WebState,
    repo: &Repository,
    ref_name: &str,
    path: Option<&str>,
) -> SourceResult<Vec<TreeEntry>> {
    let path = normalize_git_path(path)?;
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
    let spec = if path.is_empty() {
        commit
    } else {
        format!("{commit}:{path}")
    };
    // Same for a directory that is not in the commit: 404, not 500.
    if !path.is_empty() {
        let kind = git_lookup(state, &bare.path, &["cat-file", "-t", &spec])?
            .map(|out| String::from_utf8_lossy(&out).trim().to_string());
        if kind.as_deref() != Some("tree") {
            return Err(Box::new(api_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "directory not found at this ref",
            )));
        }
    }
    let out = git_output(state, &bare.path, &["ls-tree", "-z", "-l", &spec])?;
    let mut entries = Vec::new();
    for record in out
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let text = String::from_utf8_lossy(record);
        let Some((meta, name)) = text.split_once('\t') else {
            continue;
        };
        let parts: Vec<_> = meta.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let mode = parts[0];
        let object_kind = parts[1];
        let sha = parts[2].to_string();
        let size_bytes = parts[3].parse::<u64>().ok();
        let kind = match (mode, object_kind) {
            ("160000", _) => TreeEntryKind::Submodule,
            ("120000", _) => TreeEntryKind::Symlink,
            (_, "tree") => TreeEntryKind::Directory,
            _ => TreeEntryKind::File,
        };
        let full_path = if path.is_empty() {
            name.to_string()
        } else {
            format!("{path}/{name}")
        };
        entries.push(TreeEntry {
            path: full_path,
            name: name.to_string(),
            kind,
            sha,
            size_bytes,
            last_commit_sha: None,
            last_commit_message: None,
            last_commit_at: None,
        });
    }
    entries.sort_by(|a, b| {
        let ak = if a.kind == TreeEntryKind::Directory {
            0
        } else {
            1
        };
        let bk = if b.kind == TreeEntryKind::Directory {
            0
        } else {
            1
        };
        ak.cmp(&bk).then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

fn git_blob(
    state: &WebState,
    repo: &Repository,
    ref_name: &str,
    path: &str,
    render: Option<&str>,
) -> SourceResult<BlobResponse> {
    let (bytes, mime, sha) = git_blob_bytes_inner(state, repo, ref_name, path)?;
    let text = if bytes.contains(&0) {
        None
    } else {
        String::from_utf8(bytes.clone()).ok()
    };
    let is_binary = text.is_none();
    let rendered_markdown = text
        .as_deref()
        .filter(|_| render == Some("html") && is_markdown_path(path))
        .map(|text| render_repo_markdown(state, repo, ref_name, path, text));
    Ok(BlobResponse {
        repo: repo_id(repo),
        path: normalize_git_path(Some(path))?,
        ref_name: ref_name.to_string(),
        sha,
        size_bytes: bytes.len() as u64,
        mime: mime.to_string(),
        encoding: if is_binary {
            BlobEncoding::Base64
        } else {
            BlobEncoding::Utf8
        },
        text,
        base64: if is_binary {
            Some(STANDARD.encode(&bytes))
        } else {
            None
        },
        rendered_markdown,
        is_binary,
    })
}

pub(super) fn git_blob_bytes(
    state: &WebState,
    repo: &Repository,
    ref_name: &str,
    path: &str,
) -> SourceResult<(Vec<u8>, &'static str)> {
    let (bytes, mime, _) = git_blob_bytes_inner(state, repo, ref_name, path)?;
    Ok((bytes, mime))
}

fn git_blob_bytes_inner(
    state: &WebState,
    repo: &Repository,
    ref_name: &str,
    path: &str,
) -> SourceResult<(Vec<u8>, &'static str, String)> {
    let path = normalize_git_path(Some(path))?;
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
    let spec = format!("{commit}:{path}");
    // A path that is not in the commit makes `rev-parse` exit non-zero
    // ("Needed a single revision"). That is the caller naming a file that is
    // not there, a 404, not a fault of the server: a link to a generated file
    // that is on disk but never committed answered 500 before.
    let sha = git_lookup(
        state,
        &bare.path,
        &["rev-parse", "--verify", "--quiet", &spec],
    )?
    .map(|out| String::from_utf8_lossy(&out).trim().to_string())
    .filter(|sha| !sha.is_empty())
    .ok_or_else(|| {
        Box::new(api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "file not found at this ref",
        ))
    })?;
    let kind = git_output(state, &bare.path, &["cat-file", "-t", &sha])?;
    if String::from_utf8_lossy(&kind).trim() != "blob" {
        return Err(Box::new(api_error(
            StatusCode::NOT_FOUND,
            "not_a_file",
            "the path names a directory, not a file; list it with the tree route",
        )));
    }
    let size = git_object_size(state, &bare.path, &sha)?;
    let limit = preview_limit_for_path(&path);
    if size > limit {
        return Err(Box::new(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "blob_too_large",
            "file is too large to preview",
        )));
    }
    let bytes = git_output(state, &bare.path, &["cat-file", "-p", &sha])?;
    let mime = mime_for_repo_file(&path, &bytes);
    Ok((bytes, mime, sha))
}

/// Render a repository Markdown file with its relative URLs resolved: images
/// to the path-form raw file beside it, links to the web app's blob or tree
/// view (see `markdown::MarkdownBase`).
fn render_repo_markdown(
    state: &WebState,
    repo: &Repository,
    ref_name: &str,
    path: &str,
    text: &str,
) -> RenderedMarkdown {
    let commit = state
        .repo_manager
        .open_parts(&repo.owner, &repo.name)
        .ok()
        .and_then(|bare| {
            resolve_commit(state, &bare, ref_name)
                .ok()
                .map(|c| (bare, c))
        });
    let lookups = std::cell::Cell::new(0_u32);
    let is_dir = |candidate: &str| -> bool {
        // Bounded: a README with hundreds of links costs at most this many
        // `cat-file` runs; past it a link without a trailing `/` is a file.
        lookups.set(lookups.get() + 1);
        let Some((bare, commit)) = commit.as_ref().filter(|_| lookups.get() <= 64) else {
            return false;
        };
        git_lookup(
            state,
            &bare.path,
            &["cat-file", "-t", &format!("{commit}:{candidate}")],
        )
        .ok()
        .flatten()
        .is_some_and(|kind| String::from_utf8_lossy(&kind).trim() == "tree")
    };
    let repo_id = repo.id.to_string();
    let web_root = format!(
        "/repos/jeryu/{}/{}",
        encode_segment(&repo.owner),
        encode_segment(&repo.name)
    );
    let dir = path
        .trim_matches('/')
        .rsplit_once('/')
        .map_or("", |(dir, _)| dir);
    let base = MarkdownBase {
        repo_id: &repo_id,
        web_root: &web_root,
        ref_name,
        dir,
        is_dir: &is_dir,
    };
    render_markdown_in(text, Some(&base))
}

/// The README of a directory (`None` or empty: the repository root).
fn git_readme_path(
    state: &WebState,
    repo: &Repository,
    ref_name: &str,
    dir: Option<&str>,
) -> SourceResult<Option<String>> {
    let entries = git_tree(state, repo, ref_name, dir)?;
    let mut candidates: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.kind == TreeEntryKind::File)
        .filter(|entry| entry.name.to_ascii_lowercase().starts_with("readme"))
        .map(|entry| entry.path)
        .collect();
    candidates.sort_by_key(|path| {
        if path.eq_ignore_ascii_case("README.md") {
            0
        } else {
            1
        }
    });
    Ok(candidates.into_iter().next())
}

pub(super) fn resolve_commit(
    state: &WebState,
    bare: &jeryu_gitd::repo::Repository,
    ref_name: &str,
) -> SourceResult<String> {
    RefService::new((*state.repo_manager).clone())
        .resolve_commit(bare, ref_name)
        .map_err(|err| Box::new(git_source_response("resolve ref", &err.to_string())))?
        .ok_or_else(|| {
            Box::new(api_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "ref not found",
            ))
        })
}

pub(super) fn git_output(
    state: &WebState,
    cwd: &std::path::Path,
    args: &[&str],
) -> SourceResult<Vec<u8>> {
    let out = Command::new(&state.repo_manager.config().git_bin)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|err| Box::new(git_source_response("run git", &err.to_string())))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(Box::new(git_source_response("run git", stderr.trim())))
    }
}

/// Like [`git_output`] for a lookup that may legitimately find nothing:
/// `Ok(None)` when git exits non-zero, an error only when git cannot be run.
pub(super) fn git_lookup(
    state: &WebState,
    cwd: &std::path::Path,
    args: &[&str],
) -> SourceResult<Option<Vec<u8>>> {
    let out = Command::new(&state.repo_manager.config().git_bin)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|err| Box::new(git_source_response("run git", &err.to_string())))?;
    Ok(out.status.success().then_some(out.stdout))
}

pub(super) fn git_object_size(
    state: &WebState,
    cwd: &std::path::Path,
    sha: &str,
) -> SourceResult<u64> {
    let out = git_output(state, cwd, &["cat-file", "-s", sha])?;
    let text = String::from_utf8_lossy(&out);
    text.trim()
        .parse::<u64>()
        .map_err(|err| Box::new(git_source_response("read object size", &err.to_string())))
}

fn git_source_error(context: &str, err: impl std::fmt::Display) -> AxumResponse {
    git_source_response(context, &err.to_string())
}

fn git_source_response(context: &str, detail: &str) -> AxumResponse {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "git_source_failed",
        &format!("{context}: {detail}"),
    )
}

pub(super) fn is_markdown_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

fn preview_limit_for_path(path: &str) -> u64 {
    if is_markdown_path(path)
        && path
            .rsplit('/')
            .next()
            .is_some_and(|name| name.to_ascii_lowercase().starts_with("readme"))
    {
        MAX_README_PREVIEW_BYTES
    } else {
        MAX_BLOB_PREVIEW_BYTES
    }
}

/// The media type a repository file is served with, by `/blob`'s `mime` and
/// by both raw forms. Names come from the shared table; repository content
/// differs from the SPA's own files in two ways:
///
/// - JavaScript is `text/plain`, never `text/javascript`. With `nosniff` a
///   browser then refuses it as a `<script>` on any page of this origin, the
///   SPA included, whose `script-src 'self'` would otherwise admit a
///   committed file. The raw CSP cannot do that job: a response's CSP governs
///   the document it is, not the page that loads it as a script.
/// - Anything the table does not know is `text/plain` when it is text, and
///   `application/octet-stream` when it is binary (a NUL byte, or not UTF-8),
///   so an unknown image or archive is offered as bytes instead of garbage.
///
/// HTML and SVG keep their real types; the raw CSP keeps them inert.
pub(super) fn mime_for_repo_file(path: &str, bytes: &[u8]) -> &'static str {
    match super::super::media_types::extension_of(path) {
        Some(ext) if matches!(ext.to_ascii_lowercase().as_str(), "js" | "mjs" | "cjs") => {
            return "text/plain; charset=utf-8";
        }
        Some(ext) => {
            if let Some(mime) = super::super::media_types::by_extension(ext) {
                return mime;
            }
        }
        None => {}
    }
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    }
}

pub(in crate::web) async fn repo_refs(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return api_error(
            axum::http::StatusCode::NOT_FOUND,
            "not_found",
            "repository not found",
        );
    };
    let bare = match state.repo_manager.open_parts(&repo.owner, &repo.name) {
        Ok(bare) => bare,
        Err(_) => {
            return Json(vec![RefSelectorItem {
                name: repo.default_branch.clone(),
                sha: "unknown".to_string(),
                kind: RefKind::Branch,
                protected: state
                    .github
                    .core()
                    .get_branch_protection(&repo.owner, &repo.name, &repo.default_branch)
                    .is_ok(),
            }])
            .into_response();
        }
    };
    let refs = match RefService::new((*state.repo_manager).clone()).list_refs(&bare) {
        Ok(refs) => refs,
        Err(err) => return git_source_error("list refs", err),
    };
    let mut items = Vec::new();
    for item in refs {
        if let Some(name) = item.name.strip_prefix("refs/heads/") {
            items.push(RefSelectorItem {
                name: name.to_string(),
                sha: item.oid,
                kind: RefKind::Branch,
                protected: state
                    .github
                    .core()
                    .get_branch_protection(&repo.owner, &repo.name, name)
                    .is_ok(),
            });
        } else if let Some(name) = item.name.strip_prefix("refs/tags/") {
            items.push(RefSelectorItem {
                name: name.to_string(),
                sha: item.oid,
                kind: RefKind::Tag,
                protected: false,
            });
        }
    }
    if items.is_empty() {
        items.push(RefSelectorItem {
            name: repo.default_branch.clone(),
            sha: "unknown".to_string(),
            kind: RefKind::Branch,
            protected: state
                .github
                .core()
                .get_branch_protection(&repo.owner, &repo.name, &repo.default_branch)
                .is_ok(),
        });
    }
    Json(items).into_response()
}

pub(in crate::web) async fn repo_tree(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<SourceQuery>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    match git_tree(
        &state,
        &repo,
        source_ref(&repo, &query),
        query.path.as_deref(),
    ) {
        Ok(entries) => Json(entries).into_response(),
        Err(response) => *response,
    }
}

pub(in crate::web) async fn repo_blob(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<SourceQuery>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    let Some(path) = required_param(query.path.as_deref()) else {
        return missing_param_error("path");
    };
    let Some(ref_name) = required_param(query.ref_name.as_deref()) else {
        return missing_param_error("ref");
    };
    match git_blob(&state, &repo, ref_name, path, query.render.as_deref()) {
        Ok(blob) => Json(blob).into_response(),
        Err(response) => *response,
    }
}

pub(in crate::web) async fn repo_raw(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<SourceQuery>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    let Some(path) = query.path.as_deref().filter(|path| !path.trim().is_empty()) else {
        return super::raw::with_raw_headers(
            (
                [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                readme_markdown(&state, &repo),
            )
                .into_response(),
            None,
        );
    };
    let response = match git_blob_bytes(&state, &repo, source_ref(&repo, &query), path) {
        Ok((bytes, mime)) => ([(header::CONTENT_TYPE, mime)], bytes).into_response(),
        Err(response) => *response,
    };
    super::raw::with_raw_headers(response, Some(path))
}

pub(in crate::web) async fn repo_readme(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<SourceQuery>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return readme_not_found_error();
    };
    // `?path=<dir>`: that directory's README, as a folder view shows it. A
    // directory without one is a 404; the root keeps its stored fallback.
    if let Some(dir) = query
        .path
        .as_deref()
        .map(|dir| dir.trim_matches('/'))
        .filter(|dir| !dir.is_empty())
    {
        return match git_readme_path(&state, &repo, source_ref(&repo, &query), Some(dir)) {
            Ok(Some(path)) => readme_blob_response(&state, &repo, source_ref(&repo, &query), &path),
            Ok(None) => api_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "no README in this directory at this ref",
            ),
            Err(response) => *response,
        };
    }
    if let Ok(Some(path)) = git_readme_path(&state, &repo, source_ref(&repo, &query), None) {
        return readme_blob_response(&state, &repo, source_ref(&repo, &query), &path);
    }
    Json(readme_response(&state, &repo)).into_response()
}

fn readme_blob_response(
    state: &WebState,
    repo: &Repository,
    ref_name: &str,
    path: &str,
) -> AxumResponse {
    {
        match git_blob(state, repo, ref_name, path, Some("html")) {
            Ok(blob) => Json(ReadmeResponse {
                markdown: blob.text.unwrap_or_default(),
                rendered_markdown: blob
                    .rendered_markdown
                    .unwrap_or_else(|| render_markdown("")),
            })
            .into_response(),
            Err(response) => *response,
        }
    }
}

pub(in crate::web) async fn repo_readme_update(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    body: Bytes,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return readme_not_found_error();
    };
    let request: ReadmeUpdateRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return api_error_with_hint(
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                "readme update body failed validation",
                ApiErrorHint {
                    purpose: "update repository README",
                    reason: "invalid_input",
                    common_fixes: &[
                        "send JSON with a markdown string field",
                        "regenerate the managed README block from the fresh Jankurai artifact",
                    ],
                    docs_url: "docs/release.md",
                    repair_hint: &format!(
                        "rerun bash ops/ci/publish-readme-score.sh --verify (body parse error: {error})"
                    ),
                },
            );
        }
    };
    match state
        .github
        .core()
        .set_repository_readme(&repo.owner, &repo.name, request.markdown)
    {
        Ok(markdown) => {
            Json(readme_response_with_markdown(&state, &repo, markdown)).into_response()
        }
        Err(ForgeError::NotFound(_)) => readme_not_found_error(),
        Err(ForgeError::Forbidden(reason)) => api_error_with_hint(
            axum::http::StatusCode::FORBIDDEN,
            "forbidden",
            "this identity may not change the repository README",
            ApiErrorHint {
                purpose: "persist repository README",
                reason: "forbidden",
                common_fixes: &[
                    "publish the README as an account with write access to the repository",
                ],
                docs_url: "docs/release.md",
                repair_hint: &format!("retry as a permitted identity ({reason})"),
            },
        ),
        Err(ForgeError::WaitlistFull) => api_error(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "waitlist_full",
            "the waitlist is not accepting new addresses",
        ),
        Err(ForgeError::Storage(err) | ForgeError::StoreCustody(err)) => api_error_with_hint(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            "repository README could not be persisted",
            ApiErrorHint {
                purpose: "persist repository README",
                reason: "storage_failed",
                common_fixes: &[
                    "check the SQLite database path and write permissions",
                    "reopen the local forge store and rerun the publish helper",
                ],
                docs_url: "docs/release.md",
                repair_hint: &format!(
                    "rerun bash ops/ci/publish-readme-score.sh --verify (storage error: {err})"
                ),
            },
        ),
        Err(ForgeError::Conflict(err)) => api_error_with_hint(
            axum::http::StatusCode::CONFLICT,
            "conflict",
            "repository README update conflicted",
            ApiErrorHint {
                purpose: "persist repository README",
                reason: "conflict",
                common_fixes: &[
                    "refresh the local repo state before retrying the publish helper",
                    "replay the update against the latest README content",
                ],
                docs_url: "docs/release.md",
                repair_hint: &format!(
                    "rerun bash ops/ci/publish-readme-score.sh --verify (conflict: {err})"
                ),
            },
        ),
        Err(ForgeError::Validation(err)) => api_error_with_hint(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "repository README update failed validation",
            ApiErrorHint {
                purpose: "persist repository README",
                reason: "invalid_input",
                common_fixes: &[
                    "send a JSON body with a markdown string field",
                    "regenerate the managed score block from target/jankurai/repo-score.json",
                ],
                docs_url: "docs/release.md",
                repair_hint: &format!(
                    "rerun bash ops/ci/publish-readme-score.sh --verify (validation error: {err})"
                ),
            },
        ),
        Err(ForgeError::BranchProtection(err)) => api_error_with_hint(
            axum::http::StatusCode::FORBIDDEN,
            "policy_denied",
            "repository README update was blocked by policy",
            ApiErrorHint {
                purpose: "persist repository README",
                reason: "policy_denied",
                common_fixes: &[
                    "inspect the repository policy reason instead of bypassing the guard",
                    "supply the required proof or trust evidence",
                ],
                docs_url: "docs/errors.md#policy-denied",
                repair_hint: &format!(
                    "rerun bash ops/ci/publish-readme-score.sh --verify (policy error: {err})"
                ),
            },
        ),
        Err(ForgeError::RepositoryArchived(err)) => api_error_with_hint(
            axum::http::StatusCode::CONFLICT,
            "repository_archived",
            "repository is archived and read-only",
            ApiErrorHint {
                purpose: "persist repository README",
                reason: "repository_archived",
                common_fixes: &[
                    "unarchive the repository before publishing a README",
                    "publish the score for an active repository instead",
                ],
                docs_url: "docs/errors.md#policy-denied",
                repair_hint: &format!(
                    "unarchive the repository, then rerun bash ops/ci/publish-readme-score.sh --verify ({err})"
                ),
            },
        ),
    }
}
