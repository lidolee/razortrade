//! # rt-persistence
//!
//! SQLite-based persistence layer. Provides a typed API over the schema
//! defined in `migrations/`.
//!
//! ## Why SQLite?
//!
//! At our scale (single-node, low-to-medium write rate, ≤ few million rows
//! over 5 years), SQLite is superior to PostgreSQL/TimescaleDB:
//!   * Zero operational overhead: no server, no backups config, no users.
//!   * Transactional guarantees identical to "serious" RDBMS at this scale.
//!   * File-based → atomic filesystem snapshots (btrfs/zfs) = atomic backup.
//!   * Perfect for the Python-writes-Rust-reads decoupling pattern.
//!
//! ## Concurrency model
//!
//! We use WAL (write-ahead logging) mode so readers do not block writers.
//! The Python signal generator writes to `signals`; the Rust daemon polls
//! `WHERE status = 'pending'` at 1 Hz. With WAL, the polling read never
//! blocks the Python write.

pub mod models;
pub mod repo;

pub use repo::Database;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("SQL error: {0}")]
    Sql(#[from] sqlx::Error),

    #[error("Migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("Decimal parse error: {0}")]
    Decimal(String),

    #[error("JSON decode error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Invalid enum value for column {column}: {value}")]
    InvalidEnum { column: &'static str, value: String },
}

pub type Result<T> = std::result::Result<T, PersistenceError>;
