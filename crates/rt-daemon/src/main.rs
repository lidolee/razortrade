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

mod equity_writer;
mod fill_reconciler;
mod market_data;
mod portfolio_loader;
mod signal_processor;

use anyhow::{Context, Result};
use rt_core::execution_mode::ExecutionMode;
use rt_execution::Broker;
use rt_kraken_futures::rest::KrakenFuturesRestClient;
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
    #[serde(default)]
    execution: ExecutionConfig,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct KrakenConfig {
    /// Products to subscribe to on the WS feed, e.g. ["PI_XBTUSD", "PI_ETHUSD"].
    products: Vec<String>,
    /// Use the demo environment. Default true for safety.
    #[serde(default = "default_true")]
    demo: bool,
}

/// Execution-side configuration. Intentionally separate from `KrakenConfig`
/// because it applies across all brokers and controls the single most
/// critical safety switch in the system.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ExecutionConfig {
    /// The execution mode. Defaults to `dry_run` — forgetting to set this
    /// must never result in real orders.
    #[serde(default)]
    mode: rt_core::ExecutionMode,
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
            execution: ExecutionConfig::default(),
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
    // Distinguish between "user explicitly pointed us at a config file"
    // and "no env var set, fall back to the system default path".
    // In the first case, missing/unparseable config is a hard error.
    // In the second, missing config is expected (e.g. first boot before
    // the operator has installed one) and we use built-in defaults.
    let (path, explicit) = match std::env::var("RT_DAEMON_CONFIG") {
        Ok(p) => (p, true),
        Err(_) => ("/etc/razortrade/daemon.toml".to_string(), false),
    };

    // Use `File::new(path, FileFormat::Toml)` rather than `File::with_name(path)`.
    // `with_name` does stem-based lookup (strips the extension and tries
    // known formats), which silently fails on filenames like
    // `daemon.toml.local` because `.local` is not a recognised format.
    // `File::new` takes the path literally and the format explicitly.
    let file_source = config::File::new(&path, config::FileFormat::Toml).required(explicit);

    let settings = config::Config::builder()
        .add_source(file_source)
        .add_source(config::Environment::with_prefix("RT_DAEMON").separator("__"))
        .build()
        .with_context(|| format!("building config (path={path}, explicit={explicit})"))?;

    // Don't mask deserialisation errors with silent defaults. If the config
    // file IS there but malformed, we want a hard failure with a clear
    // reason, not a quiet fallback to the production defaults.
    let cfg: DaemonConfig = if explicit {
        settings
            .try_deserialize()
            .with_context(|| format!("parsing config at {path}"))?
    } else {
        settings.try_deserialize().unwrap_or_default()
    };

    info!(
        config_path = %path,
        explicit,
        "loaded configuration"
    );
    Ok(cfg)
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
    let execution_mode = config.execution.mode;
    info!(
        mode = ?execution_mode,
        description = execution_mode.description(),
        "execution mode configured"
    );

    // Broker for CryptoLeverage sleeve (Kraken Futures).
    //
    // DryRun: no broker. Paper: demo endpoint. Live: production endpoint.
    // For Paper/Live we require API credentials and we do a cheap
    // health_check() at startup. If either fails we refuse to start,
    // because silent fallback to a different endpoint would be a
    // correctness nightmare (demo funds != production funds).
    //
    // We keep TWO handles to the same client:
    //   - `crypto_leverage_broker: Arc<dyn Broker>` for the trait-using
    //     call sites (signal processor, potentially other brokers).
    //   - `kraken_concrete: Arc<KrakenFuturesRestClient>` for call sites
    //     that need broker-specific endpoints like /accounts — the
    //     equity writer today (Drop 12). Both Arcs point at the same
    //     underlying client and share its reqwest connection pool.
    let (crypto_leverage_broker, kraken_concrete): (
        Option<Arc<dyn Broker>>,
        Option<Arc<KrakenFuturesRestClient>>,
    ) = match execution_mode {
        ExecutionMode::DryRun => (None, None),
        ExecutionMode::Paper | ExecutionMode::Live => {
            let creds = Credentials::from_env().ok_or_else(|| {
                anyhow::anyhow!(
                    "execution mode is {:?} but RT_KRAKEN_API_KEY/RT_KRAKEN_API_SECRET \
                     are not set in the environment; refusing to start",
                    execution_mode
                )
            })?;
            let concrete: Arc<KrakenFuturesRestClient> =
                if matches!(execution_mode, ExecutionMode::Paper) {
                    Arc::new(KrakenFuturesRestClient::demo(Some(creds)))
                } else {
                    Arc::new(KrakenFuturesRestClient::production(Some(creds)))
                };
            info!(
                mode = ?execution_mode,
                "broker client constructed, running health check…"
            );
            concrete
                .health_check()
                .await
                .context("kraken futures broker health check failed at startup")?;
            info!("broker health check passed");
            let trait_obj: Arc<dyn Broker> = concrete.clone();
            (Some(trait_obj), Some(concrete))
        }
    };

    let processor = Arc::new(SignalProcessor::new(
        db.clone(),
        checklist.clone(),
        risk_config.clone(),
        market_data.clone(),
        execution_mode,
        crypto_leverage_broker.clone(),
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
        let cfg = risk_config.clone();
        let rx = shutdown_rx.clone();
        let interval = Duration::from_secs(config.kill_switch_check_interval_secs);
        // LF-1: supervisor needs broker access to execute panic-close.
        // In DryRun mode no broker is present; panic-close becomes a
        // log-only warning — we still record the event for audit.
        let broker = crypto_leverage_broker.clone();
        tokio::spawn(async move {
            run_kill_switch_supervisor(db, cfg, broker, interval, rx).await
        })
    };

    let fill_reconciler_handle = {
        let db = db.clone();
        let fills = ws_client.fills();
        let rx = shutdown_rx.clone();
        // 1 Hz is generous: individual fills on a human-scale swing
        // strategy are rare, and the cost of an empty tick is a single
        // read-lock acquisition on FillsStore.
        let tick = Duration::from_millis(1000);
        // Drop 19 Part B — G2: reconciler bekommt risk_config + broker
        // damit er nach jedem Leverage-Fill sofort evaluate_kill_switch_once
        // aufrufen kann (Flash-Crash-Schutz, sonst bis zu 60s Verzug).
        let cfg = risk_config.clone();
        let broker_for_reconciler: Option<Arc<dyn Broker>> =
            kraken_concrete.clone().map(|c| c as Arc<dyn Broker>);
        tokio::spawn(async move {
            fill_reconciler::run(db, fills, tick, rx, cfg, broker_for_reconciler).await
        })
    };

    // Drop 12: periodic equity snapshot writer. Only runs when we
    // have a real broker client (Paper / Live). In DryRun there is
    // nothing to snapshot against.
    let equity_writer_handle = kraken_concrete.clone().map(|client| {
        let db = db.clone();
        let tickers = ws_client.tickers();
        let risk = risk_config.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(async move { equity_writer::run(db, client, tickers, risk, rx).await })
    });

    // ---- Wait for shutdown --------------------------------------------
    wait_for_shutdown_signal(shutdown_tx).await;
    info!("shutting down…");

    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let _ = ws_handle.await;
        let _ = poller_handle.await;
        let _ = supervisor_handle.await;
        let _ = fill_reconciler_handle.await;
        if let Some(h) = equity_writer_handle {
            let _ = h.await;
        }
    })
    .await;

    info!("rt-daemon stopped");
    Ok(())
}

