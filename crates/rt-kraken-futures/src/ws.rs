//! Kraken Futures WebSocket client.
//!
//! # State machine
//!
//! ```text
//!    ┌──────────────┐ connect() ┌────────────┐ subscribe() ┌───────────┐
//!    │ Disconnected │──────────▶│ Connecting │────────────▶│ PubSynced │
//!    └──────────────┘            └────────────┘             └─────┬─────┘
//!         ▲                           │                          │
//!         │                           │                          │ creds present?
//!         │ error + backoff           │                          ▼
//!         │                           │                    ┌───────────┐
//!         │                           │                    │ Challenge │
//!         │                           ▼                    └─────┬─────┘
//!         └──────────────────── Reconnecting ◀───────────────────┘
//!                                     ▲                           │
//!                                     │   (signed subscribe ok)   ▼
//!                                     │                    ┌───────────┐
//!                                     └────────────────────│   Fully   │
//!                                                          │   Synced  │
//!                                                          └───────────┘
//! ```
//!
//! - `Disconnected` → initial and terminal state.
//! - `Connecting` → TCP+TLS+WS handshake in flight.
//! - `PubSynced` → public subscriptions ack'd, snapshots arriving.
//! - `Challenge` → (auth only) sent `challenge` request, awaiting server UUID.
//! - `FullySynced` → private subscriptions ack'd.
//! - `Reconnecting` → transient failure; exponential backoff then retry.
//!
//! # Concurrency model
//!
//! Public [`BookStore`] / [`TickerStore`] and private
//! [`OpenOrdersStore`] / [`FillsStore`] are `Arc<RwLock<_>>`s shared with
//! the risk-check poller and signal processor. WS writes hold the write
//! lock only briefly (microseconds).

use crate::auth::sign_ws_challenge;
use crate::messages::{
    BookDeltaMessage, BookSnapshotMessage, FillsMessage, FillsSnapshotMessage,
    OpenOrdersSnapshotMessage, OpenOrdersUpdateMessage, SubscribeRequest, TickerMessage, WsEvent,
    WsFeed, WsInboundMessage, WsOutboundEvent,
};
use crate::orderbook::{BookError, LocalOrderBook};
use crate::private_state::{FillsRing, FillsStore, OpenOrdersState, OpenOrdersStore};
use crate::{Credentials, KrakenError, KrakenResult};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, RwLock};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, instrument, warn};

pub type BookStore = Arc<RwLock<HashMap<String, LocalOrderBook>>>;
pub type TickerStore = Arc<RwLock<HashMap<String, TickerMessage>>>;

#[derive(Debug, Clone)]
pub struct WsConfig {
    pub url: String,
    pub products: Vec<String>,
    pub subscribe_ticker: bool,
    pub subscribe_book: bool,
    /// If credentials are present in [`KrakenFuturesWsClient`], subscribe
    /// to these private feeds after the challenge handshake completes.
    pub subscribe_fills: bool,
    pub subscribe_open_orders: bool,
    /// Use the verbose variant of open_orders. Recommended: `true`, which
    /// includes full order bodies with every update.
    pub open_orders_verbose: bool,
    pub max_backoff: Duration,
    pub initial_backoff: Duration,
    pub ping_interval: Duration,
}

impl WsConfig {
    pub fn new(products: Vec<String>) -> Self {
        Self {
            url: crate::KRAKEN_FUTURES_WS_URL.to_string(),
            products,
            subscribe_ticker: true,
            subscribe_book: true,
            subscribe_fills: true,
            subscribe_open_orders: true,
            open_orders_verbose: true,
            max_backoff: Duration::from_secs(30),
            initial_backoff: Duration::from_secs(1),
            ping_interval: Duration::from_secs(30),
        }
    }
}

pub struct KrakenFuturesWsClient {
    config: WsConfig,
    credentials: Option<Credentials>,
    books: BookStore,
    tickers: TickerStore,
    open_orders: OpenOrdersStore,
    fills: FillsStore,
}

