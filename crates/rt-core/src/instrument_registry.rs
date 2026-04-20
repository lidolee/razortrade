//! Drop 19 post-audit: Instrument-Metadaten zur Laufzeit.
//!
//! Vor diesem Modul waren `min_order_size` und `tick_size` hartcodiert
//! in `rt-daemon/src/signal_processor.rs` und `rt-daemon/src/main.rs`.
//! Das funktioniert nur solange PI_XBTUSD das einzige Instrument ist.
//! Für PI_ETHUSD, PI_SOLUSD etc. braucht Kraken andere Tick-Sizes
//! (siehe `/api/v3/instruments`) — eine Order mit falschem Tick wird
//! hart mit `INVALID_PRICE` abgelehnt.
//!
//! Dieser Registry-Typ kommt als `Arc<InstrumentRegistry>` in den
//! `SignalProcessor` und `panic_close_leverage_positions`. Wenn ein
//! Symbol nicht im Registry steht, fallen wir auf PI_XBTUSD-Defaults
//! zurück (`min_order_size=1`, `tick_size=0.5`) — das entspricht dem
//! Verhalten vor der Änderung und erhält Backward-Kompatibilität mit
//! Configs die `[[instruments]]` noch nicht deklarieren.

use rust_decimal::Decimal;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct InstrumentMeta {
    pub min_order_size: Decimal,
    pub tick_size: Decimal,
}

#[derive(Debug, Clone, Default)]
pub struct InstrumentRegistry {
    entries: HashMap<String, InstrumentMeta>,
}

impl InstrumentRegistry {
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    pub fn insert(&mut self, symbol: String, meta: InstrumentMeta) {
        self.entries.insert(symbol, meta);
    }

    /// Lookup metadata for a symbol. Falls back to PI_XBTUSD-compatible
    /// defaults (`min_order_size=1`, `tick_size=0.5`) when the symbol
    /// is not registered. This keeps existing single-instrument
    /// deployments working without config changes.
    pub fn lookup(&self, symbol: &str) -> InstrumentMeta {
        if let Some(m) = self.entries.get(symbol) {
            return m.clone();
        }
        InstrumentMeta {
            min_order_size: Decimal::ONE,
            tick_size: Decimal::new(5, 1),
        }
    }
}
