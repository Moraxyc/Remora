use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use remora_cloud::Settings;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

/// Self-hostable stand-in for the UU Remote cloud.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Settings file (TOML).
    #[arg(long, short, value_name = "PATH")]
    config: PathBuf,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("remora: {error}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let settings = Settings::load(&args.config)?;
    let filter =
        EnvFilter::try_new(&settings.log).map_err(|error| format!("invalid log: {error}"))?;
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let listen = settings.listen;
    let config = settings.into_config()?;
    tracing::warn!(
        "every QR login is confirmed automatically; anyone who can reach this server can log in"
    );
    if config.sign_key.is_none() {
        tracing::warn!("no signing key configured; request signatures are not checked");
    }

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    let mut terminate = signal(SignalKind::terminate())?;
    axum::serve(listener, remora_cloud::router(config))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        })
        .await?;
    Ok(())
}
