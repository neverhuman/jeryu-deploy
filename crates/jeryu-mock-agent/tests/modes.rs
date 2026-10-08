//! Drive the real binary under its three provider names, on pipes and PTYs.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

struct Agents {
    dir: tempfile::TempDir,
}

impl Agents {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("agents dir");
        for name in ["mock-claude", "mock-codex", "mock-grok", "mock-jekko"] {
            std::os::unix::fs::symlink(
                env!("CARGO_BIN_EXE_jeryu-mock-agent"),
                dir.path().join(name),
            )
            .expect("symlink");
        }
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn run(&self, name: &str, mode: &str, cwd: &Path) -> Output {
        Command::new(self.path(name))
            .arg(mode)
            .current_dir(cwd)
            .env_remove("JERYU_MOCK_AGENT_MODE")
            .output()
            .expect("run mock agent")
    }
}

/// A PTY pair: the master for the test, the slave as the child's terminal.
fn pty() -> (std::fs::File, OwnedFd) {
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("openpt");
    grantpt(&master).expect("grantpt");
    unlockpt(&master).expect("unlockpt");
    let name = ptsname(&master, Vec::new()).expect("ptsname");
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name.to_str().expect("utf8 pts"))
        .expect("open slave");
    (std::fs::File::from(master), OwnedFd::from(slave))
}

fn spawn_on_pty(agents: &Agents, name: &str, mode: &str) -> (Child, std::fs::File) {
    let (master, slave) = pty();
    let child = Command::new(agents.path(name))
        .arg(mode)
        .stdin(Stdio::from(slave.try_clone().expect("dup")))
        .stdout(Stdio::from(slave.try_clone().expect("dup")))
        .stderr(Stdio::from(slave))
        .spawn()
        .expect("spawn on pty");
    (child, master)
}

/// Read from the PTY master until `needle` appears or the deadline passes.
fn read_until(master: &mut std::fs::File, needle: &[u8], timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline && !seen.windows(needle.len()).any(|w| w == needle) {
        let mut fds = [rustix::event::PollFd::new(
            &*master,
            rustix::event::PollFlags::IN,
        )];
        let timeout = rustix::event::Timespec::try_from(Duration::from_millis(50)).ok();
        if matches!(rustix::event::poll(&mut fds, timeout.as_ref()), Ok(n) if n > 0) {
            let mut chunk = [0_u8; 4096];
            match master.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&chunk[..n]),
            }
        }
    }
    seen
}

fn exit_code_within(child: &mut Child, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return status.code();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    None
}

fn signal(child: &Child, signal: Signal) {
    let pid = Pid::from_raw(i32::try_from(child.id()).expect("pid")).expect("nonzero pid");
    kill_process(pid, signal).expect("signal");
}

#[test]
fn provider_comes_from_argv0_and_unknown_names_exit_2() {
    let agents = Agents::new();
    let cwd = tempfile::tempdir().expect("cwd");
    for (name, provider) in [
        ("mock-claude", "claude"),
        ("mock-codex", "codex"),
        ("mock-grok", "grok"),
    ] {
        let (mut child, mut master) = spawn_on_pty(&agents, name, "hang");
        let banner = read_until(&mut master, b"mode=hang", Duration::from_secs(5));
        let expected = format!("mock-agent provider={provider} mode=hang");
        assert!(
            String::from_utf8_lossy(&banner).contains(&expected),
            "{name}"
        );
        signal(&child, Signal::TERM);
        assert_eq!(
            exit_code_within(&mut child, Duration::from_secs(5)),
            Some(0)
        );
    }
    let unknown = agents.run("mock-jekko", "flood", cwd.path());
    assert_eq!(unknown.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown_mock_agent"));
    let conflict = Command::new(agents.path("mock-claude"))
        .arg("flood")
        .env("JERYU_MOCK_AGENT_MODE", "hang")
        .output()
        .expect("run");
    assert_eq!(conflict.status.code(), Some(2));
}

#[test]
fn flood_writes_two_mib_without_a_banner() {
    let agents = Agents::new();
    let cwd = tempfile::tempdir().expect("cwd");
    let output = agents.run("mock-codex", "flood", cwd.path());
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout.len(), 2 * 1024 * 1024);
    assert!(output.stdout.iter().all(|&b| b == b'x'));
}

