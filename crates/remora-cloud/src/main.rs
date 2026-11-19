use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::Parser;
use remora_cloud::{Config, Ping, PublicUrl};
use remora_proto::cloud::SignKey;
use tracing_subscriber::EnvFilter;

/// Self-hostable stand-in for the UU Remote cloud.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Address to serve the REST API on.
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,

    /// Origin clients reach this server at, such as `https://remora.example`, when that is not
    /// `http://` plus the listen address (behind a TLS proxy, say).
    #[arg(long, value_name = "URL")]
    public_url: Option<PublicUrl>,

    /// File holding the request signing key that clients are configured with. Without it,
    /// request signatures are not checked.
    #[arg(long, value_name = "PATH")]
    sign_key_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let sign_key = args
        .sign_key_file
        .as_deref()
        .map(read_sign_key)
        .transpose()?;
    if sign_key.is_none() {
        tracing::warn!("no signing key configured; request signatures are not checked");
    }

    let public_url = match args.public_url {
        Some(url) => url,
        None if args.listen.ip().is_unspecified() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--public-url is required when listening on all addresses",
            ));
        }
        None => format!("http://{}", args.listen)
            .parse()
            .expect("a socket address is a valid authority"),
    };

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    let config = Config {
        public_url,
        sign_key,
        ping: Ping::default(),
    };
    axum::serve(listener, remora_cloud::router(config))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

/// The whole file, minus a trailing newline, is the key.
fn read_sign_key(path: &Path) -> io::Result<SignKey> {
    let contents = std::fs::read(path)?;
    let key = contents.strip_suffix(b"\n").unwrap_or(&contents);
    let key = key.strip_suffix(b"\r").unwrap_or(key);
    if key.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the signing key file is empty",
        ));
    }
    Ok(SignKey::new(key))
}
