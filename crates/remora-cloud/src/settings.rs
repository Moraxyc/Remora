//! The settings file: one TOML document configures the whole server.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use remora_proto::cloud::SignKey;
use serde::Deserialize;

use crate::{Config, Ice, IceError, IceUrl, Ping, PublicUrl, TurnSecret};

/// What the settings file says. Secrets stay in their own files, named here and read by
/// [`Settings::into_config`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Address to serve the REST API and signaling on.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Origin clients reach this server at, when that is not `http://` plus `listen`.
    pub public_url: Option<PublicUrl>,
    /// File holding the request signing key. Without it, signatures are not checked.
    pub sign_key_file: Option<PathBuf>,
    /// Log filter, such as `info` or `info,remora_cloud=debug`.
    #[serde(default = "default_log")]
    pub log: String,
    #[serde(default)]
    pub ice: IceSettings,
    #[serde(default)]
    pub login: LoginSettings,
}

/// How QR logins get confirmed.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginSettings {
    /// Confirm every QR login without asking anyone, so that anyone who can reach the server
    /// logs in as its user. It is the only way to confirm logins for now, so it must be on.
    #[serde(default)]
    pub auto_confirm: bool,
}

/// STUN and TURN servers handed to both sides of each session.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IceSettings {
    #[serde(default)]
    pub servers: Vec<IceUrl>,
    /// File holding the secret shared with the TURN servers (coturn's `static-auth-secret`).
    pub turn_secret_file: Option<PathBuf>,
    /// Relay every session through TURN, so that peers never learn each other's addresses.
    #[serde(default)]
    pub force_relay: bool,
}

fn default_listen() -> SocketAddr {
    (Ipv4Addr::LOCALHOST, 8080).into()
}

fn default_log() -> String {
    "info".to_owned()
}

impl Settings {
    pub fn parse(text: &str) -> Result<Self, SettingsError> {
        toml::from_str(text).map_err(SettingsError::Parse)
    }

    /// Reads the settings file at `path`. Relative paths in it are relative to its directory.
    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        let text = std::fs::read_to_string(path).map_err(|source| SettingsError::Read {
            path: path.to_owned(),
            source,
        })?;
        let mut settings = Self::parse(&text)?;
        let base = path.parent().unwrap_or(Path::new(""));
        for file in [
            &mut settings.sign_key_file,
            &mut settings.ice.turn_secret_file,
        ]
        .into_iter()
        .flatten()
        {
            *file = base.join(&*file);
        }
        Ok(settings)
    }

    /// Reads the secret files and checks that the settings fit together.
    pub fn into_config(self) -> Result<Config, SettingsError> {
        if !self.login.auto_confirm {
            return Err(SettingsError::LoginsUnconfirmable);
        }
        let sign_key = self
            .sign_key_file
            .as_deref()
            .map(read_secret)
            .transpose()?
            .map(SignKey::new);
        let turn_secret = self
            .ice
            .turn_secret_file
            .as_deref()
            .map(read_secret)
            .transpose()?
            .map(TurnSecret::new);
        let ice = Ice::new(self.ice.servers, turn_secret);
        let ice = if self.ice.force_relay {
            ice.and_then(Ice::force_relay)
        } else {
            ice
        }
        .map_err(SettingsError::Ice)?;

        let public_url = match self.public_url {
            Some(url) => url,
            None if self.listen.ip().is_unspecified() => {
                return Err(SettingsError::PublicUrlRequired);
            }
            None => format!("http://{}", self.listen)
                .parse()
                .expect("a socket address is a valid authority"),
        };
        Ok(Config {
            public_url,
            sign_key,
            ping: Ping::default(),
            ice,
        })
    }
}

/// The whole file, minus a trailing newline, is the secret.
fn read_secret(path: &Path) -> Result<Vec<u8>, SettingsError> {
    let mut contents = std::fs::read(path).map_err(|source| SettingsError::Read {
        path: path.to_owned(),
        source,
    })?;
    if contents.ends_with(b"\n") {
        contents.pop();
        if contents.ends_with(b"\r") {
            contents.pop();
        }
    }
    if contents.is_empty() {
        return Err(SettingsError::EmptySecret(path.to_owned()));
    }
    Ok(contents)
}

