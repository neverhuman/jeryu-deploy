//! Isolated `gh api` smoke test for `jeryu gh-setup`.
//!
//! One `jeryu gh-setup` against a throwaway `GH_CONFIG_DIR`, then the real `gh`
//! binary talks REST to a real `jeryu serve` on loopback: identity, repository
//! read, draft-PR create and PR read. Credential failures (missing, expired,
//! insufficient scope) run against a scripted fixture that records every request.
//! Nothing here reads or writes a real credential: the token is a fixture string,
//! `HOME` and `GH_CONFIG_DIR` are temp dirs and the child environment is cleared.
//! This proves `gh api`; it does not claim `gh pr` compatibility.
//!
//! `gh` and `git` must be on PATH. When one is missing the test skips, unless
//! `JERYU_REQUIRE_GH` is set, which turns the skip into a failure.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use jeryu_api::web::{WebServerConfig, serve};
use jeryu_cli::cli::Cli;
use jeryu_cli::{InMemoryClient, dispatch_with_api_url_env};

const TOKEN: &str = "fixture-gh-smoke-token-0000";
const GITHUB_ENTRY: &str = "github.com:\n    oauth_token: gho_fixture_github_com\n    git_protocol: https\n    user: octo-fixture\n";
const OTHER_ENTRY: &str = "git.other.example:\n    oauth_token: other-fixture\n    user: someone\n";

fn tool_available(tool: &str) -> bool {
    let present = Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(
        present || std::env::var_os("JERYU_REQUIRE_GH").is_none(),
        "{tool} is required (JERYU_REQUIRE_GH is set) but is not on PATH"
    );
    present
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("probe port")
        .local_addr()
        .expect("addr")
        .port()
}

/// The supported gh: `gh version X.Y.Z (date)`. Recorded beside the test
/// binaries and required to be at least 2.40 (`gh api` host-entry format).
fn record_gh_version() -> String {
    let out = Command::new("gh").arg("--version").output().expect("gh");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let first = text.lines().next().unwrap_or_default().to_string();
    let version = first
        .strip_prefix("gh version ")
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("unrecognized gh --version output: {first:?}"));
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    assert!(
        (major, minor) >= (2, 40),
        "gh {version} is older than the supported 2.40"
    );
    let record = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gh-api-smoke-version.txt");
    std::fs::write(&record, format!("{first}\n")).expect("record gh version");
    eprintln!("gh api smoke: {first} (recorded in {})", record.display());
    first
}

