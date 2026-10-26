use bytemuck::{Pod, Zeroable};

/// Price in integer ticks of 1/10_000 dollar (ITCH native precision).
pub type Price = i64;
/// Share quantity.
pub type Qty = u32;
/// Nanoseconds since midnight of the session date.
pub type Ts = u64;
pub type OrderId = u64;

/// Price units per dollar.
pub const PRICE_SCALE: i64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Side {
    Bid = b'B',
    Ask = b'S',
}

impl Side {
    #[inline]
    pub fn from_u8(b: u8) -> Side {
        if b == b'B' { Side::Bid } else { Side::Ask }
    }

    #[inline]
    pub fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }

    /// +1 for buys, -1 for sells.
    #[inline]
    pub fn sign(self) -> i64 {
        match self {
            Side::Bid => 1,
            Side::Ask => -1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Action {
    /// New visible order rests on the book.
    Add = b'A',
    /// Visible order reduced by `qty` (partial cancel or full delete).
    Cancel = b'C',
    /// Visible resting order executed for `qty`.
    Fill = b'F',
    /// Execution against non-displayed liquidity; the visible book is unchanged.
    Trade = b'T',
    /// Session status change; the ITCH system event code is in `flags`.
    Status = b'S',
}

impl Action {
    #[inline]
    pub fn from_u8(b: u8) -> Option<Action> {
        match b {
            b'A' => Some(Action::Add),
            b'C' => Some(Action::Cancel),
            b'F' => Some(Action::Fill),
            b'T' => Some(Action::Trade),
            b'S' => Some(Action::Status),
            _ => None,
        }
    }
}

/// Bits of `MboEvent::flags` for non-status events.
pub mod flags {
    /// The resting side is not known (ITCH reports every hidden trade as a buy).
    pub const SIDE_UNKNOWN: u8 = 1;
}

/// One normalized market-by-order event: 40 bytes, fixed size, so files can be
/// memory-mapped and replayed as a plain slice.
///
/// Every event carries the side and resting price of the order it touches, so
/// consumers never need to look anything up to interpret it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct MboEvent {
    /// Exchange matching-engine timestamp.
    pub ts_exch: Ts,
    /// Capture timestamp. Equal to `ts_exch` when the source has none (e.g. raw ITCH).
    pub ts_recv: Ts,
    pub order_id: OrderId,
    pub price: Price,
    pub qty: Qty,
    /// `Action` as a byte; use [`MboEvent::action`].
    pub action: u8,
    /// `Side` as a byte; use [`MboEvent::side`].
    pub side: u8,
    pub flags: u8,
    pub _pad: [u8; 1],
}

impl MboEvent {
    #[inline]
    pub fn new(
        ts: Ts,
        action: Action,
        side: Side,
        order_id: OrderId,
        price: Price,
        qty: Qty,
    ) -> Self {
        MboEvent {
            ts_exch: ts,
            ts_recv: ts,
            order_id,
            price,
            qty,
            action: action as u8,
            side: side as u8,
            flags: 0,
            _pad: [0],
        }
    }

    pub fn add(ts: Ts, side: Side, id: OrderId, price: Price, qty: Qty) -> Self {
        Self::new(ts, Action::Add, side, id, price, qty)
    }

    pub fn cancel(ts: Ts, side: Side, id: OrderId, price: Price, qty: Qty) -> Self {
        Self::new(ts, Action::Cancel, side, id, price, qty)
    }

    pub fn fill(ts: Ts, side: Side, id: OrderId, price: Price, qty: Qty) -> Self {
        Self::new(ts, Action::Fill, side, id, price, qty)
    }

    pub fn trade(ts: Ts, side: Side, price: Price, qty: Qty) -> Self {
        Self::new(ts, Action::Trade, side, 0, price, qty)
    }

    pub fn status(ts: Ts, code: u8) -> Self {
        let mut ev = Self::new(ts, Action::Status, Side::Bid, 0, 0, 0);
        ev.flags = code;
        ev
    }

    #[inline]
    pub fn action(&self) -> Action {
        Action::from_u8(self.action).expect("corrupt action byte")
    }

    #[inline]
    pub fn side(&self) -> Side {
        Side::from_u8(self.side)
    }
}

const _: () = assert!(std::mem::size_of::<MboEvent>() == 40);
