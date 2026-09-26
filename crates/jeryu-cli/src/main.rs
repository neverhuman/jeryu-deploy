//! The `jeryu` operator/agent CLI binary.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use jeryu_cli::{Cli, cli::Commands, client::RemoteOnlyClient, dispatch};

/// Exit code for output that could not be written or flushed; the same code
/// `dispatch` returns for a `ClientError::Io` (see `docs/errors.md`).
const OUTPUT_WRITE_EXIT_CODE: i32 = 8;

/// The forge a bare `jeryu` talks to when neither `--api-url` nor
/// `JERYU_API_URL` names one: the loopback server `jeryu serve` binds.
const DEFAULT_API_URL: &str = "http://127.0.0.1:8787";

fn main() -> ExitCode {
    let mut cli = Cli::parse();
    if let Commands::Serve {
        bind,
        spa_dir,
        data_dir,
        split_manifest,
    } = &cli.command
    {
        return match serve(
            *bind,
            spa_dir.clone(),
            data_dir.clone(),
            split_manifest.clone(),
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::from(1)
            }
        };
    }

    // Every command the binary can honor goes over HTTP to a live forge, so
    // the API URL is always resolved here. The seam is backed by the
    // fail-closed client: a command with no server transport reports the
    // failure rather than a locally simulated success.
    cli.api_url = Some(cli.api_url.unwrap_or_else(|| {
        std::env::var("JERYU_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string())
    }));
    let client = RemoteOnlyClient;

    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();

    let mut code = dispatch(cli, &client, &mut out, &mut err);
    // A flush that fails lost output the caller asked for (closed pipe, full
    // disk); exit 8 rather than claiming success.
    if out.flush().is_err() || err.flush().is_err() {
        code = OUTPUT_WRITE_EXIT_CODE;
    }

    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn serve(
    bind: std::net::SocketAddr,
    spa_dir: PathBuf,
    data_dir: Option<PathBuf>,
    split_manifests: Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = jeryu_cli::data_dir::resolve(data_dir)?;
    let git_storage_root = data_dir.join("git");
    let trust_local_dev = env_flag("JERYU_WEB_TRUST_LOCAL");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(jeryu_api::web::serve(jeryu_api::web::WebServerConfig {
        bind,
        spa_dir,
        data_dir,
        git_storage_root,
        split_manifests,
        auth_required: true,
        trust_local_dev,
        secure_cookies: !trust_local_dev,
    }))
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}
