//! Drop 19 CV-6: periodische Reconciliation gegen Kraken REST.
//!
//! # Was macht das hier
//!
//! Alle `interval` Sekunden ruft der Reconciler `broker.open_orders()`
//! auf und vergleicht das Ergebnis mit unserem lokalen `orders`-State.
//! Dadurch werden Drift-Zustände aufgedeckt, die der WebSocket-Feed
//! nicht sichtbar gemacht hat — z.B. Orders, die an der Börse
//! gecancelt wurden aber bei uns noch als `acknowledged` geführt
//! werden, oder umgekehrt Orders die Kraken noch hat, die bei uns
//! aber schon als `failed` markiert wurden.
//!
//! # Warum ein separater Task statt WS-only
//!
//! Die WebSocket-Verbindung zu Kraken Demo fällt relativ häufig
//! (siehe `ws_reconnects` Dashboard-Metric). Zwischen zwei Reconnects
//! können Events verloren gehen, die der Snapshot-Replay beim
//! Reconnect zwar theoretisch abdeckt, aber nur wenn das Event noch
//! in der 24h-History liegt. Ein eigenständiger 60-Sekunden-
//! Reconciler liefert ein unabhängiges Safety-Net, das nicht von der
//! WS-Gesundheit abhängt.
//!
//! # Drift-Klassen
//!
//! 1. **LocalHasBrokerDoesNot**: lokale Order ist `submitted` oder
//!    `acknowledged`, aber Kraken hat sie nicht mehr → entweder
//!    gefüllt (aber Fill-Event verloren) oder gecancelt. Fill-Fall
//!    deckt der `fill_reconciler` ab, falls ein Fill nachkommt.
//!    Diesem Reconciler reicht es, einen Warn-Log zu schreiben —
//!    harte Aktion wäre Statusverschiebung auf `cancelled`, das
//!    machen wir aber nicht automatisch, weil ein verzögerter Fill
//!    sonst verschluckt würde.
//!
//! 2. **BrokerHasLocalDoesNot**: Kraken führt eine Order mit unserem
//!    `cli_ord_id`, lokale Order-Row ist aber weg (oder broker_order_id
//!    unpopuliert). Wird über `backfill_broker_order_id` im
//!    happy-path des fill_reconciler abgedeckt; der Periodic-
//!    Reconciler loggt nur.
//!
//! 3. **QuantityDrift**: filled_quantity unterscheidet sich → log,
//!    fill_reconciler holt den Rest nach.
//!
//! # Kein Auto-Cancel
//!
//! Dieser Reconciler löst KEINE Order-Cancels oder State-Transitions
//! aus. Seine Aufgabe in Drop 19 ist rein **observational**: Drift
//! sichtbar machen, Metriken aufzeichnen. Auto-Actions kommen in
//! Drop 20 nach einer Woche stabiler Beobachtung.

use rt_execution::Broker;
use rt_persistence::Database;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

pub async fn run(
    db: Arc<Database>,
    broker: Arc<dyn Broker>,
    tick_interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    info!(
        interval_s = tick_interval.as_secs(),
        "periodic reconciler started (CV-6)"
    );
    let mut ticker = interval(tick_interval);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(e) = reconcile_once(&db, broker.as_ref()).await {
                    error!(error = %e, "periodic reconciler tick failed");
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("periodic reconciler shutting down");
                    break;
                }
            }
        }
    }
}

async fn reconcile_once(
    db: &Arc<Database>,
    broker: &dyn Broker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let broker_orders = broker.open_orders().await?;
    let local_live = db.list_live_orders().await?;

    debug!(
        broker_count = broker_orders.len(),
        local_live_count = local_live.len(),
        "periodic reconciler tick"
    );

    // Index beider Seiten auf broker_order_id / cli_ord_id.
    use std::collections::HashSet;
    let broker_bids: HashSet<String> = broker_orders
        .iter()
        .map(|o| o.broker_order_id.clone())
        .collect();
    let broker_clis: HashSet<String> = broker_orders
        .iter()
        .filter_map(|o| o.cli_ord_id.clone())
        .collect();

    // Drift-Klasse 1: lokale live-Orders die Kraken nicht (mehr) kennt.
    for local in &local_live {
        let bid_match = local
            .broker_order_id
            .as_ref()
            .map(|b| broker_bids.contains(b))
            .unwrap_or(false);
        let cli_match = local
            .cli_ord_id
            .as_ref()
            .map(|c| broker_clis.contains(c))
            .unwrap_or(false);
        if !bid_match && !cli_match {
            warn!(
                order_id = local.id,
                signal_id = local.signal_id,
                status = %local.status,
                broker_order_id = ?local.broker_order_id,
                cli_ord_id = ?local.cli_ord_id,
                "CV-6 drift: local order is live but Kraken has no matching open order"
            );
        }
    }

    // Drift-Klasse 2: Kraken-Orders die wir lokal nicht kennen.
    let local_bids: HashSet<String> = local_live
        .iter()
        .filter_map(|o| o.broker_order_id.clone())
        .collect();
    let local_clis: HashSet<String> = local_live
        .iter()
        .filter_map(|o| o.cli_ord_id.clone())
        .collect();
    for b in &broker_orders {
        let has_bid = local_bids.contains(&b.broker_order_id);
        let has_cli = b
            .cli_ord_id
            .as_ref()
            .map(|c| local_clis.contains(c))
            .unwrap_or(false);
        if !has_bid && !has_cli {
            warn!(
                broker_order_id = %b.broker_order_id,
                cli_ord_id = ?b.cli_ord_id,
                symbol = %b.instrument_symbol,
                qty = %b.quantity,
                "CV-6 drift: Kraken has an open order we don't know about locally"
            );
        }
    }

    Ok(())
}
