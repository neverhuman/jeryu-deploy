//! Run the mock provider on an ephemeral loopback port and print its address.
//!
//! Flags: `--access-ttl-ms N`, `--refresh-ttl-ms N`, `--no-rotate-refresh`,
//! `--fail-persist-once`, `--state-file PATH`. There is no bind-address flag:
//! the mock serves `127.0.0.1` only.

use std::process::ExitCode;
use std::time::Duration;

use jeryu_mock_provider::{MockConfig, MockProvider};

fn parse(args: &[String]) -> Result<MockConfig, String> {
    let mut config = MockConfig::default();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let mut millis = |name: &str| {
            args.next()
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_millis)
                .ok_or_else(|| format!("{name} needs a number of milliseconds"))
        };
        match flag.as_str() {
            "--access-ttl-ms" => config.access_ttl = millis(flag)?,
            "--refresh-ttl-ms" => config.refresh_ttl = millis(flag)?,
            "--no-rotate-refresh" => config.rotate_refresh = false,
            "--fail-persist-once" => config.fail_persist_once = true,
            "--state-file" => {
                config.state_file = Some(args.next().ok_or("--state-file needs a path")?.into());
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(config)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match parse(&args) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let provider = match MockProvider::start(config).await {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("mock provider failed to start: {error}");
            return ExitCode::from(1);
        }
    };
    println!("MOCK_PROVIDER_URL=http://{}", provider.addr);
    let _ = tokio::signal::ctrl_c().await;
    ExitCode::SUCCESS
}
