//! Deliberately non-approving phase-0 client used by scripts/agents/forge.sh.
mod request;
mod transport;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    repo: String,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    Open {
        #[arg(long)]
        head: String,
        #[arg(long)]
        base: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        body_file: PathBuf,
    },
    Comment {
        number: u64,
        #[arg(long)]
        head_sha: String,
        #[arg(long)]
        body_file: PathBuf,
    },
    CloseAsReplaced {
        number: u64,
        #[arg(long)]
        replacement: u64,
    },
    Ready {
        number: u64,
    },
    Reviews {
        number: u64,
    },
}

fn main() {
    let args = Args::parse();
    if run(args).is_err() {
        // Error chains may contain remote bodies or credential-bearing URLs.
        eprintln!(
            "forge request failed; check configuration, custody and live state before retrying"
        );
        std::process::exit(1);
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let spec = request::spec(args)?;
    let origin = transport::origin(&std::env::var("JERYU_FORGE_URL")?)?;
    let directory = PathBuf::from(std::env::var("JERYU_CREDENTIALS_DIR")?);
    anyhow::ensure!(
        directory.is_absolute(),
        "credentials directory must be absolute"
    );
    let token = transport::read_token(&directory.join("agent-forge.token"))?;
    let output = transport::send(&origin, &token, spec)?;
    println!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests;
