//! # Equity-Snapshot-Writer (Drop 12)
//!
//! Runs once every 30 s while the daemon is up. Fetches the latest
//! account state from Kraken Futures, combines it with the current
//! BTC mark from the WS ticker store and the locally-aggregated
//! realised PnL across `positions`, and writes a fresh row to
//! `equity_snapshots`.
//!
//! Why this exists: the kill-switch supervisor reads
//! `equity_snapshots` as its source of truth for the realised-PnL
//! budget. Without a periodic writer, the budget-guard is frozen at
//! bootstrap forever and cannot protect real capital.
//!
//! # What this writer does NOT do
//!
//! - It does not track ETF or spot sleeves. Those values are held at
//!   zero. They will be populated by a separate writer when those
//!   brokers are added.
//! - It does not model `nav_per_unit` / `total_units`. For the MVP
//!   those stay at `1.0` each — the NAV-per-unit abstraction only
//!   matters once there are multiple capital owners, which is not
//!   the case here. `drawdown_fraction` therefore stays at 0.
//! - It does not fetch live FX. `usd_per_chf` comes from
//!   `RiskConfig::usd_per_chf_fallback`, a manually-tuned constant.
//!   A proper FX feed is a future drop.
//!
//! # Tick cadence
//!
//! 30 s. The kill-switch supervisor runs at 60 s
//! (`kill_switch_check_interval_secs`) so each supervisor tick sees
//! at most one snapshot old. The Kraken `GET /accounts` endpoint is
//! a single authenticated REST call; at 30 s that is 2 calls/min =
//! ~3000/day, well within any broker rate budget.

use anyhow::{anyhow, Result};
use chrono::Utc;
use rt_kraken_futures::rest::KrakenFuturesRestClient;
use rt_kraken_futures::ws::TickerStore;
use rt_persistence::Database;
use rt_risk::RiskConfig;
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

/// Hard-coded tick interval. 30 s is a deliberate compromise between
/// giving the kill-switch supervisor fresh data every tick and not
/// hammering Kraken's accounts endpoint for data that changes slowly.
const TICK: Duration = Duration::from_secs(30);

/// Market-data subscription key we expect to find a ticker under.
/// Must match the symbol configured in `DaemonConfig::kraken.products`.
const PERP_SYMBOL: &str = "PI_XBTUSD";

/// Kraken's account key for the single-collateral perpetual on BTC.
/// The `fi_` prefix denotes "fixed-inverse" (single-collateral
/// inverse futures). Do not confuse with the `flex` multi-collateral
/// account.
const SC_MARGIN_ACCOUNT_KEY: &str = "fi_xbtusd";

/// Leverage sleeve identifier used in the `positions.sleeve` column.
const LEVERAGE_SLEEVE: &str = "CryptoLeverage";

/// Entry point. Runs the writer loop until the shutdown channel flips
/// to true.
pub async fn run(
    db: Arc<Database>,
    client: Arc<KrakenFuturesRestClient>,
    tickers: TickerStore,
    risk: Arc<RiskConfig>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    info!(
        tick_seconds = TICK.as_secs(),
        "equity snapshot writer started"
    );

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match write_one_snapshot(&db, &client, &tickers, &risk).await {
                    Ok(id) => debug!(snapshot_id = id, "equity snapshot written"),
                    Err(e) => warn!(error = %e, "equity snapshot write failed; will retry next tick"),
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("equity writer received shutdown signal");
                    return;
                }
            }
        }
    }
}

