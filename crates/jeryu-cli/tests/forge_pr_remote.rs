//! `forge pr` view/checks/comment/close/approve/regate and `auth doctor`:
//! token-file Bearer auth, and a refused token stopping before any write.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;

use clap::Parser;
use jeryu_cli::cli::Cli;
use jeryu_cli::{InMemoryClient, dispatch_with_api_url_env};

const TOKEN: &str = "fixture-secret-token-value";

#[derive(Clone, Debug)]
struct Seen {
    request_line: String,
    authorization: String,
    body: String,
}

/// Answers each connection with the next `(status, body)`, recording what it saw.
fn fixture(replies: Vec<(&'static str, &'static str)>) -> (SocketAddr, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_thread = Arc::clone(&seen);
    thread::spawn(move || {
        for (status, body) in replies {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");
            let (mut length, mut authorization) = (0usize, String::new());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                if line.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().expect("length");
                    } else if name.eq_ignore_ascii_case("authorization") {
                        authorization = value.trim().to_string();
                    }
                }
            }
            let mut raw = vec![0; length];
            reader.read_exact(&mut raw).expect("body");
            seen_thread.lock().expect("lock").push(Seen {
                request_line: request_line.trim_end().to_string(),
                authorization,
                body: String::from_utf8(raw).expect("utf8"),
            });
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).expect("reply");
        }
    });
    (addr, seen)
}

fn token_file() -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("token file");
    writeln!(file, "{TOKEN}").expect("write token");
    file
}

fn run(argv: &[&str]) -> (i32, String, String) {
    let cli = Cli::try_parse_from(argv).expect("argv parses");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = dispatch_with_api_url_env(cli, &InMemoryClient::new(), &mut out, &mut err, || None);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn lines(seen: &Arc<Mutex<Vec<Seen>>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|s| s.request_line.clone())
        .collect()
}

#[test]
fn a_refused_token_stops_every_write_after_one_whoami() {
    for write in [
        &["comment", "--repo", "acme/w", "--pr", "3", "--body", "hi"][..],
        &["close", "--repo", "acme/w", "--pr", "3"],
        &[
            "approve",
            "--repo",
            "acme/w",
            "--pr",
            "3",
            "--expected-head-sha",
            "abc123",
        ],
        &["regate", "--repo", "acme/w", "--pr", "3"],
    ] {
        let (addr, seen) = fixture(vec![(
            "401 Unauthorized",
            r#"{"message":"Bad credentials"}"#,
        )]);
        let url = format!("http://{addr}");
        let file = token_file();
        let path = file.path().to_str().unwrap();
        let mut argv = vec![
            "jeryu",
            "--api-url",
            &url,
            "--token-file",
            path,
            "forge",
            "pr",
        ];
        argv.extend_from_slice(write);
        let (code, out, err) = run(&argv);
        assert_ne!(code, 0, "{write:?}");
        assert_eq!(lines(&seen), ["GET /api/v3/user HTTP/1.1"], "{write:?}");
        assert!(!out.contains(TOKEN) && !err.contains(TOKEN));
    }
}

