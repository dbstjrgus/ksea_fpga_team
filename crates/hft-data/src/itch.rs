//! Nasdaq TotalView-ITCH 5.0 → normalized [`MboEvent`]s.
//!
//! Input is the BinaryFILE framing Nasdaq distributes: each message is preceded by a
//! 2-byte big-endian length. Only messages for the requested symbols are emitted.
//! The normalizer tracks live orders so every emitted event carries side and price.

use std::io::{self, Read};

use rustc_hash::FxHashMap;

use crate::event::{Action, MboEvent, OrderId, Price, Qty, Side, Ts, flags};
use crate::store::pad_symbol;

const UNWANTED: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct LiveOrder {
    out: u32,
    side: Side,
    price: Price,
    qty: Qty,
}

pub struct ItchNormalizer {
    symbols: Vec<[u8; 8]>,
    /// Output index per stock locate code, or `UNWANTED`.
    locate_to_out: Vec<u32>,
    orders: FxHashMap<OrderId, LiveOrder>,
    pub messages: u64,
}

#[inline]
fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

#[inline]
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

#[inline]
fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// 6-byte nanoseconds-since-midnight timestamp at offset 5.
#[inline]
fn timestamp(b: &[u8]) -> Ts {
    b[5..11].iter().fold(0, |acc, &x| (acc << 8) | x as u64)
}

impl ItchNormalizer {
    /// `symbols[i]` is written to output index `i`.
    pub fn new(symbols: &[impl AsRef<str>]) -> Self {
        ItchNormalizer {
            symbols: symbols.iter().map(|s| pad_symbol(s.as_ref())).collect(),
            locate_to_out: vec![UNWANTED; 1 << 16],
            orders: FxHashMap::default(),
            messages: 0,
        }
    }

    #[inline]
    fn out_for(&self, msg: &[u8]) -> Option<u32> {
        let out = self.locate_to_out[be16(msg, 1) as usize];
        (out != UNWANTED).then_some(out)
    }

