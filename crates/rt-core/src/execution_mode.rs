//! Execution mode: the single switch that decides whether approved signals
//! result in real orders or only in audit records.
//!
//! This is the primary safety gate between testing and live trading.
//! The default is deliberately conservative: `DryRun`. A production
//! deployment must *explicitly* opt into `Live`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Orders are built and recorded to SQLite (`dry_run_orders` table) but
    /// never submitted to any broker. Default. No real money at risk.
    /// No broker connectivity required beyond market-data subscriptions.
    DryRun,

    /// Orders are submitted to the broker's paper/sandbox/demo API.
    /// No real money at risk, but the broker-side submission + fill flow
    /// is exercised end-to-end. Requires demo credentials.
    Paper,

    /// Orders are submitted to the broker's production API.
    /// REAL MONEY AT RISK. Requires production credentials and an
    /// explicit configuration override.
    Live,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        Self::DryRun
    }
}

impl ExecutionMode {
    /// Human-readable summary, safe for logs and audit trails.
    pub fn description(&self) -> &'static str {
        match self {
            ExecutionMode::DryRun => "dry-run (no broker submission)",
            ExecutionMode::Paper => "paper (broker sandbox/demo)",
            ExecutionMode::Live => "LIVE (real money)",
        }
    }

    /// Does this mode actually submit orders to a broker?
    pub fn submits_orders(&self) -> bool {
        matches!(self, ExecutionMode::Paper | ExecutionMode::Live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_dry_run() {
        // This is a safety invariant: forgetting to configure the mode
        // must never result in real orders.
        assert_eq!(ExecutionMode::default(), ExecutionMode::DryRun);
    }

    #[test]
    fn dry_run_does_not_submit() {
        assert!(!ExecutionMode::DryRun.submits_orders());
    }

    #[test]
    fn paper_and_live_submit() {
        assert!(ExecutionMode::Paper.submits_orders());
        assert!(ExecutionMode::Live.submits_orders());
    }

    #[test]
    fn serialises_snake_case() {
        assert_eq!(
            serde_json::to_string(&ExecutionMode::DryRun).unwrap(),
            "\"dry_run\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionMode::Paper).unwrap(),
            "\"paper\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionMode::Live).unwrap(),
            "\"live\""
        );
    }

    #[test]
    fn deserialises_snake_case() {
        let mode: ExecutionMode = serde_json::from_str("\"dry_run\"").unwrap();
        assert_eq!(mode, ExecutionMode::DryRun);
        let mode: ExecutionMode = serde_json::from_str("\"live\"").unwrap();
        assert_eq!(mode, ExecutionMode::Live);
    }
}
