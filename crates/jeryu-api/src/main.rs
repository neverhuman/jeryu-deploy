#[cfg(feature = "web")]
use std::net::SocketAddr;
#[cfg(feature = "web")]
use std::path::PathBuf;

#[cfg(feature = "web")]
use clap::{Parser, Subcommand};

#[cfg(feature = "web")]
#[derive(Debug, Parser)]
#[command(name = "jeryu-api")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[cfg(feature = "web")]
#[derive(Debug, Subcommand)]
enum Command {
    Web {
        #[command(subcommand)]
        command: WebCommand,
    },
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },
}

#[cfg(feature = "web")]
#[derive(Debug, Subcommand)]
enum WebCommand {
    Serve {
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
        #[arg(long, default_value = "apps/web/dist")]
        spa_dir: PathBuf,
        #[arg(long, default_value = "~/.local/share/jeryu")]
        data_dir: PathBuf,
        #[arg(long)]
        split_manifest: Vec<PathBuf>,
    },
}

#[cfg(feature = "web")]
#[derive(Debug, Subcommand)]
enum DevCommand {
    PullEvents {
        #[arg(long = "from")]
        from: Option<String>,
        #[arg(long)]
        token_file: Option<PathBuf>,
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long)]
        since: Option<String>,
    },
    Compare {
        #[arg(long)]
        data_dir: PathBuf,
    },
}

#[cfg(feature = "web")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Web {
            command:
                WebCommand::Serve {
                    bind,
                    spa_dir,
                    data_dir,
                    split_manifest,
                },
        }) => {
            let data_dir = expand_tilde(data_dir);
            let git_storage_root = data_dir.join("git");
            let trust_local_dev = env_flag("JERYU_WEB_TRUST_LOCAL");
            jeryu_api::web::serve(jeryu_api::web::WebServerConfig {
                bind,
                spa_dir,
                data_dir,
                git_storage_root,
                split_manifests: split_manifest,
                auth_required: true,
                trust_local_dev,
                secure_cookies: !trust_local_dev,
            })
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))
        }
        Some(Command::Dev {
            command:
                DevCommand::PullEvents {
                    from,
                    token_file,
                    data_dir,
                    since,
                },
        }) => {
            let summary = jeryu_api::web::dev_data::pull_events(
                jeryu_api::web::dev_data::PullConfig {
                    base_url: from
                        .or_else(|| std::env::var("JERYU_REAL_DATA_URL").ok())
                        .ok_or(anyhow::anyhow!(
                            "pass --from or set JERYU_REAL_DATA_URL in private operator config"
                        ))?,
                    token_file: expand_tilde(
                        token_file
                            .or_else(|| {
                                std::env::var_os("JERYU_REAL_DATA_TOKEN_FILE").map(PathBuf::from)
                            })
                            .ok_or(anyhow::anyhow!(
                                "pass --token-file or set JERYU_REAL_DATA_TOKEN_FILE in private operator config"
                            ))?,
                    ),
                    data_dir: expand_tilde(data_dir),
                    since,
                },
            )
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
            println!("{}", summary.line());
            Ok(())
        }
        Some(Command::Dev {
            command: DevCommand::Compare { data_dir },
        }) => {
            let report =
                jeryu_api::web::dev_data::compare(jeryu_api::web::dev_data::CompareConfig {
                    data_dir: expand_tilde(data_dir),
                })
                .map_err(|err| anyhow::anyhow!("{err}"))?;
            println!("{report}");
            Ok(())
        }
        None => {
            let mut router = jeryu_api::Router::default();
            let response = router.get("/api/phase10/ready");
            println!("{} {}", response.status, response.body);
            Ok(())
        }
    }
}

#[cfg(feature = "web")]
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

#[cfg(feature = "web")]
fn expand_tilde(path: PathBuf) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        std::env::var_os("HOME").map_or(path, PathBuf::from)
    } else if let Some(rest) = raw.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or(path)
    } else {
        path
    }
}

#[cfg(not(feature = "web"))]
fn main() {
    let mut router = jeryu_api::Router::default();
    let response = router.get("/api/phase10/ready");
    println!("{} {}", response.status, response.body);
}
