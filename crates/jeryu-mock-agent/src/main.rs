//! `jeryu-mock-agent`: one test-only binary standing in for the Claude, Codex,
//! and Grok CLIs (lane A `mock-agent-v1`). The `argv[0]` basename picks the
//! provider (`mock-claude`, `mock-codex`, `mock-grok`); `argv[1]` or
//! `JERYU_MOCK_AGENT_MODE` picks the behavior. It ships only in test images.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

mod probe;

const PROVIDERS: [(&str, &str); 3] = [
    ("mock-claude", "claude"),
    ("mock-codex", "codex"),
    ("mock-grok", "grok"),
];
const MODES: [&str; 7] = [
    "cpr",
    "flood",
    "hang",
    "ignore-sigint",
    "background-child",
    "edit-build-commit",
    "secret-probe",
];
/// Internal: the detached child of `background-child`.
const SLEEPER: &str = "--background-sleeper";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(SLEEPER) {
        return background_sleeper();
    }
    let basename = args
        .first()
        .and_then(|arg0| Path::new(arg0).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let Some(&(_, provider)) = PROVIDERS.iter().find(|(name, _)| *name == basename) else {
        eprintln!("unknown_mock_agent");
        return ExitCode::from(2);
    };
    let mode = match select_mode(args.get(1), std::env::var("JERYU_MOCK_AGENT_MODE").ok()) {
        Ok(mode) => mode,
        Err(code) => {
            eprintln!("{code}");
            return ExitCode::from(2);
        }
    };
    // Register handlers before the banner: a signal sent as soon as the banner
    // appears must already find them.
    let signals = match mode.as_str() {
        "hang" => Some(vec![signal_hook::consts::SIGTERM]),
        "ignore-sigint" => Some(vec![
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGINT,
        ]),
        _ => None,
    }
    .map(signal_hook::iterator::Signals::new);
    let signals = match signals.transpose() {
        Ok(signals) => signals,
        Err(_) => return ExitCode::from(1),
    };
    if !matches!(mode.as_str(), "cpr" | "flood" | "secret-probe") {
        println!("mock-agent provider={provider} mode={mode}");
        let _ = std::io::stdout().flush();
    }
    match mode.as_str() {
        "cpr" => cpr(),
        "flood" => flood(),
        "hang" | "ignore-sigint" => match signals {
            Some(signals) => wait_for_sigterm(signals, Duration::from_secs(3600)),
            None => ExitCode::from(1),
        },
        "background-child" => background_child(),
        "edit-build-commit" => edit_build_commit(),
        _ => probe::secret_probe(&args),
    }
}

/// `argv[1]` and `JERYU_MOCK_AGENT_MODE` may each name the mode; if both are
/// present they must agree.
fn select_mode(arg: Option<&String>, env: Option<String>) -> Result<String, &'static str> {
    let mode = match (arg, env) {
        (Some(arg), Some(env)) if *arg != env => return Err("mock_agent_mode_conflict"),
        (Some(arg), _) => arg.clone(),
        (None, Some(env)) => env,
        (None, None) => return Err("mock_agent_mode_missing"),
    };
    if MODES.contains(&mode.as_str()) {
        Ok(mode)
    } else {
        Err("mock_agent_mode_unknown")
    }
}

/// Ask the terminal for the cursor position and wait 2 s for an
/// `ESC [ row ; col R` reply. A full-screen CLI blocks the same way when
/// nothing answers, which is what the headless VT must prevent.
fn cpr() -> ExitCode {
    let stdin = std::io::stdin();
    // Raw mode, as a real full-screen CLI uses: the reply has no newline.
    let raw = rustix::termios::tcgetattr(&stdin).ok().inspect(|original| {
        let mut raw = original.clone();
        raw.make_raw();
        let _ = rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw);
    });
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b[6n").and_then(|()| stdout.flush());
    let answered = read_cpr_reply(Duration::from_secs(2));
    if let Some(original) = raw {
        let now = rustix::termios::OptionalActions::Now;
        let _ = rustix::termios::tcsetattr(&stdin, now, &original);
    }
    ExitCode::from(if answered { 0 } else { 3 })
}

fn read_cpr_reply(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    let stdin = std::io::stdin();
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        let mut fds = [rustix::event::PollFd::new(
            &stdin,
            rustix::event::PollFlags::IN,
        )];
        let timespec = rustix::event::Timespec::try_from(left).ok();
        if !matches!(rustix::event::poll(&mut fds, timespec.as_ref()), Ok(n) if n > 0) {
            continue;
        }
        let mut chunk = [0_u8; 64];
        match stdin.lock().read(&mut chunk) {
            Ok(0) | Err(_) => return false,
            Ok(n) => seen.extend_from_slice(&chunk[..n]),
        }
        if is_cpr_reply(&seen) {
            return true;
        }
    }
    false
}

