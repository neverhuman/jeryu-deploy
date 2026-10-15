//! Git smart-HTTP transport handlers for the unified `jeryu serve`.
//!
//! These axum handlers mount `git clone`/`push` over HTTP by delegating to the
//! pure `jeryu_gitd::smart_http::SmartHttpServer`, run on the blocking pool so
//! the `git` subprocess never stalls an async worker. Kept in their own module
//! to keep `web.rs` focused on the REST/WS edge.

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{ConnectInfo, Path as AxumPath, Query as AxumQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response as AxumResponse};
use flate2::bufread::GzDecoder;
use futures_util::StreamExt;
use jeryu_gitd::lfs::{LfsStore, normalize_oid};
use jeryu_gitd::smart_http::{
    HttpRequest as GitHttpRequest, HttpResponse as GitHttpResponse, SmartHttpServer,
};
use jeryu_gitd::{GitdError, RepoManager};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::web::WebState;

mod push_policy;

// Bound both wire bytes and logical request bytes. Compressed RPCs get a tighter
// wire cap so a small authenticated request cannot become a decompression bomb;
// identity RPCs use the decoded cap because their wire and logical bytes are the
// same. This keeps equivalent identity and gzip requests on the same payload
// ceiling without buffering either form without bound.
const MAX_GIT_UPLOAD_PACK_ENCODED_BYTES: usize = 4 * 1024 * 1024;
const MAX_GIT_UPLOAD_PACK_DECODED_BYTES: usize = 16 * 1024 * 1024;
const MAX_GIT_RECEIVE_PACK_ENCODED_BYTES: usize = 64 * 1024 * 1024;
const MAX_GIT_RECEIVE_PACK_DECODED_BYTES: usize = 256 * 1024 * 1024;

/// The pkt-line that ends a side-band receive-pack answer.
const FLUSH_PKT: &[u8] = b"0000";
/// Side-band channel 2: text the client prints, each line as `remote: ...`.
const SIDEBAND_PROGRESS: char = '\u{2}';

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitRpcContentEncoding {
    Identity,
    Gzip,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitRpcBodyError {
    UnsupportedEncoding,
    MalformedGzip,
    TooLarge,
    DecodeTaskFailed,
}

fn forwarded_git_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let mut forwarded = HashMap::new();
    for name in [
        header::HOST,
        header::AUTHORIZATION,
        HeaderName::from_static("git-protocol"),
        HeaderName::from_static("x-forwarded-proto"),
    ] {
        if let Some(value) = headers.get(&name).and_then(|value| value.to_str().ok()) {
            forwarded.insert(name.as_str().to_ascii_lowercase(), value.to_string());
        }
    }
    forwarded
}

fn git_rpc_content_encoding(headers: &HeaderMap) -> Result<GitRpcContentEncoding, GitRpcBodyError> {
    let values = headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Ok(GitRpcContentEncoding::Identity);
    }
    if values.len() != 1 {
        return Err(GitRpcBodyError::UnsupportedEncoding);
    }
    let value = values[0]
        .to_str()
        .map_err(|_| GitRpcBodyError::UnsupportedEncoding)?
        .trim();
    if value.eq_ignore_ascii_case("identity") {
        Ok(GitRpcContentEncoding::Identity)
    } else if value.eq_ignore_ascii_case("gzip") || value.eq_ignore_ascii_case("x-gzip") {
        Ok(GitRpcContentEncoding::Gzip)
    } else {
        // This also rejects empty, comma-separated, and stacked encodings.
        Err(GitRpcBodyError::UnsupportedEncoding)
    }
}

fn decode_git_rpc_body(
    encoding: GitRpcContentEncoding,
    encoded: Bytes,
    max_decoded_bytes: usize,
) -> Result<Vec<u8>, GitRpcBodyError> {
    if encoding == GitRpcContentEncoding::Identity {
        return Ok(encoded.to_vec());
    }

    let encoded_len = encoded.len() as u64;
    let mut decoder = GzDecoder::new(Cursor::new(encoded.as_ref()));
    let mut decoded = Vec::new();
    (&mut decoder)
        .take(max_decoded_bytes.saturating_add(1) as u64)
        .read_to_end(&mut decoded)
        .map_err(|_| GitRpcBodyError::MalformedGzip)?;
    if decoded.len() > max_decoded_bytes {
        return Err(GitRpcBodyError::TooLarge);
    }
    if decoder.into_inner().position() != encoded_len {
        return Err(GitRpcBodyError::MalformedGzip);
    }
    Ok(decoded)
}

async fn read_git_rpc_body(
    headers: &HeaderMap,
    body: Body,
    max_encoded_bytes: usize,
    max_decoded_bytes: usize,
) -> Result<Vec<u8>, GitRpcBodyError> {
    let encoding = git_rpc_content_encoding(headers)?;
    let max_wire_bytes = match encoding {
        GitRpcContentEncoding::Identity => max_decoded_bytes,
        GitRpcContentEncoding::Gzip => max_encoded_bytes,
    };
    let encoded = to_bytes(body, max_wire_bytes)
        .await
        .map_err(|_| GitRpcBodyError::TooLarge)?;
    if encoding == GitRpcContentEncoding::Identity {
        return Ok(encoded.to_vec());
    }
    tokio::task::spawn_blocking(move || decode_git_rpc_body(encoding, encoded, max_decoded_bytes))
        .await
        .map_err(|_| GitRpcBodyError::DecodeTaskFailed)?
}

