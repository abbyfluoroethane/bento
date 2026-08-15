//! SQLite persistence for the Bento control plane (SPEC section 12).
//!
//! The control plane is the only writer (SPEC section 4). One shared
//! connection serializes every operation in this process, so transactions
//! and check-then-insert pairs cannot interleave with another writer.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, Transaction, TransactionBehavior};
use time::OffsetDateTime;

mod dump;
mod hosts;
mod images;
mod instances;
mod names;
mod shares;
mod sshkeys;
mod tokens;
mod users;

pub use users::Usage;

/// The full database schema from SPEC section 12.
pub const SCHEMA_SQL: &str = include_str!("schema.sql");

/// Any failure from the Bento data layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A lookup or scoped delete matched no row.
    #[error("store: not found")]
    NotFound,
    /// A live instance already holds the requested deployment-wide name
    /// (SPEC 7.2).
    #[error("store: name is taken by an existing instance")]
    NameTaken,
    /// A token row exists but its expiry has passed (SPEC 13). The row
    /// travels with the error on purpose: the auth service enforces
    /// expiry against its own clock, so it needs the row the store
    /// already rejected.
    #[error("store: token expired")]
    TokenExpired(Box<bento_types::Token>),
    /// A create or resize would exceed one of the four limits (SPEC 6.1).
    #[error("store: quota exceeded: {limit} limit is {max}, {used} in use, {requested} requested")]
    Quota {
        limit: &'static str,
        used: i64,
        requested: i64,
        max: i64,
    },
    /// A released name is still reserved for its previous owner (SPEC 7.2).
    /// `remaining` feeds the error message shown by the CLI (SPEC 15).
    #[error(
        "store: name {name:?} was released by another user and is in cooldown for another {remaining:?}"
    )]
    NameCooldown { name: String, remaining: Duration },
    /// Every `/24` in the configured private range is allocated (SPEC 6.2).
    #[error("store: no free /24 left in the private range")]
    SubnetsExhausted,
    /// The supplied private range cannot be divided into IPv4 `/24`s.
    #[error("store: private range {0}")]
    InvalidPrivateRange(String),
    /// A database dump never replaces an existing file (SPEC 12.1).
    #[error("store: dump destination {path} already exists")]
    DumpDestinationExists { path: String },
    /// The dump destination could not be inspected or created.
    #[error("store: dump destination {path}: {source}")]
    DumpDestination {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The single connection mutex was poisoned by a panic.
    #[error("store: database connection mutex poisoned")]
    MutexPoisoned,
    /// Explicit close requires this to be the final handle to the store.
    #[error("store: cannot close database while another store handle or operation is active")]
    ConnectionInUse,
    /// A blocking database task was cancelled or panicked.
    #[error("store: database task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// SQLite rejected an operation or stored value.
    #[error("store: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// The result type returned by store operations.
pub type Result<T> = std::result::Result<T, Error>;

type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// Bento's data layer. All methods are safe for concurrent use; the single
/// underlying connection serializes them.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    now: Clock,
}

impl Store {
    /// Opens or creates the database, applies the connection pragmas before
    /// the schema, and leaves the connection ready for use (SPEC 12).
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_clock(path, OffsetDateTime::now_utc).await
    }

    /// Opens a store with an injected time source. Deterministic callers use
    /// this for name cooldown and token expiry behavior.
    pub async fn open_with_clock<F>(path: impl AsRef<Path>, now: F) -> Result<Self>
    where
        F: Fn() -> OffsetDateTime + Send + Sync + 'static,
    {
        let path = path.as_ref().to_path_buf();
        let conn = tokio::task::spawn_blocking(move || open_connection(&path)).await??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            now: Arc::new(now),
        })
    }

    /// Closes the database. This must be the final cloned handle and no
    /// cancelled blocking operation may still be finishing; otherwise the
    /// shared connection remains open and [`Error::ConnectionInUse`] is
    /// returned.
    pub async fn close(self) -> Result<()> {
        let connection = Arc::try_unwrap(self.conn).map_err(|_| Error::ConnectionInUse)?;
        tokio::task::spawn_blocking(move || {
            let connection = connection.into_inner().map_err(|_| Error::MutexPoisoned)?;
            connection
                .close()
                .map_err(|(_, error)| Error::Sqlite(error))
        })
        .await?
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| Error::MutexPoisoned)?;
            f(&guard)
        })
        .await?
    }

    /// Runs `f` in `BEGIN IMMEDIATE`, holding SQLite's write lock from the
    /// transaction's first statement.
    async fn with_tx<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_| Error::MutexPoisoned)?;
            let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        })
        .await?
    }

    fn clock(&self) -> Clock {
        Arc::clone(&self.now)
    }
}

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(conn)
}

fn format_time(value: OffsetDateTime) -> rusqlite::Result<String> {
    value
        .to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn parse_time(column: usize, value: &str) -> rusqlite::Result<OffsetDateTime> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests;