#[test]
fn a_write_goes_out_with_bearer_auth_after_whoami() {
    let (addr, seen) = fixture(vec![
        ("200 OK", r#"{"login":"agent-bot"}"#),
        ("201 Created", r#"{"id":"c1"}"#),
    ]);
    let url = format!("http://{addr}");
    let file = token_file();
    let (code, out, err) = run(&[
        "jeryu",
        "--api-url",
        &url,
        "--token-file",
        file.path().to_str().unwrap(),
        "forge",
        "pr",
        "comment",
        "--repo",
        "acme/w",
        "--pr",
        "7",
        "--body",
        "looks good",
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("commented on #7"));
    assert!(!out.contains(TOKEN) && !err.contains(TOKEN));
    assert_eq!(
        lines(&seen),
        [
            "GET /api/v3/user HTTP/1.1",
            "POST /api/v1/repos/acme%2Fw/pulls/7/comments HTTP/1.1"
        ]
    );
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .all(|s| s.authorization == format!("Bearer {TOKEN}"))
    );
    assert_eq!(seen[1].body, r#"{"body_markdown":"looks good"}"#);
}

#[test]
fn no_token_sends_nothing() {
    let (addr, seen) = fixture(vec![]);
    let url = format!("http://{addr}");
    // JERYU_TOKEN_FILE / JERYU_TOKEN may be set on the host; an explicit
    // unreadable file is the deterministic way to have no usable token.
    let (code, _, err) = run(&[
        "jeryu",
        "--api-url",
        &url,
        "--token-file",
        "/nonexistent/token",
        "forge",
        "pr",
        "regate",
        "--repo",
        "acme/w",
        "--pr",
        "1",
    ]);
    assert_eq!(code, 4);
    assert!(err.contains("token"));
    assert!(lines(&seen).is_empty());
}

#[test]
fn checks_and_view_read_without_a_write_preflight() {
    let (addr, seen) = fixture(vec![
        (
            "200 OK",
            r#"{"total":2,"passing":1,"failing":1,"pending":0,"skipped":0,"checks":[]}"#,
        ),
        (
            "200 OK",
            r#"{"number":4,"title":"T","state":"open","head":{"ref":"h","sha":"abc"},"base":{"ref":"main"}}"#,
        ),
    ]);
    let url = format!("http://{addr}");
    let file = token_file();
    let path = file.path().to_str().unwrap();
    let (code, out, _) = run(&[
        "jeryu",
        "--api-url",
        &url,
        "--token-file",
        path,
        "forge",
        "pr",
        "checks",
        "--repo",
        "acme/w",
        "--pr",
        "4",
    ]);
    assert_eq!(code, 0);
    assert!(out.contains("2 checks: 1 passing, 1 failing"));
    let (code, out, _) = run(&[
        "jeryu",
        "--api-url",
        &url,
        "--token-file",
        path,
        "forge",
        "pr",
        "view",
        "--repo",
        "acme/w",
        "--pr",
        "4",
    ]);
    assert_eq!(code, 0);
    assert!(out.contains("#4 T [open] h -> main"));
    assert_eq!(
        lines(&seen),
        [
            "GET /api/v1/repos/acme%2Fw/pulls/4/checks HTTP/1.1",
            "GET /api/v3/repos/acme/w/pulls/4 HTTP/1.1"
        ]
    );
}

#[test]
fn approve_without_a_sha_approves_the_head_the_forge_reports() {
    let (addr, seen) = fixture(vec![
        ("200 OK", r#"{"login":"agent-bot"}"#),
        ("200 OK", r#"{"head":{"ref":"h","sha":"deadbeef"}}"#),
        ("200 OK", r#"{"ok":true}"#),
    ]);
    let url = format!("http://{addr}");
    let file = token_file();
    let (code, _, err) = run(&[
        "jeryu",
        "--api-url",
        &url,
        "--token-file",
        file.path().to_str().unwrap(),
        "forge",
        "pr",
        "approve",
        "--repo",
        "acme/w",
        "--pr",
        "9",
    ]);
    assert_eq!(code, 0, "{err}");
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[2].request_line,
        "POST /api/v1/repos/acme%2Fw/pulls/9/approve HTTP/1.1"
    );
    assert_eq!(seen[2].body, r#"{"expected_head_sha":"deadbeef"}"#);
}

#[test]
fn auth_doctor_reports_the_login_and_fails_on_a_refused_token() {
    let file = token_file();
    let path = file.path().to_str().unwrap();
    let (addr, seen) = fixture(vec![("200 OK", r#"{"login":"agent-bot"}"#)]);
    let url = format!("http://{addr}");
    let (code, out, _) = run(&[
        "jeryu",
        "--api-url",
        &url,
        "--token-file",
        path,
        "auth",
        "doctor",
    ]);
    assert_eq!(code, 0);
    assert!(out.contains("authenticated as agent-bot"));
    assert!(!out.contains(TOKEN));
    assert_eq!(lines(&seen), ["GET /api/v3/user HTTP/1.1"]);

    let (addr, _) = fixture(vec![(
        "401 Unauthorized",
        r#"{"message":"Bad credentials"}"#,
    )]);
    let url = format!("http://{addr}");
    let (code, out, err) = run(&[
        "jeryu",
        "--api-url",
        &url,
        "--token-file",
        path,
        "auth",
        "doctor",
    ]);
    assert_eq!(code, 6);
    assert!(!out.contains(TOKEN) && !err.contains(TOKEN));
}