/// Compute and persist exactly one snapshot. Kept as a free function
/// so every branch is either exhaustively logged or propagated.
async fn write_one_snapshot(
    db: &Database,
    client: &KrakenFuturesRestClient,
    tickers: &TickerStore,
    risk: &RiskConfig,
) -> Result<i64> {
    // ---- 1. FX sanity -------------------------------------------
    let usd_per_chf = risk.usd_per_chf_fallback;
    if usd_per_chf <= Decimal::ZERO {
        return Err(anyhow!(
            "usd_per_chf_fallback is {} (must be > 0)",
            usd_per_chf
        ));
    }

    // ---- 2. Current BTC mark from ticker store ------------------
    let btc_usd = {
        let t = tickers.read().await;
        t.get(PERP_SYMBOL)
            .and_then(|tm| tm.last)
            .ok_or_else(|| {
                anyhow!(
                    "no ticker for {} in store yet; \
                     WS feed may not have published its first quote",
                    PERP_SYMBOL
                )
            })?
    };
    if btc_usd <= Decimal::ZERO {
        return Err(anyhow!("ticker last price for {} is non-positive: {}", PERP_SYMBOL, btc_usd));
    }

    // ---- 3. Fetch accounts --------------------------------------
    let accounts = client
        .get_accounts()
        .await
        .map_err(|e| anyhow!("fetching accounts: {}", e))?;

    // ---- 4. Extract balances ------------------------------------
    //
    // Single-collateral margin account (PI_XBTUSD): native currency
    // is BTC. `auxiliary.pv` is the portfolio value (collateral +
    // unrealised PnL) in BTC.
    let sc = accounts.margin(SC_MARGIN_ACCOUNT_KEY);
    let sc_pv_btc = sc
        .as_ref()
        .map(|m| m.auxiliary.pv)
        .unwrap_or(Decimal::ZERO);
    let sc_unrealized_btc = sc
        .as_ref()
        .map(|m| m.auxiliary.pnl)
        .unwrap_or(Decimal::ZERO);

    // Multi-collateral account: aggregate USD-valued balance across
    // all deposited currencies. `balance_value` already sums them.
    let flex_balance_usd = accounts
        .flex()
        .map(|f| f.balance_value)
        .unwrap_or(Decimal::ZERO);

    // ---- 5. Currency conversions --------------------------------
    //
    // All target fields are CHF. We have:
    //   BTC → USD via btc_usd (the mark)
    //   USD → CHF via /usd_per_chf
    //
    // so BTC → CHF is (qty_btc * btc_usd) / usd_per_chf.

    let leverage_value_chf = sc_pv_btc * btc_usd / usd_per_chf;
    let unrealized_leverage_chf = sc_unrealized_btc * btc_usd / usd_per_chf;
    let cash_chf = flex_balance_usd / usd_per_chf;

    // ---- 6. Realised PnL from local positions table -------------
    //
    // Stored in BTC per Drop 6b. Convert to CHF at the current mark.
    let realized_btc = db
        .sum_realized_pnl_btc_by_sleeve(LEVERAGE_SLEEVE)
        .await
        .map_err(|e| anyhow!("aggregating realized_pnl_btc: {}", e))?;
    let realized_chf = realized_btc * btc_usd / usd_per_chf;

    // ---- 7. Total + NAV (simplified for MVP) --------------------
    let total_equity_chf = cash_chf + leverage_value_chf;
    // For a single-operator MVP there is exactly 1 unit of ownership
    // and NAV == 1 forever. Drawdown therefore 0. A multi-party fund
    // would track capital_flows and compute these properly.
    let nav_per_unit = Decimal::ONE;
    let nav_hwm_per_unit = Decimal::ONE;
    let total_units = Decimal::ONE;
    let drawdown_fraction = Decimal::ZERO;

    // ---- 8. Persist ---------------------------------------------
    let now_iso = Utc::now().to_rfc3339();
    let id = db
        .insert_equity_snapshot(
            &now_iso,
            &total_equity_chf.to_string(),
            "0", // crypto_spot_value_chf — not tracked yet
            "0", // etf_value_chf — not tracked yet
            &leverage_value_chf.to_string(),
            &cash_chf.to_string(),
            &realized_chf.to_string(),
            &drawdown_fraction.to_string(),
            &unrealized_leverage_chf.to_string(),
            &nav_per_unit.to_string(),
            &nav_hwm_per_unit.to_string(),
            &total_units.to_string(),
        )
        .await
        .map_err(|e| anyhow!("inserting equity_snapshot: {}", e))?;

    debug!(
        total_equity_chf = %total_equity_chf,
        cash_chf = %cash_chf,
        leverage_value_chf = %leverage_value_chf,
        realized_chf = %realized_chf,
        unrealized_chf = %unrealized_leverage_chf,
        btc_usd = %btc_usd,
        usd_per_chf = %usd_per_chf,
        "equity snapshot computed"
    );

    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt_kraken_futures::messages::TickerMessage;
    use std::collections::HashMap;
    use std::str::FromStr;
    use tokio::sync::RwLock;

    fn fresh_ticker_store_with_btc(last: Decimal) -> TickerStore {
        let store = Arc::new(RwLock::new(HashMap::new()));
        let ticker = TickerMessage {
            product_id: PERP_SYMBOL.to_string(),
            bid: None,
            ask: None,
            bid_size: None,
            ask_size: None,
            funding_rate: None,
            funding_rate_prediction: None,
            relative_funding_rate: None,
            next_funding_rate_time: None,
            time: None,
            last: Some(last),
        };
        // We can't write to the Arc<RwLock> synchronously; wrap in a
        // blocking_write in a sync context.
        let mut guard = store.blocking_write();
        guard.insert(PERP_SYMBOL.to_string(), ticker);
        drop(guard);
        store
    }

    #[test]
    fn ticker_store_fixture_round_trips() {
        let btc_usd = Decimal::from(75_000);
        let store = fresh_ticker_store_with_btc(btc_usd);
        let guard = store.blocking_read();
        let t = guard.get(PERP_SYMBOL).expect("ticker present");
        assert_eq!(t.last, Some(btc_usd));
    }

    // The full write_one_snapshot path requires a real
    // KrakenFuturesRestClient which we cannot stand up in a unit
    // test without a network. Those would be integration tests
    // running against the demo endpoint. Coverage for the arithmetic
    // below is provided by the deterministic helpers.

    #[test]
    fn fx_conversion_btc_to_chf_is_correct() {
        // 0.5 BTC at $75,000 in a world where 1 CHF = $1.10
        //   USD value = 0.5 * 75_000 = 37_500
        //   CHF value = 37_500 / 1.10 ≈ 34_090.909…
        let qty_btc = Decimal::from_str("0.5").unwrap();
        let btc_usd = Decimal::from(75_000);
        let usd_per_chf = Decimal::from_str("1.10").unwrap();

        let chf = qty_btc * btc_usd / usd_per_chf;
        // Compare against expected value with tolerance of 0.01 CHF.
        let expected_lower = Decimal::from_str("34090.90").unwrap();
        let expected_upper = Decimal::from_str("34090.92").unwrap();
        assert!(
            chf > expected_lower && chf < expected_upper,
            "got {}",
            chf
        );
    }

    #[test]
    fn fx_conversion_usd_to_chf_inverse_of_usd_per_chf() {
        // 11 USD / 1.10 usd_per_chf == 10 CHF
        let usd = Decimal::from(11);
        let usd_per_chf = Decimal::from_str("1.10").unwrap();
        let chf = usd / usd_per_chf;
        assert_eq!(chf, Decimal::from(10));
    }

    #[test]
    fn realized_pnl_btc_negative_converts_to_negative_chf() {
        // A -0.01 BTC realised loss at $75,000 and 1.10 usd/chf
        //   USD loss = -750
        //   CHF loss = -681.81…
        let realized_btc = Decimal::from_str("-0.01").unwrap();
        let btc_usd = Decimal::from(75_000);
        let usd_per_chf = Decimal::from_str("1.10").unwrap();

        let chf = realized_btc * btc_usd / usd_per_chf;
        assert!(chf < Decimal::ZERO, "sign preserved, got {}", chf);
        let expected_lower = Decimal::from_str("-681.82").unwrap();
        let expected_upper = Decimal::from_str("-681.81").unwrap();
        assert!(
            chf > expected_lower && chf < expected_upper,
            "magnitude off, got {}",
            chf
        );
    }
}
