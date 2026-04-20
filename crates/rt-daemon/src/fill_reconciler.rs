//! Drains new fill events from the Kraken WebSocket `FillsStore` and
//! persists them to the local `orders` table via
//! `Database::apply_fill_to_order`.
//!
//! Scope (Drop 6a): orders-level reconciliation only — updates
//! `filled_quantity`, `avg_fill_price`, `fees_paid`, and `status`.
//!
//! Position-table updates (opening, updating, closing positions,
//! computing realised P&L on opposite-side fills) are deliberately
//! deferred to Drop 6b. They require symmetric entry/exit logic and
//! a definition of sleeve attribution that is cleaner to reason about
//! once orders-level reconciliation has run stably in paper for a
//! while.
//!
//! Idempotency model:
//! - `FillsRing` already deduplicates by `seq` inside the WS layer.
//! - This task keeps an in-memory `last_applied_seq` watermark so the
//!   same fill is never submitted to the database twice within a
//!   single daemon run.
//! - Across daemon restarts, the WS snapshot on reconnect replays the
//!   full visible history of fills. A restart-resilient watermark
//!   (stored in the database) is out of scope for Drop 6a; in
//!   practice the `apply_fill_to_order` query matches on broker
//!   `order_id`, so a replayed fill on an order that has already been
//!   fully updated will produce a second, additive update. This will
//!   be addressed in Drop 6b by tracking applied `fill_id`s in a new
//!   table.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rt_execution::Broker;
use rt_kraken_futures::private_state::FillsStore;
use rt_persistence::Database;
use rt_risk::RiskConfig;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

/// Run the fill reconciler until shutdown.
///
/// Polls the shared `FillsStore` at the given interval, picks up any
/// fills with `seq > last_applied_seq`, and applies them to the
/// `orders` table.
///
/// Drop 19 Part B — G2: after each successful fill application on the
/// `crypto_leverage` sleeve, the reconciler triggers an immediate
/// kill-switch evaluation via `crate::evaluate_kill_switch_once`. That
/// closes the Flash-Crash-Gap between 60s supervisor ticks.
pub async fn run(
    db: Arc<Database>,
    fills: FillsStore,
    tick_interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
    risk_config: Arc<RiskConfig>,
    broker: Option<Arc<dyn Broker>>,
) {
    info!(
        interval_ms = tick_interval.as_millis() as u64,
        "fill reconciler started"
    );

    let mut last_applied_seq: u64 = 0;
    let mut ticker = interval(tick_interval);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(e) = reconcile_once(
                    &db,
                    &fills,
                    &mut last_applied_seq,
                    &risk_config,
                    broker.as_ref(),
                ).await {
                    error!(error = %e, "fill reconciler tick failed");
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("fill reconciler shutting down");
                    break;
                }
            }
        }
    }
}

