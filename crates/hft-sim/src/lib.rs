//! Discrete-event backtester for L3 data.
//!
//! Two clocks run side by side. History is applied to the *exchange* book at its
//! recorded time. The strategy sees each event only after a feed delay, in its own
//! lagged book, is busy for a compute delay per callback, and its orders reach the
//! exchange after an entry delay. Wall-clock speed of the simulator never enters the
//! results; only the modeled delays do.
//!
//! FPGA designs are modeled two ways: [`ComputeLatency::Pipelined`] for logic that takes
//! a new message every few clock cycles instead of one callback at a time, and
//! [`run_hybrid`] for hardware that fires on parameters written by slower software.

pub mod engine;
pub mod exchange;
pub mod latency;
pub mod metrics;
pub mod report;
pub mod strategy;

pub use engine::{Fees, HybridConfig, SimConfig, run, run_hybrid};
pub use latency::{ComputeLatency, Latency, LatencyConfig};
pub use metrics::{DEFAULT_HORIZONS_NS, Markout, Metrics};
pub use report::{FillRecord, Report};
pub use strategy::{
    ClientOrderId, Ctx, Idle, Liquidity, OrderUpdate, Path, Request, Strategy, TimeInForce,
};