impl KrakenFuturesWsClient {
    pub fn new(config: WsConfig, credentials: Option<Credentials>) -> Self {
        let books: BookStore = Arc::new(RwLock::new(HashMap::new()));
        let tickers: TickerStore = Arc::new(RwLock::new(HashMap::new()));
        let open_orders: OpenOrdersStore = Arc::new(RwLock::new(OpenOrdersState::new()));
        let fills: FillsStore = Arc::new(RwLock::new(FillsRing::default()));

        for product in &config.products {
            let mut w = books
                .try_write()
                .expect("fresh BookStore is not contended");
            w.insert(product.clone(), LocalOrderBook::new(product.clone()));
        }

        Self {
            config,
            credentials,
            books,
            tickers,
            open_orders,
            fills,
        }
    }

    pub fn books(&self) -> BookStore {
        self.books.clone()
    }
    pub fn tickers(&self) -> TickerStore {
        self.tickers.clone()
    }
    pub fn open_orders(&self) -> OpenOrdersStore {
        self.open_orders.clone()
    }
    pub fn fills(&self) -> FillsStore {
        self.fills.clone()
    }

    #[instrument(skip_all, fields(url = %self.config.url, authenticated = self.credentials.is_some()))]
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) -> KrakenResult<()> {
        let mut backoff = self.config.initial_backoff;

        loop {
            if *shutdown.borrow() {
                info!("shutdown requested before connect");
                return Ok(());
            }

            match self.run_once(&mut shutdown).await {
                Ok(()) => {
                    info!("websocket loop exited cleanly");
                    return Ok(());
                }
                Err(e) => {
                    self.invalidate_all("connection lost").await;

                    warn!(
                        error = %e,
                        backoff_ms = backoff.as_millis() as u64,
                        "websocket iteration failed; reconnecting after backoff"
                    );

                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = shutdown.changed() => {
                            if *shutdown.borrow() {
                                return Ok(());
                            }
                        }
                    }

                    backoff = (backoff * 2).min(self.config.max_backoff);
                }
            }
        }
    }

    async fn invalidate_all(&self, reason: &str) {
        let mut books = self.books.write().await;
        for book in books.values_mut() {
            book.invalidate(reason);
        }
        drop(books);
        self.open_orders.write().await.invalidate();
        self.fills.write().await.invalidate();
    }

    async fn run_once(&self, shutdown: &mut watch::Receiver<bool>) -> KrakenResult<()> {
        info!(url = %self.config.url, "connecting");
        let (ws_stream, _response) = connect_async(&self.config.url).await?;
        info!("websocket handshake complete");

        let (mut writer, mut reader) = ws_stream.split();

        // ---- 1. Public subscriptions ----------------------------------
        if self.config.subscribe_book {
            let req = WsOutboundEvent::Subscribe(SubscribeRequest {
                feed: "book".to_string(),
                product_ids: self.config.products.clone(),
                api_key: None,
                original_challenge: None,
                signed_challenge: None,
            });
            writer.send(Message::Text(serde_json::to_string(&req)?)).await?;
            info!(products = ?self.config.products, "subscribed to book feed");
        }
        if self.config.subscribe_ticker {
            let req = WsOutboundEvent::Subscribe(SubscribeRequest {
                feed: "ticker".to_string(),
                product_ids: self.config.products.clone(),
                api_key: None,
                original_challenge: None,
                signed_challenge: None,
            });
            writer.send(Message::Text(serde_json::to_string(&req)?)).await?;
            info!(products = ?self.config.products, "subscribed to ticker feed");
        }

        // ---- 2. Private-feed challenge request (if credentialed) ------
        let wants_private = self.credentials.is_some()
            && (self.config.subscribe_fills || self.config.subscribe_open_orders);
        if wants_private {
            let creds = self.credentials.as_ref().unwrap();
            let req = WsOutboundEvent::Challenge {
                api_key: creds.api_key.clone(),
            };
            writer.send(Message::Text(serde_json::to_string(&req)?)).await?;
            info!("challenge request sent");
        }

        // ---- 3. Event loop --------------------------------------------
        let mut ping_ticker = tokio::time::interval(self.config.ping_interval);
        ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut challenge_completed = !wants_private;

        loop {
            tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("shutdown signalled; closing websocket");
                        let _ = writer.close().await;
                        return Ok(());
                    }
                }

                _ = ping_ticker.tick() => {
                    if let Err(e) = writer.send(Message::Ping(Vec::new())).await {
                        return Err(KrakenError::from(e));
                    }
                }

                msg = reader.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            match self.handle_text(&text).await {
                                Ok(Some(HandlerAction::SubscribePrivate(challenge))) => {
                                    if challenge_completed {
                                        debug!("redundant challenge response; ignoring");
                                        continue;
                                    }
                                    if let Err(e) = self.subscribe_private_feeds(&mut writer, &challenge).await {
                                        error!(error = %e, "failed to subscribe to private feeds");
                                        return Err(e);
                                    }
                                    challenge_completed = true;
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    if matches!(e, KrakenError::SequenceGap { .. }) {
                                        error!(error = %e, "sequence gap detected; forcing reconnect");
                                        return Err(e);
                                    }
                                    warn!(error = %e, "non-fatal message handling error");
                                }
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            writer.send(Message::Pong(data)).await?;
                        }
                        Some(Ok(Message::Pong(_))) => {
                            debug!("received pong");
                        }
                        Some(Ok(Message::Close(frame))) => {
                            info!(?frame, "server closed websocket");
                            return Err(KrakenError::Timeout {
                                what: "server closed connection".to_string(),
                            });
                        }
                        Some(Ok(Message::Binary(_))) | Some(Ok(Message::Frame(_))) => {
                            debug!("ignoring unexpected binary/frame message");
                        }
                        Some(Err(e)) => {
                            return Err(KrakenError::from(e));
                        }
                        None => {
                            info!("websocket stream ended");
                            return Err(KrakenError::Timeout {
                                what: "stream ended".to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    async fn subscribe_private_feeds<W>(
        &self,
        writer: &mut W,
        original_challenge: &str,
    ) -> KrakenResult<()>
    where
        W: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        let creds = self
            .credentials
            .as_ref()
            .expect("subscribe_private called without credentials");

        let signed = sign_ws_challenge(&creds.api_secret_b64, original_challenge)?;

        // Private feeds that don't take product_ids use an empty vec.
        let make_req = |feed: &str| WsOutboundEvent::Subscribe(SubscribeRequest {
            feed: feed.to_string(),
            product_ids: Vec::new(),
            api_key: Some(creds.api_key.clone()),
            original_challenge: Some(original_challenge.to_string()),
            signed_challenge: Some(signed.clone()),
        });

        if self.config.subscribe_fills {
            let req = make_req("fills");
            writer
                .send(Message::Text(serde_json::to_string(&req)?))
                .await?;
            info!("subscribed to fills feed");
        }

        if self.config.subscribe_open_orders {
            let feed = if self.config.open_orders_verbose {
                "open_orders_verbose"
            } else {
                "open_orders"
            };
            let req = make_req(feed);
            writer
                .send(Message::Text(serde_json::to_string(&req)?))
                .await?;
            info!(feed, "subscribed to open_orders feed");
        }

        Ok(())
    }

    /// Returns an optional [`HandlerAction`] that the outer loop must act on
    /// (e.g. sending the private subscribe after the challenge arrives).
    async fn handle_text(&self, text: &str) -> KrakenResult<Option<HandlerAction>> {
        let msg: WsInboundMessage = serde_json::from_str(text)?;

        match msg {
            WsInboundMessage::Event(event) => self.handle_event(event),
            WsInboundMessage::Feed(feed) => {
                self.handle_feed(feed).await?;
                Ok(None)
            }
        }
    }

    fn handle_event(&self, event: WsEvent) -> KrakenResult<Option<HandlerAction>> {
        match event {
            WsEvent::Info { version } => {
                info!(?version, "received info event");
            }
            WsEvent::Subscribed { feed, product_ids } => {
                info!(feed, ?product_ids, "subscription confirmed");
            }
            WsEvent::Unsubscribed { feed, product_ids } => {
                info!(feed, ?product_ids, "unsubscription confirmed");
            }
            WsEvent::Challenge { message } => {
                info!(challenge_len = message.len(), "received challenge UUID");
                return Ok(Some(HandlerAction::SubscribePrivate(message)));
            }
            WsEvent::Alert { message } => {
                warn!(%message, "server alert");
            }
            WsEvent::Error { message, .. } => {
                error!(%message, "server error event");
                return Err(KrakenError::Api {
                    code: None,
                    message,
                });
            }
            WsEvent::Pong => {
                debug!("received application-level pong");
            }
        }
        Ok(None)
    }

    async fn handle_feed(&self, feed: WsFeed) -> KrakenResult<()> {
        match feed {
            WsFeed::Heartbeat { .. } => Ok(()),
            WsFeed::BookSnapshot(snap) => self.handle_book_snapshot(snap).await,
            WsFeed::BookDelta(delta) => self.handle_book_delta(delta).await,
            WsFeed::Ticker(ticker) => self.handle_ticker(ticker).await,
            WsFeed::FillsSnapshot(snap) => self.handle_fills_snapshot(snap).await,
            WsFeed::Fills(msg) => self.handle_fills(msg).await,
            WsFeed::OpenOrdersSnapshot(snap) | WsFeed::OpenOrdersVerboseSnapshot(snap) => {
                self.handle_open_orders_snapshot(snap).await
            }
            WsFeed::OpenOrders(update) | WsFeed::OpenOrdersVerbose(update) => {
                self.handle_open_orders_update(update).await
            }
            WsFeed::Other => Ok(()),
        }
    }

    // ---- public feed handlers --------------------------------------

    async fn handle_book_snapshot(&self, snap: BookSnapshotMessage) -> KrakenResult<()> {
        let product = snap.product_id.clone();
        let mut books = self.books.write().await;
        let book = books
            .entry(product.clone())
            .or_insert_with(|| LocalOrderBook::new(product.clone()));
        book.apply_snapshot(&snap)?;
        debug!(%product, seq = snap.seq, "book snapshot applied");
        Ok(())
    }

    async fn handle_book_delta(&self, delta: BookDeltaMessage) -> KrakenResult<()> {
        let product = delta.product_id.clone();
        let mut books = self.books.write().await;
        let book = match books.get_mut(&product) {
            Some(b) => b,
            None => {
                warn!(%product, "delta for unknown product; placeholder inserted");
                books.insert(product.clone(), LocalOrderBook::new(product.clone()));
                return Err(KrakenError::UnexpectedMessage(format!(
                    "delta before snapshot for {}",
                    product
                )));
            }
        };
        match book.apply_delta(&delta) {
            Ok(()) => Ok(()),
            Err(BookError::SequenceGap {
                product,
                expected,
                got,
            }) => Err(KrakenError::SequenceGap {
                product,
                expected,
                got,
            }),
            Err(e) => Err(KrakenError::from(e)),
        }
    }

    async fn handle_ticker(&self, ticker: TickerMessage) -> KrakenResult<()> {
        let product = ticker.product_id.clone();
        let mut tickers = self.tickers.write().await;
        tickers.insert(product, ticker);
        Ok(())
    }

    // ---- private feed handlers -------------------------------------

    async fn handle_fills_snapshot(&self, snap: FillsSnapshotMessage) -> KrakenResult<()> {
        let n = snap.fills.len();
        self.fills.write().await.apply_snapshot(snap.fills);
        info!(account = %snap.account, count = n, "fills snapshot applied");
        Ok(())
    }

    async fn handle_fills(&self, msg: FillsMessage) -> KrakenResult<()> {
        let n = msg.fills.len();
        let result = self.fills.write().await.apply_fills(msg.fills);
        if result.gap_detected {
            warn!(
                account = %msg.account,
                appended = result.appended,
                duplicates = result.duplicates,
                "fills seq gap detected; REST reconciliation required"
            );
        } else {
            debug!(account = %msg.account, count = n, "fills update applied");
        }
        Ok(())
    }

    async fn handle_open_orders_snapshot(
        &self,
        snap: OpenOrdersSnapshotMessage,
    ) -> KrakenResult<()> {
        let n = snap.orders.len();
        self.open_orders.write().await.apply_snapshot(snap.orders);
        info!(account = %snap.account, count = n, "open_orders snapshot applied");
        Ok(())
    }

    async fn handle_open_orders_update(
        &self,
        update: OpenOrdersUpdateMessage,
    ) -> KrakenResult<()> {
        let is_cancel = update.is_cancel;
        let reason = update.reason.clone();
        self.open_orders.write().await.apply_update(update);
        debug!(is_cancel, ?reason, "open_orders update applied");
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum HandlerAction {
    /// The outer loop must sign this challenge and send the private
    /// subscription messages.
    SubscribePrivate(String),
}