/// `gh` with a cleared environment: only the isolated config dir and HOME.
fn gh(home: &Path, config_dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new("gh")
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GH_CONFIG_DIR", config_dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .output()
        .expect("run gh");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn gh_setup(args: &[&str]) -> (i32, String, String) {
    let mut argv = vec!["jeryu", "gh-setup"];
    argv.extend_from_slice(args);
    let cli = Cli::try_parse_from(argv).expect("argv parses");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = dispatch_with_api_url_env(cli, &InMemoryClient::new(), &mut out, &mut err, || None);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

struct Sandbox {
    _root: tempfile::TempDir,
    home: PathBuf,
    config_dir: PathBuf,
    token_file: PathBuf,
}

impl Sandbox {
    fn new(seed_hosts: &str) -> Self {
        let root = tempfile::tempdir().expect("sandbox");
        let home = root.path().join("home");
        let config_dir = root.path().join("gh-config");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        if !seed_hosts.is_empty() {
            std::fs::write(config_dir.join("hosts.yml"), seed_hosts).unwrap();
        }
        let token_file = root.path().join("token");
        std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
        Self {
            _root: root,
            home,
            config_dir,
            token_file,
        }
    }

    fn hosts(&self) -> PathBuf {
        self.config_dir.join("hosts.yml")
    }

    fn gh(&self, args: &[&str]) -> (i32, String, String) {
        gh(&self.home, &self.config_dir, args)
    }
}

/// `gh api` against a plain-HTTP local forge. gh 2.45 rejects `--hostname
/// host:port` and reaches a `host:port` over https, so the endpoint is the full
/// `http://host:port/api/v3/...` URL; gh still takes the token from the
/// matching `hosts.yml` entry.
fn api_get_or(sandbox: &Sandbox, host: &str, path: &str, extra: &[&str]) -> (i32, String, String) {
    let url = format!("http://{host}/api/v3/{path}");
    let mut args = vec!["api"];
    args.extend_from_slice(extra);
    args.push(&url);
    sandbox.gh(&args)
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn wait_healthy(addr: SocketAddr) {
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(resp) = client.get(format!("http://{addr}/health")).send()
            && resp.status().is_success()
            && resp
                .json::<serde_json::Value>()
                .is_ok_and(|b| b["service"] == "jeryu-api")
        {
            return;
        }
        assert!(Instant::now() < deadline, "forge never became healthy");
        thread::sleep(Duration::from_millis(25));
    }
}

/// gh itself may add a `users:` block to a host it reads; the login and token
/// the fixture seeded must still be there.
fn github_login_intact(hosts: &str) -> bool {
    hosts.starts_with("github.com:\n    oauth_token: gho_fixture_github_com\n")
        && hosts.contains("user: octo-fixture")
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not json ({e}): {text}"))
}

#[test]
fn gh_api_reads_and_opens_a_draft_pr_after_one_setup() {
    if !tool_available("gh") || !tool_available("git") {
        eprintln!("gh or git missing; skipping the gh api smoke test");
        return;
    }
    record_gh_version();

    let data = tempfile::tempdir().expect("forge data");
    let spa = data.path().join("spa");
    std::fs::create_dir_all(&spa).unwrap();
    std::fs::write(spa.join("index.html"), "<!doctype html><div id=root></div>").unwrap();
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let config = WebServerConfig {
        bind: addr,
        spa_dir: spa,
        data_dir: data.path().join("data"),
        git_storage_root: data.path().join("git"),
        split_manifests: Vec::new(),
        auth_required: false,
        trust_local_dev: true,
        secure_cookies: false,
    };
    thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("forge runtime")
            .block_on(async move { serve(config).await.expect("serve") });
    });
    wait_healthy(addr);

    // Disposable repository with a pushed head branch to open a PR from.
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap();
    let created = http
        .post(format!("http://{addr}/repos"))
        .json(&serde_json::json!({ "name": "demo" }))
        .send()
        .expect("create repo");
    assert_eq!(created.status().as_u16(), 201);
    let work = data.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let url = format!("http://{addr}/git/jeryu/demo.git");
    git(&work, &["clone", &url, "clone"]);
    let clone = work.join("clone");
    git(&clone, &["config", "user.email", "fixture@jeryu.invalid"]);
    git(&clone, &["config", "user.name", "Fixture"]);
    std::fs::write(clone.join("README.md"), "gh api smoke\n").unwrap();
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-m", "fixture head"]);
    git(&clone, &["push", "origin", "HEAD:refs/heads/feature"]);

    // Setup: github.com and an unrelated host are seeded and must survive.
    let sandbox = Sandbox::new(&format!("{GITHUB_ENTRY}{OTHER_ENTRY}"));
    let host = format!("localhost:{port}");
    let alias = format!("127.0.0.1:{port}");
    let base = format!("http://{host}");
    let hosts = sandbox.hosts();
    let setup_args = [
        "--host",
        base.as_str(),
        "--alias",
        alias.as_str(),
        "--token-file",
        sandbox.token_file.to_str().unwrap(),
        "--path",
        hosts.to_str().unwrap(),
    ];
    let (code, out, err) = gh_setup(&setup_args);
    assert_eq!(code, 0, "setup: {out}{err}");
    assert!(!out.contains(TOKEN) && !err.contains(TOKEN), "token leaked");
    let first = std::fs::read_to_string(&hosts).unwrap();
    assert!(first.starts_with(&format!("{GITHUB_ENTRY}{OTHER_ENTRY}")));
    assert!(first.contains(&format!("{host}:\n")) && first.contains(&format!("{alias}:\n")));
    // Rerun is a no-op on disk.
    let (code, ..) = gh_setup(&setup_args);
    assert_eq!(code, 0);
    assert_eq!(std::fs::read_to_string(&hosts).unwrap(), first);

    let on = |path: &str| api_get_or(&sandbox, &host, path, &[]);

    let (code, out, err) = on("user");
    assert_eq!(code, 0, "gh api user: {out}{err}");
    assert!(
        json(&out)["login"].as_str().is_some_and(|l| !l.is_empty()),
        "identity: {out}"
    );

    let (code, out, err) = on("repos/jeryu/demo");
    assert_eq!(code, 0, "gh api repo: {out}{err}");
    assert_eq!(json(&out)["name"], "demo");

    let (code, out, err) = api_get_or(
        &sandbox,
        &host,
        "repos/jeryu/demo/pulls",
        &[
            "-X",
            "POST",
            "-f",
            "title=gh api smoke",
            "-f",
            "head=feature",
            "-f",
            "base=main",
            "-F",
            "draft=true",
        ],
    );
    assert_eq!(code, 0, "gh api create pr: {out}{err}");
    let pr = json(&out);
    assert_eq!(pr["draft"], true, "pr: {out}");
    let number = pr["number"].as_u64().expect("pr number").to_string();

    let (code, out, err) = on(&format!("repos/jeryu/demo/pulls/{number}"));
    assert_eq!(code, 0, "gh api read pr: {out}{err}");
    let read = json(&out);
    assert_eq!(read["number"].to_string(), number);
    assert_eq!(read["draft"], true);
    assert_eq!(read["title"], "gh api smoke");

    // The alias host resolves to the same forge.
    let (code, out, err) = api_get_or(&sandbox, &alias, "user", &[]);
    assert_eq!(code, 0, "alias host: {out}{err}");

    // The github.com login was never touched.
    let after = std::fs::read_to_string(&hosts).unwrap();
    assert!(github_login_intact(&after), "github.com changed: {after}");
    assert!(after.contains("oauth_token: other-fixture"), "{after}");
}

