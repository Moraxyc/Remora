//! What survives a restart, kept in SQLite: the token key, users, registered devices, user
//! sessions, signed-in browsers, devices' LAN reports, uploaded files and devices' wallpapers.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

use crate::contracts::random;

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
    "
    -- Images clients uploaded. Only the last few of each uploader are kept, and those that
    -- are a device's wallpaper.
    CREATE TABLE files (
        -- Random, because it is in the file's URL, which is served to anyone who has it.
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL,
        -- The user's device that uploaded it, if the client named one of theirs.
        device_id TEXT,
        mime TEXT NOT NULL,
        md5 TEXT NOT NULL,
        content BLOB NOT NULL
    ) STRICT;
    CREATE INDEX files_by_uploader ON files (user_id, device_id);
    -- The image each device shows on its card.
    CREATE TABLE wallpapers (
        device_id TEXT PRIMARY KEY,
        file_id TEXT NOT NULL
    ) STRICT;
",
    "
    ALTER TABLE devices ADD COLUMN controllable INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE devices ADD COLUMN version_name TEXT NOT NULL DEFAULT '';
    CREATE TABLE device_apps (
        device_id TEXT PRIMARY KEY,
        apps_json TEXT NOT NULL
    ) STRICT;
",
    "
    CREATE TABLE guests (
        id TEXT PRIMARY KEY,
        device_id TEXT NOT NULL,
        room_created INTEGER NOT NULL DEFAULT 0,
        created INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX guests_by_device ON guests (device_id);
    CREATE TABLE device_shares (
        device_id TEXT PRIMARY KEY,
        connect_id TEXT NOT NULL UNIQUE,
        guest_id TEXT,
        active INTEGER NOT NULL DEFAULT 1,
        updated INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE shared_devices (
        user_id TEXT NOT NULL,
        device_id TEXT NOT NULL,
        remark TEXT NOT NULL DEFAULT '',
        is_favorite INTEGER NOT NULL DEFAULT 0,
        last_connected_at INTEGER NOT NULL DEFAULT 0,
        favorited_at INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (user_id, device_id)
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
    token_key: [u8; 32],
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
        let token_key = token_key(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            token_key,
        })
    }

    pub(crate) fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().expect("database lock poisoned")
    }

    pub(crate) fn token_key(&self) -> [u8; 32] {
        self.token_key
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
        use crate::contracts::{TokenKey, UserToken};

        let user = UserToken {
            user_id: "1".into(),
            session_id: "s".into(),
        };
        let key = TokenKey::new(Database::open(&path).unwrap().token_key());
        let token = key.issue_user(&user);
        let reopened = TokenKey::new(Database::open(&path).unwrap().token_key());
        assert_eq!(reopened.verify_user(&token), Some(user));
        let ephemeral = TokenKey::new(Database::in_memory().token_key());
        assert_eq!(ephemeral.verify_user(&token), None);
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
