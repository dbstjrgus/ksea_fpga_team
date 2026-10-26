//! L3 (market-by-order) book rebuilt from [`MboEvent`]s.
//!
//! Each order gets a monotonically increasing `seq` when it joins the book. Because
//! priority is price-time FIFO, "order A is ahead of order B at the same price" is
//! simply `a.seq < b.seq`, which is what the simulator uses for queue position.

use hft_data::{Action, MboEvent, OrderId, Price, Qty, Side};
use rustc_hash::FxHashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    pub price: Price,
    pub qty: u64,
    pub count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestingOrder {
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub seq: u64,
}

/// One side's levels sorted worst→best, so the touch is at the end of the vec and
/// the common case (activity near the touch) inserts and removes near the end.
#[derive(Clone, Debug)]
struct Ladder {
    side: Side,
    levels: Vec<Level>,
}

impl Ladder {
    fn new(side: Side) -> Self {
        Ladder {
            side,
            levels: Vec::with_capacity(256),
        }
    }

    #[inline]
    fn find(&self, price: Price) -> Result<usize, usize> {
        // Map both sides onto an ascending key so one search works for both.
        let key = |p: Price| if self.side == Side::Bid { p } else { -p };
        let k = key(price);
        self.levels.binary_search_by(|l| key(l.price).cmp(&k))
    }

    #[inline]
    fn add(&mut self, price: Price, qty: Qty) {
        match self.find(price) {
            Ok(i) => {
                let l = &mut self.levels[i];
                l.qty += qty as u64;
                l.count += 1;
            }
            Err(i) => self.levels.insert(
                i,
                Level {
                    price,
                    qty: qty as u64,
                    count: 1,
                },
            ),
        }
    }