#[test]
fn cpr_blocks_until_the_terminal_answers() {
    let agents = Agents::new();
    let (mut child, mut master) = spawn_on_pty(&agents, "mock-grok", "cpr");
    let query = read_until(&mut master, b"\x1b[6n", Duration::from_secs(5));
    assert!(query.ends_with(b"\x1b[6n"), "no banner before the query");
    master.write_all(b"\x1b[24;80R").expect("answer");
    assert_eq!(
        exit_code_within(&mut child, Duration::from_secs(5)),
        Some(0)
    );

    let (mut silent, mut master) = spawn_on_pty(&agents, "mock-grok", "cpr");
    read_until(&mut master, b"\x1b[6n", Duration::from_secs(5));
    assert_eq!(
        exit_code_within(&mut silent, Duration::from_secs(5)),
        Some(3),
        "an unanswered query times out"
    );
}

#[test]
fn ignore_sigint_reports_each_signal_and_exits_on_sigterm() {
    let agents = Agents::new();
    let (mut child, mut master) = spawn_on_pty(&agents, "mock-claude", "ignore-sigint");
    read_until(&mut master, b"mode=ignore-sigint", Duration::from_secs(5));
    signal(&child, Signal::INT);
    let seen = read_until(&mut master, b"SIGINT_IGNORED", Duration::from_secs(5));
    assert!(seen.windows(14).any(|w| w == b"SIGINT_IGNORED"));
    assert_eq!(child.try_wait().expect("wait"), None, "still running");
    signal(&child, Signal::TERM);
    assert_eq!(
        exit_code_within(&mut child, Duration::from_secs(5)),
        Some(0)
    );

    let (mut hang, mut master) = spawn_on_pty(&agents, "mock-claude", "hang");
    read_until(&mut master, b"mode=hang", Duration::from_secs(5));
    signal(&hang, Signal::INT);
    let status = hang.wait().expect("wait hang");
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(2),
        "SIGINT keeps its default and kills hang"
    );
}

#[test]
fn background_child_outlives_its_parent_in_its_own_session() {
    let agents = Agents::new();
    let (mut parent, mut master) = spawn_on_pty(&agents, "mock-codex", "background-child");
    let seen = read_until(&mut master, b"\n", Duration::from_secs(5));
    let seen = [
        seen,
        read_until(&mut master, b"PID=", Duration::from_secs(5)),
    ]
    .concat();
    let seen = [
        seen,
        read_until(&mut master, b"\r\n", Duration::from_secs(1)),
    ]
    .concat();
    assert_eq!(
        exit_code_within(&mut parent, Duration::from_secs(5)),
        Some(0)
    );
    let text = String::from_utf8_lossy(&seen);
    let pid: i32 = text
        .split("BACKGROUND_CHILD_PID=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|pid| pid.parse().ok())
        .expect("child pid");
    let pid = Pid::from_raw(pid).expect("pid");
    // The child calls setsid right after it starts; wait for it with a
    // deadline rather than a fixed delay.
    let deadline = Instant::now() + Duration::from_secs(10);
    while rustix::process::getsid(Some(pid)).ok() != Some(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        rustix::process::getsid(Some(pid)).ok(),
        Some(pid),
        "child leads a new session"
    );
    kill_process(pid, Signal::KILL).expect("kill child");
}

#[test]
fn edit_build_commit_commits_as_mock_agent() {
    let agents = Agents::new();
    let repo = tempfile::tempdir().expect("repo");
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .output()
            .expect("git");
        assert!(status.status.success(), "{args:?}");
        String::from_utf8_lossy(&status.stdout).trim().to_string()
    };
    git(&["init", "-q"]);
    let output = agents.run("mock-claude", "edit-build-commit", repo.path());
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        git(&["log", "-1", "--format=%an <%ae>|%s"]),
        "mock-agent <mock@jeryu.invalid>|mock-agent edit"
    );
    assert!(repo.path().join("mock-agent.txt").exists());

    let not_a_repo = tempfile::tempdir().expect("plain dir");
    let failed = agents.run("mock-claude", "edit-build-commit", not_a_repo.path());
    assert_eq!(failed.status.code(), Some(4));
}

