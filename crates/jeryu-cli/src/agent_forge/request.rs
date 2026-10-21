use super::{Action, Args};
use anyhow::{Result, ensure};
use reqwest::Method;
use serde_json::{Value, json};
use std::{io::Read, path::Path};

pub(super) struct Request {
    pub method: Method,
    pub route: String,
    pub body: Option<Value>,
}

pub(super) fn spec(args: Args) -> Result<Request> {
    let parts: Vec<_> = args.repo.split('/').collect();
    ensure!(
        parts.len() == 2
            && parts.iter().all(|s| {
                s.as_bytes()
                    .first()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
                    && s.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
            }),
        "expected owner/repository"
    );
    let root = format!("/api/v1/repos/{}/pulls", args.repo);
    let (number, method, suffix, body) = match args.action {
        Action::Open {
            head,
            base,
            title,
            body_file,
        } => {
            return Ok(Request {
                method: Method::POST,
                route: format!("/api/v3/repos/{}/pulls", args.repo),
                body: Some(json!({"head":head,"base":base,"title":title,
                "body":read_body(&body_file)?,"draft":true})),
            });
        }
        Action::Comment {
            number,
            head_sha,
            body_file,
        } => {
            ensure!(
                head_sha.len() == 40 && head_sha.bytes().all(|c| c.is_ascii_hexdigit()),
                "expected head SHA"
            );
            (
                number,
                Method::POST,
                "/reviews",
                Some(json!({"verdict":"comment",
                "expected_head_sha":head_sha,"body_markdown":read_body(&body_file)?,
                "thread_comments":[]})),
            )
        }
        Action::CloseAsReplaced {
            number,
            replacement,
        } => {
            ensure!(
                replacement > 0 && replacement != number,
                "invalid replacement"
            );
            (
                number,
                Method::POST,
                "/close",
                Some(json!({"superseded_by":replacement})),
            )
        }
        Action::Ready { number } => (number, Method::POST, "/ready", Some(json!({}))),
        Action::Reviews { number } => (number, Method::GET, "", None),
    };
    ensure!(number > 0, "expected positive PR number");
    Ok(Request {
        method,
        route: format!("{root}/{number}{suffix}"),
        body,
    })
}

fn read_body(path: &Path) -> Result<String> {
    let mut body = String::new();
    std::fs::File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_string(&mut body)?;
    ensure!(body.len() <= 1024 * 1024, "body exceeds size limit");
    Ok(body)
}