/// Answers each connection with the next scripted reply and records the request line.
fn scripted_forge(replies: Vec<(&'static str, &'static str)>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for (status, body) in replies.into_iter().cycle() {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut length = 0usize;
            let mut auth = String::new();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap_or(0);
                } else if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("authorization")
                {
                    auth = value.trim().to_string();
                }
            }
            let mut raw = vec![0; length];
            let _ = reader.read_exact(&mut raw);
            log.lock()
                .unwrap()
                .push(format!("{}|{auth}", line.trim_end()));
            let reply = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (port, seen)
}

fn configured(port: u16) -> Sandbox {
    let sandbox = Sandbox::new(GITHUB_ENTRY);
    let base = format!("http://localhost:{port}");
    let hosts = sandbox.hosts();
    let (code, out, err) = gh_setup(&[
        "--host",
        &base,
        "--token-file",
        sandbox.token_file.to_str().unwrap(),
        "--path",
        hosts.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "setup: {out}{err}");
    sandbox
}

#[test]
fn gh_api_credential_failures_are_clear_single_attempts_without_the_token() {
    if !tool_available("gh") {
        eprintln!("gh missing; skipping the gh api credential-failure test");
        return;
    }
    let cases: [(&str, &str, &str, &str); 2] = [
        (
            "expired",
            "401 Unauthorized",
            r#"{"message":"Bad credentials"}"#,
            "401",
        ),
        (
            "insufficient scope",
            "403 Forbidden",
            r#"{"message":"Resource not accessible by personal access token"}"#,
            "403",
        ),
    ];
    for (label, status, body, code_text) in cases {
        let (port, seen) = scripted_forge(vec![(status, body)]);
        let sandbox = configured(port);
        let host = format!("localhost:{port}");
        let (code, out, err) = api_get_or(&sandbox, &host, "user", &[]);
        assert_ne!(code, 0, "{label}: gh api must fail");
        let said = format!("{out}{err}");
        assert!(said.contains(code_text), "{label}: unclear failure: {said}");
        assert!(!said.contains(TOKEN), "{label}: token disclosed: {said}");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "{label}: gh must make exactly one request, no login or retry"
        );
        // A failed call must not rewrite the host entry or the GitHub.com login.
        let hosts = std::fs::read_to_string(sandbox.hosts()).unwrap();
        assert!(github_login_intact(&hosts), "{label}: github.com changed");
    }
}

#[test]
fn missing_credentials_fail_before_any_write_and_unconfigured_hosts_get_no_token() {
    if !tool_available("gh") {
        eprintln!("gh missing; skipping the gh api missing-credential test");
        return;
    }
    let (port, seen) = scripted_forge(vec![(
        "401 Unauthorized",
        r#"{"message":"Requires authentication"}"#,
    )]);
    let sandbox = Sandbox::new(GITHUB_ENTRY);
    let base = format!("http://localhost:{port}");
    let hosts = sandbox.hosts();
    let missing = sandbox.home.join("no-such-token");
    let (code, out, err) = gh_setup(&[
        "--host",
        &base,
        "--token-file",
        missing.to_str().unwrap(),
        "--path",
        hosts.to_str().unwrap(),
    ]);
    assert_ne!(code, 0, "setup with a missing token file must fail");
    assert!(
        err.contains("cannot read gh token file") || out.contains("cannot read gh token file"),
        "unclear error: {out}{err}"
    );
    assert_eq!(std::fs::read_to_string(&hosts).unwrap(), GITHUB_ENTRY);

    // An unconfigured host gets one anonymous request: gh sends no credential
    // and does not start a login or hunt for one elsewhere.
    let host = format!("localhost:{port}");
    let (code, out, err) = api_get_or(&sandbox, &host, "user", &[]);
    assert_ne!(code, 0, "gh api without credentials must fail: {out}{err}");
    assert!(!format!("{out}{err}").contains(TOKEN));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "exactly one request, got {seen:?}");
    assert!(seen[0].ends_with('|'), "no credential expected: {seen:?}");
    drop(seen);

    // An empty token file is refused, not written.
    let empty = sandbox.home.join("empty-token");
    std::fs::write(&empty, "\n").unwrap();
    let (code, ..) = gh_setup(&[
        "--host",
        &base,
        "--token-file",
        empty.to_str().unwrap(),
        "--path",
        hosts.to_str().unwrap(),
    ]);
    assert_ne!(code, 0);
    assert!(github_login_intact(
        &std::fs::read_to_string(&hosts).unwrap()
    ));
}