async fn run_kill_switch_supervisor(
    db: Arc<Database>,
    config: Arc<RiskConfig>,
    broker: Option<Arc<dyn Broker>>,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    info!(
        interval_secs = interval.as_secs(),
        broker_available = broker.is_some(),
        "kill-switch supervisor started (evaluator active)"
    );

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(e) = evaluate_kill_switch_once(&db, &config, broker.as_ref()).await {
                    error!(error = %e, "kill-switch evaluation iteration failed");
                }
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

/// One supervisor iteration: load state, evaluate, persist on trigger.
///
/// Kept as a free function so it is trivially unit-testable with an
/// in-memory SQLite database if we want to later.
/// Drop 19 Part B — G2/G3: Kill-Switch-Auswertung, die bei Bedarf
/// sofort Panic-Close ausführt. Wird sowohl vom 60s-Supervisor als
/// auch vom Fill-Reconciler (nach jedem applied fill im Leverage-
/// Sleeve) aufgerufen, damit Flash-Crash-Losses nicht bis zu 60s
/// warten müssen.
pub(crate) async fn evaluate_kill_switch_once(
    db: &Arc<Database>,
    config: &Arc<RiskConfig>,
    broker: Option<&Arc<dyn Broker>>,
) -> anyhow::Result<()> {
    use rt_risk::{KillSwitchDecision, KillSwitchEvaluator, KillSwitchReason};

    let now = chrono::Utc::now();
    let portfolio = crate::portfolio_loader::load_portfolio_state(db, now).await?;

    // LF-1: the evaluator must be called even when soft-disabled so
    // that a runaway mark-to-market loss on still-open positions can
    // escalate to a hard panic-close. However we do not re-fire
    // panic-close if one is already unresolved.
    let hard_already = db.has_active_hard_trigger().await?;

    let decision = KillSwitchEvaluator::evaluate(&portfolio, config.as_ref(), now);

    match decision {
        KillSwitchDecision::NoChange => {
            tracing::debug!(
                nav = %portfolio.nav_per_unit,
                drawdown = %portfolio.drawdown_fraction(),
                realised_leverage = %portfolio.leverage_sleeve_realized_pnl(),
                effective_leverage = %portfolio.leverage_sleeve_effective_pnl(),
                "kill-switch state nominal"
            );
        }
        KillSwitchDecision::Disable { reason, at } => {
            // Stable reason_kind for SQL filtering, independent of the
            // full JSON payload which carries the numeric detail.
            let reason_kind = match &reason {
                KillSwitchReason::LifetimeRealisedLossBudgetExhausted { .. } => {
                    "lifetime_realised_loss_budget"
                }
                KillSwitchReason::EffectivePnlBudgetExhausted { .. } => {
                    // Not reachable via Disable; the evaluator routes
                    // effective-breach through PanicClose. Logged for
                    // completeness in case of future refactor.
                    "effective_pnl_budget"
                }
                KillSwitchReason::PortfolioDrawdown { .. } => "portfolio_drawdown",
                KillSwitchReason::ManualTrigger { .. } => "manual_trigger",
            };

            error!(
                reason_kind,
                summary = %reason.summary(),
                "KILL-SWITCH TRIGGERED (soft) — leveraged trading will be refused \
                 by pre-trade checklist until manually resolved"
            );

            let reason_json = serde_json::to_string(&reason)?;
            let snapshot_json = serde_json::to_string(&portfolio)?;

            let event_id = db
                .record_kill_switch(
                    &rt_core::time::canonical_iso(at),
                    reason_kind,
                    &reason_json,
                    &snapshot_json,
                )
                .await?;

            info!(event_id, "kill-switch event persisted");
        }
        KillSwitchDecision::PanicClose { reason, at } => {
            if hard_already {
                tracing::debug!(
                    summary = %reason.summary(),
                    "hard kill-switch already active; skipping duplicate panic-close"
                );
                return Ok(());
            }

            // Use `hard_` prefix on reason_kind so has_active_hard_trigger
            // can filter cheaply via SQL LIKE.
            let reason_kind = "hard_effective_pnl_budget";

            error!(
                reason_kind,
                summary = %reason.summary(),
                "KILL-SWITCH TRIGGERED (HARD) — initiating panic-close of all open \
                 leverage positions"
            );

            // Record the event FIRST. If the broker calls fail below
            // we still have an audit trail and has_active_hard_trigger
            // will block re-entry on the next tick.
            let reason_json = serde_json::to_string(&reason)?;
            let snapshot_json = serde_json::to_string(&portfolio)?;
            let event_id = db
                .record_kill_switch(
                    &rt_core::time::canonical_iso(at),
                    reason_kind,
                    &reason_json,
                    &snapshot_json,
                )
                .await?;

            info!(event_id, "hard kill-switch event persisted");

            // Execute the panic-close routine. In DryRun there is no
            // broker — we stop here with a loud warning.
            let broker = match broker {
                Some(b) => b,
                None => {
                    error!(
                        event_id,
                        "panic-close requested but no broker is configured (DryRun mode); \
                         hard kill-switch is recorded but no orders will be sent"
                    );
                    return Ok(());
                }
            };

            if let Err(e) = panic_close_leverage_positions(db, broker, event_id).await {
                error!(
                    event_id,
                    error = %e,
                    "panic-close routine failed; hard kill-switch remains active"
                );
            }
        }
    }

    Ok(())
}

/// LF-1 panic-close routine. Iterates every open leverage position and
/// submits a market, reduce-only order on the opposite side to flatten
/// it. We submit all orders in parallel (best-effort) rather than
/// sequentially: if one call fails, the others still have a chance
/// to close their position. Each submission is logged for audit.
///
/// No retry loop here — if an order fails, it is logged and the next
/// supervisor tick will NOT retry (has_active_hard_trigger gates the
/// whole path). The operator is expected to intervene.
async fn panic_close_leverage_positions(
    db: &Arc<Database>,
    broker: &Arc<dyn Broker>,
    event_id: i64,
) -> anyhow::Result<()> {
    use rt_core::instrument::{AssetClass, Broker as BrokerKind, Instrument};
    use rt_core::order::{Order, OrderStatus, OrderType, Side, TimeInForce};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let positions = db.list_open_leverage_positions().await?;
    info!(
        event_id,
        count = positions.len(),
        "panic-close: enumerating open leverage positions"
    );

    for (symbol, broker_name, sleeve, qty_str) in positions {
        // Quantity is stored as a TEXT Decimal. Positive = long, negative
        // = short. The close side is the opposite sign.
        let qty = match Decimal::from_str(&qty_str) {
            Ok(q) => q,
            Err(e) => {
                error!(
                    %symbol,
                    qty_str = %qty_str,
                    error = %e,
                    "panic-close: could not parse position quantity; skipping"
                );
                continue;
            }
        };

        let (side, size) = if qty.is_sign_positive() {
            (Side::Sell, qty)
        } else {
            (Side::Buy, qty.abs())
        };

        let now = chrono::Utc::now();
        let order = Order {
            id: 0,
            signal_id: None,
            broker: BrokerKind::KrakenFutures,
            broker_order_id: None,
            cli_ord_id: Some(format!("rt-panic-{}-{}", event_id, now.timestamp())),
            instrument: Instrument {
                symbol: symbol.clone(),
                broker: BrokerKind::KrakenFutures,
                asset_class: AssetClass::CryptoPerp,
                min_order_size: Decimal::ONE,
                tick_size: Decimal::new(5, 1),
            },
            side,
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Ioc,
            quantity: size,
            limit_price: None,
            stop_price: None,
            status: OrderStatus::PendingSubmission,
            filled_quantity: Decimal::ZERO,
            avg_fill_price: None,
            fees_paid: Decimal::ZERO,
            created_at: now,
            updated_at: now,
            error_message: None,
            // Drop 19 Part B — G3: reduce_only=true verhindert, dass ein
            // Panic-Close-Market-Order das Konto versehentlich in die
            // Gegenrichtung (Short) positioniert. Szenarien die ohne
            // dieses Flag kaputt gehen:
            //   * Doppel-Trigger (Supervisor + Event-Trigger feuern
            //     beide in derselben Sekunde vor Status-Update)
            //   * Ein Fill ist schon teilweise durch, Position bereits
            //     kleiner als berechnete `size`
            // Kraken lehnt reduceOnly-Orders ab, wenn sie über die
            // Position hinausgehen würden — exakt was wir wollen.
            reduce_only: Some(true),
        };

        error!(
            event_id,
            %symbol,
            %broker_name,
            %sleeve,
            side = ?side,
            size = %size,
            "panic-close: submitting market reduce order"
        );

        match broker.submit_order(&order).await {
            Ok(res) => {
                info!(
                    event_id,
                    %symbol,
                    broker_order_id = %res.broker_order_id,
                    status = ?res.status,
                    "panic-close: order submitted"
                );
            }
            Err(e) => {
                error!(
                    event_id,
                    %symbol,
                    error = %e,
                    "panic-close: order submission failed"
                );
            }
        }
    }

    Ok(())
}
