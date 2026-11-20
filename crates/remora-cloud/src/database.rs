//! What survives a restart, kept in SQLite: the token key, users, registered devices, user
//! sessions, signed-in browsers and devices' LAN reports.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

use crate::token::{TokenKey, random};

/// Schema changes, applied in order. `PRAGMA user_version` counts those already applied.
const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE secrets (
        name TEXT PRIMARY KEY,
        value BLOB NOT NULL
    ) STRICT;
    CREATE TABLE devices (
        id TEXT PRIMARY KEY,
        client_id TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        platform INTEGER NOT NULL,
        owner TEXT
    ) STRICT;
",
    "
    CREATE TABLE sessions (
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL,
        -- The device whose host logged in with it, once that host has created a room.
        device_id TEXT
    ) STRICT;
    CREATE INDEX sessions_by_device ON sessions (device_id);
",
    "
    CREATE TABLE users (
        -- Numeric, because clients and TURN usernames carry it.
        id TEXT PRIMARY KEY,
        -- Its OpenID Connect identity; none for the owner, who signs in with the password.
        issuer TEXT,
        subject TEXT,
        name TEXT NOT NULL,
        UNIQUE (issuer, subject)
    ) STRICT;
    INSERT INTO users (id, name) VALUES ('1', 'Owner');
    -- Browsers signed in to the web app.
    CREATE TABLE browsers (
        -- SHA-256 of the cookie, so that the database holds no usable cookie.
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL,
        csrf TEXT NOT NULL,
        created INTEGER NOT NULL,
        expires INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX browsers_by_user ON browsers (user_id);
",
    "
    ALTER TABLE sessions ADD COLUMN created INTEGER NOT NULL DEFAULT 0;
",
    "
    -- Each device's last LAN identity, for Wake-on-LAN.
    CREATE TABLE lan_reports (
        device_id TEXT PRIMARY KEY,
        mac TEXT NOT NULL,
        ip TEXT NOT NULL,
        subnet_mask TEXT NOT NULL
    ) STRICT;
",
];

/// What `PRAGMA user_version` says once every migration is applied.
const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// How long a write waits for another one to finish before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The open database, and the token key kept in it. Queries are small enough to run on the
/// async workers.
#[derive(Clone)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
    tokens: Arc<TokenKey>,
}

impl Database {
    /// Opens the database at `path`, creating it if needed.
    pub fn open(path: &Path) -> Result<Self, DatabaseError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(connection)
    }

    /// A database that lives and dies with the process.
    pub fn in_memory() -> Self {
        Connection::open_in_memory()
            .map_err(DatabaseError::from)
            .and_then(Self::init)
            .expect("an in-memory database opens")
    }

    fn init(mut connection: Connection) -> Result<Self, DatabaseError> {
        connection.busy_timeout(BUSY_TIMEOUT)?;
        migrate(&mut connection)?;
        let tokens = Arc::new(TokenKey::new(token_key(&connection)?));
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            tokens,
        })
    }

    pub(crate) fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().expect("database lock poisoned")
    }

    pub(crate) fn tokens(&self) -> Arc<TokenKey> {
        self.tokens.clone()
    }
}

impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Database(..)")
    }
}

fn migrate(connection: &mut Connection) -> Result<(), DatabaseError> {
    let applied: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let Some(pending) = MIGRATIONS.get(applied as usize..) else {
        return Err(DatabaseError::Newer);
    };
    let transaction = connection.transaction()?;
    for migration in pending {
        transaction.execute_batch(migration)?;
    }
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(transaction.commit()?)
}

/// The key that signs tokens, made the first time the database is opened, so that tokens
/// outlive restarts.
fn token_key(connection: &Connection) -> rusqlite::Result<[u8; 32]> {
    const QUERY: &str = "SELECT value FROM secrets WHERE name = 'token_key'";
    if let Some(key) = connection
        .query_row(QUERY, [], |row| row.get(0))
        .optional()?
    {
        return Ok(key);
    }
    let key = random();
    connection.execute(
        "INSERT INTO secrets (name, value) VALUES ('token_key', ?1)",
        [key],
    )?;
    Ok(key)
}

#[derive(Debug)]
pub enum DatabaseError {
    Sqlite(rusqlite::Error),
    /// Its schema is newer than this Remora knows.
    Newer,
}

impl From<rusqlite::Error> for DatabaseError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => error.fmt(f),
            Self::Newer => f.write_str("it was written by a newer Remora"),
        }
    }
}

impl std::error::Error for DatabaseError {}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("remora-db-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("remora.db")
    }

    #[test]
    fn the_token_key_outlives_a_restart() {
        let path = temp_file("key");
        let user = crate::token::UserToken {
            user_id: "1".into(),
            session_id: "s".into(),
        };
        let token = Database::open(&path).unwrap().tokens().issue_user(&user);
        let reopened = Database::open(&path).unwrap();
        assert_eq!(reopened.tokens().verify_user(&token), Some(user));
        assert_eq!(Database::in_memory().tokens().verify_user(&token), None);
    }

    #[test]
    fn a_database_from_a_newer_remora_is_refused() {
        let path = temp_file("newer");
        drop(Database::open(&path).unwrap());
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(connection);
        assert!(matches!(Database::open(&path), Err(DatabaseError::Newer)));
    }
}
