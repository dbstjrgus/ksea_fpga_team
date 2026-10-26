use hft_book::Book;
use hft_data::{MboEvent, Price, Qty, Side, Ts};

pub type ClientOrderId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeInForce {
    /// Rests until filled or canceled.
    Gtc,
    /// Takes what it can on arrival; the rest is canceled.
    Ioc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    New {
        id: ClientOrderId,
        side: Side,
        price: Price,
        qty: Qty,
        tif: TimeInForce,
    },
    Cancel {
        id: ClientOrderId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liquidity {
    Maker,
    Taker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderUpdate {
    Accepted {
        id: ClientOrderId,
    },
    Filled {
        id: ClientOrderId,
        side: Side,
        price: Price,
        qty: Qty,
        leaves: Qty,
        liquidity: Liquidity,
    },
    /// Order removed with `qty` unfilled (explicit cancel or IOC remainder).
    Canceled {
        id: ClientOrderId,
        qty: Qty,
    },
    /// The cancel arrived after the order was already gone.
    CancelRejected {
        id: ClientOrderId,
    },
}

impl OrderUpdate {
    pub fn id(&self) -> ClientOrderId {
        match *self {
            OrderUpdate::Accepted { id }
            | OrderUpdate::Filled { id, .. }
            | OrderUpdate::Canceled { id, .. }
            | OrderUpdate::CancelRejected { id } => id,
        }
    }
}

/// Which side of a hybrid system a strategy runs on. A plain [`crate::run`] uses `Fast`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    /// The hardware (FPGA) path in a hybrid run, or the only strategy otherwise.
    Fast = 0,
    /// The software path in a hybrid run.
    Slow = 1,
}

impl Path {
    const ID_SHIFT: u32 = 56;

    /// The path that sent order `id`. Each path numbers its orders in its own range.
    #[inline]
    pub fn of(id: ClientOrderId) -> Path {
        if id >> Self::ID_SHIFT == 0 {
            Path::Fast
        } else {
            Path::Slow
        }
    }
}

/// The strategy's handle on the simulator during a callback.
///
/// `M` is the message type exchanged with the other path of a hybrid run (see
/// [`Ctx::post`]); single-path strategies leave it as `()`.
pub struct Ctx<M = ()> {
    pub(crate) now: Ts,
    pub(crate) position: i64,
    next_id: ClientOrderId,
    pub(crate) requests: Vec<Request>,
    pub(crate) outbox: Vec<M>,
}

impl<M> Ctx<M> {
    pub(crate) fn new(path: Path) -> Self {
        Ctx {
            now: 0,
            position: 0,
            next_id: ((path as u64) << Path::ID_SHIFT) + 1,
            requests: Vec::with_capacity(16),
            outbox: Vec::new(),
        }
    }

    /// Strategy-local time at the start of this callback.
    pub fn now(&self) -> Ts {
        self.now
    }

    /// Position as known from fills this path has received (not the exchange's truth,
    /// and not the other path's fills).
    pub fn position(&self) -> i64 {
        self.position
    }

    /// Queues a new order; it leaves after this callback's compute time.
    pub fn send(&mut self, side: Side, price: Price, qty: Qty, tif: TimeInForce) -> ClientOrderId {
        let id = self.next_id;
        self.next_id += 1;
        self.requests.push(Request::New {
            id,
            side,
            price,
            qty,
            tif,
        });
        id
    }

    /// Cancels one of this path's orders.
    pub fn cancel(&mut self, id: ClientOrderId) {
        self.requests.push(Request::Cancel { id });
    }

    /// Sends a message to the other path of a hybrid run, e.g. new parameters for the
    /// FPGA or a fill notification for software. It leaves after this callback's compute
    /// time and arrives after the link delay. Dropped in single-path runs.
    pub fn post(&mut self, msg: M) {
        self.outbox.push(msg);
    }
}

/// Implemented by algorithms. The same callbacks can be driven by a live gateway.
///
/// `book` is this path's view: history delayed by its feed latency, without our own orders.
/// `M` is the message type shared with the other path of a hybrid run.
pub trait Strategy<M = ()> {
    fn on_market_data(&mut self, ctx: &mut Ctx<M>, book: &Book, ev: &MboEvent) {
        let _ = (ctx, book, ev);
    }

    /// Responses for this path's own orders only.
    fn on_order_update(&mut self, ctx: &mut Ctx<M>, book: &Book, update: &OrderUpdate) {
        let _ = (ctx, book, update);
    }

    /// A message from the other path of a hybrid run.
    fn on_message(&mut self, ctx: &mut Ctx<M>, book: &Book, msg: M) {
        let _ = (ctx, book, msg);
    }
}

/// Does nothing. Stands in for the missing path of a single-path run.
pub struct Idle;

impl<M> Strategy<M> for Idle {}
