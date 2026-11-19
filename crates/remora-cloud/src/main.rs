use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::Parser;
use remora_cloud::{Config, Ice, IceUrl, Ping, PublicUrl, TurnSecret};
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

    /// STUN or TURN server for sessions, such as `turn:turn.example:3478`. Repeat for several.
    /// Without any, peers only find each other on the same network.
    #[arg(long = "ice-server", value_name = "URL")]
    ice_servers: Vec<IceUrl>,

    /// File holding the secret shared with the TURN servers (coturn's `static-auth-secret`).
    #[arg(long, value_name = "PATH")]
    turn_secret_file: Option<PathBuf>,

    /// Relay every session through TURN, so that peers never learn each other's addresses.
    #[arg(long)]
    force_relay: bool,
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
        .map(|path| read_secret(path, "signing key"))
        .transpose()?
        .map(SignKey::new);
    if sign_key.is_none() {
        tracing::warn!("no signing key configured; request signatures are not checked");
    }
    let turn_secret = args
        .turn_secret_file
        .as_deref()
        .map(|path| read_secret(path, "TURN secret"))
        .transpose()?
        .map(TurnSecret::new);
    let ice = Ice::new(args.ice_servers, turn_secret);
    let ice = if args.force_relay {
        ice.and_then(Ice::force_relay)
    } else {
        ice
    }
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

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
        ice,
    };
    axum::serve(listener, remora_cloud::router(config))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

/// The whole file, minus a trailing newline, is the secret.
fn read_secret(path: &Path, what: &str) -> io::Result<Vec<u8>> {
    let mut contents = std::fs::read(path)?;
    if contents.ends_with(b"\n") {
        contents.pop();
        if contents.ends_with(b"\r") {
            contents.pop();
        }
    }
    if contents.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the {what} file is empty"),
        ));
    }
    Ok(contents)
}