async fn reconcile_once(
    db: &Arc<Database>,
    fills: &FillsStore,
    last_applied_seq: &mut u64,
    risk_config: &Arc<RiskConfig>,
    broker: Option<&Arc<dyn Broker>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Pull new fills out under the read lock, then release before DB I/O.
    // The FillsRing clones cheaply; copying a handful of Decimal+String
    // structs per tick is a non-issue vs the cost of holding a read lock
    // across SQLite writes.
    let snapshot: Vec<_> = {
        let guard = fills.read().await;
        if !guard.is_synced() {
            // Pre-snapshot phase; nothing to reconcile yet.
            return Ok(());
        }
        if guard.highest_seq() <= *last_applied_seq {
            // No new fills since last tick.
            return Ok(());
        }
        guard
            .iter()
            .filter(|f| f.seq > *last_applied_seq)
            .cloned()
            .collect()
    };

    debug!(
        new_fills = snapshot.len(),
        high_watermark = *last_applied_seq,
        "fill reconciler picked up new fills"
    );

    // Apply in seq order; stop at the first error so we can retry
    // next tick without skipping a fill.
    for fill in snapshot {
        let now_iso = Utc::now().to_rfc3339();

        // Drop 6c: restart-resilient dedup. The in-memory
        // `last_applied_seq` watermark only protects within a single
        // daemon run; on reconnect the broker replays the full
        // visible fill history. Check the database before we do any
        // order- or position-level work.
        match db.has_fill_been_applied(BROKER_TAG, &fill.fill_id).await {
            Ok(true) => {
                // Already processed in a previous run; advance the
                // in-memory watermark and move on silently.
                *last_applied_seq = (*last_applied_seq).max(fill.seq);
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                error!(
                    broker_order_id = %fill.order_id,
                    fill_id = %fill.fill_id,
                    error = %e,
                    "could not check applied_fills table; will retry next tick"
                );
                return Err(Box::new(e));
            }
        }

        match db
            .apply_fill_to_order(
                &fill.order_id,
                fill.qty,
                fill.price,
                fill.fee_paid,
                &now_iso,
            )
            .await
        {
            Ok(Some(outcome)) => {
                info!(
                    order_id = outcome.order_id,
                    broker_order_id = %fill.order_id,
                    fill_id = %fill.fill_id,
                    fill_qty = %fill.qty,
                    fill_price = %fill.price,
                    fee = %fill.fee_paid,
                    fee_currency = %fill.fee_currency,
                    fill_type = %fill.fill_type,
                    new_filled_quantity = %outcome.new_filled_quantity,
                    new_avg_fill_price = %outcome.new_avg_fill_price,
                    new_fees_paid = %outcome.new_fees_paid,
                    new_status = %outcome.new_status,
                    fully_filled = outcome.is_fully_filled,
                    "fill applied to order"
                );

                // Drop 6b: also reconcile into the positions table.
                // Skip only if the order had no originating signal
                // (so we have no sleeve attribution) — we warn once
                // rather than silently dropping.
                let sleeve = match &outcome.sleeve {
                    Some(s) => s.clone(),
                    None => {
                        warn!(
                            order_id = outcome.order_id,
                            broker_order_id = %fill.order_id,
                            "fill applied to order but sleeve is unknown; position not updated"
                        );
                        // Still mark the fill as applied — there is
                        // no future state in which the sleeve becomes
                        // known retroactively, so retrying would not
                        // help.
                        if let Err(e) =
                            db.record_fill_applied(BROKER_TAG, &fill.fill_id, &now_iso).await
                        {
                            error!(error = %e, "record_fill_applied failed");
                            return Err(Box::new(e));
                        }
                        *last_applied_seq = (*last_applied_seq).max(fill.seq);
                        continue;
                    }
                };

                match db
                    .apply_fill_to_position(
                        &outcome.instrument_symbol,
                        &outcome.broker,
                        &sleeve,
                        outcome.is_buy,
                        fill.qty,
                        fill.price,
                        &now_iso,
                    )
                    .await
                {
                    Ok(pos_outcome) => {
                        info!(
                            position_id = pos_outcome.position_id,
                            instrument = %outcome.instrument_symbol,
                            sleeve = %sleeve,
                            new_quantity = %pos_outcome.new_quantity,
                            new_avg_entry_price = ?pos_outcome.new_avg_entry_price,
                            realized_pnl_btc = %pos_outcome.realized_pnl_btc,
                            transition = ?pos_outcome.transition,
                            "fill applied to position"
                        );

                        // Drop 19 Part B — G2: sofortiger Kill-Switch-
                        // Event-Trigger. Nur für Leverage-Sleeve; Spot
                        // und ETF haben keine hard-stop-Semantik.
                        if sleeve == "crypto_leverage" {
                            if let Err(e) = crate::evaluate_kill_switch_once(
                                db, risk_config, broker,
                            ).await {
                                error!(
                                    error = %e,
                                    "event-triggered kill-switch evaluation failed; \
                                     supervisor will retry on next 60s tick"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            broker_order_id = %fill.order_id,
                            fill_id = %fill.fill_id,
                            error = %e,
                            "order was updated but position update failed; will retry next tick"
                        );
                        return Err(Box::new(e));
                    }
                }
            }
            Ok(None) => {
                // CV-1 orphan-catcher: the order row matching
                // `broker_order_id` is unknown. Before we give up,
                // check whether the fill carries a `cli_ord_id` we
                // generated ourselves — this happens when our REST
                // submit landed at Kraken but the HTTP response (with
                // the broker_order_id) was lost to a network error.
                // In that case we backfill the broker_order_id onto
                // our pending order and re-apply the fill.
                let backfilled = if let Some(cli) = fill.cli_ord_id.as_deref() {
                    match db
                        .find_order_id_by_cli_ord_id(BROKER_TAG, cli)
                        .await
                    {
                        Ok(Some(order_id)) => {
                            if let Err(e) = db
                                .backfill_broker_order_id(
                                    order_id,
                                    &fill.order_id,
                                    &now_iso,
                                )
                                .await
                            {
                                error!(
                                    order_id,
                                    cli_ord_id = %cli,
                                    broker_order_id = %fill.order_id,
                                    error = %e,
                                    "orphan-catcher: backfill failed"
                                );
                                false
                            } else {
                                info!(
                                    order_id,
                                    cli_ord_id = %cli,
                                    broker_order_id = %fill.order_id,
                                    fill_id = %fill.fill_id,
                                    "orphan-catcher: matched fill to pending order via cli_ord_id; broker_order_id backfilled"
                                );
                                true
                            }
                        }
                        Ok(None) => false,
                        Err(e) => {
                            error!(
                                cli_ord_id = %cli,
                                error = %e,
                                "orphan-catcher: cli_ord_id lookup failed"
                            );
                            false
                        }
                    }
                } else {
                    false
                };

                if backfilled {
                    // Retry the apply now that the order row exists
                    // with our broker_order_id set.
                    match db
                        .apply_fill_to_order(
                            &fill.order_id,
                            fill.qty,
                            fill.price,
                            fill.fee_paid,
                            &now_iso,
                        )
                        .await
                    {
                        Ok(Some(outcome)) => {
                            info!(
                                order_id = outcome.order_id,
                                broker_order_id = %fill.order_id,
                                fill_id = %fill.fill_id,
                                fill_qty = %fill.qty,
                                fill_price = %fill.price,
                                new_filled_quantity = %outcome.new_filled_quantity,
                                new_status = %outcome.new_status,
                                fully_filled = outcome.is_fully_filled,
                                "fill applied to order (via orphan-catcher)"
                            );
                            // Position update path — kept in sync with
                            // the happy-path branch above.
                            if let Some(sleeve) = &outcome.sleeve {
                                if let Err(e) = db
                                    .apply_fill_to_position(
                                        &outcome.instrument_symbol,
                                        &outcome.broker,
                                        sleeve,
                                        outcome.is_buy,
                                        fill.qty,
                                        fill.price,
                                        &now_iso,
                                    )
                                    .await
                                {
                                    error!(
                                        broker_order_id = %fill.order_id,
                                        fill_id = %fill.fill_id,
                                        error = %e,
                                        "orphan-catcher: position update failed after apply"
                                    );
                                    return Err(Box::new(e));
                                }
                                // Drop 19 Part B — G2: Event-Trigger
                                // auch im Orphan-Pfad.
                                if sleeve == "crypto_leverage" {
                                    if let Err(e) = crate::evaluate_kill_switch_once(
                                        db, risk_config, broker,
                                    ).await {
                                        error!(
                                            error = %e,
                                            "orphan-catcher: event-triggered kill-switch eval failed"
                                        );
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            warn!(
                                broker_order_id = %fill.order_id,
                                fill_id = %fill.fill_id,
                                "orphan-catcher: second lookup still None; abandoning"
                            );
                        }
                        Err(e) => {
                            error!(
                                broker_order_id = %fill.order_id,
                                fill_id = %fill.fill_id,
                                error = %e,
                                "orphan-catcher: apply_fill_to_order failed"
                            );
                            return Err(Box::new(e));
                        }
                    }
                } else {
                    warn!(
                        broker_order_id = %fill.order_id,
                        fill_id = %fill.fill_id,
                        cli_ord_id = ?fill.cli_ord_id,
                        "fill received for unknown order (externally submitted?)"
                    );
                }
            }
            Err(e) => {
                error!(
                    broker_order_id = %fill.order_id,
                    fill_id = %fill.fill_id,
                    error = %e,
                    "failed to apply fill to order; will retry next tick"
                );
                // Leave last_applied_seq unchanged so we re-try this and
                // everything after it on the next tick.
                return Err(Box::new(e));
            }
        }

        // Record success (or stable-terminal "unknown order"). After
        // this the fill will never be re-processed from the WS
        // snapshot replay.
        if let Err(e) = db.record_fill_applied(BROKER_TAG, &fill.fill_id, &now_iso).await {
            error!(
                fill_id = %fill.fill_id,
                error = %e,
                "record_fill_applied failed after successful apply; next tick will retry"
            );
            return Err(Box::new(e));
        }

        *last_applied_seq = (*last_applied_seq).max(fill.seq);
    }

    Ok(())
}

/// The broker tag used for applied_fills rows. Hard-coded for now
/// because this reconciler only handles Kraken Futures feeds; a
/// multi-broker daemon would pipe this through from the feed's own
/// identity.
const BROKER_TAG: &str = "kraken_futures";
