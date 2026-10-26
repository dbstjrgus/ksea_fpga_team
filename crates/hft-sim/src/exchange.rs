//! Matching of strategy orders against replayed history.
//!
//! History is never altered by our orders (no market impact). We only take fills the
//! recorded order flow would have given us at our price and queue position:
//!
//! * Marketable on arrival: take visible liquidity from the exchange book, best first.
//! * Resting: we join the back of our level. Historical orders with a lower `seq` are
//!   ahead of us. We fill when an execution hits an order *behind* us at our price
//!   (FIFO means everything ahead is gone), when an execution prints through our price,
//!   when hidden liquidity at our price trades (displayed has priority), or when an
//!   incoming order would have crossed us instead of resting.

use hft_book::Book;
use hft_data::{Action, MboEvent, Price, Qty, Side, flags};

use crate::strategy::{ClientOrderId, Liquidity, OrderUpdate, Request, TimeInForce};

/// A strategy order resting at the simulated exchange.
#[derive(Clone, Copy, Debug)]
pub struct SimOrder {
    pub id: ClientOrderId,
    pub side: Side,
    pub price: Price,
    pub leaves: Qty,
    /// Historical orders with a lower `seq` were ahead of us on arrival.
    pub seq: u64,
    /// Visible quantity still ahead of us at our price.
    pub qty_ahead: u64,
}

/// Would an order on `side` at `price` trade with an opposite order at `other`?
#[inline]
fn crosses(side: Side, price: Price, other: Price) -> bool {
    match side {
        Side::Bid => other <= price,
        Side::Ask => other >= price,
    }
}

#[inline]
fn fill(o: &mut SimOrder, avail: &mut Qty, liquidity: Liquidity, out: &mut Vec<OrderUpdate>) {
    let q = o.leaves.min(*avail);
    if q == 0 {
        return;
    }
    o.leaves -= q;
    o.qty_ahead = 0;
    *avail -= q;
    out.push(OrderUpdate::Filled {
        id: o.id,
        side: o.side,
        price: o.price,
        qty: q,
        leaves: o.leaves,
        liquidity,
    });
}

#[derive(Debug, Default)]
pub struct Exchange {
    /// In arrival order, which is also queue priority among our own orders at a price.
    orders: Vec<SimOrder>,
}

impl Exchange {
    pub fn resting(&self) -> &[SimOrder] {
        &self.orders
    }

    /// Handles an order message arriving at the exchange.
    pub fn on_request(&mut self, book: &Book, req: Request, out: &mut Vec<OrderUpdate>) {
        match req {
            Request::New {
                id,
                side,
                price,
                qty,
                tif,
            } => {
                out.push(OrderUpdate::Accepted { id });
                let mut leaves = qty;
                for level in book.levels(side.opposite()) {
                    if leaves == 0 || !crosses(side, price, level.price) {
                        break;
                    }
                    let q = (leaves as u64).min(level.qty) as Qty;
                    leaves -= q;
                    out.push(OrderUpdate::Filled {
                        id,
                        side,
                        price: level.price,
                        qty: q,
                        leaves,
                        liquidity: Liquidity::Taker,
                    });
                }
                if leaves == 0 {
                    return;
                }
                match tif {
                    TimeInForce::Ioc => out.push(OrderUpdate::Canceled { id, qty: leaves }),
                    TimeInForce::Gtc => self.orders.push(SimOrder {
                        id,
                        side,
                        price,
                        leaves,
                        seq: book.next_seq(),
                        qty_ahead: book.level_qty(side, price),
                    }),
                }
            }
            Request::Cancel { id } => match self.orders.iter().position(|o| o.id == id) {
                Some(i) => {
                    let o = self.orders.remove(i);
                    out.push(OrderUpdate::Canceled { id, qty: o.leaves });
                }
                None => out.push(OrderUpdate::CancelRejected { id }),
            },
        }
    }

    /// Call after `ev` has been applied to the exchange book. `hist_seq` is what
    /// [`Book::apply`] returned for it.
    pub fn on_market_event(
        &mut self,
        ev: &MboEvent,
        hist_seq: Option<u64>,
        out: &mut Vec<OrderUpdate>,
    ) {
        if self.orders.is_empty() {
            return;
        }
        let action = ev.action();
        let ev_side = ev.side();
        let mut avail = ev.qty;
        match action {
            Action::Add => {
                // An incoming order that would have traded with us instead of resting.
                for o in self.orders.iter_mut().filter(|o| o.side != ev_side) {
                    if crosses(ev_side, ev.price, o.price) {
                        fill(o, &mut avail, Liquidity::Maker, out);
                    }
                }
            }
            Action::Cancel => {
                let Some(seq) = hist_seq else { return };
                for o in self.orders.iter_mut() {
                    if o.side == ev_side && o.price == ev.price && seq < o.seq {
                        o.qty_ahead = o.qty_ahead.saturating_sub(ev.qty as u64);
                    }
                }
            }
            Action::Fill | Action::Trade => {
                if action == Action::Trade && ev.flags & flags::SIDE_UNKNOWN != 0 {
                    return;
                }
                for o in self.orders.iter_mut().filter(|o| o.side == ev_side) {
                    if ev.price == o.price {
                        let behind = match (action, hist_seq) {
                            (Action::Trade, _) => true,
                            (_, Some(seq)) => seq >= o.seq,
                            (_, None) => false,
                        };
                        if behind {
                            fill(o, &mut avail, Liquidity::Maker, out);
                        } else {
                            o.qty_ahead = o.qty_ahead.saturating_sub(ev.qty as u64);
                        }
                    } else if crosses(ev_side.opposite(), ev.price, o.price) {
                        // Printed through our price: the aggressor would have hit us first.
                        fill(o, &mut avail, Liquidity::Maker, out);
                    }
                }
            }
            Action::Status => return,
        }
        self.orders.retain(|o| o.leaves > 0);
    }
}
