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
pub(crate) mod migrate;
mod names;
mod pairings;
mod runners;
mod shares;
mod sshkeys;
mod tokens;
mod users;

pub use runners::{HostHealth, HostSeen, Observation};
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
    /// A create or resize would provision more than the host holds
    /// (SPEC 6.1). `resource` is `memory` or `disk`, and the three
    /// numbers share that resource's unit.
    #[error(
        "store: the host has no room: the {resource} limit is {limit}, {used} provisioned, {requested} requested"
    )]
    Capacity {
        resource: &'static str,
        used: i64,
        requested: i64,
        limit: i64,
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
    /// Another controller process holds the lease (MULTI-NODE 11.3).
    #[error("store: the controller lease is held by {holder} until {expires_at}")]
    LeaseHeld {
        holder: String,
        expires_at: OffsetDateTime,
    },
    /// This process no longer holds the lease it tried to renew. It must
    /// stop dispatching: a later controller has raised the epoch.
    #[error("store: the controller lease was lost")]
    LeaseLost,
    /// The runner prefix must be 24, 25, 26, or 27 (MULTI-NODE 7.1).
    #[error("store: runner prefix {0} is not 24, 25, 26, or 27")]
    RunnerPrefix(u8),
    /// A smaller prefix would leave an owned slot with no number.
    #[error("store: prefix {prefix} has no room for slot {slot}, which is owned")]
    RunnerPrefixStrandsSlot { prefix: u8, slot: i64 },
    /// The slot number is outside what the runner prefix divides into.
    #[error("store: slot {slot} does not exist: the deployment has {count}")]
    NoSuchSlot { slot: i64, count: i64 },
    /// Instances still hold addresses the slot supplied (MULTI-NODE 17).
    #[error("store: slot {slot} still holds {occupied} instances of another host")]
    SlotInUse { slot: i64, occupied: i64 },
    /// No runner can take a new instance (MULTI-NODE 12). The message
    /// names every runner and why each one was refused, because "no
    /// room" alone does not say which machine to fix.
    #[error("store: no runner can take this instance: {reasons}")]
    NoPlacement { reasons: String },
    /// A restore source must exist and be a readable SQLite database.
    #[error("store: restore source {path}: {source}")]
    RestoreSource {
        path: String,
        #[source]
        source: std::io::Error,
    },
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
        let now: Clock = Arc::new(now);
        let clock = Arc::clone(&now);
        let conn = tokio::task::spawn_blocking(move || open_connection(&path, clock)).await??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            now,
        })
    }

    /// Opens an existing database for reading and nothing else.
    ///
    /// The control plane is the only writer (SPEC 4), and a migration is
    /// a deliberate operation an operator takes a copy before
    /// (CLAUDE.md). A reader must therefore neither create the file nor
    /// carry it forward: `bento-monitor` reads the fleet through this
    /// while `bentod serve` is running, and a reader that migrated would
    /// change the database behind the process that owns it.
    ///
    /// The connection is opened read-write and then held to
    /// `query_only`, rather than opened `SQLITE_OPEN_READONLY`. A
    /// read-only connection cannot map the shared-memory file that WAL
    /// needs, so it refuses a database whose writer is not running: the
    /// one case where an operator most wants to read it.
    pub async fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let now: Clock = Arc::new(OffsetDateTime::now_utc);
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let conn = Connection::open_with_flags(&path, flags)?;
            conn.busy_timeout(Duration::from_millis(5000))?;
            conn.pragma_update(None, "query_only", true)?;
            Ok(conn)
        })
        .await??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            now,
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

    /// Like [`Store::with_conn`], for the two callers that need the
    /// connection itself: SQLite's backup API and the migrations both
    /// write through a `&mut Connection`.
    async fn with_conn_mut<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_| Error::MutexPoisoned)?;
            f(&mut guard)
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

fn open_connection(path: &Path, now: Clock) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    // The baseline creates a new database and leaves an existing one
    // alone; the numbered migrations carry it the rest of the way
    // (MULTI-NODE 16).
    conn.execute_batch(SCHEMA_SQL)?;
    migrate::apply(&mut conn, now)?;
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
