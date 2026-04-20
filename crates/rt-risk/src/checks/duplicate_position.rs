//! # Check: Duplicate Position
//!
//! Drop 19 — CV-A1d remediation.
//!
//! Rejects a new leverage-sleeve signal if the portfolio already holds
//! an open position on the same instrument **in the same direction**.
//! A second entry on a persistent-trend breakout would deterministically
//! double exposure; combined with the Python signal generator not
//! checking open positions before writing (fixed in `donchian_signal.py`
//! Drop 19) and the possibility of REST-timeout-induced orphan orders
//! (also Drop 19), the path to an unintended doubled position was
//! real, not theoretical.
//!
//! ## Semantics
//!
//! - No open position on this instrument -> `Approved`.
//! - Open position on this instrument, same side -> `Rejected`.
//! - Open position on this instrument, opposite side -> `Approved`.
//!   The operator explicitly signalled a reversal; if a flip is
//!   dangerous in the current regime, the other checks (volatility,
//!   hard-limit, worst-case) already cover that.
//! - Non-leverage sleeves -> `NotApplicable`. Spot accumulation is
//!   additive by design (DCA); the check would be structurally wrong
//!   there.
//!
//! ## Where the data comes from
//!
//! `PortfolioState::open_positions` is populated by
//! `rt-daemon::portfolio_loader::load_portfolio_state` from
//! `positions WHERE closed_at IS NULL`. The check does not make its
//! own database query — keeps it pure and unit-testable.

use crate::checklist::{CheckOutcome, PreTradeCheck, PreTradeContext, RejectionReason};
use rt_core::instrument::Sleeve;
use rt_core::order::Side;

pub struct DuplicatePositionCheck;

