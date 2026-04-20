//! The five concrete pre-trade checks.
//!
//! Each check is its own module, implements [`PreTradeCheck`], and has its
//! own unit tests. This structure makes it trivial to audit a single check
//! in isolation.

pub mod duplicate_position;
pub mod funding_rate;
pub mod hard_limit;
pub mod notional_cap;
pub mod spread_liquidity;
pub mod staleness;
pub mod volatility;

pub use duplicate_position::DuplicatePositionCheck;
pub use funding_rate::FundingRateCheck;
pub use hard_limit::HardLimitCheck;
pub use notional_cap::NotionalCapCheck;
pub use spread_liquidity::SpreadLiquidityCheck;
pub use staleness::StalenessCheck;
pub use volatility::VolatilityRegimeCheck;
