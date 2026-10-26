//! Synthetic MBO stream for testing the engine (throughput, determinism, invariants).
//!
//! It has none of the microstructure real strategies trade on, so it must never be used
//! to judge whether a strategy makes money.

use std::collections::{BTreeMap, VecDeque};
use std::io;

use crate::event::{MboEvent, OrderId, PRICE_SCALE, Price, Qty, Side, Ts};
use crate::rng::Rng;

#[derive(Clone, Debug)]
pub struct SyntheticConfig {
    pub seed: u64,
    pub events: usize,
    pub start_price: Price,
    pub tick: Price,
    pub mean_gap_ns: u64,
    pub start_ts: Ts,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        SyntheticConfig {
            seed: 1,
            events: 1_000_000,
            start_price: 100 * PRICE_SCALE,
            tick: PRICE_SCALE / 100,
            mean_gap_ns: 2_000,
            start_ts: 34_200_000_000_000, // 09:30
        }
    }
}

const TARGET_ORDERS: usize = 150;

/// One side's FIFO queues by price.
struct Ladder {
    side: Side,
    levels: BTreeMap<Price, VecDeque<(OrderId, Qty)>>,
    orders: usize,
}

impl Ladder {
    fn nth_price(&self, n: usize) -> Option<Price> {
        match self.side {
            Side::Bid => self.levels.keys().rev().nth(n).copied(),
            Side::Ask => self.levels.keys().nth(n).copied(),
        }
    }

    fn best(&self) -> Option<Price> {
        self.nth_price(0)
    }

    /// Reduces the order at `price`/`idx`, dropping it and its level when empty.
    fn reduce(&mut self, price: Price, idx: usize, qty: Qty) {
        let queue = self.levels.get_mut(&price).unwrap();
        queue[idx].1 -= qty;
        if queue[idx].1 == 0 {
            queue.remove(idx);
            self.orders -= 1;
            if queue.is_empty() {
                self.levels.remove(&price);
            }
        }
    }
}

pub fn generate(
    cfg: &SyntheticConfig,
    mut emit: impl FnMut(MboEvent) -> io::Result<()>,
) -> io::Result<()> {
    let mut rng = Rng::new(cfg.seed);
    let mut sides = [
        Ladder {
            side: Side::Bid,
            levels: BTreeMap::new(),
            orders: 0,
        },
        Ladder {
            side: Side::Ask,
            levels: BTreeMap::new(),
            orders: 0,
        },
    ];
    let tick = cfg.tick;
    let mut ts = cfg.start_ts;
    let mut next_id: OrderId = 1;
    // Latent fair ask; fair bid is one tick below. Random-walks, and liquidity left on
    // the wrong side of it is swept, which is what moves the touch.
    let mut fair = cfg.start_price;

    for _ in 0..cfg.events {
        ts += 1 + rng.below(2 * cfg.mean_gap_ns);
        if rng.below(100) < 2 {
            fair = (fair + if rng.below(2) == 0 { tick } else { -tick }).max(2 * tick);
        }

        let stale_ask = sides[1].best().is_some_and(|p| p < fair);
        let stale_bid = sides[0].best().is_some_and(|p| p > fair - tick);
        let ev = if stale_ask || stale_bid {
            let si = if stale_ask { 1 } else { 0 };
            let price = sides[si].best().unwrap();
            let (id, qty) = sides[si].levels[&price][0];
            sides[si].reduce(price, 0, qty);
            MboEvent::fill(ts, sides[si].side, id, price, qty)
        } else {
            let si = rng.below(2) as usize;
            let side = sides[si].side;
            let roll = rng.below(100);
            // Mean-revert depth toward TARGET_ORDERS per side.
            let add_pct = if sides[si].orders < TARGET_ORDERS {
                55
            } else {
                30
            };

            if sides[si].levels.is_empty() || roll < add_pct {
                // Mostly at or near the fair touch, sometimes up to 8 ticks back.
                let k = (rng.below(10) * rng.below(10) / 10) as i64;
                let mut price = match side {
                    Side::Bid => fair - tick - k * tick,
                    Side::Ask => fair + k * tick,
                };
                if let Some(opp) = sides[1 - si].best() {
                    price = match side {
                        Side::Bid => price.min(opp - tick),
                        Side::Ask => price.max(opp + tick),
                    };
                }
                let price = price.max(tick);
                let qty = 100 * (1 + rng.below(5) as Qty);
                let id = next_id;
                next_id += 1;
                sides[si]
                    .levels
                    .entry(price)
                    .or_default()
                    .push_back((id, qty));
                sides[si].orders += 1;
                MboEvent::add(ts, side, id, price, qty)
            } else if roll < add_pct + 45 {
                let depth = sides[si].levels.len().min(10);
                let price = sides[si]
                    .nth_price(rng.below(depth as u64) as usize)
                    .unwrap();
                let queue = &sides[si].levels[&price];
                let idx = rng.below(queue.len() as u64) as usize;
                let (id, qty) = queue[idx];
                let cq = if qty >= 200 && rng.below(2) == 0 {
                    qty / 2
                } else {
                    qty
                };
                sides[si].reduce(price, idx, cq);
                MboEvent::cancel(ts, side, id, price, cq)
            } else {
                let price = sides[si].best().unwrap();
                let (id, qty) = sides[si].levels[&price][0];
                let eq = 1 + rng.below(qty as u64) as Qty;
                sides[si].reduce(price, 0, eq);
                MboEvent::fill(ts, side, id, price, eq)
            }
        };
        emit(ev)?;
    }
    Ok(())
}
