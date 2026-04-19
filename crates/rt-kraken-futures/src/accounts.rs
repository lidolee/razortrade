//! Parser for the Kraken Futures `GET /api/v3/accounts` response.
//!
//! The response groups balances by account type:
//!   - `fi_*` / `fv_*`: single-collateral margin accounts keyed per
//!     instrument (e.g. `fi_xbtusd`). Each holds a currency balance in
//!     its base coin plus `auxiliary.pv` for portfolio value in that
//!     coin.
//!   - `cash`: multi-currency cash balances, not used as margin.
//!   - `flex`: the multi-collateral margin account, which carries
//!     aggregated `balanceValue` / `portfolioValue` / `marginEquity`
//!     across all deposited collaterals (BTC, ETH, EUR, USD, etc.).
//!
//! We only deserialise strongly-typed structs for the three kinds we
//! actually consume in the equity writer (Drop 12). All other accounts
//! are retained as raw `serde_json::Value` so future Kraken-side
//! additions never break the parse.

use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;

/// Top-level response envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct AccountsResponse {
    pub result: String,
    pub accounts: HashMap<String, serde_json::Value>,
    #[serde(rename = "serverTime")]
    pub server_time: String,
}

impl AccountsResponse {
    /// The multi-collateral account ("flex"), if present. This is the
    /// only place where a single CHF-denominated number summarises all
    /// deposited collaterals.
    pub fn flex(&self) -> Option<FlexAccount> {
        self.accounts
            .get("flex")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
    }

    /// A single-collateral margin account by its Kraken-assigned key
    /// (e.g. `fi_xbtusd` for the PI_XBTUSD perpetual). Returns `None`
    /// if the key is absent or the payload cannot be parsed.
    pub fn margin(&self, key: &str) -> Option<MarginAccount> {
        self.accounts
            .get(key)
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
    }

    /// The `cash` multi-currency account.
    pub fn cash(&self) -> Option<CashAccount> {
        self.accounts
            .get("cash")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
    }
}

/// Single-collateral margin account (e.g. `fi_xbtusd`).
#[derive(Debug, Clone, Deserialize)]
pub struct MarginAccount {
    #[serde(rename = "type")]
    pub account_type: String,
    pub currency: String,
    pub auxiliary: MarginAuxiliary,
    /// Currency-keyed free balance. For PI_XBTUSD this is `{"xbt": <qty>}`.
    pub balances: HashMap<String, Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarginAuxiliary {
    /// USD-equivalent of the account value. Kraken computes this
    /// internally and sometimes returns 0 for single-collateral BTC.
    pub usd: Decimal,
    /// Portfolio value in the account's native currency (e.g. BTC for
    /// `fi_xbtusd`). Equals collateral + unrealised PnL.
    pub pv: Decimal,
    /// Unrealised PnL in the account's native currency.
    pub pnl: Decimal,
    /// Available free balance (available for new orders).
    pub af: Decimal,
    /// Accrued but not yet paid funding.
    pub funding: Decimal,
}

/// The `cash` account: multi-currency cash balances.
#[derive(Debug, Clone, Deserialize)]
pub struct CashAccount {
    #[serde(rename = "type")]
    pub account_type: String,
    pub balances: HashMap<String, Decimal>,
}

/// Multi-collateral margin account ("flex"). This is the aggregated
/// view across all deposited currencies. Numeric fields come back as
/// USD-valued in the Kraken demo environment.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlexAccount {
    #[serde(rename = "type")]
    pub account_type: String,
    pub currencies: HashMap<String, FlexCurrency>,
    pub initial_margin: Decimal,
    pub initial_margin_with_orders: Decimal,
    pub maintenance_margin: Decimal,
    /// Sum of all collateral values (valued in USD).
    pub balance_value: Decimal,
    /// Portfolio value (USD). Includes unrealised PnL of open positions.
    pub portfolio_value: Decimal,
    /// Balance value minus haircuts — the amount that counts as margin.
    pub collateral_value: Decimal,
    pub pnl: Decimal,
    pub unrealized_funding: Decimal,
    pub total_unrealized: Decimal,
    pub total_unrealized_as_margin: Decimal,
    pub available_margin: Decimal,
    pub margin_equity: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlexCurrency {
    /// Deposited quantity in this currency.
    pub quantity: Decimal,
    /// USD-equivalent of `quantity` at Kraken's mark.
    pub value: Decimal,
    /// USD-equivalent after Kraken's haircut — what actually counts.
    pub collateral: Decimal,
    pub available: Decimal,
    pub excluded_collateral: Decimal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    /// Captured live response from demo-futures.kraken.com on
    /// 2026-04-19 15:28 UTC. Trimmed to the accounts we parse; the
    /// other `fi_*` / `fv_*` keys are omitted because the parser
    /// explicitly skips unknown keys.
    const FIXTURE: &str = r#"{
      "result": "success",
      "accounts": {
        "fi_xbtusd": {
          "auxiliary": {
            "usd": 0,
            "pv": 0.06615619029,
            "pnl": 0.0,
            "af": 0.06615619029,
            "funding": 0.0
          },
          "marginRequirements": {"im": 0.0, "mm": 0.0, "lt": 0.0, "tt": 0.0},
          "triggerEstimates": {"im": 0.0, "mm": 0.0, "lt": 0.0, "tt": 0.0},
          "balances": {"xbt": 0.06615619029},
          "currency": "xbt",
          "type": "marginAccount"
        },
        "cash": {
          "balances": {
            "usd": 5000.0,
            "eur": 5000.0,
            "xbt": 0.06616863739
          },
          "type": "cashAccount"
        },
        "flex": {
          "currencies": {
            "BTC": {
              "quantity": 0.06616863739,
              "value": 5029.073837639447,
              "collateral": 4928.492360886658,
              "available": 0.06616863739,
              "excludedCollateral": 0
            },
            "USD": {
              "quantity": 4999.02353918093,
              "value": 4999.02353918093,
              "collateral": 4999.02353918093,
              "available": 4999.02353918093,
              "excludedCollateral": 0
            }
          },
          "initialMargin": 0,
          "initialMarginWithOrders": 0,
          "maintenanceMargin": 0,
          "balanceValue": 20916.9328979138,
          "portfolioValue": 20916.9328979138,
          "collateralValue": 20498.27800031727,
          "pnl": 0,
          "unrealizedFunding": 0,
          "totalUnrealized": 0,
          "totalUnrealizedAsMargin": 0,
          "availableMargin": 20498.27800031727,
          "marginEquity": 20498.27800031727,
          "excludedCollateralValue": 0,
          "type": "multiCollateralMarginAccount"
        }
      },
      "serverTime": "2026-04-19T15:28:43.530Z"
    }"#;

