use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
};

#[test]
fn actions_send_bearer_with_exact_contracts() {
    let directory = tempfile::tempdir().unwrap();
    let body = directory.path().join("body");
    std::fs::write(&body, "text").unwrap();
    let body = body.to_str().unwrap();
    let sha = "a".repeat(40);
    let cases = [
        (
            vec![
                "open",
                "--head",
                "topic",
                "--base",
                "main",
                "--title",
                "Title",
                "--body-file",
                body,
            ],
            "/api/v3/repos/team/repo/pulls",
            serde_json::json!({"head":"topic","base":"main","title":"Title","body":"text","draft":true}),
        ),
        (
            vec!["comment", "3", "--head-sha", &sha, "--body-file", body],
            "/api/v1/repos/team/repo/pulls/3/reviews",
            serde_json::json!({"verdict":"comment","expected_head_sha":sha,"body_markdown":"text","thread_comments":[]}),
        ),
        (
            vec!["close-as-replaced", "3", "--replacement", "4"],
            "/api/v1/repos/team/repo/pulls/3/close",
            serde_json::json!({"superseded_by":4}),
        ),
        (
            vec!["ready", "3"],
            "/api/v1/repos/team/repo/pulls/3/ready",
            serde_json::json!({}),
        ),
        (
            vec!["reviews", "3"],
            "/api/v1/repos/team/repo/pulls/3",
            serde_json::Value::Null,
        ),
    ];
    for (args, route, payload) in cases {
        let spec = request::spec(Args::parse_from(
            ["forge", "--repo", "team/repo"].into_iter().chain(args),
        ))
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut byte = [0];
            while !bytes.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            let headers = String::from_utf8(bytes).unwrap().to_lowercase();
            assert!(headers.starts_with(&format!(
                "{} {route} http/1.1",
                if payload.is_null() { "get" } else { "post" }
            )));
            assert!(headers.contains("authorization: bearer test-secret\r\n"));
            let length: usize = headers
                .lines()
                .find_map(|s| s.strip_prefix("content-length: "))
                .unwrap_or("0")
                .parse()
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            if !payload.is_null() {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                    payload
                );
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .unwrap();
        });
        assert_eq!(transport::send(&origin, "test-secret", spec).unwrap(), "{}");
        server.join().unwrap();
    }
}

#[test]
fn custody_rejects_symlinks_permissions_and_unbounded_tokens() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("token");
    std::fs::write(&path, "secret\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(transport::read_token(&path).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(transport::read_token(&path).unwrap(), "secret");
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(transport::read_token(&link).is_err());
    for token in ["".to_owned(), "a b".to_owned(), "a".repeat(4097)] {
        std::fs::write(&path, token).unwrap();
        assert!(transport::read_token(&path).is_err());
    }
}

#[test]
fn unsafe_inputs_and_approval_are_refused() {
    for origin in [
        "http://forge.invalid",
        "https://user:secret@forge.invalid",
        "https://forge.invalid/path",
        "https://forge.invalid?query",
    ] {
        assert!(transport::origin(origin).is_err());
    }
    assert_eq!(
        transport::origin("https://forge.invalid/").unwrap(),
        "https://forge.invalid"
    );
    for args in [vec!["approve", "1"], vec!["merge", "1"]] {
        assert!(
            Args::try_parse_from(["forge", "--repo", "team/repo"].into_iter().chain(args)).is_err()
        );
    }
    for (repo, args) in [
        ("../repo", vec!["reviews", "1"]),
        ("team/repo", vec!["ready", "0"]),
        (
            "team/repo",
            vec!["close-as-replaced", "1", "--replacement", "1"],
        ),
    ] {
        assert!(
            request::spec(Args::parse_from(
                ["forge", "--repo", repo].into_iter().chain(args)
            ))
            .is_err()
        );
    }
}

#[test]
fn redirects_and_forbidden_responses_fail_closed() {
    for status in ["302 Found", "403 Forbidden"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = [0; 4096];
            assert!(stream.read(&mut data).unwrap() > 0);
            write!(stream, "HTTP/1.1 {status}\r\nLocation: /elsewhere\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecret").unwrap();
        });
        let spec = request::spec(Args::parse_from([
            "forge",
            "--repo",
            "team/repo",
            "reviews",
            "1",
        ]))
        .unwrap();
        assert!(transport::send(&origin, "test-secret", spec).is_err());
        server.join().unwrap();
    }
}
