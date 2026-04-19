//! # rt-daemon
//!
//! Wires together:
//!
//! 1. SQLite persistence (`rt-persistence`)
//! 2. Pre-trade checklist (`rt-risk`)
//! 3. Kill-switch supervisor
//! 4. Kraken Futures WebSocket client (`rt-kraken-futures::ws`) for market data
//!    and private feeds
//! 5. Market data service (`market_data`) that bridges WS stores to
//!    `MarketSnapshot` for the risk layer
//! 6. Signal processor that polls `signals` table, evaluates checklist,
//!    and (in a future drop) submits orders via the Broker trait
//!
//! ## Shutdown
//!
//! SIGTERM / SIGINT → the watch channel flips → all tasks drain and exit.
//! systemd sends SIGTERM by default with a 15 s `TimeoutStopSec`.

mod market_data;
mod signal_processor;

use anyhow::{Context, Result};
use rt_kraken_futures::ws::{KrakenFuturesWsClient, WsConfig};
use rt_kraken_futures::Credentials;
use rt_persistence::Database;
use rt_risk::{PreTradeChecklist, RiskConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tracing::{error, info};

use crate::market_data::MarketDataService;
use crate::signal_processor::SignalProcessor;

#[derive(Debug, Clone, serde::Deserialize)]
struct DaemonConfig {
    database_path: String,
    signal_poll_interval_ms: u64,
    kill_switch_check_interval_secs: u64,
    kraken: KrakenConfig,
    #[serde(default)]
    risk: RiskConfig,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct KrakenConfig {
    /// Products to subscribe to on the WS feed, e.g. ["PI_XBTUSD", "PI_ETHUSD"].
    products: Vec<String>,
    /// Use the demo environment. Default true for safety.
    #[serde(default = "default_true")]
    demo: bool,
}

fn default_true() -> bool {
    true
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            database_path: "/var/lib/razortrade/razortrade.sqlite".to_string(),
            signal_poll_interval_ms: 1000,
            kill_switch_check_interval_secs: 60,
            kraken: KrakenConfig {
                products: vec!["PI_XBTUSD".to_string()],
                demo: true,
            },
            risk: RiskConfig::default(),
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,rt_=debug,sqlx=warn"));

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .json()
        .init();
}

fn load_config() -> Result<DaemonConfig> {
    let path = std::env::var("RT_DAEMON_CONFIG")
        .unwrap_or_else(|_| "/etc/razortrade/daemon.toml".to_string());

    let settings = config::Config::builder()
        .add_source(config::File::with_name(&path).required(false))
        .add_source(config::Environment::with_prefix("RF_DAEMON").separator("__"))
        .build()
        .context("building config")?;

    Ok(settings.try_deserialize().unwrap_or_default())
}

async fn wait_for_shutdown_signal(tx: watch::Sender<bool>) {
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => info!("SIGTERM received"),
        _ = sigint.recv() => info!("SIGINT received"),
    }

    let _ = tx.send(true);
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    info!(version = env!("CARGO_PKG_VERSION"), "rt-daemon starting");

    let config = load_config().context("loading daemon configuration")?;
    info!(?config, "configuration loaded");

    // ---- Persistence ---------------------------------------------------
    let db = Arc::new(
        Database::open(&config.database_path)
            .await
            .context("opening database")?,
    );

    // ---- Risk ----------------------------------------------------------
    let risk_config = Arc::new(config.risk.clone());
    let checklist = Arc::new(PreTradeChecklist::standard());
    info!(checks = ?checklist.check_ids(), "pre-trade checklist built");

    // ---- Kraken Futures WS client -------------------------------------
    let ws_config = {
        let mut c = WsConfig::new(config.kraken.products.clone());
        if config.kraken.demo {
            c.url = rt_kraken_futures::KRAKEN_FUTURES_DEMO_WS_URL.to_string();
        }
        c
    };
    let credentials = Credentials::from_env();
    if credentials.is_none() {
        info!("no RT_KRAKEN_API_KEY/SECRET in env; running in public-only mode");
    }
    let ws_client = Arc::new(KrakenFuturesWsClient::new(ws_config, credentials));
    let market_data = Arc::new(MarketDataService::new(
        ws_client.books(),
        ws_client.tickers(),
    ));

    // ---- Signal processor ---------------------------------------------
    let processor = Arc::new(SignalProcessor::new(
        db.clone(),
        checklist.clone(),
        risk_config.clone(),
        market_data.clone(),
    ));

    // ---- Shutdown plumbing --------------------------------------------
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // ---- Spawn tasks ---------------------------------------------------
    let ws_handle = {
        let client = ws_client.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = client.run(rx).await {
                error!(error = %e, "kraken ws client exited with error");
            }
        })
    };

    let poller_handle = {
        let processor = processor.clone();
        let rx = shutdown_rx.clone();
        let interval = Duration::from_millis(config.signal_poll_interval_ms);
        tokio::spawn(async move { processor.run(interval, rx).await })
    };

    let supervisor_handle = {
        let db = db.clone();
        let rx = shutdown_rx.clone();
        let interval = Duration::from_secs(config.kill_switch_check_interval_secs);
        tokio::spawn(async move { run_kill_switch_supervisor(db, interval, rx).await })
    };

    // ---- Wait for shutdown --------------------------------------------
    wait_for_shutdown_signal(shutdown_tx).await;
    info!("shutting down…");

    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let _ = ws_handle.await;
        let _ = poller_handle.await;
        let _ = supervisor_handle.await;
    })
    .await;

    info!("rt-daemon stopped");
    Ok(())
}

async fn run_kill_switch_supervisor(
    db: Arc<Database>,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    info!(interval_secs = interval.as_secs(), "kill-switch supervisor started");

    let mut ticker = tokio::time::interval(interval);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match db.kill_switch_active().await {
                    Ok(active) => tracing::debug!(active, "kill-switch state check"),
                    Err(e) => tracing::warn!(error = %e, "kill-switch DB check failed"),
                }
                // TODO(next-drop): compute PortfolioState from equity_snapshots +
                // positions, evaluate KillSwitchEvaluator, and persist on trigger.
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("kill-switch supervisor received shutdown signal");
                    return;
                }
            }
        }
    }
}
