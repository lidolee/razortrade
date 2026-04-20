-- Drop 18 CV-1: client order id on orders.
-- Populated as "rt-s<signal_id>" by signal_processor. Used by the
-- fill_reconciler orphan-catcher to match fills back to pending orders
-- whose broker_order_id was lost to a REST timeout.

ALTER TABLE orders ADD COLUMN cli_ord_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_orders_broker_cli_ord_id
    ON orders (broker, cli_ord_id)
    WHERE cli_ord_id IS NOT NULL;
