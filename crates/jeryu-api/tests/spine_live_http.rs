#![cfg(feature = "web")]
//! S4 live-HTTP e2e: boot `serve()` on a real loopback socket, create a repo
//! over HTTP (which materializes a bare repo on disk), then clone, commit, push
//! an allowed branch over smart-HTTP, and prove a direct client push to
//! `refs/heads/main` is rejected. This exercises create-repo-to-disk (S3), the
//! git transport, and the main-protection receive policy on unified `jeryu serve`
//! (S4) end to end.

use std::fs::File;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;
use jeryu_api::web::{WebServerConfig, serve};
use sha2::Digest;

mod common;

fn git_available() -> bool {
    common::git_command()
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn git_lfs_available() -> bool {
    common::git_command()
        .args(["lfs", "version"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = common::git_command()
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

fn run_git_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) {
    let mut command = common::git_command();
    command.args(args).current_dir(dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    let status = command
        .status()
        .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

fn run_git_failure(dir: &Path, args: &[&str]) -> String {
    let output = common::git_command()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
    assert!(
        !output.status.success(),
        "git {args:?} unexpectedly succeeded in {}",
        dir.display()
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn write_bytes(path: &Path, seed: u8, len: usize) -> Vec<u8> {
    let bytes: Vec<u8> = (0..len)
        .map(|idx| seed.wrapping_add((idx % 251) as u8))
        .collect();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

fn write_incompressible_file(path: &Path, len: usize) {
    let mut file = File::create(path).unwrap();
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15 ^ (len as u64);
    let mut remaining = len;
    let mut buffer = vec![0u8; 8192];
    while remaining > 0 {
        for chunk in buffer.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bytes = state.to_le_bytes();
            let len = chunk.len();
            chunk.copy_from_slice(&bytes[..len]);
        }
        let write_len = remaining.min(buffer.len());
        file.write_all(&buffer[..write_len]).unwrap();
        remaining -= write_len;
    }
    file.flush().unwrap();
}

/// Git http config that aborts a stalled transfer instead of hanging.
const GIT_HTTP_GUARD: &[&str] = &["-c", "http.lowSpeedLimit=100", "-c", "http.lowSpeedTime=20"];

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let length = payload.len() + 4;
    assert!(length <= 0xffff);
    let mut encoded = format!("{length:04x}").into_bytes();
    encoded.extend_from_slice(payload);
    encoded
}

/// A stub SPA shell, so tests build without an embedded jeryu-web dist. The
/// readiness probe deliberately does not go through it: see `backend_is_ready`.
fn write_spa_shell(spa_dir: &std::path::Path) {
    std::fs::create_dir_all(spa_dir).unwrap();
    std::fs::write(
        spa_dir.join("index.html"),
        r#"<!doctype html><html><body><div id="root"></div></body></html>"#,
    )
    .unwrap();
}

/// Probe the backend `/health` route and require its JSON body. An unknown path
/// is answered by the SPA fallback with the HTML shell, so a status-only check
/// on a path the backend does not serve would pass against a broken server.
async fn backend_is_ready(client: &reqwest::Client, addr: SocketAddr) -> bool {
    match client.get(format!("http://{addr}/health")).send().await {
        Ok(response) if response.status().is_success() => response
            .json::<serde_json::Value>()
            .await
            .is_ok_and(|body| body["status"] == "ok" && body["service"] == "jeryu-api"),
        _ => false,
    }
}

async fn wait_until_listening(addr: SocketAddr, server: &mut tokio::task::JoinHandle<()>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    loop {
        if server.is_finished() {
            let result = server.await;
            panic!("server task exited before readiness on {addr}: {result:?}");
        }
        if backend_is_ready(&client, addr).await {
            tokio::task::yield_now().await;
            if server.is_finished() {
                let result = server.await;
                panic!("server task exited before readiness on {addr}: {result:?}");
            }
            return;
        }
        assert!(Instant::now() < deadline, "server never listened on {addr}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
#[should_panic(expected = "server task exited before readiness")]
async fn readiness_rejects_an_already_exited_server_task() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut server = tokio::spawn(async {});
    tokio::task::yield_now().await;

    wait_until_listening(addr, &mut server).await;
}

/// A server that answers every path with the SPA shell must not read as ready:
/// that is exactly the shape a broken backend behind an SPA fallback has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_rejects_a_server_serving_only_the_spa_shell() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let shell = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let body = r#"<!doctype html><html><body><div id="root"></div></body></html>"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert!(
        !backend_is_ready(&client, addr).await,
        "SPA shell response passed as a healthy backend"
    );
    shell.join().unwrap();
}

fn assert_lfs_content_type(resp: &reqwest::Response) {
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.starts_with("application/vnd.git-lfs+json"),
        "expected Git LFS JSON content type, got {content_type}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s4_create_repo_to_disk_and_git_push_over_http_blocks_main() {
    if !git_available() {
        eprintln!("git unavailable; skipping s4 live-HTTP e2e");
        return;
    }

    let base = std::env::temp_dir().join(format!("jeryu-s4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let git_root = base.join("git");
    let spa_dir = base.join("spa");
    let work = base.join("work");
    write_spa_shell(&spa_dir);
    std::fs::create_dir_all(&work).unwrap();

    // Reserve a free loopback port, then release it for serve() to bind.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let config = WebServerConfig {
        bind: addr,
        spa_dir,
        data_dir,
        git_storage_root: git_root.clone(),
        split_manifests: Vec::new(),
        auth_required: false,
        trust_local_dev: true,
        secure_cookies: false,
    };
    let mut server = tokio::spawn(async move { serve(config).await.unwrap() });
    wait_until_listening(addr, &mut server).await;

    // 1. Create the repo over HTTP -> materializes a bare repo on disk.
    eprintln!("[s4] POST /repos ...");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/repos"))
        .json(&serde_json::json!({ "name": "demo" }))
        .send()
        .await
        .expect("POST /repos");
    let status = resp.status().as_u16();
    eprintln!("[s4] POST /repos -> {status}");
    assert_eq!(status, 201, "POST /repos should return 201");
    let bare = git_root.join("jeryu").join("demo.git");
    assert!(
        bare.join("HEAD").is_file(),
        "bare repo must exist on disk after create"
    );
    eprintln!("[s4] bare repo materialized on disk");

    // 2. Clone over HTTP (loopback-permissive: no credentials required).
    let clone_url = format!("http://{addr}/git/jeryu/demo.git");
    eprintln!("[s4] git clone {clone_url}");
    run_git(
        &work,
        &[GIT_HTTP_GUARD, &["clone", clone_url.as_str(), "clone"]].concat(),
    );
    let clone_dir = work.join("clone");
    eprintln!("[s4] cloned");

    // 3. Commit and push back over the same transport to an allowed branch.
    run_git(
        &clone_dir,
        &["config", "user.email", "tester@jeryu.invalid"],
    );
    run_git(&clone_dir, &["config", "user.name", "Tester"]);
    write_incompressible_file(&clone_dir.join("hello.bin"), 3 * 1024 * 1024);
    // A GitHub-Actions workflow so the push path still carries realistic repo
    // contents; the branch itself is not the protected main ref.
    std::fs::create_dir_all(clone_dir.join(".github/workflows")).unwrap();
    std::fs::write(
        clone_dir.join(".github/workflows/ci.yml"),
        format!(
            "name: ci\non: [push]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: |\n          test -d .git\n          test \"$(git remote get-url origin)\" = \"{clone_url}\"\n          git rev-parse --verify origin/main\n          test \"$(git rev-parse HEAD)\" = \"$JERYU_COMMIT_SHA\"\n          test \"$(git rev-parse origin/main)\" = \"$JERYU_COMMIT_SHA\"\n          test \"$JERYU_NETWORK_POLICY\" = \"egress-only\"\n          test \"$JERYU_SECRETS\" = \"disabled\"\n          test -z \"${{GITHUB_TOKEN:-}}\"\n",
        ),
    )
    .unwrap();
    run_git(&clone_dir, &["add", "."]);
    run_git(&clone_dir, &["commit", "-m", "first commit"]);
    eprintln!("[s4] git push feature");
    run_git(
        &clone_dir,
        &[
            GIT_HTTP_GUARD,
            &["-c", "pack.compression=0", "-c", "core.compression=0"],
            &["push", "origin", "HEAD:refs/heads/feature"],
        ]
        .concat(),
    );
    eprintln!("[s4] pushed feature");

    let sha = String::from_utf8(
        common::git_command()
            .args(["-C", clone_dir.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    // 4. Assert the allowed branch landed in the on-disk bare repo.
    let feature = common::git_command()
        .args([
            "--git-dir",
            bare.to_str().unwrap(),
            "rev-parse",
            "refs/heads/feature",
        ])
        .output()
        .unwrap();
    assert!(
        feature.status.success(),
        "refs/heads/feature must exist in the bare repo after push: {}",
        String::from_utf8_lossy(&feature.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&feature.stdout).trim(), sha);

    // 5. The same HTTP client cannot advance protected main directly.
    eprintln!("[s4] git push main should be rejected");
    let failure = run_git_failure(
        &clone_dir,
        &[
            GIT_HTTP_GUARD,
            &["-c", "pack.compression=0", "-c", "core.compression=0"],
            &["push", "origin", "HEAD:refs/heads/main"],
        ]
        .concat(),
    );
    assert!(
        failure.contains("direct pushes to refs/heads/main are blocked")
            || failure.contains("The requested URL returned error: 403"),
        "main push should be rejected by the protected-ref policy: {failure}"
    );
    let main = common::git_command()
        .args([
            "--git-dir",
            bare.to_str().unwrap(),
            "rev-parse",
            "--verify",
            "refs/heads/main",
        ])
        .output()
        .unwrap();
    assert!(
        !main.status.success(),
        "refs/heads/main must not be created by a rejected direct push"
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s4_git_pack_rpc_routes_decode_gzip_before_git() {
    let base = std::env::temp_dir().join(format!("jeryu-s4-gzip-rpc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let git_root = base.join("git");
    let spa_dir = base.join("spa");
    write_spa_shell(&spa_dir);

    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let config = WebServerConfig {
        bind: addr,
        spa_dir,
        data_dir,
        git_storage_root: git_root,
        split_manifests: Vec::new(),
        auth_required: false,
        trust_local_dev: true,
        secure_cookies: false,
    };
    let mut server = tokio::spawn(async move { serve(config).await.unwrap() });
    wait_until_listening(addr, &mut server).await;

    let client = reqwest::Client::new();
    let create = client
        .post(format!("http://{addr}/repos"))
        .json(&serde_json::json!({ "name": "gzip-rpc" }))
        .send()
        .await
        .expect("POST /repos");
    assert_eq!(create.status().as_u16(), 201);

    // Protocol v2 permits repeated ref-prefix arguments. This is a valid
    // request larger than Git's request-compression threshold, matching the
    // complete-ref mirror failure that exposed the adapter bug.
    let mut upload_request = pkt_line(b"command=ls-refs\n");
    upload_request.extend_from_slice(b"0001");
    upload_request.extend_from_slice(&pkt_line(b"peel\n"));
    upload_request.extend_from_slice(&pkt_line(b"symrefs\n"));
    for index in 0..64 {
        upload_request.extend_from_slice(&pkt_line(
            format!("ref-prefix refs/heads/fixture-{index:03}\n").as_bytes(),
        ));
    }
    upload_request.extend_from_slice(b"0000");
    assert!(upload_request.len() > 1_024);

    let upload = client
        .post(format!(
            "http://{addr}/git/jeryu/gzip-rpc.git/git-upload-pack"
        ))
        .header(reqwest::header::CONTENT_ENCODING, "gzip")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-git-upload-pack-request",
        )
        .header("git-protocol", "version=2")
        .body(gzip(&upload_request))
        .send()
        .await
        .expect("gzip upload-pack POST");
    assert_eq!(upload.status().as_u16(), 200);
    assert_eq!(
        upload
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/x-git-upload-pack-result")
    );

    // A flush-only receive-pack request is valid and side-effect free. Keeping
    // this second route-level assertion prevents either handler from silently
    // dropping the shared decoder call.
    let receive = client
        .post(format!(
            "http://{addr}/git/jeryu/gzip-rpc.git/git-receive-pack"
        ))
        .header(reqwest::header::CONTENT_ENCODING, "x-gzip")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-git-receive-pack-request",
        )
        .body(gzip(b"0000"))
        .send()
        .await
        .expect("gzip receive-pack POST");
    assert_eq!(receive.status().as_u16(), 200);
    assert_eq!(
        receive
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/x-git-receive-pack-result")
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s4_git_lfs_batch_and_locks_verify_routes_return_protocol_json() {
    let base = std::env::temp_dir().join(format!("jeryu-s4-lfs-routes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let git_root = base.join("git");
    let spa_dir = base.join("spa");
    write_spa_shell(&spa_dir);

    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let config = WebServerConfig {
        bind: addr,
        spa_dir,
        data_dir,
        git_storage_root: git_root,
        split_manifests: Vec::new(),
        auth_required: false,
        trust_local_dev: true,
        secure_cookies: false,
    };
    let mut server = tokio::spawn(async move { serve(config).await.unwrap() });
    wait_until_listening(addr, &mut server).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/repos"))
        .json(&serde_json::json!({ "name": "lfs-routes" }))
        .send()
        .await
        .expect("POST /repos");
    assert_eq!(resp.status().as_u16(), 201);

    let credential = "Basic dGVzdC11c2VyOnRlc3QtdG9rZW4=";
    let object = b"authenticated-lfs-download";
    let oid = hex::encode(sha2::Sha256::digest(object));
    let upload = client
        .put(format!(
            "http://{addr}/git/jeryu/lfs-routes.git/info/lfs/objects/{oid}"
        ))
        .header(reqwest::header::AUTHORIZATION, credential)
        .body(object.to_vec())
        .send()
        .await
        .expect("PUT LFS object");
    assert_eq!(upload.status().as_u16(), 200);

    let batch = client
        .post(format!(
            "http://{addr}/git/jeryu/lfs-routes.git/info/lfs/objects/batch"
        ))
        .header(reqwest::header::AUTHORIZATION, credential)
        .json(&serde_json::json!({
            "operation": "download",
            "transfers": ["basic"],
            "objects": [{ "oid": oid, "size": object.len() }]
        }))
        .send()
        .await
        .expect("POST LFS batch");
    assert_eq!(batch.status().as_u16(), 200);
    assert_lfs_content_type(&batch);
    let batch_body: serde_json::Value = batch.json().await.expect("LFS batch JSON body");
    assert_eq!(batch_body["transfer"], "basic");
    assert_eq!(batch_body["objects"].as_array().unwrap().len(), 1);
    assert_eq!(
        batch_body["objects"][0]["actions"]["download"]["header"]["Authorization"],
        credential
    );

    let locks = client
        .post(format!(
            "http://{addr}/git/jeryu/lfs-routes.git/info/lfs/locks/verify"
        ))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("POST LFS locks verify");
    assert_eq!(locks.status().as_u16(), 200);
    assert_lfs_content_type(&locks);
    let locks_body: serde_json::Value = locks.json().await.expect("LFS locks verify JSON body");
    assert_eq!(locks_body["ours"].as_array().unwrap().len(), 0);
    assert_eq!(locks_body["theirs"].as_array().unwrap().len(), 0);

    server.abort();
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s4_git_lfs_cpkt_versions_roundtrip_over_http() {
    if !git_available() || !git_lfs_available() {
        eprintln!("git or git-lfs unavailable; skipping s4 LFS live-HTTP e2e");
        return;
    }

    let base = std::env::temp_dir().join(format!("jeryu-s4-lfs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let git_root = base.join("git");
    let spa_dir = base.join("spa");
    let work = base.join("work");
    write_spa_shell(&spa_dir);
    std::fs::create_dir_all(&work).unwrap();

    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let config = WebServerConfig {
        bind: addr,
        spa_dir,
        data_dir,
        git_storage_root: git_root.clone(),
        split_manifests: Vec::new(),
        auth_required: false,
        trust_local_dev: true,
        secure_cookies: false,
    };
    let mut server = tokio::spawn(async move { serve(config).await.unwrap() });
    wait_until_listening(addr, &mut server).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/repos"))
        .json(&serde_json::json!({ "name": "lfs-demo" }))
        .send()
        .await
        .expect("POST /repos");
    assert_eq!(resp.status().as_u16(), 201);

    let clone_url = format!("http://{addr}/git/jeryu/lfs-demo.git");
    run_git(
        &work,
        &[GIT_HTTP_GUARD, &["clone", clone_url.as_str(), "source"]].concat(),
    );
    let source = work.join("source");
    run_git(&source, &["config", "user.email", "tester@jeryu.invalid"]);
    run_git(&source, &["config", "user.name", "Tester"]);
    run_git(&source, &["lfs", "install", "--local", "--skip-repo"]);
    assert!(
        !source.join(".git/hooks/pre-push").exists(),
        "the live LFS fixture must not install or execute Git hooks"
    );
    run_git(&source, &["lfs", "track", "*.cpkt"]);

    let v1 = write_bytes(&source.join("model.cpkt"), 11, 4096);
    run_git(&source, &["add", ".gitattributes", "model.cpkt"]);
    run_git(&source, &["commit", "-m", "model v1"]);
    run_git(&source, &["lfs", "push", "origin", "HEAD"]);
    run_git(
        &source,
        &[
            GIT_HTTP_GUARD,
            &["push", "origin", "HEAD:refs/heads/feature"],
        ]
        .concat(),
    );

    let v2 = write_bytes(&source.join("model.cpkt"), 29, 6144);
    run_git(&source, &["add", "model.cpkt"]);
    run_git(&source, &["commit", "-m", "model v2"]);
    run_git(&source, &["lfs", "push", "origin", "HEAD"]);
    run_git(
        &source,
        &[
            GIT_HTTP_GUARD,
            &["push", "origin", "HEAD:refs/heads/feature"],
        ]
        .concat(),
    );

    run_git_env(
        &work,
        &[
            GIT_HTTP_GUARD,
            &[
                "clone",
                "--branch",
                "feature",
                clone_url.as_str(),
                "skip-smudge",
            ],
        ]
        .concat(),
        &[
            ("GIT_LFS_SKIP_SMUDGE", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ],
    );
    let skip = work.join("skip-smudge");
    let pointer = std::fs::read_to_string(skip.join("model.cpkt")).unwrap();
    assert!(
        pointer.contains("version https://git-lfs.github.com/spec/v1")
            && pointer.contains("oid sha256:"),
        "expected LFS pointer, got {pointer}"
    );

    // Host CI runs with GIT_CONFIG_NOSYSTEM=1 and no global LFS filters, so the
    // skip-smudge clone must install local LFS before pull can materialize payloads.
    run_git_env(
        &skip,
        &["lfs", "install", "--local", "--skip-repo"],
        &[
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ],
    );
    assert!(
        !skip.join(".git/hooks/pre-push").exists(),
        "the skip-smudge fixture must not install or execute Git hooks"
    );

    run_git(&skip, &["lfs", "pull"]);
    assert_eq!(std::fs::read(skip.join("model.cpkt")).unwrap(), v2);

    run_git(&skip, &["checkout", "HEAD~1"]);
    run_git(&skip, &["lfs", "pull"]);
    assert_eq!(std::fs::read(skip.join("model.cpkt")).unwrap(), v1);

    server.abort();
    let _ = std::fs::remove_dir_all(&base);
}

/// A real Git client must negotiate protocol v2 over smart HTTP and complete ls-remote and clone.
/// Forwarding Git-Protocol is what enables v2; the 2026-09-17 production outage showed that the
/// handcrafted RPC tests above do not prove a real client works, so this drives the git binary
/// and asserts from its packet trace that the server actually answered in version 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s4_real_git_client_negotiates_protocol_v2_over_http() {
    if !git_available() {
        eprintln!("[s4-v2] git not available; skipping");
        return;
    }
    let base = std::env::temp_dir().join(format!("jeryu-s4-git-v2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let git_root = base.join("git");
    let spa_dir = base.join("spa");
    let work = base.join("work");
    write_spa_shell(&spa_dir);
    std::fs::create_dir_all(&work).unwrap();

    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let config = WebServerConfig {
        bind: addr,
        spa_dir,
        data_dir,
        git_storage_root: git_root.clone(),
        split_manifests: Vec::new(),
        auth_required: false,
        trust_local_dev: true,
        secure_cookies: false,
    };
    let mut server = tokio::spawn(async move { serve(config).await.unwrap() });
    wait_until_listening(addr, &mut server).await;

    let create = reqwest::Client::new()
        .post(format!("http://{addr}/repos"))
        .json(&serde_json::json!({ "name": "v2-demo" }))
        .send()
        .await
        .expect("POST /repos");
    assert_eq!(create.status().as_u16(), 201);

    // Seed history straight into the bare repository: the transport under test is the read path.
    let bare = git_root.join("jeryu").join("v2-demo.git");
    let seed = work.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    run_git(&seed, &["init", "-q", "-b", "main"]);
    run_git(
        &seed,
        &[
            "-c",
            "user.name=Tester",
            "-c",
            "user.email=tester@jeryu.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "seed",
        ],
    );
    run_git(
        &seed,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "fetch",
            "-q",
            seed.to_str().unwrap(),
            "+main:refs/heads/main",
        ],
    );

    let url = format!("http://{addr}/git/jeryu/v2-demo.git");
    let trace = work.join("packet-trace");
    let ls_remote = common::git_command()
        .args(GIT_HTTP_GUARD)
        .args(["-c", "protocol.version=2", "ls-remote", url.as_str()])
        .env("GIT_TRACE_PACKET", &trace)
        .current_dir(&work)
        .output()
        .expect("git ls-remote");
    assert!(
        ls_remote.status.success(),
        "git ls-remote over v2 failed: {}",
        String::from_utf8_lossy(&ls_remote.stderr)
    );
    assert!(String::from_utf8_lossy(&ls_remote.stdout).contains("refs/heads/main"));
    let packets = std::fs::read_to_string(&trace).unwrap_or_default();
    assert!(
        packets.contains("< version 2"),
        "server did not answer in protocol v2; packet trace:\n{packets}"
    );

    run_git(
        &work,
        &[
            GIT_HTTP_GUARD,
            &[
                "-c",
                "protocol.version=2",
                "clone",
                "-q",
                url.as_str(),
                "clone",
            ],
        ]
        .concat(),
    );
    assert!(work.join("clone/.git").is_dir());

    server.abort();
    let _ = std::fs::remove_dir_all(&base);
}

/// A rename over the GitHub edge moves the bare repository on disk, and a
/// clone by the old URL still reaches it: the git transport follows the
/// old-name alias the way GitHub redirects a renamed repository.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s4_clone_by_old_url_after_rename() {
    if !git_available() {
        eprintln!("git unavailable; skipping rename clone e2e");
        return;
    }

    let base = std::env::temp_dir().join(format!("jeryu-s4-rename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let git_root = base.join("git");
    let spa_dir = base.join("spa");
    let work = base.join("work");
    write_spa_shell(&spa_dir);
    std::fs::create_dir_all(&work).unwrap();

    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let config = WebServerConfig {
        bind: addr,
        spa_dir,
        data_dir: base.join("data"),
        git_storage_root: git_root.clone(),
        split_manifests: Vec::new(),
        auth_required: false,
        trust_local_dev: true,
        secure_cookies: false,
    };
    let mut server = tokio::spawn(async move { serve(config).await.unwrap() });
    wait_until_listening(addr, &mut server).await;

    let client = reqwest::Client::new();
    let created = client
        .post(format!("http://{addr}/repos"))
        .json(&serde_json::json!({ "name": "before" }))
        .send()
        .await
        .expect("POST /repos");
    assert_eq!(created.status().as_u16(), 201);

    // Seed one commit so the clone has something to check out.
    let seed = work.join("seed");
    run_git(
        &work,
        &[
            GIT_HTTP_GUARD,
            &[
                "clone",
                &format!("http://{addr}/git/jeryu/before.git"),
                "seed",
            ],
        ]
        .concat(),
    );
    run_git(&seed, &["config", "user.email", "tester@example.com"]);
    run_git(&seed, &["config", "user.name", "Tester"]);
    std::fs::write(seed.join("README.md"), "renamed\n").unwrap();
    run_git(&seed, &["add", "."]);
    run_git(&seed, &["commit", "-m", "seed"]);
    run_git(
        &seed,
        &[
            GIT_HTTP_GUARD,
            &["push", "origin", "HEAD:refs/heads/feature"],
        ]
        .concat(),
    );

    let renamed = client
        .patch(format!("http://{addr}/repos/jeryu/before"))
        .json(&serde_json::json!({ "name": "after" }))
        .send()
        .await
        .expect("PATCH /repos/jeryu/before");
    let status = renamed.status().as_u16();
    let renamed_body = renamed.text().await.unwrap();
    assert_eq!(status, 200, "rename: {renamed_body}");
    assert!(
        git_root
            .join("jeryu")
            .join("after.git")
            .join("HEAD")
            .is_file(),
        "the bare repository moves with the rename"
    );
    assert!(
        !git_root.join("jeryu").join("before.git").exists(),
        "nothing is left at the old path"
    );

    for (url, dir) in [
        (format!("http://{addr}/git/jeryu/before.git"), "by-old-url"),
        (format!("http://{addr}/git/jeryu/after.git"), "by-new-url"),
    ] {
        run_git(
            &work,
            &[GIT_HTTP_GUARD, &["clone", "--branch", "feature", &url, dir]].concat(),
        );
        assert_eq!(
            std::fs::read_to_string(work.join(dir).join("README.md")).unwrap(),
            "renamed\n",
            "clone of {url} must carry the pushed commit"
        );
    }

    server.abort();
    let _ = std::fs::remove_dir_all(&base);
}