#[test]
fn secret_probe_reports_presence_without_printing_the_canary() {
    let agents = Agents::new();
    let clean = tempfile::tempdir().expect("clean");
    let probe = |dir: &Path, env: Option<&str>| {
        let mut command = Command::new(agents.path("mock-grok"));
        command
            .arg("secret-probe")
            .current_dir(dir)
            .env("HOME", dir)
            .env("JERYU_MOCK_AGENT_PROBE_ROOTS", dir);
        command.env_remove("JERYU_MOCK_AGENT_MODE");
        if let Some(value) = env {
            command.env("PROVIDER_TOKEN", value);
        }
        command.output().expect("probe")
    };
    let canary = format!("jeryu-canary-{}", std::process::id());
    let absent = probe(clean.path(), None);
    assert_eq!(
        String::from_utf8_lossy(&absent.stdout),
        "LEAK_ORACLE_PROBE=absent\n"
    );

    let via_env = probe(clean.path(), Some(&canary));
    assert_eq!(
        String::from_utf8_lossy(&via_env.stdout),
        "LEAK_ORACLE_PROBE=present\n"
    );

    let leaky = tempfile::tempdir().expect("leaky");
    std::fs::create_dir(leaky.path().join(".config")).expect("dir");
    std::fs::write(leaky.path().join(".config/auth.json"), &canary).expect("leak");
    let via_file = probe(leaky.path(), None);
    let stdout = String::from_utf8_lossy(&via_file.stdout);
    assert_eq!(stdout, "LEAK_ORACLE_PROBE=present\n");
    assert!(
        !stdout.contains(&canary) && !String::from_utf8_lossy(&via_file.stderr).contains(&canary)
    );
}

/// mock-agent-v1: `absent` only after a complete walk. A probe that runs out
/// of entry budget exits 5 and prints no verdict at all.
#[test]
fn secret_probe_never_says_absent_after_an_early_stop() {
    let agents = Agents::new();
    let tree = tempfile::tempdir().expect("tree");
    for index in 0..8 {
        std::fs::write(tree.path().join(format!("file-{index}")), "clean").expect("file");
    }
    let probe = |budget: &str| {
        let mut command = Command::new(agents.path("mock-codex"));
        command
            .arg("secret-probe")
            .current_dir(tree.path())
            .env("HOME", tree.path())
            .env("JERYU_MOCK_AGENT_PROBE_ROOTS", tree.path())
            .env("JERYU_MOCK_AGENT_PROBE_BUDGET", budget)
            .env_remove("JERYU_MOCK_AGENT_MODE");
        command.output().expect("probe")
    };
    let cut_short = probe("3");
    assert_eq!(cut_short.status.code(), Some(5));
    assert!(
        cut_short.stdout.is_empty(),
        "no verdict after an early stop"
    );
    let complete = probe("1000");
    assert_eq!(complete.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&complete.stdout),
        "LEAK_ORACLE_PROBE=absent\n"
    );
    // A root that does not exist is not an early stop.
    let mut missing = Command::new(agents.path("mock-codex"));
    missing
        .arg("secret-probe")
        .current_dir(tree.path())
        .env("HOME", tree.path())
        .env(
            "JERYU_MOCK_AGENT_PROBE_ROOTS",
            std::env::join_paths([tree.path(), &tree.path().join("absent-root")]).expect("roots"),
        )
        .env_remove("JERYU_MOCK_AGENT_MODE");
    let output = missing.output().expect("probe");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "LEAK_ORACLE_PROBE=absent\n"
    );
}
