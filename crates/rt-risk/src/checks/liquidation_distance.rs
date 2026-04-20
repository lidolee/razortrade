//! # Check: Liquidation Distance
//!
//! Drop 19 LF-A1.
//!
//! Rejects a new leverage-sleeve signal if any existing open leverage
//! position on the **same instrument** is closer than
//! `RiskConfig::min_liq_distance_atrs` ATRs to its liquidation price.
//! Admitting a new entry in that state would pile additional adverse
//! exposure onto a position already on the cliff.
//!
//! ## NotApplicable-Pfad
//!
//! Feld-Feeding der Kraken-Account-Liquidationspreise in
//! `OpenPositionSummary.liquidation_price` ist in Drop 19 noch nicht
//! verdrahtet (kommt mit Drop 20 `equity_writer`-Erweiterung). Solange
//! `liquidation_price` auf einer offenen Position `None` ist, gibt
//! dieser Check für genau diese Position `NotApplicable` zurück. Nur
//! wenn alle relevanten Positionen `None` liefern, ist der Check
//! insgesamt `NotApplicable`; sobald *eine* Position einen echten
//! Liq-Wert + Mark + ATR hat, wird geprüft.
//!
//! ## Berechnung
//!
//! `distance_atrs = abs(mark - liq) / atr_absolute`.
//!
//! `mark` und `atr_absolute` stammen aus `MarketSnapshot`. Wenn der
//! Check auf das falsche Instrument greifen würde (kein Mark da),
//! ebenfalls `NotApplicable`.

use crate::checklist::{CheckOutcome, PreTradeCheck, PreTradeContext, RejectionReason};
use rt_core::instrument::Sleeve;
use rust_decimal::Decimal;

pub struct LiquidationDistanceCheck;

impl PreTradeCheck for LiquidationDistanceCheck {
    fn id(&self) -> &'static str {
        "liquidation_distance"
    }

    fn description(&self) -> &'static str {
        "Reject if an open leverage position is closer than min_liq_distance_atrs to liquidation"
    }

    fn evaluate(&self, ctx: &PreTradeContext<'_>) -> CheckOutcome {
        if ctx.signal.sleeve != Sleeve::CryptoLeverage {
            return CheckOutcome::NotApplicable;
        }
        if ctx.market.atr_absolute.is_zero() {
            return CheckOutcome::NotApplicable;
        }
        let mark = ctx.market.last_price;
        let min_required = ctx.config.min_liq_distance_atrs;
        let mut any_checked = false;

        for pos in &ctx.portfolio.open_positions {
            if pos.sleeve != Sleeve::CryptoLeverage {
                continue;
            }
            if pos.instrument_symbol != ctx.signal.instrument_symbol {
                continue;
            }
            let liq = match pos.liquidation_price {
                Some(l) => l,
                None => continue, // feeding missing, skip this position
            };
            any_checked = true;
            let distance = (mark - liq).abs() / ctx.market.atr_absolute;
            if distance < min_required {
                return CheckOutcome::Rejected(RejectionReason::LiquidationTooClose {
                    instrument_symbol: pos.instrument_symbol.clone(),
                    distance_atrs: round2(distance),
                    min_required_atrs: min_required,
                });
            }
        }

        if any_checked {
            CheckOutcome::Approved
        } else {
            CheckOutcome::NotApplicable
        }
    }
}