fn git_rpc_body_error_response(error: GitRpcBodyError) -> AxumResponse {
    match error {
        GitRpcBodyError::UnsupportedEncoding => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported Git RPC Content-Encoding\n",
        )
            .into_response(),
        GitRpcBodyError::MalformedGzip => {
            (StatusCode::BAD_REQUEST, "malformed gzip Git RPC body\n").into_response()
        }
        GitRpcBodyError::TooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "Git RPC body exceeds the configured limit\n",
        )
            .into_response(),
        GitRpcBodyError::DecodeTaskFailed => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Git RPC body decoder failed\n",
        )
            .into_response(),
    }
}

async fn route_git(
    state: &WebState,
    peer: SocketAddr,
    method: &str,
    path: String,
    query: HashMap<String, String>,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> AxumResponse {
    gitd_to_axum_response(&route_gitd(state, peer, method, path, query, headers, body).await)
}

/// The gitd router's own answer, before it becomes an axum response: the
/// receive-pack handler adds to the body (see [`append_sideband`]), and
/// everything else hands this straight to [`gitd_to_axum_response`].
async fn route_gitd(
    state: &WebState,
    peer: SocketAddr,
    method: &str,
    path: String,
    query: HashMap<String, String>,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> GitHttpResponse {
    let request = GitHttpRequest {
        method: method.to_string(),
        path,
        query,
        headers: forwarded_git_headers(headers),
        body,
        is_loopback: peer.ip().is_loopback(),
        auth_prechecked: true,
    };
    // The gitd router shells out to `git` (blocking) and does file IO; run it on
    // the blocking pool so it never stalls an async worker.
    let manager = (*state.repo_manager).clone();
    tokio::task::spawn_blocking(move || SmartHttpServer::new(manager).route(request))
        .await
        .expect("git smart-http task panicked")
}

/// The branches a receive-pack request moved or created, short names, deletes
/// left out: a deleted branch strands nothing.
fn pushed_branches(commands: &[jeryu_gitd::pack::ReceivePackCommand]) -> Vec<String> {
    commands
        .iter()
        .filter(|command| command.new_oid != crate::ci_bridge::ZERO_OID)
        .filter_map(|command| {
            command
                .ref_name
                .strip_prefix("refs/heads/")
                .map(str::to_string)
        })
        .collect()
}

/// Whether the client asked for `side-band-64k`, which is what makes a
/// `remote:` line readable: without it the body is a bare report-status and
/// an extra packet would be read as part of it. Capabilities ride on the
/// first command record, after a NUL.
fn wants_sideband(body: &[u8]) -> bool {
    let mut rest = body;
    while rest.len() >= 4 {
        let Ok(header) = std::str::from_utf8(&rest[..4]) else {
            return false;
        };
        let Ok(length) = usize::from_str_radix(header, 16) else {
            return false;
        };
        if length < 4 {
            return false; // a flush or a control packet: no command came first
        }
        let payload_len = length - 4;
        rest = &rest[4..];
        if rest.len() < payload_len {
            return false;
        }
        let (payload, tail) = rest.split_at(payload_len);
        if let Some(position) = payload.iter().position(|byte| *byte == 0) {
            return String::from_utf8_lossy(&payload[position + 1..])
                .split_whitespace()
                .any(|capability| capability == "side-band-64k");
        }
        rest = tail;
    }
    false
}

/// Add `remote:` lines to a side-band receive-pack answer.
///
/// With `side-band-64k` the answer is multiplexed packets terminated by a
/// flush, band 1 carrying the report-status the client parses and band 2 the
/// text it prints. The lines go on band 2, before that terminating flush: a
/// packet after it is never read, and nothing already in the body is touched,
/// so the report the client acts on is exactly what git wrote.
fn append_sideband(response: GitHttpResponse, lines: &[String]) -> GitHttpResponse {
    if lines.is_empty() {
        return response;
    }
    let Some(body) = response.body().strip_suffix(FLUSH_PKT) else {
        // Not a flush-terminated stream after all: say nothing rather than
        // append to a body whose shape is not the one this understands.
        return response;
    };
    let mut out = body.to_vec();
    for line in lines {
        let line: String = line
            .chars()
            .filter(|ch| !ch.is_control())
            .take(4000)
            .collect();
        let payload = format!("{SIDEBAND_PROGRESS}{line}\n");
        out.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
        out.extend_from_slice(payload.as_bytes());
    }
    out.extend_from_slice(FLUSH_PKT);
    let mut rebuilt = GitHttpResponse::bytes(response.status(), response.content_type(), out);
    for (name, value) in response.extra_headers() {
        rebuilt = rebuilt.with_header(name, value);
    }
    rebuilt
}

fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload);
    out
}