impl PreTradeCheck for DuplicatePositionCheck {
    fn id(&self) -> &'static str {
        "duplicate_position"
    }

    fn description(&self) -> &'static str {
        "Reject if an open leverage position on the same instrument + side already exists"
    }

    fn evaluate(&self, ctx: &PreTradeContext<'_>) -> CheckOutcome {
        if ctx.signal.sleeve != Sleeve::CryptoLeverage {
            return CheckOutcome::NotApplicable;
        }

        let want_long = matches!(ctx.signal.side, Side::Buy);

        for pos in &ctx.portfolio.open_positions {
            if pos.instrument_symbol != ctx.signal.instrument_symbol {
                continue;
            }
            if pos.sleeve != Sleeve::CryptoLeverage {
                continue;
            }
            let pos_long = pos.is_long();
            let pos_short = pos.is_short();
            let same_direction =
                (want_long && pos_long) || (!want_long && pos_short);
            if same_direction {
                let side_str = if want_long { "buy" } else { "sell" };
                return CheckOutcome::Rejected(
                    RejectionReason::DuplicatePositionOpen {
                        instrument_symbol: ctx.signal.instrument_symbol.clone(),
                        side: side_str.to_string(),
                        existing_quantity: pos.quantity,
                    },
                );
            }
        }

        CheckOutcome::Approved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RiskConfig;
    use chrono::Utc;
    use rt_core::{
        instrument::{Broker, Sleeve},
        market_data::{MarketSnapshot, OrderBookLevel, OrderBookSnapshot},
        order::Side,
        portfolio::{OpenPositionSummary, PortfolioState, SleeveState},
        signal::{Signal, SignalStatus, SignalType},
    };
    use rust_decimal::Decimal;

    fn make_ctx(
        signal_side: Side,
        existing_positions: Vec<OpenPositionSummary>,
    ) -> (Signal, MarketSnapshot, PortfolioState, RiskConfig, chrono::DateTime<Utc>) {
        let now = Utc::now();
        let signal = Signal {
            id: 1,
            created_at: now,
            instrument_symbol: "PI_XBTUSD".to_string(),
            side: signal_side,
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
                bids: vec![OrderBookLevel {
                    price: Decimal::from(50_000),
                    quantity: Decimal::from(10),
                }],
                asks: vec![OrderBookLevel {
                    price: Decimal::from(50_010),
                    quantity: Decimal::from(10),
                }],
                timestamp: now,
            },
            last_price: Decimal::from(50_000),
            last_trade_at: now,
            atr_absolute: Decimal::from(1500),
            atr_pct: Decimal::from(3),
            funding_rate_per_8h: None,
            fx_quote_per_chf: Decimal::new(78, 2), // 0.78
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
            open_positions: existing_positions,
        };

        (signal, market, portfolio, RiskConfig::default(), now)
    }

    #[test]
    fn approves_when_no_open_position() {
        let (s, m, p, c, now) = make_ctx(Side::Buy, Vec::new());
        let ctx = PreTradeContext {
            signal: &s,
            market: &m,
            portfolio: &p,
            config: &c,
            now,
        };
        assert_eq!(DuplicatePositionCheck.evaluate(&ctx), CheckOutcome::Approved);
    }

    #[test]
    fn rejects_when_open_long_and_new_long() {
        let existing = vec![OpenPositionSummary {
            instrument_symbol: "PI_XBTUSD".to_string(),
            sleeve: Sleeve::CryptoLeverage,
            quantity: Decimal::from(50), // long
            avg_entry_price: None,
            liquidation_price: None,
            leverage: None,
        }];
        let (s, m, p, c, now) = make_ctx(Side::Buy, existing);
        let ctx = PreTradeContext {
            signal: &s,
            market: &m,
            portfolio: &p,
            config: &c,
            now,
        };
        match DuplicatePositionCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::DuplicatePositionOpen {
                instrument_symbol,
                side,
                existing_quantity,
            }) => {
                assert_eq!(instrument_symbol, "PI_XBTUSD");
                assert_eq!(side, "buy");
                assert_eq!(existing_quantity, Decimal::from(50));
            }
            other => panic!("expected DuplicatePositionOpen, got {:?}", other),
        }
    }

    #[test]
    fn approves_opposite_direction_flip() {
        // Open long; new signal is sell -> this is a reversal, the
        // check lets it through. Other checks (volatility, hard-limit)
        // handle regime-specific concerns.
        let existing = vec![OpenPositionSummary {
            instrument_symbol: "PI_XBTUSD".to_string(),
            sleeve: Sleeve::CryptoLeverage,
            quantity: Decimal::from(50),
            avg_entry_price: None,
            liquidation_price: None,
            leverage: None,
        }];
        let (s, m, p, c, now) = make_ctx(Side::Sell, existing);
        let ctx = PreTradeContext {
            signal: &s,
            market: &m,
            portfolio: &p,
            config: &c,
            now,
        };
        assert_eq!(DuplicatePositionCheck.evaluate(&ctx), CheckOutcome::Approved);
    }

    #[test]
    fn rejects_when_open_short_and_new_short() {
        let existing = vec![OpenPositionSummary {
            instrument_symbol: "PI_XBTUSD".to_string(),
            sleeve: Sleeve::CryptoLeverage,
            quantity: Decimal::from(-25), // short
            avg_entry_price: None,
            liquidation_price: None,
            leverage: None,
        }];
        let (s, m, p, c, now) = make_ctx(Side::Sell, existing);
        let ctx = PreTradeContext {
            signal: &s,
            market: &m,
            portfolio: &p,
            config: &c,
            now,
        };
        match DuplicatePositionCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::DuplicatePositionOpen {
                side,
                existing_quantity,
                ..
            }) => {
                assert_eq!(side, "sell");
                assert_eq!(existing_quantity, Decimal::from(-25));
            }
            other => panic!("expected DuplicatePositionOpen, got {:?}", other),
        }
    }

    #[test]
    fn ignores_positions_on_other_instruments() {
        let existing = vec![OpenPositionSummary {
            instrument_symbol: "PI_ETHUSD".to_string(),
            sleeve: Sleeve::CryptoLeverage,
            quantity: Decimal::from(50),
            avg_entry_price: None,
            liquidation_price: None,
            leverage: None,
        }];
        let (s, m, p, c, now) = make_ctx(Side::Buy, existing);
        let ctx = PreTradeContext {
            signal: &s,
            market: &m,
            portfolio: &p,
            config: &c,
            now,
        };
        assert_eq!(DuplicatePositionCheck.evaluate(&ctx), CheckOutcome::Approved);
    }

    #[test]
    fn spot_sleeve_signal_is_not_applicable() {
        let existing = vec![OpenPositionSummary {
            instrument_symbol: "PI_XBTUSD".to_string(),
            sleeve: Sleeve::CryptoLeverage,
            quantity: Decimal::from(50),
            avg_entry_price: None,
            liquidation_price: None,
            leverage: None,
        }];
        let mut sig_side = Side::Buy;
        let _ = &sig_side;
        let (mut s, m, p, c, now) = make_ctx(sig_side, existing);
        s.sleeve = Sleeve::CryptoSpot;
        let ctx = PreTradeContext {
            signal: &s,
            market: &m,
            portfolio: &p,
            config: &c,
            now,
        };
        assert_eq!(
            DuplicatePositionCheck.evaluate(&ctx),
            CheckOutcome::NotApplicable
        );
    }

    #[test]
    fn zero_quantity_position_is_treated_as_closed() {
        // Defensive: positions.closed_at IS NULL but quantity=0 can
        // occur after a full reduce that hasn't flipped the closed_at
        // flag yet. We must not reject on such ghost rows.
        let existing = vec![OpenPositionSummary {
            instrument_symbol: "PI_XBTUSD".to_string(),
            sleeve: Sleeve::CryptoLeverage,
            quantity: Decimal::ZERO,
            avg_entry_price: None,
            liquidation_price: None,
            leverage: None,
        }];
        let (s, m, p, c, now) = make_ctx(Side::Buy, existing);
        let ctx = PreTradeContext {
            signal: &s,
            market: &m,
            portfolio: &p,
            config: &c,
            now,
        };
        assert_eq!(DuplicatePositionCheck.evaluate(&ctx), CheckOutcome::Approved);
    }
}
