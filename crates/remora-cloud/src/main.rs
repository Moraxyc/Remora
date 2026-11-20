use std::error::Error;
use std::io::{BufRead, IsTerminal};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use listenfd::ListenFd;
use remora_cloud::{OwnerPassword, Settings};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

/// Self-hostable stand-in for the UU Remote cloud.
#[derive(Parser)]
#[command(version, args_conflicts_with_subcommands = true)]
struct Args {
    /// Settings file (TOML) to serve with.
    #[arg(long, short, value_name = "PATH", required = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Hash the owner's password for `login.password_file`. Reads the password from the
    /// terminal, or the first line of standard input, and prints the hash.
    HashPassword,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let result = match (args.command, args.config) {
        (Some(Command::HashPassword), _) => hash_password(),
        (None, Some(config)) => passed_socket().and_then(|socket| serve(config, socket)),
        (None, None) => unreachable!("clap requires --config without a subcommand"),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("remora: {error}");
            ExitCode::FAILURE
        }
    }
}

fn hash_password() -> Result<(), Box<dyn Error>> {
    let password = if std::io::stdin().is_terminal() {
        let password = rpassword::prompt_password("Password: ")?;
        if rpassword::prompt_password("Again: ")? != password {
            return Err("the passwords differ".into());
        }
        password
    } else {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    };
    if password.is_empty() {
        return Err("the password is empty".into());
    }
    println!("{}", OwnerPassword::hash(&password).as_phc());
    Ok(())
}

/// The listening socket systemd passed, if it passed one. Reading it clears `LISTEN_FDS` and
/// `LISTEN_PID`, which is only sound before any other thread runs.
fn passed_socket() -> Result<Option<std::net::TcpListener>, Box<dyn Error>> {
    ListenFd::from_env()
        .take_tcp_listener(0)
        .map_err(|error| format!("the socket systemd passed is unusable: {error}").into())
}

#[tokio::main]
async fn serve(
    config: PathBuf,
    socket: Option<std::net::TcpListener>,
) -> Result<(), Box<dyn Error>> {
    let settings = Settings::load(&config)?;
    let filter =
        EnvFilter::try_new(&settings.log).map_err(|error| format!("invalid log: {error}"))?;
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let listen = settings.listen;
    let in_memory = settings.database.is_none();
    let config = settings.into_config()?;
    if in_memory {
        tracing::warn!(
            "no database configured; a restart logs everyone out and forgets every device"
        );
    }
    if config.auto_confirm {
        tracing::warn!(
            "every QR login is confirmed automatically; anyone who can reach this server can log in"
        );
    }
    if config.owner_password.is_some() && !config.public_url.is_secure() {
        tracing::warn!("public_url is not https; the owner's password travels unencrypted");
    }
    if config.sign_key.is_none() {
        tracing::warn!("no signing key configured; request signatures are not checked");
    }

    let listener = match socket {
        Some(socket) => {
            socket.set_nonblocking(true)?;
            let listener = tokio::net::TcpListener::from_std(socket)?;
            let addr = listener.local_addr()?;
            tracing::info!(%addr, "listening on the socket systemd passed");
            if addr != listen {
                tracing::warn!(%addr, %listen, "the socket systemd passed is not on listen");
            }
            listener
        }
        None => {
            let listener = tokio::net::TcpListener::bind(listen).await?;
            tracing::info!(addr = %listener.local_addr()?, "listening");
            listener
        }
    };
    let mut terminate = signal(SignalKind::terminate())?;
    let app = remora_cloud::router(config).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        })
        .await?;
    Ok(())
}