    #[inline]
    fn reduce(&mut self, price: Price, qty: Qty, removes_order: bool) {
        let Ok(i) = self.find(price) else {
            debug_assert!(false, "reduce on missing level {price}");
            return;
        };
        let l = &mut self.levels[i];
        l.qty -= qty as u64;
        if removes_order {
            l.count -= 1;
            if l.count == 0 {
                self.levels.remove(i);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Book {
    orders: FxHashMap<OrderId, RestingOrder>,
    bids: Ladder,
    asks: Ladder,
    next_seq: u64,
}

impl Default for Book {
    fn default() -> Self {
        Self::new()
    }
}

impl Book {
    pub fn new() -> Self {
        Book {
            orders: FxHashMap::with_capacity_and_hasher(1 << 16, Default::default()),
            bids: Ladder::new(Side::Bid),
            asks: Ladder::new(Side::Ask),
            next_seq: 0,
        }
    }

    #[inline]
    fn ladder(&self, side: Side) -> &Ladder {
        match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        }
    }

    #[inline]
    fn ladder_mut(&mut self, side: Side) -> &mut Ladder {
        match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        }
    }

    /// Applies one event. Returns the queue `seq` of the order it added or reduced, or
    /// `None` if it touched no known visible order.
    #[inline]
    pub fn apply(&mut self, ev: &MboEvent) -> Option<u64> {
        match ev.action() {
            Action::Add => {
                let side = ev.side();
                let seq = self.next_seq;
                self.next_seq += 1;
                let order = RestingOrder {
                    side,
                    price: ev.price,
                    qty: ev.qty,
                    seq,
                };
                if let Some(stale) = self.orders.insert(ev.order_id, order) {
                    debug_assert!(false, "duplicate order id {}", ev.order_id);
                    self.ladder_mut(stale.side)
                        .reduce(stale.price, stale.qty, true);
                }
                self.ladder_mut(side).add(ev.price, ev.qty);
                Some(seq)
            }
            Action::Cancel | Action::Fill => {
                let o = self.orders.get_mut(&ev.order_id)?;
                let q = ev.qty.min(o.qty);
                o.qty -= q;
                let (side, price, seq, gone) = (o.side, o.price, o.seq, o.qty == 0);
                if gone {
                    self.orders.remove(&ev.order_id);
                }
                self.ladder_mut(side).reduce(price, q, gone);
                Some(seq)
            }
            Action::Trade | Action::Status => None,
        }
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.levels.last().map(|l| l.price)
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.levels.last().map(|l| l.price)
    }

    pub fn best(&self, side: Side) -> Option<&Level> {
        self.ladder(side).levels.last()
    }

    pub fn mid(&self) -> Option<Price> {
        Some((self.best_bid()? + self.best_ask()?) / 2)
    }

    /// Levels of one side, best first.
    pub fn levels(&self, side: Side) -> impl Iterator<Item = &Level> {
        self.ladder(side).levels.iter().rev()
    }

    pub fn level_qty(&self, side: Side, price: Price) -> u64 {
        let ladder = self.ladder(side);
        ladder.find(price).map_or(0, |i| ladder.levels[i].qty)
    }

    pub fn order(&self, id: OrderId) -> Option<&RestingOrder> {
        self.orders.get(&id)
    }

    pub fn order_count(&self) -> usize {
        self.orders.len()
    }

    /// The `seq` the next added order will get. Every order on the book now has a
    /// smaller one; every later order a greater or equal one.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hft_data::synthetic::{SyntheticConfig, generate};

    #[test]
    fn levels_track_adds_cancels_and_fills() {
        let mut b = Book::new();
        assert_eq!(b.apply(&MboEvent::add(1, Side::Bid, 1, 100, 10)), Some(0));
        assert_eq!(b.apply(&MboEvent::add(2, Side::Bid, 2, 101, 20)), Some(1));
        assert_eq!(b.apply(&MboEvent::add(3, Side::Bid, 3, 100, 5)), Some(2));
        assert_eq!(b.apply(&MboEvent::add(4, Side::Ask, 4, 103, 7)), Some(3));
        assert_eq!(b.apply(&MboEvent::add(5, Side::Ask, 5, 102, 8)), Some(4));

        assert_eq!(b.best_bid(), Some(101));
        assert_eq!(b.best_ask(), Some(102));
        assert_eq!(
            b.levels(Side::Bid)
                .map(|l| (l.price, l.qty, l.count))
                .collect::<Vec<_>>(),
            [(101, 20, 1), (100, 15, 2)]
        );
        assert_eq!(
            b.levels(Side::Ask).map(|l| l.price).collect::<Vec<_>>(),
            [102, 103]
        );

        // Partial fill keeps the order; full cancel removes it and the level.
        assert_eq!(b.apply(&MboEvent::fill(6, Side::Bid, 2, 101, 5)), Some(1));
        assert_eq!(b.level_qty(Side::Bid, 101), 15);
        assert_eq!(
            b.apply(&MboEvent::cancel(7, Side::Bid, 2, 101, 15)),
            Some(1)
        );
        assert_eq!(b.best_bid(), Some(100));
        assert!(b.order(2).is_none());

        // Over-sized cancel is clamped to what is left.
        b.apply(&MboEvent::cancel(8, Side::Bid, 1, 100, 999));
        assert_eq!(
            b.levels(Side::Bid)
                .map(|l| (l.price, l.qty, l.count))
                .collect::<Vec<_>>(),
            [(100, 5, 1)]
        );

        assert_eq!(b.apply(&MboEvent::cancel(9, Side::Bid, 42, 100, 1)), None);
        assert_eq!(b.mid(), Some(101));
        assert_eq!(b.next_seq(), 5);
    }

    #[test]
    fn synthetic_stream_is_consistent() {
        let cfg = SyntheticConfig {
            events: 200_000,
            ..Default::default()
        };
        let mut b = Book::new();
        generate(&cfg, |ev| {
            let touched = b.apply(&ev);
            if matches!(ev.action(), Action::Cancel | Action::Fill) {
                assert!(touched.is_some(), "event for unknown order: {ev:?}");
            }
            if let (Some(bid), Some(ask)) = (b.best_bid(), b.best_ask()) {
                assert!(bid < ask, "crossed book {bid} >= {ask}");
            }
            Ok(())
        })
        .unwrap();
        assert!(b.order_count() > 0);
    }
}
