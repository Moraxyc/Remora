//! The settings file: one TOML document configures the whole server.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use remora_proto::cloud::SignKey;
use serde::Deserialize;

use crate::oidc::{InvalidOidc, OidcProvider, OidcRegistration};
use crate::{
    Config, Database, DatabaseError, Ice, IceError, IceUrl, InvalidPasswordHash, OwnerPassword,
    Ping, PublicUrl, ReconnectGrace, TrustedProxies, TrustedProxy, TurnSecret,
};

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
    /// Reverse proxies, by address or network, whose `X-Forwarded-For` names the client.
    #[serde(default)]
    pub trusted_proxies: Vec<TrustedProxy>,
    /// File holding the request signing key. Without it, signatures are not checked.
    pub sign_key_file: Option<PathBuf>,
    /// SQLite database for what outlives a restart: registered devices and the key that signs
    /// tokens. Without it, they live in memory, so a restart logs everyone out and forgets
    /// every device.
    pub database: Option<PathBuf>,
    /// Log filter, such as `info` or `info,remora_cloud=debug`.
    #[serde(default = "default_log")]
    pub log: String,
    #[serde(default)]
    pub ice: IceSettings,
    #[serde(default)]
    pub login: LoginSettings,
    /// The OpenID Connect provider users sign in with, if any.
    pub oidc: Option<OidcSettings>,
}

/// An OpenID Connect provider, where Remora is registered as a client whose redirect URI is
/// `{public_url}/api/web/oidc/callback`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcSettings {
    /// The provider's issuer URL, where its discovery document is.
    pub issuer: String,
    pub client_id: String,
    /// File holding the client secret. Without it, Remora is a public client and relies on PKCE.
    pub client_secret_file: Option<PathBuf>,
    /// What the sign-in button calls the provider; by default its host name.
    pub name: Option<String>,
}

/// How QR logins get confirmed: by the owner, signed in with the password, or automatically.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginSettings {
    /// File holding the owner's password hash, as `remora hash-password` makes it.
    pub password_file: Option<PathBuf>,
    /// Confirm every QR login without asking anyone, so that anyone who can reach the server
    /// logs in as its user.
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
        let oidc_secret = settings
            .oidc
            .as_mut()
            .and_then(|oidc| oidc.client_secret_file.as_mut());
        for file in [
            settings.sign_key_file.as_mut(),
            settings.ice.turn_secret_file.as_mut(),
            settings.login.password_file.as_mut(),
            oidc_secret,
            settings.database.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            *file = base.join(&*file);
        }
        Ok(settings)
    }

    /// Reads the secret files, opens the database and checks that the settings fit together.
    pub fn into_config(self) -> Result<Config, SettingsError> {
        if !self.login.auto_confirm && self.login.password_file.is_none() && self.oidc.is_none() {
            return Err(SettingsError::LoginsUnconfirmable);
        }
        let owner_password = self
            .login
            .password_file
            .as_deref()
            .map(read_password)
            .transpose()?;
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
        let database = match &self.database {
            Some(path) => Database::open(path).map_err(|source| SettingsError::Database {
                path: path.clone(),
                source,
            })?,
            None => Database::in_memory(),
        };
        let oidc = self
            .oidc
            .map(|oidc| oidc.into_provider(&public_url))
            .transpose()?;
        Ok(Config {
            public_url,
            sign_key,
            ping: Ping::default(),
            reconnect_grace: ReconnectGrace::default(),
            ice,
            database,
            auto_confirm: self.login.auto_confirm,
            owner_password,
            oidc,
            trusted_proxies: TrustedProxies::new(self.trusted_proxies),
        })
    }
}

impl OidcSettings {
    fn into_provider(self, public_url: &PublicUrl) -> Result<OidcProvider, SettingsError> {
        let client_secret = match &self.client_secret_file {
            Some(path) => Some(
                String::from_utf8(read_secret(path)?)
                    .map_err(|_| SettingsError::NotText(path.clone()))?,
            ),
            None => None,
        };
        let name = match self.name {
            Some(name) => name,
            None => openidconnect::url::Url::parse(&self.issuer)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| self.issuer.clone()),
        };
        let registration = OidcRegistration {
            issuer: self.issuer,
            client_id: self.client_id,
            client_secret,
            name,
        };
        OidcProvider::new(registration, &public_url.to_string()).map_err(SettingsError::Oidc)
    }
}