fn round2(d: Decimal) -> Decimal {
    d.round_dp(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RiskConfig;
    use chrono::Utc;
    use rt_core::{
        instrument::Sleeve,
        market_data::{MarketSnapshot, OrderBookLevel, OrderBookSnapshot},
        order::Side,
        portfolio::{OpenPositionSummary, PortfolioState, SleeveState},
        signal::{Signal, SignalStatus, SignalType},
    };
    use rust_decimal::Decimal;

    fn make_ctx(
        positions: Vec<OpenPositionSummary>,
        mark: Decimal,
        atr_abs: Decimal,
    ) -> (Signal, MarketSnapshot, PortfolioState, RiskConfig, chrono::DateTime<Utc>) {
        let now = Utc::now();
        let signal = Signal {
            id: 1,
            created_at: now,
            instrument_symbol: "PI_XBTUSD".to_string(),
            side: Side::Buy,
            signal_type: SignalType::TrendEntry,
            sleeve: Sleeve::CryptoLeverage,
            notional_chf: Decimal::from(300),
            leverage: Decimal::from(2),
            metadata: None,
            status: SignalStatus::Pending,
            processed_at: None,
            rejection_reason: None,
            expires_at: None,
        };
        let market = MarketSnapshot {
            instrument_symbol: "PI_XBTUSD".to_string(),
            order_book: OrderBookSnapshot {
                bids: vec![OrderBookLevel { price: mark, quantity: Decimal::from(10) }],
                asks: vec![OrderBookLevel { price: mark, quantity: Decimal::from(10) }],
                timestamp: now,
            },
            last_price: mark,
            last_trade_at: now,
            atr_absolute: atr_abs,
            atr_pct: Decimal::from(3),
            funding_rate_per_8h: None,
            fx_quote_per_chf: Decimal::new(78, 2),
        };
        let portfolio = PortfolioState {
            snapshot_at: now,
            sleeves: vec![SleeveState {
                sleeve: Sleeve::CryptoLeverage,
                market_value_chf: Decimal::ZERO,
                cash_chf: Decimal::from(1_000),
                realized_pnl_lifetime_chf: Decimal::ZERO,
                unrealized_pnl_chf: Decimal::ZERO,
            }],
            nav_per_unit: Decimal::from(1),
            nav_hwm_per_unit: Decimal::from(1),
            total_units: Decimal::from(1_000),
            kill_switch_active: false,
            open_positions: positions,
        };
        let mut cfg = RiskConfig::default();
        cfg.min_liq_distance_atrs = Decimal::from(2);
        (signal, market, portfolio, cfg, now)
    }

    fn pos_with_liq(liq: Option<Decimal>) -> OpenPositionSummary {
        OpenPositionSummary {
            instrument_symbol: "PI_XBTUSD".to_string(),
            sleeve: Sleeve::CryptoLeverage,
            quantity: Decimal::from(50),
            avg_entry_price: Some(Decimal::from(60_000)),
            liquidation_price: liq,
            leverage: Some(Decimal::from(2)),
        }
    }

    #[test]
    fn not_applicable_without_liq_feed() {
        let (s, m, p, c, now) = make_ctx(
            vec![pos_with_liq(None)],
            Decimal::from(60_000),
            Decimal::from(1_000),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now };
        assert_eq!(LiquidationDistanceCheck.evaluate(&ctx), CheckOutcome::NotApplicable);
    }

    #[test]
    fn not_applicable_without_open_position() {
        let (s, m, p, c, now) = make_ctx(vec![], Decimal::from(60_000), Decimal::from(1_000));
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now };
        assert_eq!(LiquidationDistanceCheck.evaluate(&ctx), CheckOutcome::NotApplicable);
    }

    #[test]
    fn approves_when_far_from_liquidation() {
        // liq 30k, mark 60k, ATR 1k -> distance = 30 ATRs, far above 2.
        let (s, m, p, c, now) = make_ctx(
            vec![pos_with_liq(Some(Decimal::from(30_000)))],
            Decimal::from(60_000),
            Decimal::from(1_000),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now };
        assert_eq!(LiquidationDistanceCheck.evaluate(&ctx), CheckOutcome::Approved);
    }

    #[test]
    fn rejects_when_inside_atr_buffer() {
        // liq 59'500, mark 60'000, ATR 1'000 -> distance = 0.5 ATR (< 2).
        let (s, m, p, c, now) = make_ctx(
            vec![pos_with_liq(Some(Decimal::from(59_500)))],
            Decimal::from(60_000),
            Decimal::from(1_000),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now };
        match LiquidationDistanceCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::LiquidationTooClose {
                instrument_symbol, distance_atrs, min_required_atrs
            }) => {
                assert_eq!(instrument_symbol, "PI_XBTUSD");
                assert_eq!(distance_atrs, Decimal::new(50, 2)); // 0.50
                assert_eq!(min_required_atrs, Decimal::from(2));
            }
            other => panic!("expected LiquidationTooClose, got {:?}", other),
        }
    }

    #[test]
    fn spot_signal_is_not_applicable() {
        let (mut s, m, p, c, now) = make_ctx(
            vec![pos_with_liq(Some(Decimal::from(59_500)))],
            Decimal::from(60_000),
            Decimal::from(1_000),
        );
        s.sleeve = Sleeve::CryptoSpot;
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now };
        assert_eq!(LiquidationDistanceCheck.evaluate(&ctx), CheckOutcome::NotApplicable);
    }

    #[test]
    fn zero_atr_is_not_applicable() {
        let (s, m, p, c, now) = make_ctx(
            vec![pos_with_liq(Some(Decimal::from(59_500)))],
            Decimal::from(60_000),
            Decimal::ZERO,
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now };
        assert_eq!(LiquidationDistanceCheck.evaluate(&ctx), CheckOutcome::NotApplicable);
    }

    #[test]
    fn other_instrument_is_ignored() {
        let mut other = pos_with_liq(Some(Decimal::from(59_500)));
        other.instrument_symbol = "PI_ETHUSD".to_string();
        let (s, m, p, c, now) = make_ctx(
            vec![other],
            Decimal::from(60_000),
            Decimal::from(1_000),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now };
        assert_eq!(LiquidationDistanceCheck.evaluate(&ctx), CheckOutcome::NotApplicable);
    }
}
