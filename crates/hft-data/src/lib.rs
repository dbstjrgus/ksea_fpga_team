//! Market-by-order event data: the normalized record, its on-disk format, and sources
//! (Nasdaq TotalView-ITCH 5.0 and a synthetic generator for engine testing).

pub mod event;
pub mod itch;
pub mod rng;
pub mod store;
pub mod synthetic;

pub use event::*;
