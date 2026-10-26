use std::fmt;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use hft_data::{PRICE_SCALE, Price, Qty, Side, Ts};

use crate::engine::Fees;
use crate::strategy::{ClientOrderId, Liquidity};

/// A fill as it happened at the exchange (exchange time, not when the strategy learned of it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FillRecord {
    pub ts: Ts,
    pub id: ClientOrderId,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub liquidity: Liquidity,
    /// Fee charged for this fill in price units (negative = rebate).
    pub fee: i64,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub events: u64,
    pub callbacks: u64,
    pub orders_sent: u64,
    pub shares_sent: u64,
    pub cancels_sent: u64,
    pub fills: Vec<FillRecord>,
    pub position: i64,
    pub max_abs_position: i64,
    /// Trade cash flow in price units × shares.
    pub cash: i64,
    /// Fees paid (negative when rebates exceed fees), price units.
    pub fees: i64,
    pub maker_shares: u64,
    pub taker_shares: u64,
    /// Price the final position is marked at (exchange mid at the end).
    pub mark_price: Option<Price>,
    /// Exchange time of the market event that triggered an order → that order's arrival
    /// at the exchange: feed + queueing behind earlier callbacks + compute + entry.
    /// Orders from the strategy, or from the fast path of a hybrid run.
    pub tick_to_order_ns: Vec<u64>,
    /// The same for orders the slow path of a hybrid run sent itself.
    pub slow_tick_to_order_ns: Vec<u64>,
    /// Exchange time of the market event the slow path reacted to → its message being
    /// handled by the fast path. How stale the FPGA's parameters are.
    pub tick_to_param_ns: Vec<u64>,
    /// Exchange mid each time it changed, for markouts and the equity curve.
    pub mids: Vec<(Ts, Price)>,
    pub wall_time: Duration,
}

pub(crate) fn dollars(units: i64) -> f64 {
    units as f64 / PRICE_SCALE as f64
}

impl Report {
    pub(crate) fn record_fill(&mut self, mut fill: FillRecord, fees: &Fees) {
        let qty = fill.qty as i64;
        self.position += fill.side.sign() * qty;
        self.max_abs_position = self.max_abs_position.max(self.position.abs());
        self.cash -= fill.side.sign() * fill.price * qty;
        match fill.liquidity {
            Liquidity::Maker => {
                self.maker_shares += fill.qty as u64;
                fill.fee = fees.maker_per_share * qty;
            }
            Liquidity::Taker => {
                self.taker_shares += fill.qty as u64;
                fill.fee = fees.taker_per_share * qty;
            }
        }
        self.fees += fill.fee;
        self.fills.push(fill);
    }

    /// Net PnL in price units: trading PnL marked to `mark_price`, minus fees.
    pub fn pnl(&self) -> i64 {
        let mark = self.mark_price.unwrap_or(0) * self.position;
        self.cash + mark - self.fees
    }

    pub fn events_per_sec(&self) -> f64 {
        self.events as f64 / self.wall_time.as_secs_f64().max(1e-9)
    }

    /// Percentile (0..=100) of tick-to-order latency, if any orders were sent.
    pub fn tick_to_order_pct(&self, pct: f64) -> Option<u64> {
        percentile(&self.tick_to_order_ns, pct)
    }

    pub fn write_fills_csv(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let mut w = io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(w, "ts_ns,order_id,side,price,qty,liquidity,fee")?;
        for f in &self.fills {
            let side = if f.side == Side::Bid { "buy" } else { "sell" };
            let liq = if f.liquidity == Liquidity::Maker {
                "maker"
            } else {
                "taker"
            };
            writeln!(
                w,
                "{},{},{},{:.4},{},{},{:.4}",
                f.ts,
                f.id,
                side,
                dollars(f.price),
                f.qty,
                liq,
                dollars(f.fee)
            )?;
        }
        w.flush()
    }
}

/// Percentile (0..=100) of `samples`, if there are any.
pub fn percentile(samples: &[u64], pct: f64) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut v = samples.to_vec();
    v.sort_unstable();
    let idx = ((pct / 100.0) * (v.len() - 1) as f64).round() as usize;
    Some(v[idx])
}

/// One latency distribution line, or nothing if there are no samples.
fn latency_line(f: &mut fmt::Formatter<'_>, label: &str, samples: &[u64]) -> fmt::Result {
    if let (Some(p50), Some(p99), Some(max)) = (
        percentile(samples, 50.0),
        percentile(samples, 99.0),
        percentile(samples, 100.0),
    ) {
        write!(f, "\n{label:<12}p50 {p50}ns, p99 {p99}ns, max {max}ns")?;
    }
    Ok(())
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "replay      {} events in {:.3}s ({:.1}M events/s)",
            self.events,
            self.wall_time.as_secs_f64(),
            self.events_per_sec() / 1e6
        )?;
        writeln!(
            f,
            "strategy    {} callbacks, {} orders, {} cancels",
            self.callbacks, self.orders_sent, self.cancels_sent
        )?;
        writeln!(
            f,
            "fills       {} ({} maker / {} taker shares)",
            self.fills.len(),
            self.maker_shares,
            self.taker_shares
        )?;
        writeln!(
            f,
            "position    {} final, {} max abs",
            self.position, self.max_abs_position
        )?;
        writeln!(f, "fees        ${:.2}", dollars(self.fees))?;
        write!(f, "net pnl     ${:.2}", dollars(self.pnl()))?;
        latency_line(f, "tick→order", &self.tick_to_order_ns)?;
        latency_line(f, "slow t→o", &self.slow_tick_to_order_ns)?;
        latency_line(f, "tick→param", &self.tick_to_param_ns)
    }
}