fn read_password(path: &Path) -> Result<OwnerPassword, SettingsError> {
    let invalid = |source| SettingsError::PasswordHash {
        path: path.to_owned(),
        source,
    };
    let hash = String::from_utf8(read_secret(path)?).map_err(|_| invalid(InvalidPasswordHash))?;
    OwnerPassword::parse(&hash).map_err(invalid)
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
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Parse(toml::de::Error),
    EmptySecret(PathBuf),
    NotText(PathBuf),
    Oidc(InvalidOidc),
    PasswordHash {
        path: PathBuf,
        source: InvalidPasswordHash,
    },
    Ice(IceError),
    Database {
        path: PathBuf,
        source: DatabaseError,
    },
    PublicUrlRequired,
    LoginsUnconfirmable,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse(error) => write!(f, "invalid settings: {error}"),
            Self::EmptySecret(path) => write!(f, "{} is empty", path.display()),
            Self::NotText(path) => write!(f, "{} is not UTF-8 text", path.display()),
            Self::Oidc(error) => error.fmt(f),
            Self::PasswordHash { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Ice(error) => error.fmt(f),
            Self::Database { path, source } => {
                write!(f, "cannot open {}: {source}", path.display())
            }
            Self::PublicUrlRequired => {
                f.write_str("public_url is required when listening on all addresses")
            }
            Self::LoginsUnconfirmable => f.write_str(
                "nobody can confirm logins: set login.password_file or oidc, or login.auto_confirm = true",
            ),
        }
    }
}

impl std::error::Error for SettingsError {}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIRMING: &str = "[login]\nauto_confirm = true\n";

    /// `text` plus a way to confirm logins, which every working settings file needs.
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
    fn someone_must_confirm_logins() {
        assert!(matches!(
            Settings::parse("").unwrap().into_config(),
            Err(SettingsError::LoginsUnconfirmable)
        ));
    }

    #[test]
    fn the_owner_password_is_an_argon2id_hash() {
        let dir = temp_dir("password");
        let hash = OwnerPassword::hash("hunter2");
        std::fs::write(dir.join("good"), format!("{}\n", hash.as_phc())).unwrap();
        std::fs::write(dir.join("plain"), "hunter2\n").unwrap();
        let with_password = |file: &str| {
            Settings::parse(&format!(
                "[login]\npassword_file = \"{}\"",
                dir.join(file).display()
            ))
            .unwrap()
            .into_config()
        };

        let config = with_password("good").unwrap();
        assert!(!config.auto_confirm);
        assert!(config.owner_password.unwrap().verify("hunter2"));
        let error = with_password("plain").err().unwrap().to_string();
        assert!(
            error.contains("plain: expected an argon2id hash"),
            "{error}"
        );
        std::fs::remove_dir_all(dir).unwrap();
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
        assert!(!settings.login.auto_confirm);
        assert!(settings.login.password_file.is_some());
        assert_eq!(settings.ice.servers.len(), 2);
        assert_eq!(settings.trusted_proxies.len(), 2);
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
        assert!(Settings::parse("trusted_proxies = [\"localhost\"]").is_err());
    }

    #[test]
    fn paths_are_relative_to_the_settings_file() {
        let dir = temp_dir("relative");
        std::fs::write(dir.join("turn.secret"), "s3cret\n").unwrap();
        let path = dir.join("remora.toml");
        std::fs::write(
            &path,
            format!(
                "database = \"remora.db\"\n\
                 [ice]\nservers = [\"turn:turn.example\"]\nturn_secret_file = \"turn.secret\"\n\
                 {CONFIRMING}"
            ),
        )
        .unwrap();

        let settings = Settings::load(&path).unwrap();
        assert_eq!(settings.ice.turn_secret_file, Some(dir.join("turn.secret")));
        assert!(settings.into_config().is_ok());
        assert!(dir.join("remora.db").exists());
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
    fn an_oidc_provider_confirms_logins_too() {
        let dir = temp_dir("oidc");
        std::fs::write(dir.join("oidc.secret"), "s3cret\n").unwrap();
        let path = dir.join("remora.toml");
        std::fs::write(
            &path,
            "[oidc]\nissuer = \"https://auth.example/realm\"\nclient_id = \"remora\"\n\
             client_secret_file = \"oidc.secret\"\n",
        )
        .unwrap();
        let config = Settings::load(&path).unwrap().into_config().unwrap();
        let provider = config.oidc.unwrap();
        assert_eq!(provider.name, "auth.example", "named after its host");
        assert!(!format!("{provider:?}").contains("s3cret"));

        let unnamed = Settings::parse("[oidc]\nissuer = \"not a url\"\nclient_id = \"r\"");
        assert!(matches!(
            unnamed.unwrap().into_config(),
            Err(SettingsError::Oidc(InvalidOidc::Issuer))
        ));
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
