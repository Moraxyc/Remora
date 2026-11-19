//! What survives a restart, kept in SQLite.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

use crate::token::{TokenKey, random};

/// Schema changes, applied in order. `PRAGMA user_version` counts those already applied.
const MIGRATIONS: &[&str] = &["
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
"];

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
        let token = Database::open(&path).unwrap().tokens().issue_user("1");
        let reopened = Database::open(&path).unwrap();
        assert_eq!(reopened.tokens().verify_user(&token).as_deref(), Some("1"));
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