fn receive_pack_refusal(
    commands: &[jeryu_gitd::pack::ReceivePackCommand],
    blocked_ref: &str,
    message: &str,
    sideband: bool,
) -> GitHttpResponse {
    let message: String = message
        .chars()
        .filter(|ch| !ch.is_control())
        .take(4000)
        .collect();
    let mut report = Vec::new();
    report.extend_from_slice(&pkt_line(
        b"unpack branch policy refused before unpacking\n",
    ));
    let mut progress = vec![message.clone()];
    for command in commands {
        let reason = if command.ref_name == blocked_ref {
            message.clone()
        } else {
            format!(
                "batch not updated because {blocked_ref} was refused; retry this ref separately"
            )
        };
        let prefix = format!("ng {} ", command.ref_name);
        let budget = 65_516usize.saturating_sub(prefix.len() + 1);
        let mut end = reason.len().min(budget);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        report.extend_from_slice(&pkt_line(
            format!("{prefix}{}\n", &reason[..end]).as_bytes(),
        ));
        if progress.len() <= 64 {
            progress.push(format!("{}: {}", command.ref_name, &reason[..end]));
        }
    }
    report.extend_from_slice(FLUSH_PKT);
    let body = if sideband {
        let mut body = Vec::new();
        for line in progress {
            let line: String = line
                .chars()
                .filter(|ch| !ch.is_control())
                .take(4000)
                .collect();
            body.extend_from_slice(&pkt_line(format!("{SIDEBAND_PROGRESS}{line}\n").as_bytes()));
        }
        for chunk in report.chunks(60_000) {
            let mut status = Vec::with_capacity(chunk.len() + 1);
            status.push(1);
            status.extend_from_slice(chunk);
            body.extend_from_slice(&pkt_line(&status));
        }
        body.extend_from_slice(FLUSH_PKT);
        body
    } else {
        report
    };
    GitHttpResponse::bytes(200, "application/x-git-receive-pack-result", body)
}

/// Convert a gitd [`GitHttpResponse`] into an axum response, preserving status,
/// content type, and any extra headers (e.g. `WWW-Authenticate`).
fn gitd_to_axum_response(response: &GitHttpResponse) -> AxumResponse {
    let status =
        StatusCode::from_u16(response.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut out = (status, response.body().to_vec()).into_response();
    let map = out.headers_mut();
    if let Ok(content_type) = HeaderValue::from_str(response.content_type()) {
        map.insert(header::CONTENT_TYPE, content_type);
    }
    for (name, value) in response.extra_headers() {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            map.insert(name, value);
        }
    }
    out
}

pub(crate) async fn git_info_refs(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
    AxumQuery(query): AxumQuery<HashMap<String, String>>,
    headers: HeaderMap,
) -> AxumResponse {
    let write = matches!(
        query.get("service").map(String::as_str),
        Some("git-receive-pack")
    );
    if let Err(response) = authorize_git_core(&state, peer, &headers, &owner, &repo, write) {
        return *response;
    }
    route_git(
        &state,
        peer,
        "GET",
        format!("/git/{owner}/{repo}/info/refs"),
        query,
        &headers,
        Vec::new(),
    )
    .await
}

pub(crate) async fn git_upload_pack(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
    request: axum::extract::Request,
) -> AxumResponse {
    let headers = request.headers().clone();
    if let Err(response) = authorize_git_core(&state, peer, &headers, &owner, &repo, false) {
        return *response;
    }
    let body = match read_git_rpc_body(
        &headers,
        request.into_body(),
        MAX_GIT_UPLOAD_PACK_ENCODED_BYTES,
        MAX_GIT_UPLOAD_PACK_DECODED_BYTES,
    )
    .await
    {
        Ok(body) => body,
        Err(error) => return git_rpc_body_error_response(error),
    };
    route_git(
        &state,
        peer,
        "POST",
        format!("/git/{owner}/{repo}/git-upload-pack"),
        HashMap::new(),
        &headers,
        body,
    )
    .await
}

