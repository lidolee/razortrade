//! # rt-kraken-futures
//!
//! Broker client for Kraken Futures (the perpetuals venue at
//! `futures.kraken.com`, separate from spot `api.kraken.com`).
//!
//! ## Layout
//!
//! - [`auth`] — HMAC-SHA-512 signing for REST `Authent` header and the
//!   WebSocket challenge/response flow.
//! - [`messages`] — typed JSON payloads for the WS and REST APIs.
//! - [`orderbook`] — local L2 order book with sequence-number validation.
//! - [`rest`] — REST client implementing [`rt_execution::Broker`].
//! - [`ws`] — WebSocket client with reconnect state machine.
//!
//! ## What this crate does NOT do
//!
//! - It does not take trading decisions. Every method translates a
//!   well-specified Kraken request/response and propagates errors.
//! - It does not cache or aggregate orders beyond the order book. Order
//!   state persistence is the daemon's job.
//! - It does not handle margin or position reconciliation; those queries
//!   are made via the public `Broker` trait methods.

pub mod auth;
pub mod accounts;
pub mod messages;
pub mod orderbook;
pub mod private_state;
pub mod rest;
pub mod ws;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum KrakenError {
    #[error("auth error: {0}")]
    Auth(#[from] auth::AuthError),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("websocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("kraken api returned error: {code:?} — {message}")]
    Api { code: Option<String>, message: String },

    #[error("orderbook error: {0}")]
    Book(#[from] orderbook::BookError),

    #[error("unexpected message kind: {0}")]
    UnexpectedMessage(String),

    #[error("timeout waiting for {what}")]
    Timeout { what: String },

    #[error("sequence number gap on {product}: expected {expected}, got {got}")]
    SequenceGap { product: String, expected: u64, got: u64 },
}

pub type KrakenResult<T> = std::result::Result<T, KrakenError>;

/// Credentials for authenticated endpoints.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub api_key: String,
    /// Base64-encoded secret, exactly as displayed in Kraken account management.
    /// Do NOT pre-decode — the signing code handles that.
    pub api_secret_b64: String,
}

impl Credentials {
    /// Load credentials from environment variables
    /// `RT_KRAKEN_API_KEY` and `RT_KRAKEN_API_SECRET`. Returns `None`
    /// if either variable is missing, so the caller can explicitly decide
    /// between failing and falling back to read-only mode.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("RT_KRAKEN_API_KEY").ok()?;
        let api_secret_b64 = std::env::var("RT_KRAKEN_API_SECRET").ok()?;
        Some(Self {
            api_key,
            api_secret_b64,
        })
    }
}

/// Default production URL for Kraken Futures REST v3.
pub const KRAKEN_FUTURES_REST_BASE: &str = "https://futures.kraken.com";

/// Default production URL for Kraken Futures WebSocket v1.
pub const KRAKEN_FUTURES_WS_URL: &str = "wss://futures.kraken.com/ws/v1";

/// Demo / paper-trading environment. Use for all initial integration testing.
pub const KRAKEN_FUTURES_DEMO_REST_BASE: &str = "https://demo-futures.kraken.com";
pub const KRAKEN_FUTURES_DEMO_WS_URL: &str = "wss://demo-futures.kraken.com/ws/v1";
