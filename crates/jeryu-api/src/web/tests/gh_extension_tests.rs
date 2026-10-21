//! The real extension and gh binary read a disposable native forge over HTTP.
use super::*;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    case: WorkflowCase,
    state: Arc<WebState>,
    calls: AtomicUsize,
    transition: &'static str,
}

async fn aggregate(
    State(fixture): State<Arc<Fixture>>,
    headers: axum::http::HeaderMap,
) -> AxumResponse {
    assert_eq!(headers.get("authorization").unwrap(), "token fixture-token");
    let call = fixture.calls.fetch_add(1, Ordering::SeqCst);
    match fixture.transition {
        "auth" => return StatusCode::UNAUTHORIZED.into_response(),
        "forbidden" => return StatusCode::FORBIDDEN.into_response(),
        "transient" | "cancel" => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        "retry" if call == 0 => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        "unsupported" => return Json(json!({})).into_response(),
        _ => {}
    }
    let response = crate::web::pulls::detail(
        State(fixture.state.clone()),
        authenticated_account("bob"),
        AxumPath((fixture.case.repo.id.to_string(), fixture.case.number)),
    )
    .await;
    if call == 0 {
        match fixture.transition {
            "ready" => fixture
                .case
                .check(WORKFLOW_HEAD, Some(CheckConclusion::Success)),
            "merged" => {
                fixture
                    .case
                    .core
                    .merge_pull_request("acme", "flow", fixture.case.number, Default::default())
                    .unwrap();
            }
            "head" => {
                fixture
                    .case
                    .core
                    .refresh_pull_request_heads_for_ref(
                        "acme",
                        "flow",
                        "feature",
                        WORKFLOW_OLD_HEAD,
                    )
                    .unwrap();
            }
            _ => {}
        }
    }
    response
}

async fn exercise(transition: &'static str, extra: &[&str]) -> (i32, Value, usize) {
    let case = WorkflowCase::new(false);
    case.approve(WORKFLOW_HEAD);
    case.check(
        WORKFLOW_HEAD,
        match transition {
            "merged" | "retry" => Some(CheckConclusion::Success),
            "failed" => Some(CheckConclusion::Failure),
            _ => None,
        },
    );
    let fixture = Arc::new(Fixture {
        state: Arc::new(WebState::new(case.core.clone())),
        case,
        calls: AtomicUsize::new(0),
        transition,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/api/v1/repos/acme/flow/pulls/1",
            axum::routing::get(aggregate),
        )
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let sandbox = tempdir().unwrap();
    if transition != "missing-auth" {
        std::fs::write(
            sandbox.path().join("hosts.yml"),
            format!(
                "{host}:\n    oauth_token: fixture-token\n    user: bob\n    git_protocol: https\n"
            ),
        )
        .unwrap();
    }
    let extension = concat!(env!("CARGO_MANIFEST_DIR"), "/../../extensions/gh-jeryu");
    if transition == "status-installed" {
        let installed = Command::new("gh")
            .args(["extension", "install", "."])
            .current_dir(extension)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", sandbox.path())
            .env("GH_CONFIG_DIR", sandbox.path())
            .output()
            .unwrap();
        assert!(installed.status.success(), "{installed:?}");
    }
    let mut command = Command::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../extensions/gh-jeryu/gh-jeryu"
    ));
    if transition == "status-installed" {
        command = Command::new("gh");
        command.arg("jeryu");
    }
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", sandbox.path())
        .env("GH_CONFIG_DIR", sandbox.path())
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .args([
            "pr",
            if transition.starts_with("status") {
                "status"
            } else {
                "wait"
            },
            "1",
            "--repo",
            "acme/flow",
            "--host",
        ])
        .arg(format!("http://{host}"))
        .args(extra)
        .stdin(Stdio::null());
    if transition != "status-human" {
        command.arg("--json");
    }
    if !transition.starts_with("status") {
        command.args([
            "--target",
            if transition == "merged" {
                "merged"
            } else {
                "ready"
            },
        ]);
    }
    let observed = fixture.clone();
    let output = tokio::task::spawn_blocking(move || {
        if transition != "cancel" {
            return command.output().unwrap();
        }
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while observed.calls.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    server.abort();
    let result: Value = if transition == "status-human" {
        Value::String(String::from_utf8(output.stdout).unwrap())
    } else {
        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!("extension output: {:?}", output);
        })
    };
    (
        output.status.code().unwrap(),
        result,
        fixture.calls.load(Ordering::SeqCst),
    )
}

#[tokio::test]
async fn gh_extension_status_shows_the_native_workflow_in_both_formats() {
    let (code, value, calls) = exercise("status-json", &[]).await;
    assert_eq!((code, calls), (0, 1));
    assert_eq!(value["workflow"]["head_sha"], WORKFLOW_HEAD);
    assert_eq!(value["workflow"]["required_checks"][0]["state"], "pending");
    assert_eq!(value["workflow"]["review"]["approvals"], 1);
    assert!(value["workflow"]["passport"]["blockers"].is_array());
    assert_eq!(value["workflow"]["next_action"]["code"], "wait_for_checks");
    let (code, value, calls) = exercise("status-human", &[]).await;
    assert_eq!((code, calls), (0, 1));
    for label in [
        "Head:",
        "Checks:",
        "Review:",
        "Blockers:",
        "Next action:",
        WORKFLOW_HEAD,
    ] {
        assert!(value.as_str().unwrap().contains(label), "{value}");
    }
}

#[tokio::test]
async fn gh_extension_transport_and_local_install_contract() {
    for (transition, code, result, calls) in [
        ("auth", 4, "authentication_error", 1),
        ("missing-auth", 4, "authentication_error", 0),
        ("forbidden", 4, "authentication_error", 1),
        ("unsupported", 1, "unsupported_response", 1),
        ("retry", 0, "success", 2),
        ("transient", 3, "timeout", 2),
        ("cancel", 130, "cancelled", 1),
        ("status-installed", 0, "checks_pending", 1),
    ] {
        let extra = if transition == "status-installed" {
            vec![]
        } else {
            vec![
                "--timeout",
                if transition == "transient" { "3" } else { "10" },
            ]
        };
        let (actual_code, value, actual_calls) = exercise(transition, &extra).await;
        assert_eq!(actual_code, code, "{transition}: {value}");
        assert_eq!(value["result"], result, "{transition}: {value}");
        assert_eq!(actual_calls, calls, "{transition}");
    }
}

#[tokio::test]
async fn gh_extension_waits_on_native_forge_and_keeps_the_selected_head() {
    for (transition, code, result, calls) in [
        ("ready", 0, "success", 2),
        ("merged", 0, "success", 2),
        ("failed", 2, "checks_failed", 1),
        ("head", 5, "head_changed", 2),
        ("pending", 3, "timeout", 2),
    ] {
        let timeout = if transition == "pending" { "3" } else { "10" };
        let (actual_code, value, actual_calls) =
            exercise(transition, &["--timeout", timeout]).await;
        assert_eq!(actual_code, code, "{transition}: {value}");
        assert_eq!(value["result"], result, "{transition}: {value}");
        assert_eq!(value["selected_head_sha"], WORKFLOW_HEAD);
        assert_eq!(actual_calls, calls, "only aggregate reads: {transition}");
    }
}