pub(crate) async fn git_receive_pack(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
    request: axum::extract::Request,
) -> AxumResponse {
    let headers = request.headers().clone();
    if let Err(response) = authorize_git_core(&state, peer, &headers, &owner, &repo, true) {
        return *response;
    }
    let body = match read_git_rpc_body(
        &headers,
        request.into_body(),
        MAX_GIT_RECEIVE_PACK_ENCODED_BYTES,
        MAX_GIT_RECEIVE_PACK_DECODED_BYTES,
    )
    .await
    {
        Ok(body) => body,
        Err(error) => return git_rpc_body_error_response(error),
    };
    // The merge queue owns refs/queue/* and refs/queue-meta/*: a pushed queue
    // commit would be gated and landed as if the forge had built it.
    let commands = match push_policy::commands(&body) {
        Ok(commands) => commands,
        Err(response) => return *response,
    };
    if let Some(command) = commands
        .iter()
        .find(|command| crate::web::is_queue_owned_ref(&command.ref_name))
    {
        return (
            StatusCode::FORBIDDEN,
            format!(
                "{} is owned by the merge queue and cannot be pushed\n",
                command.ref_name
            ),
        )
            .into_response();
    }
    let sideband = wants_sideband(&body);
    let pushed_branches = pushed_branches(&commands);
    let check_state = state.clone();
    let check_owner = owner.clone();
    let check_repo = repo.trim_end_matches(".git").to_string();
    let check_commands = commands.clone();
    let check_headers = headers.clone();
    let policy = match tokio::task::spawn_blocking(move || {
        push_policy::PushPolicy::inspect(
            &check_state,
            &check_owner,
            &check_repo,
            &check_commands,
            &check_headers,
            sideband,
            || {
                check_state
                    .core
                    .list_pull_requests(&check_owner, &check_repo, None)
            },
        )
    })
    .await
    {
        Ok(Ok(policy)) => policy,
        Ok(Err(response)) => return *response,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "branch guard worker failed",
            )
                .into_response();
        }
    };
    let manager = (*state.repo_manager).clone();
    let before = snapshot_refs(&manager, &owner, &repo);
    let origin_base_url = origin_base_url(&headers);
    // What the client said it can read back, and which branches it moved: both
    // are only in the request, so they are taken before the body is handed on.
    let gitd = route_gitd(
        &state,
        peer,
        "POST",
        format!("/git/{owner}/{repo}/git-receive-pack"),
        HashMap::new(),
        &headers,
        body,
    )
    .await;
    // A branch whose pull request closed without merging still gets a message
    // after the ref lands. Updates to recent merged pull request branches
    // were refused above, as were late-work recreations, unless an exact
    // administrator exception was durably audited. The request's one PR
    // snapshot supplies at most 16 branch notices. Recreating at the recorded
    // merged head and deleting a branch remain available.
    let gitd = if sideband && gitd.status() == StatusCode::OK.as_u16() {
        let notice_state = state.clone();
        let notice_owner = owner.clone();
        let notice_repo = repo.trim_end_matches(".git").to_string();
        let lines = tokio::task::spawn_blocking(move || {
            policy.notice_lines(&notice_state, &notice_owner, &notice_repo, &pushed_branches)
        })
        .await
        .unwrap_or_default();
        append_sideband(gitd, &lines)
    } else {
        gitd
    };
    let response = gitd_to_axum_response(&gitd);
    // The refs are durable once receive-pack succeeds, so the push->CI bridge
    // for any moved branch runs detached: scoring a pushed commit must not hold
    // the acknowledgement the pushing agent is waiting for.
    let core = state.core.clone();
    // The web state the detached post-push work needs: the gate signal and the
    // tag mirror both ride on it.
    let post_push = state.clone();
    spawn_post_push(&response, move || {
        let after = snapshot_refs(&manager, &owner, &repo);
        let updates = crate::ci_bridge::ref_updates(&before, &after);
        // A moved branch is the first thing the gate could want: record it
        // before the check-runs so a runner long-polling the signal starts its
        // tick while this push's own CI is still being compiled. The signal is
        // a hint -- whether the ref is a pull request head is the runner's
        // question, not this one's.
        crate::web::gate_wanted::record_push(&post_push, &owner, &repo, &updates);
        // Commits a person authored are that person's time on the change
        // (`web/human_time.rs`).
        crate::web::human_time::record_push(&post_push, &owner, &repo, &updates);
        crate::ci_bridge::on_push(&core, &manager, &owner, &repo, &updates, &origin_base_url);
        // A default-branch push changes what the code tools should answer, so
        // the repository's codegraph index is rebuilt from the new head. It
        // runs after the refs are durable and reports its own failures: an
        // index that cannot be built must not fail the push.
        crate::web::codegraph_schedule::refresh_pushed_branches(
            &post_push,
            &owner,
            repo.trim_end_matches(".git"),
            &updates,
        );
        // A pushed tag is mirrored to GitHub the same way a merge mirrors main:
        // new tags only, and an existing GitHub tag is reported, never moved.
        let tags = crate::ci_bridge::tag_updates(&before, &after);
        crate::web::mirror_reconcile::mirror_pushed_tags(
            &post_push,
            &owner,
            repo.trim_end_matches(".git"),
            &tags,
        );
    });
    response
}