#[derive(Debug)]
pub enum SettingsError {
    Read { path: PathBuf, source: io::Error },
    Parse(toml::de::Error),
    EmptySecret(PathBuf),
    Ice(IceError),
    PublicUrlRequired,
    LoginsUnconfirmable,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse(error) => write!(f, "invalid settings: {error}"),
            Self::EmptySecret(path) => write!(f, "{} is empty", path.display()),
            Self::Ice(error) => error.fmt(f),
            Self::PublicUrlRequired => {
                f.write_str("public_url is required when listening on all addresses")
            }
            Self::LoginsUnconfirmable => f.write_str(
                "login.auto_confirm must be true: Remora cannot ask anyone to confirm logins yet",
            ),
        }
    }
}

impl std::error::Error for SettingsError {}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIRMING: &str = "[login]\nauto_confirm = true\n";

    /// `text` plus what every working settings file needs for now.
    fn parse(text: &str) -> Settings {
        Settings::parse(&format!("{text}\n{CONFIRMING}")).unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("remora-settings-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn logins_must_be_confirmed_automatically_for_now() {
        assert!(matches!(
            Settings::parse("").unwrap().into_config(),
            Err(SettingsError::LoginsUnconfirmable)
        ));
    }

    #[test]
    fn defaults_serve_on_localhost() {
        let config = parse("").into_config().unwrap();
        assert_eq!(config.public_url, "http://127.0.0.1:8080".parse().unwrap());
        assert!(config.sign_key.is_none());
    }

    #[test]
    fn the_shipped_example_is_valid() {
        let settings = Settings::parse(include_str!("../../../contrib/remora.toml")).unwrap();
        assert!(settings.ice.force_relay);
        assert!(settings.login.auto_confirm);
        assert_eq!(settings.ice.servers.len(), 2);
    }

    #[test]
    fn misspelt_settings_are_rejected() {
        for text in ["listn = \"127.0.0.1:1\"", "[ice]\nforce_rely = true"] {
            assert!(
                matches!(Settings::parse(text), Err(SettingsError::Parse(_))),
                "{text}"
            );
        }
        assert!(Settings::parse("public_url = \"remora.example\"").is_err());
        assert!(Settings::parse("[ice]\nservers = [\"http://x\"]").is_err());
    }

    #[test]
    fn secret_files_are_relative_to_the_settings_file() {
        let dir = temp_dir("relative");
        std::fs::write(dir.join("turn.secret"), "s3cret\n").unwrap();
        let path = dir.join("remora.toml");
        std::fs::write(
            &path,
            format!("[ice]\nservers = [\"turn:turn.example\"]\nturn_secret_file = \"turn.secret\"\n{CONFIRMING}"),
        )
        .unwrap();

        let settings = Settings::load(&path).unwrap();
        assert_eq!(settings.ice.turn_secret_file, Some(dir.join("turn.secret")));
        assert!(settings.into_config().is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn errors_name_the_file() {
        let missing = Path::new("/nonexistent/remora.toml");
        let error = Settings::load(missing).unwrap_err().to_string();
        assert!(error.contains("/nonexistent/remora.toml"), "{error}");

        let dir = temp_dir("empty");
        std::fs::write(dir.join("key"), "\n").unwrap();
        let settings = Settings {
            sign_key_file: Some(dir.join("key")),
            ..parse("")
        };
        let error = settings.into_config().err().unwrap().to_string();
        assert!(error.ends_with("key is empty"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn listening_on_all_addresses_needs_a_public_url() {
        let settings = parse("listen = \"[::]:8080\"");
        assert!(matches!(
            settings.into_config(),
            Err(SettingsError::PublicUrlRequired)
        ));
        let settings = parse("listen = \"[::]:8080\"\npublic_url = \"https://remora.example\"");
        assert!(settings.into_config().is_ok());
    }

    #[test]
    fn relaying_needs_turn() {
        let settings = parse("[ice]\nforce_relay = true");
        assert!(matches!(
            settings.into_config(),
            Err(SettingsError::Ice(IceError::RelayWithoutTurn))
        ));
    }
}
