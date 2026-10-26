use hft_book::Book;
use hft_data::{MboEvent, Price, Qty, Side};
use hft_sim::{ClientOrderId, Ctx, OrderUpdate, Strategy, TimeInForce};

#[derive(Clone, Copy, Debug)]
pub struct HybridTakerConfig {
    /// Take when the opposite touch is at least this far through fair value (price units).
    pub edge: Price,
    /// Time constant of the fair value's moving average of the mid.
    pub tau_ns: u64,
    /// Price increment thresholds are rounded to (100 = 1 cent).
    pub tick: Price,
    pub qty: Qty,
    pub max_position: i64,
}

/// What the two paths say to each other over the link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TakerMsg {
    /// Software → FPGA: arm both sides. Buy if the best ask is at or below `buy_at`,
    /// sell if the best bid is at or above `sell_at`.
    Arm { buy_at: Price, sell_at: Price },
    /// FPGA → software: an order on `side` is finished and that side is disarmed.
    Done { side: Side, filled: Qty },
}

/// The hardware path: compares the touch with two armed thresholds on every message and
/// fires an IOC the moment one is crossed. Each side fires once, then waits for software
/// to re-arm it, as a hardware safety interlock would. Enforces the position limit itself.
pub struct ArmedTaker {
    cfg: HybridTakerConfig,
    /// [buy, sell] thresholds; `None` while disarmed.
    armed: [Option<Price>; 2],
    /// [buy, sell] order in flight and how much of it has filled.
    live: [Option<(ClientOrderId, Qty)>; 2],
}

/// The software path: tracks a fair value and re-arms the FPGA whenever the thresholds
/// move or the FPGA reports a side as done.
pub struct FairValueArmer {
    cfg: HybridTakerConfig,
    fair: Option<(u64, f64)>,
    last: Option<(Price, Price)>,
}

/// The two halves of the example hybrid taker.
///
/// A baseline to exercise the hybrid path, not a strategy expected to make money.
pub fn hybrid_taker(cfg: HybridTakerConfig) -> (ArmedTaker, FairValueArmer) {
    (
        ArmedTaker {
            cfg,
            armed: [None; 2],
            live: [None; 2],
        },
        FairValueArmer {
            cfg,
            fair: None,
            last: None,
        },
    )
}

fn slot(side: Side) -> usize {
    (side == Side::Ask) as usize
}

impl ArmedTaker {
    /// Fires on `side` if it is armed, has no order out, and `crossed` holds.
    fn fire(&mut self, ctx: &mut Ctx<TakerMsg>, side: Side, crossed: impl Fn(Price) -> bool) {
        let k = slot(side);
        let pos = ctx.position() + side.sign() * self.cfg.qty as i64;
        if let Some(limit) = self.armed[k]
            && self.live[k].is_none()
            && crossed(limit)
            && pos.abs() <= self.cfg.max_position
        {
            let id = ctx.send(side, limit, self.cfg.qty, TimeInForce::Ioc);
            self.armed[k] = None;
            self.live[k] = Some((id, 0));
        }
    }
}

impl Strategy<TakerMsg> for ArmedTaker {
    fn on_market_data(&mut self, ctx: &mut Ctx<TakerMsg>, book: &Book, _: &MboEvent) {
        if let Some(ask) = book.best_ask() {
            self.fire(ctx, Side::Bid, |buy_at| ask <= buy_at);
        }
        if let Some(bid) = book.best_bid() {
            self.fire(ctx, Side::Ask, |sell_at| bid >= sell_at);
        }
    }

    fn on_order_update(&mut self, ctx: &mut Ctx<TakerMsg>, _: &Book, update: &OrderUpdate) {
        let Some(k) = (0..2).find(|&k| self.live[k].is_some_and(|(id, _)| id == update.id()))
        else {
            return;
        };
        let (id, filled) = self.live[k].unwrap();
        let filled = match *update {
            OrderUpdate::Filled { qty, leaves: 0, .. } => filled + qty,
            OrderUpdate::Filled { qty, .. } => {
                self.live[k] = Some((id, filled + qty));
                return;
            }
            OrderUpdate::Canceled { .. } => filled,
            _ => return,
        };
        self.live[k] = None;
        let side = if k == 0 { Side::Bid } else { Side::Ask };
        ctx.post(TakerMsg::Done { side, filled });
    }

    fn on_message(&mut self, _: &mut Ctx<TakerMsg>, _: &Book, msg: TakerMsg) {
        if let TakerMsg::Arm { buy_at, sell_at } = msg {
            self.armed = [Some(buy_at), Some(sell_at)];
        }
    }
}

impl FairValueArmer {
    fn arm(&mut self, ctx: &mut Ctx<TakerMsg>, force: bool) {
        let Some((_, fair)) = self.fair else { return };
        let tick = self.cfg.tick as f64;
        let buy_at = ((fair - self.cfg.edge as f64) / tick).floor() as Price * self.cfg.tick;
        let sell_at = ((fair + self.cfg.edge as f64) / tick).ceil() as Price * self.cfg.tick;
        if force || self.last != Some((buy_at, sell_at)) {
            self.last = Some((buy_at, sell_at));
            ctx.post(TakerMsg::Arm { buy_at, sell_at });
        }
    }
}

impl Strategy<TakerMsg> for FairValueArmer {
    fn on_market_data(&mut self, ctx: &mut Ctx<TakerMsg>, book: &Book, ev: &MboEvent) {
        let Some(mid) = book.mid() else { return };
        let mid = mid as f64;
        self.fair = Some(match self.fair {
            None => (ev.ts_exch, mid),
            Some((t, fair)) => {
                let dt = ev.ts_exch.saturating_sub(t) as f64;
                let w = dt / (dt + self.cfg.tau_ns as f64);
                (ev.ts_exch, fair + w * (mid - fair))
            }
        });
        self.arm(ctx, false);
    }

    fn on_message(&mut self, ctx: &mut Ctx<TakerMsg>, _: &Book, msg: TakerMsg) {
        if let TakerMsg::Done { .. } = msg {
            self.arm(ctx, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hft_data::synthetic::{SyntheticConfig, generate};
    use hft_sim::{HybridConfig, Report, run_hybrid};

    fn run_once(events: &[MboEvent]) -> Report {
        let cfg = HybridTakerConfig {
            edge: 200,
            tau_ns: 1_000_000,
            tick: 100,
            qty: 100,
            max_position: 500,
        };
        let (mut fast, mut slow) = hybrid_taker(cfg);
        run_hybrid(events, &mut fast, &mut slow, HybridConfig::default())
    }

    #[test]
    fn arms_fires_and_respects_the_position_limit() {
        let mut events = Vec::new();
        generate(
            &SyntheticConfig {
                events: 200_000,
                ..Default::default()
            },
            |ev| {
                events.push(ev);
                Ok(())
            },
        )
        .unwrap();
        let a = run_once(&events);
        assert!(!a.fills.is_empty());
        assert!(!a.tick_to_param_ns.is_empty());
        assert!(a.max_abs_position <= 500);
        assert_eq!(a.maker_shares, 0);
        let b = run_once(&events);
        assert_eq!(a.fills, b.fills);
    }
}