pub(crate) async fn git_lfs_batch(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> AxumResponse {
    if let Err(response) = authorize_git_core(&state, peer, &headers, &owner, &repo, true) {
        return *response;
    }
    route_git(
        &state,
        peer,
        "POST",
        format!("/git/{owner}/{repo}/info/lfs/objects/batch"),
        HashMap::new(),
        &headers,
        body.to_vec(),
    )
    .await
}

pub(crate) async fn git_lfs_download(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath((owner, repo, oid)): AxumPath<(String, String, String)>,
    headers: HeaderMap,
) -> AxumResponse {
    if let Err(response) = authorize_git_core(&state, peer, &headers, &owner, &repo, false) {
        return *response;
    }
    route_git(
        &state,
        peer,
        "GET",
        format!("/git/{owner}/{repo}/info/lfs/objects/{oid}"),
        HashMap::new(),
        &headers,
        Vec::new(),
    )
    .await
}

pub(crate) async fn git_lfs_verify(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath((owner, repo, oid)): AxumPath<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> AxumResponse {
    if let Err(response) = authorize_git_core(&state, peer, &headers, &owner, &repo, true) {
        return *response;
    }
    route_git(
        &state,
        peer,
        "POST",
        format!("/git/{owner}/{repo}/info/lfs/objects/{oid}/verify"),
        HashMap::new(),
        &headers,
        body.to_vec(),
    )
    .await
}

pub(crate) async fn git_lfs_locks_verify(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> AxumResponse {
    if let Err(response) = authorize_git_core(&state, peer, &headers, &owner, &repo, true) {
        return *response;
    }
    route_git(
        &state,
        peer,
        "POST",
        format!("/git/{owner}/{repo}/info/lfs/locks/verify"),
        HashMap::new(),
        &headers,
        body.to_vec(),
    )
    .await
}

pub(crate) async fn git_lfs_upload(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath((owner, repo, oid)): AxumPath<(String, String, String)>,
    request: axum::extract::Request,
) -> AxumResponse {
    let headers = request.headers().clone();
    let manager = (*state.repo_manager).clone();
    if let Err(response) = authorize_git_core(&state, peer, &headers, &owner, &repo, true) {
        return *response;
    }
    let oid = match normalize_oid(&oid) {
        Ok(oid) => oid,
        Err(err) => return gitd_error_to_axum(err),
    };
    let repo = match manager.open_parts(&owner, &repo) {
        Ok(repo) => repo,
        Err(err) => return gitd_error_to_axum(err),
    };
    let expected_size = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let max_size = manager.config().lfs_max_object_bytes;
    if expected_size.is_some_and(|size| size > max_size) {
        return lfs_json_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "LFS object is larger than the configured limit",
        );
    }
    let store = LfsStore::for_repo(&repo.path);
    match stream_lfs_upload(store, oid, expected_size, max_size, request.into_body()).await {
        Ok(()) => lfs_empty_response(StatusCode::OK),
        Err(err) => gitd_error_to_axum(err),
    }
}

fn origin_base_url(headers: &HeaderMap) -> String {
    match headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .filter(|host| !host.trim().is_empty())
    {
        Some(host) => format!("http://{host}"),
        None => String::new(),
    }
}

fn authorize_git_core(
    state: &WebState,
    peer: SocketAddr,
    headers: &HeaderMap,
    owner: &str,
    repo: &str,
    write: bool,
) -> Result<(), Box<AxumResponse>> {
    if !state.auth_required || crate::web::auth::local_dev_trusted(state, Some(peer), headers) {
        return Ok(());
    }
    // Use the transport resolver's identity for every authorization decision.
    // Independently stripping a suffix can authorize a different metadata row
    // from the bare repository that Git actually opens.
    let resolved = state
        .repo_manager
        .resolve_parts(owner, repo)
        .map_err(|error| Box::new(gitd_error_to_axum(error)))?;
    let owner = resolved.id.owner.as_str();
    let logical_repo = resolved.id.name.as_str();
    // A public repository is readable by anyone, as the UI's PUBLIC chip and
    // Clone button promise; writes always need a principal.
    let public_read = !write
        && state
            .core
            .get_repository(owner, logical_repo)
            .is_ok_and(|found| !found.private);
    // A supplied Authorization header must carry a valid bearer or basic
    // credential: never fall back to a cookie session or to anonymous read,
    // so a revoked or mistyped token is refused instead of silently accepted.
    let auth = crate::web::auth::authenticate_headers(state, headers).filter(|auth| {
        !headers.contains_key(header::AUTHORIZATION)
            || auth.source == crate::web::auth::AuthSource::Bearer
    });
    let Some(auth) = auth else {
        if public_read && !headers.contains_key(header::AUTHORIZATION) {
            return Ok(());
        }
        return Err(Box::new(gitd_to_axum_response(
            &GitHttpResponse::text(401, "Requires authentication\n")
                .with_header("WWW-Authenticate", "Basic realm=\"jeryu\""),
        )));
    };
    if let Some(bot) = auth.bot.clone() {
        let call = crate::web::bots::bot_call(&bot, owner, logical_repo, write, None, None);
        return match state.core.authorize_bot(&call) {
            Ok(_) => Ok(()),
            Err(error) => Err(Box::new(git_bot_denied(owner, repo, write, &error))),
        };
    }
    let account = auth.account;
    let allowed = if write {
        state
            .core
            .user_can_write_repo(&account.login, owner, logical_repo)
    } else {
        public_read
            || state
                .core
                .user_can_read_repo(&account.login, owner, logical_repo)
    };
    if allowed {
        Ok(())
    } else {
        Err(Box::new(gitd_to_axum_response(&GitHttpResponse::bytes(
            403,
            "application/json; charset=utf-8",
            format!(
                "{{\"message\":\"principal not authorized to {} {owner}/{repo}\",\"documentation_url\":\"https://docs.jeryu/auth\"}}",
                if write { "write" } else { "read" }
            )
            .into_bytes(),
        ))))
    }
}

fn git_bot_denied(
    owner: &str,
    repo: &str,
    write: bool,
    error: &jeryu_core::ForgeError,
) -> AxumResponse {
    let (status, code) = match error {
        jeryu_core::ForgeError::Forbidden(code) => {
            (403, jeryu_core::ForgeError::machine_text(code))
        }
        jeryu_core::ForgeError::Validation(_) => (401, "invalid_token"),
        _ => (403, "insufficient_scope"),
    };
    let verb = if write { "write" } else { "read" };
    gitd_to_axum_response(&GitHttpResponse::bytes(
        status,
        "application/json; charset=utf-8",
        format!(
            "{{\"code\":\"{code}\",\"message\":\"principal not authorized to {verb} {owner}/{repo}\",\"documentation_url\":\"https://docs.jeryu/auth\"}}"
        )
        .into_bytes(),
    ))
}

async fn stream_lfs_upload(
    store: LfsStore,
    oid: String,
    expected_size: Option<u64>,
    max_size: u64,
    body: Body,
) -> Result<(), GitdError> {
    let path = store.object_path(&oid)?;
    let parent = path
        .parent()
        .ok_or_else(|| GitdError::Lfs("LFS object path has no parent".to_string()))?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp = lfs_tmp_path(&path);
    let result = stream_lfs_upload_inner(&oid, expected_size, max_size, body, &tmp, &path).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result.map(|_| ())
}

async fn stream_lfs_upload_inner(
    oid: &str,
    expected_size: Option<u64>,
    max_size: u64,
    body: Body,
    tmp: &Path,
    path: &Path,
) -> Result<u64, GitdError> {
    let mut file = tokio::fs::File::create(tmp).await?;
    let mut stream = body.into_data_stream();
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| GitdError::Lfs(format!("read LFS upload body: {err}")))?;
        total = total
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| GitdError::Lfs("LFS object size overflowed u64".to_string()))?;
        if total > max_size {
            return Err(GitdError::Lfs(format!(
                "LFS object {oid} is larger than the configured limit of {max_size} bytes"
            )));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    drop(file);
    if let Some(expected) = expected_size
        && total != expected
    {
        return Err(GitdError::Lfs(format!(
            "size mismatch for {oid}: expected {expected}, got {total}"
        )));
    }
    let actual = hex::encode(hasher.finalize());
    if actual != oid {
        return Err(GitdError::Lfs(format!(
            "sha256 mismatch: expected {oid}, got {actual}"
        )));
    }
    tokio::fs::rename(tmp, path).await?;
    Ok(total)
}

fn lfs_tmp_path(path: &Path) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("object");
    path.with_file_name(format!(".{file_name}.tmp.{}.{}", std::process::id(), now))
}

