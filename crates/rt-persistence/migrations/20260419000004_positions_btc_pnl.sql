-- Drop 6b: position-level reconciliation needs a realised-PnL field
-- expressed in the contract's settlement currency (BTC for inverse
-- perpetuals like PI_XBTUSD). The existing realized_pnl_chf column
-- remains for later conversion via a BTC/CHF FX snapshot, but for
-- Drop 6b we record the native-collateral number directly to avoid
-- taking an FX dependency before it is needed.
ALTER TABLE positions ADD COLUMN realized_pnl_btc TEXT;