/// True when `bytes` contains `ESC [ digits ; digits R`.
fn is_cpr_reply(bytes: &[u8]) -> bool {
    bytes.windows(2).enumerate().any(|(start, pair)| {
        if pair != b"\x1b[" {
            return false;
        }
        let rest = &bytes[start + 2..];
        let row = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        let after_row = &rest[row..];
        if row == 0 || after_row.first() != Some(&b';') {
            return false;
        }
        let col = after_row[1..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        col > 0 && after_row.get(1 + col) == Some(&b'R')
    })
}

fn flood() -> ExitCode {
    let chunk = [b'x'; 64 * 1024];
    let mut stdout = std::io::stdout().lock();
    for _ in 0..32 {
        if stdout.write_all(&chunk).is_err() {
            return ExitCode::from(1);
        }
    }
    let _ = stdout.flush();
    ExitCode::SUCCESS
}

/// `hang` registers only SIGTERM, so SIGINT keeps its default and kills it.
/// `ignore-sigint` also registers SIGINT and reports each one. Both exit 0 on
/// SIGTERM, and on their own after `limit`.
fn wait_for_sigterm(mut signals: signal_hook::iterator::Signals, limit: Duration) -> ExitCode {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        for signal in signals.pending() {
            if signal == signal_hook::consts::SIGTERM {
                return ExitCode::SUCCESS;
            }
            println!("SIGINT_IGNORED");
            let _ = std::io::stdout().flush();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    ExitCode::SUCCESS
}

/// Leave a detached child holding the terminal, then exit at once. The child
/// starts its own session (`setsid`) and sleeps.
fn background_child() -> ExitCode {
    let Ok(exe) = std::env::current_exe() else {
        return ExitCode::from(1);
    };
    match Command::new(exe).arg(SLEEPER).spawn() {
        Ok(child) => {
            println!("BACKGROUND_CHILD_PID={}", child.id());
            let _ = std::io::stdout().flush();
            ExitCode::SUCCESS
        }
        Err(_) => ExitCode::from(1),
    }
}

fn background_sleeper() -> ExitCode {
    let _ = rustix::process::setsid();
    std::thread::sleep(Duration::from_secs(600));
    ExitCode::SUCCESS
}

/// Write, stage, and commit `mock-agent.txt`; compile a trivial C file when a
/// compiler is on PATH.
fn edit_build_commit() -> ExitCode {
    if std::fs::write("mock-agent.txt", "edited by mock-agent\n").is_err() {
        return ExitCode::from(4);
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    let committed = git(&["add", "mock-agent.txt"])
        && git(&[
            "-c",
            "user.name=mock-agent",
            "-c",
            "user.email=mock@jeryu.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--author=mock-agent <mock@jeryu.invalid>",
            "-m",
            "mock-agent edit",
        ]);
    if !committed {
        return ExitCode::from(4);
    }
    let has_cc = Command::new("cc")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if has_cc {
        let built = std::fs::write("mock-agent.c", "int main(void){return 0;}\n").is_ok()
            && Command::new("cc")
                .args(["mock-agent.c", "-o", "mock-agent.bin"])
                .status()
                .is_ok_and(|status| status.success());
        if !built {
            return ExitCode::from(4);
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpr_replies_are_recognized_only_when_complete() {
        assert!(is_cpr_reply(b"\x1b[12;40R"));
        assert!(is_cpr_reply(b"noise\x1b[1;1Rtrail"));
        assert!(!is_cpr_reply(b"\x1b[12;40"));
        assert!(!is_cpr_reply(b"\x1b[;40R"));
        assert!(!is_cpr_reply(b"\x1b[12;R"));
    }

    #[test]
    fn mode_sources_must_agree() {
        let flood = "flood".to_string();
        assert_eq!(select_mode(Some(&flood), None), Ok("flood".into()));
        assert_eq!(select_mode(None, Some("hang".into())), Ok("hang".into()));
        assert_eq!(
            select_mode(Some(&flood), Some("hang".into())),
            Err("mock_agent_mode_conflict")
        );
        assert_eq!(select_mode(None, None), Err("mock_agent_mode_missing"));
        assert_eq!(
            select_mode(Some(&"rm".to_string()), None),
            Err("mock_agent_mode_unknown")
        );
    }
}
