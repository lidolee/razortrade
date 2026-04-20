//! Typed JSON payloads for Kraken Futures WebSocket and REST APIs.
//!
//! We use `serde(untagged)` and explicit variants rather than
//! `serde_json::Value` wherever possible, so a schema drift produces a
//! compile-time or parse-time error rather than silent data loss.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// ============================================================
// WebSocket: outbound (client → server)
// ============================================================

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum WsOutboundEvent {
    Subscribe(SubscribeRequest),
    Unsubscribe(UnsubscribeRequest),
    Challenge { api_key: String },
    #[serde(rename = "pong")]
    Pong,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubscribeRequest {
    pub feed: String,
    pub product_ids: Vec<String>,
    /// For private feeds: the original challenge UUID received from the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_challenge: Option<String>,
    /// For private feeds: the signed challenge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signed_challenge: Option<String>,
    /// For private feeds: the API key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnsubscribeRequest {
    pub feed: String,
    pub product_ids: Vec<String>,
}

// ============================================================
// WebSocket: inbound (server → client)
// ============================================================

/// All inbound WS messages have either a `feed` field (data messages)
/// or an `event` field (control messages). We parse permissively and
/// route on the presence of these fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum WsInboundMessage {
    /// Control messages: subscribe/unsubscribe acks, challenge, errors.
    Event(WsEvent),
    /// Data messages: book snapshots, deltas, tickers, heartbeats.
    Feed(WsFeed),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WsEvent {
    Info {
        version: Option<u64>,
    },
    #[serde(rename = "subscribed")]
    Subscribed {
        feed: String,
        product_ids: Option<Vec<String>>,
    },
    #[serde(rename = "unsubscribed")]
    Unsubscribed {
        feed: String,
        product_ids: Option<Vec<String>>,
    },
    Challenge {
        message: String,
    },
    #[serde(rename = "alert")]
    Alert {
        message: String,
    },
    /// Subscription error or other runtime error from the server.
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default)]
        #[allow(dead_code)]
        code: Option<i64>,
    },
    /// Server-side pong (we sent a ping event).
    Pong,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "feed", rename_all = "snake_case")]
pub enum WsFeed {
    Heartbeat {
        time: Option<u64>,
    },
    #[serde(rename = "book_snapshot")]
    BookSnapshot(BookSnapshotMessage),
    #[serde(rename = "book")]
    BookDelta(BookDeltaMessage),
    Ticker(TickerMessage),

    // ---- Private feeds ------------------------------------------------
    #[serde(rename = "fills_snapshot")]
    FillsSnapshot(FillsSnapshotMessage),
    #[serde(rename = "fills")]
    Fills(FillsMessage),
    #[serde(rename = "open_orders_snapshot")]
    OpenOrdersSnapshot(OpenOrdersSnapshotMessage),
    #[serde(rename = "open_orders")]
    OpenOrders(OpenOrdersUpdateMessage),
    #[serde(rename = "open_orders_verbose_snapshot")]
    OpenOrdersVerboseSnapshot(OpenOrdersSnapshotMessage),
    #[serde(rename = "open_orders_verbose")]
    OpenOrdersVerbose(OpenOrdersUpdateMessage),

