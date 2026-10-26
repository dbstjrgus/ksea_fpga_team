//! Example strategies.

mod hybrid_taker;
mod market_maker;

pub use hybrid_taker::{ArmedTaker, FairValueArmer, HybridTakerConfig, TakerMsg, hybrid_taker};
pub use market_maker::{MarketMaker, MarketMakerConfig};

/// Does nothing; measures raw replay throughput.
pub struct Noop;

impl hft_sim::Strategy for Noop {}