fn gitd_error_to_axum(err: GitdError) -> AxumResponse {
    match err {
        GitdError::Unauthorized => gitd_to_axum_response(
            &GitHttpResponse::text(401, "Requires authentication\n")
                .with_header("WWW-Authenticate", "Basic realm=\"jeryu\""),
        ),
        GitdError::Forbidden(msg) | GitdError::ProtectedRefDenied(msg) => {
            gitd_to_axum_response(&GitHttpResponse::bytes(
                403,
                "application/json; charset=utf-8",
                serde_json::json!({ "message": msg })
                    .to_string()
                    .into_bytes(),
            ))
        }
        GitdError::RepoNotFound(_) => {
            lfs_json_response(StatusCode::NOT_FOUND, "repository not found")
        }
        GitdError::Lfs(msg) => lfs_json_response(StatusCode::UNPROCESSABLE_ENTITY, &msg),
        err => {
            let body = format!("jeryu_gitd error: {err}\n");
            (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
        }
    }
}

fn lfs_empty_response(status: StatusCode) -> AxumResponse {
    let mut response = (status, "{}").into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.git-lfs+json"),
    );
    response
}

fn lfs_json_response(status: StatusCode, message: &str) -> AxumResponse {
    let mut response = (
        status,
        serde_json::json!({ "message": message }).to_string(),
    )
        .into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.git-lfs+json"),
    );
    response
}

/// Run a successful push's follow-up work on the blocking pool without holding
/// the response. A panic in the follow-up is logged, never surfaced to the
/// pusher: the refs it reacts to are already durable.
fn spawn_post_push(response: &AxumResponse, work: impl FnOnce() + Send + 'static) {
    if !response.status().is_success() {
        return;
    }
    let handle = tokio::task::spawn_blocking(work);
    tokio::spawn(async move {
        if let Err(error) = handle.await {
            eprintln!("jeryu-api git transport: post-push bridge failed: {error}");
        }
    });
}