    /// Unknown / not-yet-modelled feeds are parsed as `Other`. The daemon
    /// logs them and moves on rather than crashing.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BookSnapshotMessage {
    pub product_id: String,
    pub timestamp: u64,
    pub seq: u64,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BookDeltaMessage {
    pub product_id: String,
    pub timestamp: u64,
    pub seq: u64,
    pub side: BookSide,
    pub price: Decimal,
    pub qty: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BookSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriceLevel {
    pub price: Decimal,
    pub qty: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TickerMessage {
    pub product_id: String,
    #[serde(default)]
    pub bid: Option<Decimal>,
    #[serde(default)]
    pub ask: Option<Decimal>,
    #[serde(default)]
    pub bid_size: Option<Decimal>,
    #[serde(default)]
    pub ask_size: Option<Decimal>,
    /// Per-second funding rate for perpetuals.
    #[serde(default)]
    pub funding_rate: Option<Decimal>,
    #[serde(default)]
    pub funding_rate_prediction: Option<Decimal>,
    /// Relative funding per-second (rate × mark price).
    #[serde(default)]
    pub relative_funding_rate: Option<Decimal>,
    #[serde(default)]
    pub next_funding_rate_time: Option<u64>,
    #[serde(default)]
    pub time: Option<u64>,
    #[serde(default)]
    pub last: Option<Decimal>,
}

impl TickerMessage {
    /// Kraken publishes per-second funding rates. For our checklist the
    /// relevant horizon is the per-8h figure (used by
    /// `RiskConfig::max_funding_rate_per_8h`). Multiply by 8 × 3600.
    pub fn funding_rate_per_8h(&self) -> Option<Decimal> {
        self.funding_rate.map(|r| r * Decimal::from(8 * 3600))
    }
}

// ============================================================
// Private WS feeds: fills
// ============================================================

/// Initial snapshot of recent fills, sent immediately after subscribing to
/// the `fills` feed.
#[derive(Debug, Clone, Deserialize)]
pub struct FillsSnapshotMessage {
    pub account: String,
    #[serde(default)]
    pub fills: Vec<FillEntry>,
}

/// Incremental fill message. Kraken sends one of these per executed trade.
#[derive(Debug, Clone, Deserialize)]
pub struct FillsMessage {
    pub account: String,
    #[serde(default)]
    pub fills: Vec<FillEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FillEntry {
    pub instrument: String,
    pub time: u64,
    pub price: Decimal,
    /// Per-account monotonic sequence number of this fill message.
    pub seq: u64,
    /// true if the fill was on the buy side.
    pub buy: bool,
    pub qty: Decimal,
    #[serde(default)]
    pub remaining_order_qty: Option<Decimal>,
    pub order_id: String,
    pub fill_id: String,
    /// "maker" | "taker" | "liquidation" | "assignee" | "assignor" | ...
    pub fill_type: String,
    pub fee_paid: Decimal,
    pub fee_currency: String,
    #[serde(default)]
    pub cli_ord_id: Option<String>,
    #[serde(default)]
    pub order_type: Option<String>,
    #[serde(default)]
    pub taker_order_type: Option<String>,
}

impl FillEntry {
    pub fn is_maker(&self) -> bool {
        self.fill_type == "maker"
    }

    pub fn side(&self) -> crate::messages::BookSide {
        if self.buy {
            BookSide::Buy
        } else {
            BookSide::Sell
        }
    }
}

// ============================================================
// Private WS feeds: open_orders
// ============================================================

/// Snapshot of open orders at subscription time. Shared between
/// `open_orders_snapshot` and `open_orders_verbose_snapshot`.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenOrdersSnapshotMessage {
    pub account: String,
    #[serde(default)]
    pub orders: Vec<OpenOrderVerboseEntry>,
}

/// Incremental open-order update. Shared between `open_orders` and
/// `open_orders_verbose`. `is_cancel=true` means the order must be
/// removed from the local snapshot.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenOrdersUpdateMessage {
    #[serde(default)]
    pub order: Option<OpenOrderVerboseEntry>,
    #[serde(default)]
    pub is_cancel: bool,
    /// When `is_cancel=true`, Kraken may send only the order_id + reason
    /// without the full order body. We deserialise those into `order_id`.
    #[serde(default)]
    pub order_id: Option<String>,
    /// Textual reason. One of the documented strings such as
    /// `new_placed_order_by_user`, `liquidation`, `cancelled_by_user`, ...
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenOrderVerboseEntry {
    pub instrument: String,
    pub time: u64,
    #[serde(default)]
    pub last_update_time: Option<u64>,
    pub qty: Decimal,
    #[serde(default)]
    pub filled: Option<Decimal>,
    #[serde(default)]
    pub limit_price: Option<Decimal>,
    #[serde(default)]
    pub stop_price: Option<Decimal>,
    /// "limit" | "stop" | "take_profit" | "trailing_stop" | ...
    #[serde(default, rename = "type")]
    pub order_type: Option<String>,
    pub order_id: String,
    #[serde(default)]
    pub cli_ord_id: Option<String>,
    /// 0 = buy, 1 = sell (Kraken convention).
    pub direction: u8,
    #[serde(default)]
    pub reduce_only: Option<bool>,
    #[serde(default, rename = "triggerSignal")]
    pub trigger_signal: Option<String>,
}

impl OpenOrderVerboseEntry {
    pub fn side(&self) -> BookSide {
        if self.direction == 0 {
            BookSide::Buy
        } else {
            BookSide::Sell
        }
    }
}

// ============================================================
// REST: order submission
// ============================================================

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOrderRequest {
    pub order_type: KrakenOrderType,
    pub symbol: String,
    pub side: KrakenSide,
    pub size: Decimal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_ord_id: Option<String>,
    /// For `PostOnly` submissions via REST, Kraken uses a separate flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
    /// Kraken's time-in-force options: `gtc` | `ioc` | `gtd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_signal: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KrakenOrderType {
    /// Market order.
    Mkt,
    /// Limit order.
    Lmt,
    /// Post-only limit.
    Post,
    /// Immediate-or-cancel.
    Ioc,
    /// Stop market.
    Stp,
    /// Stop limit.
    #[serde(rename = "take_profit")]
    TakeProfit,
    #[serde(rename = "trailing_stop")]
    TrailingStop,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KrakenSide {
    Buy,
    Sell,
}

// ============================================================
// REST: responses
// ============================================================

/// Envelope shared by all Kraken Futures v3 responses. `result` is
/// `"success"` for happy paths; any other value means an error and
/// `error` contains the detail.
#[derive(Debug, Clone, Deserialize)]
pub struct ResponseEnvelope {
    pub result: String,
    pub error: Option<String>,
    #[serde(rename = "serverTime")]
    pub server_time: Option<String>,
}

impl ResponseEnvelope {
    pub fn is_success(&self) -> bool {
        self.result == "success"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendOrderResponse {
    pub result: String,
    pub error: Option<String>,
    #[serde(rename = "sendStatus")]
    pub send_status: Option<SendStatus>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendStatus {
    #[serde(rename = "order_id")]
    pub order_id: Option<String>,
    pub status: Option<String>,
    pub received_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CancelOrderResponse {
    pub result: String,
    pub error: Option<String>,
    #[serde(rename = "cancelStatus")]
    pub cancel_status: Option<CancelStatus>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelStatus {
    #[serde(rename = "order_id")]
    pub order_id: Option<String>,
    pub status: Option<String>,
    pub received_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenOrdersResponse {
    pub result: String,
    pub error: Option<String>,
    #[serde(rename = "openOrders", default)]
    pub open_orders: Vec<OpenOrderEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOrderEntry {
    pub order_id: String,
    pub symbol: String,
    pub side: KrakenSide,
    pub order_type: Option<String>,
    pub limit_price: Option<Decimal>,
    pub filled_size: Option<Decimal>,
    pub unfilled_size: Option<Decimal>,
    pub status: Option<String>,
    pub received_time: Option<String>,
    pub last_update_time: Option<String>,
    /// Drop 19 — CV-A1a: Kraken echoes the client-generated id we sent
    /// on the original `sendorder`. Present on orders we submitted
    /// ourselves; absent for orders placed via the web UI.
    #[serde(default)]
    pub cli_ord_id: Option<String>,
}
