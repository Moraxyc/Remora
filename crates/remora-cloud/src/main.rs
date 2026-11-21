use std::error::Error;
use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use std::sync::Arc;

use clap::{Parser, Subcommand};
use listenfd::ListenFd;
use remora_cloud::bus::unix::{UnixBus, bind_hub};
use remora_cloud::{Bus, CloudRole, Listen, LocalBus, OwnerPassword, PeerAddress, Settings};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

/// Self-hostable stand-in for the UU Remote cloud.
#[derive(Parser)]
#[command(version, args_conflicts_with_subcommands = true)]
struct Args {
    /// Settings file (TOML) to serve with.
    #[arg(long, short, value_name = "PATH", required = true)]
    config: Option<PathBuf>,
    /// Comma-separated service roles to enable (api,gateway,room,relay). Defaults to all
    /// roles or `roles` in the settings file.
    #[arg(long, value_delimiter = ',', value_enum)]
    roles: Option<Vec<CloudRole>>,
    /// Bind an internal Unix-socket bus hub at PATH for role-split instances.
    #[arg(long, value_name = "PATH", conflicts_with = "bus_connect")]
    bus_listen: Option<PathBuf>,
    /// Connect to an internal Unix-socket bus hub at PATH.
    #[arg(long, value_name = "PATH", conflicts_with = "bus_listen")]
    bus_connect: Option<PathBuf>,
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
        (None, Some(config)) => passed_socket().and_then(|socket| {
            serve(
                config,
                args.roles,
                args.bus_listen,
                args.bus_connect,
                socket,
            )
        }),
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

enum PassedSocket {
    Tcp(std::net::TcpListener),
    Unix(std::os::unix::net::UnixListener),
}

/// The listening socket systemd passed, if it passed one. Reading it clears `LISTEN_FDS` and
/// `LISTEN_PID`, which is only sound before any other thread runs.
fn passed_socket() -> Result<Option<PassedSocket>, Box<dyn Error>> {
    let mut listen_fd = ListenFd::from_env();
    if let Some(tcp) = listen_fd
        .take_tcp_listener(0)
        .map_err(|error| format!("the socket systemd passed is unusable: {error}"))?
    {
        return Ok(Some(PassedSocket::Tcp(tcp)));
    }
    if let Some(unix) = listen_fd
        .take_unix_listener(0)
        .map_err(|error| format!("the socket systemd passed is unusable: {error}"))?
    {
        return Ok(Some(PassedSocket::Unix(unix)));
    }
    Ok(None)
}

async fn shutdown_signal() -> Result<(), std::io::Error> {
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    Ok(())
}

fn bind_unix(path: &Path) -> Result<tokio::net::UnixListener, Box<dyn Error>> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => return Err(format!("{} is already in use", path.display()).into()),
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("cannot check {}: {error}", path.display()).into());
                }
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(tokio::net::UnixListener::bind(path)?)
}

#[tokio::main]
async fn serve(
    config: PathBuf,
    roles: Option<Vec<CloudRole>>,
    bus_listen: Option<PathBuf>,
    bus_connect: Option<PathBuf>,
    socket: Option<PassedSocket>,
) -> Result<(), Box<dyn Error>> {
    let mut settings = Settings::load(&config)?;
    if let Some(roles) = roles {
        settings.roles = roles;
    }
    let filter =
        EnvFilter::try_new(&settings.log).map_err(|error| format!("invalid log: {error}"))?;
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let listen = settings.listen.clone();
    let in_memory = settings.database.is_none();
    let config = settings.into_config()?;
    let has_http =
        config.roles.contains(CloudRole::Api) || config.roles.contains(CloudRole::Gateway);
    if in_memory {
        tracing::warn!(
            "no database configured; a restart logs everyone out and forgets every device"
        );
    }
    if config.roles.contains(CloudRole::Api) {
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
    }

    let (bus, unix_bus): (Arc<dyn Bus>, Option<Arc<UnixBus>>) = match (bus_listen, bus_connect) {
        (Some(path), None) => {
            let b = bind_hub(&path)?;
            (b.clone(), Some(b))
        }
        (None, Some(path)) => {
            let b = UnixBus::connect(&path).await?;
            (b.clone(), Some(b))
        }
        (None, None) => (Arc::new(LocalBus::new()), None),
        (Some(_), Some(_)) => unreachable!("clap enforces mutual exclusion"),
    };

    let router = remora_cloud::router_with_bus(config, bus);
    if let Some(unix_bus) = unix_bus {
        unix_bus.sync().await;
    }
    let shutdown = shutdown_signal();
    if !has_http {
        tracing::info!("running background roles without HTTP listener");
        let _keep = router;
        let _ = shutdown.await;
        return Ok(());
    }
    let app = router.into_make_service_with_connect_info::<PeerAddress>();
    let shutdown = shutdown_signal();
    match (socket, listen) {
        (Some(PassedSocket::Tcp(socket)), listen) => {
            socket.set_nonblocking(true)?;
            let listener = tokio::net::TcpListener::from_std(socket)?;
            let addr = listener.local_addr()?;
            tracing::info!(%addr, "listening on the socket systemd passed");
            if Listen::Tcp(addr) != listen {
                tracing::warn!(%addr, %listen, "the socket systemd passed is not on listen");
            }
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.await;
                })
                .await?;
        }
        (Some(PassedSocket::Unix(socket)), listen) => {
            socket.set_nonblocking(true)?;
            let listener = tokio::net::UnixListener::from_std(socket)?;
            let addr = listener.local_addr()?;
            let path = addr
                .as_pathname()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(unnamed)".to_owned());
            tracing::info!(%path, "listening on the socket systemd passed");
            if !matches!(&listen, Listen::Unix(configured) if addr.as_pathname() == Some(configured.as_path()))
            {
                tracing::warn!(%path, %listen, "the socket systemd passed is not on listen");
            }
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.await;
                })
                .await?;
        }
        (None, Listen::Tcp(addr)) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            tracing::info!(addr = %listener.local_addr()?, "listening");
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.await;
                })
                .await?;
        }
        (None, Listen::Unix(path)) => {
            let listener = bind_unix(&path)?;
            tracing::info!(path = %path.display(), "listening");
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.await;
                })
                .await;
            let _ = std::fs::remove_file(&path);
            result?;
        }
    }
    Ok(())
}