    /// Handles one unframed message.
    pub fn process(
        &mut self,
        msg: &[u8],
        emit: &mut impl FnMut(usize, MboEvent) -> io::Result<()>,
    ) -> io::Result<()> {
        let need = match msg.first() {
            Some(b'S') => 12,
            Some(b'R') => 39,
            Some(b'A') => 36,
            Some(b'F') => 40,
            Some(b'E') => 31,
            Some(b'C') => 36,
            Some(b'X') => 23,
            Some(b'D') => 19,
            Some(b'U') => 35,
            Some(b'P') => 44,
            _ => return Ok(()),
        };
        if msg.len() < need {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated ITCH message",
            ));
        }
        let ts = timestamp(msg);
        match msg[0] {
            b'S' => {
                for out in 0..self.symbols.len() {
                    emit(out, MboEvent::status(ts, msg[11]))?;
                }
            }
            b'R' => {
                let stock: [u8; 8] = msg[11..19].try_into().unwrap();
                if let Some(out) = self.symbols.iter().position(|s| *s == stock) {
                    self.locate_to_out[be16(msg, 1) as usize] = out as u32;
                }
            }
            b'A' | b'F' => {
                let Some(out) = self.out_for(msg) else {
                    return Ok(());
                };
                let id = be64(msg, 11);
                let side = Side::from_u8(msg[19]);
                let qty = be32(msg, 20);
                let price = be32(msg, 32) as Price;
                self.orders.insert(
                    id,
                    LiveOrder {
                        out,
                        side,
                        price,
                        qty,
                    },
                );
                emit(out as usize, MboEvent::add(ts, side, id, price, qty))?;
            }
            // Order lookups below need no locate filter: only wanted orders are tracked.
            b'E' | b'C' => self.reduce(be64(msg, 11), be32(msg, 19), ts, Action::Fill, emit)?,
            b'X' => self.reduce(be64(msg, 11), be32(msg, 19), ts, Action::Cancel, emit)?,
            b'D' => self.reduce(be64(msg, 11), Qty::MAX, ts, Action::Cancel, emit)?,
            b'U' => {
                // Replace loses queue priority, so it is exactly a delete plus an add.
                let orig = be64(msg, 11);
                let Some(old) = self.orders.remove(&orig) else {
                    return Ok(());
                };
                let new = be64(msg, 19);
                let qty = be32(msg, 27);
                let price = be32(msg, 31) as Price;
                emit(
                    old.out as usize,
                    MboEvent::cancel(ts, old.side, orig, old.price, old.qty),
                )?;
                self.orders.insert(new, LiveOrder { price, qty, ..old });
                emit(
                    old.out as usize,
                    MboEvent::add(ts, old.side, new, price, qty),
                )?;
            }
            b'P' => {
                let Some(out) = self.out_for(msg) else {
                    return Ok(());
                };
                let mut ev = MboEvent::trade(
                    ts,
                    Side::from_u8(msg[19]),
                    be32(msg, 32) as Price,
                    be32(msg, 20),
                );
                ev.flags |= flags::SIDE_UNKNOWN;
                emit(out as usize, ev)?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn reduce(
        &mut self,
        id: OrderId,
        qty: Qty,
        ts: Ts,
        action: Action,
        emit: &mut impl FnMut(usize, MboEvent) -> io::Result<()>,
    ) -> io::Result<()> {
        let Some(o) = self.orders.get_mut(&id) else {
            return Ok(());
        };
        let q = qty.min(o.qty);
        let ev = MboEvent::new(ts, action, o.side, id, o.price, q);
        let out = o.out as usize;
        o.qty -= q;
        if o.qty == 0 {
            self.orders.remove(&id);
        }
        emit(out, ev)
    }
}

/// Reads length-prefixed ITCH messages until EOF.
pub fn normalize_stream(
    mut input: impl Read,
    norm: &mut ItchNormalizer,
    emit: &mut impl FnMut(usize, MboEvent) -> io::Result<()>,
) -> io::Result<()> {
    let mut len_buf = [0u8; 2];
    let mut buf = [0u8; 256];
    loop {
        match input.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let len = u16::from_be_bytes(len_buf) as usize;
        if len == 0 || len > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad ITCH frame length {len}"),
            ));
        }
        input.read_exact(&mut buf[..len])?;
        norm.process(&buf[..len], emit)?;
        norm.messages += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: u8, locate: u16, ts: u64, len: usize) -> Vec<u8> {
        let mut m = vec![0u8; len];
        m[0] = kind;
        m[1..3].copy_from_slice(&locate.to_be_bytes());
        m[5..11].copy_from_slice(&ts.to_be_bytes()[2..]);
        m
    }

    fn directory(locate: u16, stock: &str) -> Vec<u8> {
        let mut m = header(b'R', locate, 0, 39);
        m[11..19].copy_from_slice(&pad_symbol(stock));
        m
    }

    fn add(locate: u16, ts: u64, id: u64, side: u8, qty: u32, price: u32) -> Vec<u8> {
        let mut m = header(b'A', locate, ts, 36);
        m[11..19].copy_from_slice(&id.to_be_bytes());
        m[19] = side;
        m[20..24].copy_from_slice(&qty.to_be_bytes());
        m[32..36].copy_from_slice(&price.to_be_bytes());
        m
    }

    fn with_ref_qty(kind: u8, len: usize, locate: u16, ts: u64, id: u64, qty: u32) -> Vec<u8> {
        let mut m = header(kind, locate, ts, len);
        m[11..19].copy_from_slice(&id.to_be_bytes());
        if len >= 23 {
            m[19..23].copy_from_slice(&qty.to_be_bytes());
        }
        m
    }

    fn replace(locate: u16, ts: u64, orig: u64, new: u64, qty: u32, price: u32) -> Vec<u8> {
        let mut m = header(b'U', locate, ts, 35);
        m[11..19].copy_from_slice(&orig.to_be_bytes());
        m[19..27].copy_from_slice(&new.to_be_bytes());
        m[27..31].copy_from_slice(&qty.to_be_bytes());
        m[31..35].copy_from_slice(&price.to_be_bytes());
        m
    }

    fn frame(msgs: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for m in msgs {
            out.extend_from_slice(&(m.len() as u16).to_be_bytes());
            out.extend_from_slice(m);
        }
        out
    }

    #[test]
    fn normalizes_order_lifecycle_for_wanted_symbol_only() {
        let stream = frame(&[
            directory(1, "AAPL"),
            directory(2, "MSFT"),
            add(1, 10, 100, b'B', 300, 1_500_000),
            add(2, 11, 200, b'S', 50, 3_000_000), // MSFT: filtered out
            with_ref_qty(b'E', 31, 1, 12, 100, 100),
            with_ref_qty(b'X', 23, 1, 13, 100, 50),
            replace(1, 14, 100, 101, 80, 1_500_100),
            with_ref_qty(b'D', 19, 1, 15, 101, 0),
            with_ref_qty(b'D', 19, 2, 16, 200, 0),
        ]);
        let mut norm = ItchNormalizer::new(&["AAPL"]);
        let mut got = Vec::new();
        normalize_stream(&stream[..], &mut norm, &mut |out, ev| {
            got.push((out, ev));
            Ok(())
        })
        .unwrap();

        let expected = [
            MboEvent::add(10, Side::Bid, 100, 1_500_000, 300),
            MboEvent::fill(12, Side::Bid, 100, 1_500_000, 100),
            MboEvent::cancel(13, Side::Bid, 100, 1_500_000, 50),
            MboEvent::cancel(14, Side::Bid, 100, 1_500_000, 150),
            MboEvent::add(14, Side::Bid, 101, 1_500_100, 80),
            MboEvent::cancel(15, Side::Bid, 101, 1_500_100, 80),
        ];
        assert_eq!(norm.messages, 9);
        assert!(got.iter().all(|(out, _)| *out == 0));
        assert_eq!(got.iter().map(|(_, ev)| *ev).collect::<Vec<_>>(), expected);
    }
}
