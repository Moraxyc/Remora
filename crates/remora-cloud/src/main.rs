use std::net::SocketAddr;

use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Self-hostable stand-in for the UU Remote cloud.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Address to serve the REST API on.
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, remora_cloud::router(Default::default()))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