/// Snapshot a repo's refs. A repo that cannot be resolved or listed yields an
/// empty snapshot, so the post-push diff simply finds no updates.
fn snapshot_refs(manager: &RepoManager, owner: &str, repo: &str) -> Vec<jeryu_gitd::refs::GitRef> {
    manager
        .resolve_parts(owner, repo)
        .and_then(|resolved| {
            jeryu_gitd::refs::RefService::new(manager.clone()).list_refs(&resolved)
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        GitHttpResponse, GitRpcBodyError, GitRpcContentEncoding, append_sideband,
        decode_git_rpc_body, forwarded_git_headers, git_rpc_content_encoding, pushed_branches,
        read_git_rpc_body, receive_pack_refusal, spawn_post_push, wants_sideband,
    };
    use axum::body::{Body, Bytes};
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::response::IntoResponse;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn pkt_line(payload: &str) -> Vec<u8> {
        let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
        out.extend_from_slice(payload.as_bytes());
        out
    }

    /// A receive-pack request prelude: the commands, capabilities on the first
    /// one, then a flush. The packfile that follows is not parsed.
    fn receive_pack_body(commands: &[&str], capabilities: &str) -> Vec<u8> {
        let mut body = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            body.extend_from_slice(&pkt_line(&if index == 0 {
                format!("{command}\0{capabilities}\n")
            } else {
                format!("{command}\n")
            }));
        }
        body.extend_from_slice(b"0000");
        body
    }

    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";
    const ZERO: &str = "0000000000000000000000000000000000000000";

    /// A branch the push moved is a branch to look at; a deleted branch and a
    /// tag are not, and neither is a push whose client cannot read a message.
    #[test]
    fn a_push_is_read_for_its_branches_and_for_a_band_to_answer_on() {
        let body = receive_pack_body(
            &[
                &format!("{OLD} {NEW} refs/heads/feature"),
                &format!("{OLD} {ZERO} refs/heads/gone"),
                &format!("{ZERO} {NEW} refs/tags/v1"),
            ],
            "report-status side-band-64k agent=git/2.43",
        );
        let commands = jeryu_gitd::pack::receive_pack_commands(&body).unwrap();
        assert_eq!(pushed_branches(&commands), vec!["feature".to_string()]);
        assert!(wants_sideband(&body));

        let bare = receive_pack_body(
            &[&format!("{OLD} {NEW} refs/heads/feature")],
            "report-status",
        );
        assert!(!wants_sideband(&bare), "no band to print a message on");
        assert!(!wants_sideband(b"0000"), "a flush carries no command");
        assert!(!wants_sideband(b"xx"), "a truncated body is not a push");
    }

    #[test]
    fn receive_pack_refusal_reports_rejected_refs_without_writing() {
        let body = receive_pack_body(
            &[&format!("{OLD} {NEW} refs/heads/feature")],
            "report-status side-band-64k agent=git/2.43",
        );
        let commands = jeryu_gitd::pack::receive_pack_commands(&body).unwrap();
        let response = receive_pack_refusal(
            &commands,
            "refs/heads/feature",
            "this branch's pull request #3 was already merged; push to a new branch and open a new pull request",
            true,
        );
        let written = String::from_utf8_lossy(response.body());

        assert_eq!(response.status(), 200);
        assert!(
            written.contains("unpack branch policy refused before unpacking"),
            "{written}"
        );
        assert!(!written.contains("unpack ok"), "{written}");
        assert!(
            written
                .contains("ng refs/heads/feature this branch's pull request #3 was already merged"),
            "{written}"
        );
        assert!(
            written.contains("push to a new branch and open a new pull request"),
            "{written}"
        );
    }

    #[test]
    fn a_batch_refusal_identifies_only_the_blocked_ref_and_bounds_sideband_text() {
        const OLD: &str = "1111111111111111111111111111111111111111";
        const NEW: &str = "2222222222222222222222222222222222222222";
        let body = receive_pack_body(
            &[
                &format!("{OLD} {NEW} refs/heads/feature"),
                &format!("{OLD} {NEW} refs/heads/other"),
            ],
            "report-status side-band-64k",
        );
        let commands = jeryu_gitd::pack::receive_pack_commands(&body).unwrap();
        let response =
            receive_pack_refusal(&commands, "refs/heads/feature", "already merged", true);
        let text = String::from_utf8_lossy(response.body());
        assert!(text.contains("ng refs/heads/feature already merged"));
        assert!(text.contains("ng refs/heads/other batch not updated because refs/heads/feature was refused; retry this ref separately"));
        let appended = append_sideband(response, &[format!("bad\n{}", "é".repeat(100_000))]);
        let mut rest = appended.body();
        while !rest.is_empty() {
            let length =
                usize::from_str_radix(std::str::from_utf8(&rest[..4]).unwrap(), 16).unwrap();
            if length == 0 {
                assert_eq!(rest.len(), 4);
                break;
            }
            assert!((4..=65_520).contains(&length));
            assert!(length <= rest.len());
            rest = &rest[length..];
        }
    }

    /// The lines ride band 2, before the flush that ends the stream, and the
    /// report-status the client acts on is left exactly as git wrote it.
    #[test]
    fn remote_lines_go_on_band_two_ahead_of_the_terminating_flush() {
        let mut report = Vec::new();
        report.extend_from_slice(&pkt_line("\u{1}000eunpack ok\n0000"));
        report.extend_from_slice(b"0000");
        let response =
            GitHttpResponse::bytes(200, "application/x-git-receive-pack-result", report.clone())
                .with_header("Cache-Control", "no-cache");

        let appended = append_sideband(response, &["#7 already merged.".to_string()]);
        let body = appended.body();
        assert_eq!(appended.header("Cache-Control"), Some("no-cache"));
        assert_eq!(
            appended.content_type(),
            "application/x-git-receive-pack-result"
        );
        assert!(
            body.starts_with(&report[..report.len() - 4]),
            "the report git wrote is untouched: {:?}",
            String::from_utf8_lossy(body)
        );
        assert!(
            body.ends_with(b"0000"),
            "the stream still ends with a flush"
        );
        let expected = pkt_line("\u{2}#7 already merged.\n");
        assert!(
            body.windows(expected.len())
                .any(|window| window == expected),
            "the message is a band-2 packet: {:?}",
            String::from_utf8_lossy(body)
        );
    }

    /// Nothing to say, or a body that is not a flush-terminated stream: the
    /// answer is handed back byte for byte.
    #[test]
    fn an_answer_is_left_alone_when_there_is_nothing_to_add_to_it() {
        let response = || {
            GitHttpResponse::bytes(
                200,
                "application/x-git-receive-pack-result",
                b"0000".to_vec(),
            )
        };
        assert_eq!(append_sideband(response(), &[]).body(), b"0000");
        let unterminated = GitHttpResponse::bytes(
            200,
            "application/x-git-receive-pack-result",
            b"nope".to_vec(),
        );
        assert_eq!(
            append_sideband(unterminated, &["said nothing".to_string()]).body(),
            b"nope"
        );
    }

    #[tokio::test]
    async fn a_slow_post_push_bridge_does_not_hold_the_response() {
        let response = (StatusCode::OK, "ok").into_response();
        let (started, was_started) = std::sync::mpsc::channel();
        let (finished, was_finished) = std::sync::mpsc::channel();

        let handed_back = std::time::Instant::now();
        spawn_post_push(&response, move || {
            started.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(2));
            finished.send(()).unwrap();
        });
        let elapsed = handed_back.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "spawning the bridge took {elapsed:?}"
        );
        was_started
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("bridge runs detached");
        was_finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("bridge runs to completion");
    }

    #[tokio::test]
    async fn a_failed_receive_pack_runs_no_post_push_bridge() {
        let response = (StatusCode::FORBIDDEN, "no").into_response();
        let (ran, was_run) = std::sync::mpsc::channel();

        spawn_post_push(&response, move || ran.send(()).unwrap());

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(was_run.try_recv().is_err());
    }

    #[test]
    fn git_rpc_encoding_is_closed_and_forwards_only_git_protocol() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        headers.insert("git-protocol", HeaderValue::from_static("version=2"));
        assert_eq!(
            git_rpc_content_encoding(&headers),
            Ok(GitRpcContentEncoding::Gzip)
        );
        let forwarded = forwarded_git_headers(&headers);
        assert_eq!(
            forwarded.get("git-protocol").map(String::as_str),
            Some("version=2")
        );
        assert!(!forwarded.contains_key("content-encoding"));

        headers.insert(
            header::CONTENT_ENCODING,
            HeaderValue::from_static("gzip, br"),
        );
        assert_eq!(
            git_rpc_content_encoding(&headers),
            Err(GitRpcBodyError::UnsupportedEncoding)
        );
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
        assert_eq!(
            git_rpc_content_encoding(&headers),
            Err(GitRpcBodyError::UnsupportedEncoding)
        );
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("x-gzip"));
        assert_eq!(
            git_rpc_content_encoding(&headers),
            Ok(GitRpcContentEncoding::Gzip)
        );
        headers.append(
            header::CONTENT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        assert_eq!(
            git_rpc_content_encoding(&headers),
            Err(GitRpcBodyError::UnsupportedEncoding)
        );
    }

    #[test]
    fn gzip_git_rpc_body_decodes_exactly_and_rejects_hostile_forms() {
        let mut request = Vec::new();
        while request.len() < 2_048 {
            request.extend_from_slice(b"0032want 0123456789012345678901234567890123456789\n");
        }
        let encoded = Bytes::from(gzip(&request));
        assert_eq!(
            decode_git_rpc_body(GitRpcContentEncoding::Gzip, encoded.clone(), request.len())
                .unwrap(),
            request
        );
        assert_eq!(
            decode_git_rpc_body(
                GitRpcContentEncoding::Gzip,
                encoded.clone(),
                request.len() - 1
            ),
            Err(GitRpcBodyError::TooLarge)
        );

        let mut trailing = encoded.to_vec();
        trailing.extend_from_slice(b"not-a-second-content-coding");
        assert_eq!(
            decode_git_rpc_body(
                GitRpcContentEncoding::Gzip,
                Bytes::from(trailing),
                request.len() + 64,
            ),
            Err(GitRpcBodyError::MalformedGzip)
        );
        let truncated = encoded.slice(..encoded.len() - 1);
        assert_eq!(
            decode_git_rpc_body(GitRpcContentEncoding::Gzip, truncated, request.len()),
            Err(GitRpcBodyError::MalformedGzip)
        );
        assert_eq!(
            decode_git_rpc_body(
                GitRpcContentEncoding::Gzip,
                Bytes::from_static(b"not gzip"),
                1024,
            ),
            Err(GitRpcBodyError::MalformedGzip)
        );
    }

    #[tokio::test]
    async fn git_rpc_reader_bounds_identity_and_gzip_bodies() {
        let plain = Bytes::from_static(b"0008done");
        assert_eq!(
            read_git_rpc_body(&HeaderMap::new(), Body::from(plain.clone()), 0, plain.len(),)
                .await
                .unwrap(),
            plain
        );
        assert_eq!(
            read_git_rpc_body(
                &HeaderMap::new(),
                Body::from(plain.clone()),
                usize::MAX,
                plain.len() - 1,
            )
            .await,
            Err(GitRpcBodyError::TooLarge)
        );

        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        let decoded = b"12345678";
        let encoded = gzip(decoded);
        assert_eq!(
            read_git_rpc_body(
                &headers,
                Body::from(encoded.clone()),
                encoded.len(),
                decoded.len(),
            )
            .await
            .unwrap(),
            decoded
        );
        assert_eq!(
            read_git_rpc_body(
                &headers,
                Body::from(encoded.clone()),
                encoded.len() - 1,
                decoded.len(),
            )
            .await,
            Err(GitRpcBodyError::TooLarge)
        );
        assert_eq!(
            read_git_rpc_body(&headers, Body::from(encoded), usize::MAX, decoded.len() - 1,).await,
            Err(GitRpcBodyError::TooLarge)
        );
    }
}