    #[test]
    fn parses_envelope_and_all_three_account_types() {
        let resp: AccountsResponse = serde_json::from_str(FIXTURE).expect("parse");
        assert_eq!(resp.result, "success");
        assert!(resp.flex().is_some(), "flex account must parse");
        assert!(
            resp.margin("fi_xbtusd").is_some(),
            "fi_xbtusd margin account must parse"
        );
        assert!(resp.cash().is_some(), "cash account must parse");
    }

    #[test]
    fn flex_carries_aggregate_margin_fields() {
        let resp: AccountsResponse = serde_json::from_str(FIXTURE).expect("parse");
        let flex = resp.flex().expect("flex present");
        assert_eq!(flex.account_type, "multiCollateralMarginAccount");
        assert_eq!(flex.balance_value, Decimal::from_str("20916.9328979138").unwrap());
        assert_eq!(flex.portfolio_value, Decimal::from_str("20916.9328979138").unwrap());
        assert_eq!(flex.margin_equity, Decimal::from_str("20498.27800031727").unwrap());
        assert_eq!(flex.pnl, Decimal::ZERO);
    }

    #[test]
    fn flex_currencies_are_keyed_by_symbol() {
        let resp: AccountsResponse = serde_json::from_str(FIXTURE).expect("parse");
        let flex = resp.flex().expect("flex present");
        let btc = flex.currencies.get("BTC").expect("BTC row");
        assert_eq!(btc.quantity, Decimal::from_str("0.06616863739").unwrap());
        let usd = flex.currencies.get("USD").expect("USD row");
        assert_eq!(usd.quantity, Decimal::from_str("4999.02353918093").unwrap());
    }

    #[test]
    fn margin_account_exposes_pv_and_pnl() {
        let resp: AccountsResponse = serde_json::from_str(FIXTURE).expect("parse");
        let m = resp.margin("fi_xbtusd").expect("fi_xbtusd present");
        assert_eq!(m.account_type, "marginAccount");
        assert_eq!(m.currency, "xbt");
        assert_eq!(m.auxiliary.pv, Decimal::from_str("0.06615619029").unwrap());
        assert_eq!(m.auxiliary.pnl, Decimal::ZERO);
        assert_eq!(
            m.balances.get("xbt").copied(),
            Some(Decimal::from_str("0.06615619029").unwrap())
        );
    }

    #[test]
    fn unknown_key_returns_none_rather_than_error() {
        let resp: AccountsResponse = serde_json::from_str(FIXTURE).expect("parse");
        assert!(resp.margin("fi_does_not_exist").is_none());
    }

    #[test]
    fn unknown_account_types_are_ignored_not_fatal() {
        // Injecting an unknown account key must NOT fail the top-level parse.
        let mut doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        doc.get_mut("accounts")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(
                "fi_future_product_we_dont_know".to_string(),
                serde_json::json!({"weird": "shape"}),
            );
        let resp: AccountsResponse = serde_json::from_value(doc).expect("tolerates unknown keys");
        // We still get our known accounts.
        assert!(resp.flex().is_some());
    }
}
