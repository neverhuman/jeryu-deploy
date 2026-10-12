//! The comment thread of one todo, append-only, kept with the queue.
//!
//! `note` is one field every status action overwrites, so there was no place
//! to add context to a todo — a second outage, a measurement, an owner's
//! reply — without losing what was there. A thread is a sibling file on the
//! `queue` branch, `comments/<todo-id>.jsonl`: one JSON object per line
//! (`author`, `at`, `body`), appended under the same compare-and-swap push
//! every todo action uses. JSON escapes the body's newlines, so no line of a
//! comment can ever look like the start of the next one, and a worker that
//! clones the queue reads the thread as plain text beside the todo.
//!
//! A comment is only ever added: writing one leaves the todo file alone, so a
//! comment changes no status, note, lease or attempt count.

use serde::{Deserialize, Serialize};

/// Where a todo's thread lives on the queue branch.
pub(crate) fn path_for(todo_id: &str) -> String {
    format!("comments/{todo_id}.jsonl")
}

/// The longest comment the API accepts, in characters. Long enough for an
/// incident write-up, short enough that a queue clone stays small.
pub(crate) const MAX_BODY_CHARS: usize = 16 * 1024;

/// A todo id safe to spell as a file name: todoq's ids, and nothing that
/// could step out of `comments/`.
pub(crate) fn id_is_safe(todo_id: &str) -> bool {
    !todo_id.is_empty()
        && todo_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// One comment, as the API returns it and the queue file stores it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct Comment {
    /// The login that posted it.
    pub author: String,
    /// When the server stored it, RFC 3339 to the second.
    pub at: String,
    /// Markdown, as the author wrote it.
    pub body: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CommentsResponse {
    pub family: String,
    pub todo_id: String,
    /// Oldest first: the order they were posted in.
    pub comments: Vec<Comment>,
    pub total: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CommentRequest {
    pub body: String,
}

/// Read a thread file. A line that is not a comment is skipped, so one bad
/// line hides itself rather than the whole thread.
pub(crate) fn parse_thread(text: &str) -> Vec<Comment> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Comment>(line).ok())
        .collect()
}

/// The thread file with `comment` appended as its last line.
pub(crate) fn append(existing: Option<&str>, comment: &Comment) -> String {
    let mut out = existing.unwrap_or_default().to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&serde_json::to_string(comment).unwrap_or_default());
    out.push('\n');
    out
}

/// The comment a request asks for, or why it is not one.
pub(crate) fn comment_from(
    request: &CommentRequest,
    author: &str,
    at: &str,
) -> Result<Comment, String> {
    let body = request.body.trim_matches('\n').trim_end().to_string();
    if body.trim().is_empty() {
        return Err("body must not be empty".to_string());
    }
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(format!("body must be at most {MAX_BODY_CHARS} characters"));
    }
    Ok(Comment {
        author: author.to_string(),
        at: at.to_string(),
        body,
    })
}
