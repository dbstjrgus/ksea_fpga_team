use std::collections::VecDeque;
use std::time::Instant;

use hft_book::Book;
use hft_data::{MboEvent, Ts};

use crate::exchange::Exchange;
use crate::latency::{ComputeLatency, Latency, LatencyConfig};
use crate::report::{FillRecord, Report};
use crate::strategy::{Ctx, Idle, OrderUpdate, Path, Request, Strategy};

/// Per-share fees in price units (1/10_000 dollar). Negative means a rebate.
#[derive(Clone, Copy, Debug)]
pub struct Fees {
    pub maker_per_share: i64,
    pub taker_per_share: i64,
}

impl Default for Fees {
    /// Typical Nasdaq-style maker rebate / taker fee: -$0.0020 / +$0.0030 per share.
    fn default() -> Self {
        Fees {
            maker_per_share: -20,
            taker_per_share: 30,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SimConfig {
    pub latency: LatencyConfig,
    pub fees: Fees,
}

/// A hybrid system: FPGA logic on the fast path and the software that steers it on the
/// slow path. Each has its own feed, order session and compute model.
#[derive(Clone, Debug)]
pub struct HybridConfig {
    pub fast: LatencyConfig,
    pub slow: LatencyConfig,
    /// One-way delay of messages between the paths, e.g. a PCIe write or DMA.
    pub link: Latency,
    pub fees: Fees,
}

impl Default for HybridConfig {
    /// Default network delays on both paths, FPGA logic on the fast one and a 500ns link.
    fn default() -> Self {
        HybridConfig {
            fast: LatencyConfig::fpga(),
            slow: LatencyConfig::default(),
            link: Latency::Constant(500),
            fees: Fees::default(),
        }
    }
}

/// An in-order link: delivery times never go backwards, like a TCP session or a
/// multicast feed, even when the sampled delay does.
struct Link<T> {
    q: VecDeque<(Ts, T)>,
    last: Ts,
}

impl<T> Link<T> {
    fn new() -> Self {
        Link {
            q: VecDeque::with_capacity(1024),
            last: 0,
        }
    }

    #[inline]
    fn push(&mut self, ts: Ts, item: T) {
        let ts = ts.max(self.last);
        self.last = ts;
        self.q.push_back((ts, item));
    }

    #[inline]
    fn peek_ts(&self) -> Ts {
        self.q.front().map_or(Ts::MAX, |x| x.0)
    }

    #[inline]
    fn pop(&mut self) -> (Ts, T) {
        self.q.pop_front().unwrap()
    }
}

/// One strategy path: its delayed view of the market and its links to the exchange
/// and to the other path.
struct Unit<M> {
    lat: LatencyConfig,
    /// False for the missing slow path of a single-path run.
    active: bool,
    book: Book,
    ctx: Ctx<M>,
    /// Market data: event index, delivered after feed latency.
    md: Link<usize>,
    /// Exchange responses in flight to this path.
    resp: Link<OrderUpdate>,
    /// Messages from the other path, with the exchange time of the event behind them.
    inbox: Link<(M, Option<Ts>)>,
    /// Orders in flight to the exchange, with the exchange time of the event that triggered them.
    to_exch: Link<(Request, Option<Ts>)>,
    /// A path handles one callback (or fills one pipeline slot) at a time; later
    /// deliveries wait for it.
    free_at: Ts,
}

impl<M> Unit<M> {
    fn new(path: Path, lat: LatencyConfig, active: bool) -> Self {
        Unit {
            lat,
            active,
            book: Book::new(),
            ctx: Ctx::new(path),
            md: Link::new(),
            resp: Link::new(),
            inbox: Link::new(),
            to_exch: Link::new(),
            free_at: 0,
        }
    }

    /// When this path's next callback can start.
    #[inline]
    fn next_callback(&self) -> Ts {
        let t = self
            .md
            .peek_ts()
            .min(self.resp.peek_ts())
            .min(self.inbox.peek_ts());
        if t == Ts::MAX {
            Ts::MAX
        } else {
            t.max(self.free_at)
        }
    }
}

/// Replays `events` (sorted by `ts_exch`) through `strategy`. The run ends when history
/// does; anything still in flight is dropped and the position is marked to the final mid.
pub fn run<S: Strategy>(events: &[MboEvent], strategy: &mut S, cfg: SimConfig) -> Report {
    let SimConfig { latency, fees } = cfg;
    simulate(
        events,
        strategy,
        &mut Idle,
        latency,
        None,
        Latency::Constant(0),
        fees,
    )
}

/// Replays `events` through a hybrid system: `fast` models the FPGA logic and `slow` the
/// software that steers it. Each sees the market through its own feed and trades through
/// its own order session; they talk through [`Ctx::post`] with `cfg.link` delay.
pub fn run_hybrid<M, F: Strategy<M>, S: Strategy<M>>(
    events: &[MboEvent],
    fast: &mut F,
    slow: &mut S,
    cfg: HybridConfig,
) -> Report {
    simulate(
        events,
        fast,
        slow,
        cfg.fast,
        Some(cfg.slow),
        cfg.link,
        cfg.fees,
    )
}

fn simulate<M, F: Strategy<M>, S: Strategy<M>>(
    events: &[MboEvent],
    fast: &mut F,
    slow: &mut S,
    fast_latency: LatencyConfig,
    slow_latency: Option<LatencyConfig>,
    mut link: Latency,
    fees: Fees,
) -> Report {
    let started = Instant::now();
    let mut report = Report::default();

    let mut exch_book = Book::new();
    let mut exchange = Exchange::default();
    let mut updates: Vec<OrderUpdate> = Vec::with_capacity(16);
    let has_slow = slow_latency.is_some();
    let mut units = [
        Unit::new(Path::Fast, fast_latency, true),
        Unit::new(Path::Slow, slow_latency.unwrap_or_default(), has_slow),
    ];
    let mut last_mid = None;

    let mut i = 0;
    while i < events.len() {
        let t_hist = events[i].ts_exch;
        let t_req = [units[0].to_exch.peek_ts(), units[1].to_exch.peek_ts()];
        let t_cb = [units[0].next_callback(), units[1].next_callback()];
        let req = (t_req[1] < t_req[0]) as usize;
        let cb = (t_cb[1] < t_cb[0]) as usize;

        // Ties go to history, then to arriving orders, then to the fast path: our order
        // never beats a historical event stamped at the same nanosecond.
        if t_hist <= t_req[req] && t_hist <= t_cb[cb] {
            let ev = &events[i];
            let seq = exch_book.apply(ev);
            let mid = exch_book.mid();
            if mid != last_mid {
                last_mid = mid;
                if let Some(mid) = mid {
                    report.mids.push((ev.ts_exch, mid));
                }
            }
            exchange.on_market_event(ev, seq, &mut updates);
            dispatch(&mut updates, ev.ts_exch, &mut units, &mut report, &fees);
            for u in units.iter_mut().filter(|u| u.active) {
                u.md.push(ev.ts_recv + u.lat.feed.sample(), i);
            }
            i += 1;
        } else if t_req[req] <= t_cb[cb] {
            let (t, (r, trigger)) = units[req].to_exch.pop();
            if let Some(trigger) = trigger {
                let samples = if req == 0 {
                    &mut report.tick_to_order_ns
                } else {
                    &mut report.slow_tick_to_order_ns
                };
                samples.push(t - trigger);
            }
            exchange.on_request(&exch_book, r, &mut updates);
            dispatch(&mut updates, t, &mut units, &mut report, &fees);
        } else if cb == 0 {
            callback(events, &mut units, Path::Fast, fast, &mut link, &mut report);
        } else {
            callback(events, &mut units, Path::Slow, slow, &mut link, &mut report);
        }
    }

    report.events = events.len() as u64;
    report.mark_price = exch_book.mid().or(report.fills.last().map(|f| f.price));
    report.wall_time = started.elapsed();
    report
}

/// Runs `path`'s next callback on whichever of its inputs is due first, then sends what
/// the strategy queued once its compute time has passed.
#[inline]
fn callback<M, S: Strategy<M>>(
    events: &[MboEvent],
    units: &mut [Unit<M>; 2],
    path: Path,
    strategy: &mut S,
    link: &mut Latency,
    report: &mut Report,
) {
    let [fast, slow] = units;
    let (u, other) = match path {
        Path::Fast => (fast, slow),
        Path::Slow => (slow, fast),
    };
    let t = u.next_callback();
    u.ctx.now = t;
    let wall = matches!(u.lat.compute, ComputeLatency::Measured).then(Instant::now);
    // Responses win ties, then messages from the other path: a fill is more urgent than
    // new parameters, and both are more urgent than a book update.
    let (t_resp, t_msg, t_md) = (u.resp.peek_ts(), u.inbox.peek_ts(), u.md.peek_ts());
    let trigger = if t_resp <= t_msg && t_resp <= t_md {
        let (_, update) = u.resp.pop();
        if let OrderUpdate::Filled { side, qty, .. } = update {
            u.ctx.position += side.sign() * qty as i64;
        }
        strategy.on_order_update(&mut u.ctx, &u.book, &update);
        None
    } else if t_msg <= t_md {
        let (_, (msg, trigger)) = u.inbox.pop();
        if path == Path::Fast
            && let Some(trigger) = trigger
        {
            report.tick_to_param_ns.push(t - trigger);
        }
        strategy.on_message(&mut u.ctx, &u.book, msg);
        trigger
    } else {
        let (_, idx) = u.md.pop();
        let ev = &events[idx];
        u.book.apply(ev);
        strategy.on_market_data(&mut u.ctx, &u.book, ev);
        Some(ev.ts_exch)
    };
    // How long until the path takes its next input, and until its output leaves.
    let (busy, done) = match &mut u.lat.compute {
        ComputeLatency::Fixed(ns) => (*ns, *ns),
        ComputeLatency::Sampled(l) => {
            let ns = l.sample();
            (ns, ns)
        }
        ComputeLatency::Measured => {
            let ns = wall.unwrap().elapsed().as_nanos() as u64;
            (ns, ns)
        }
        ComputeLatency::Pipelined { latency, interval } => (*interval, *latency),
    };
    u.free_at = t + busy;
    let done = t + done;
    report.callbacks += 1;
    for req in u.ctx.requests.drain(..) {
        match req {
            Request::New { qty, .. } => {
                report.orders_sent += 1;
                report.shares_sent += qty as u64;
            }
            Request::Cancel { .. } => report.cancels_sent += 1,
        }
        u.to_exch
            .push(done + u.lat.order_entry.sample(), (req, trigger));
    }
    for msg in u.ctx.outbox.drain(..) {
        if other.active {
            other.inbox.push(done + link.sample(), (msg, trigger));
        }
    }
}

/// Accounts for exchange-side fills and sends every update back to the path that owns
/// the order.
#[inline]
fn dispatch<M>(
    updates: &mut Vec<OrderUpdate>,
    ts: Ts,
    units: &mut [Unit<M>; 2],
    report: &mut Report,
    fees: &Fees,
) {
    for update in updates.drain(..) {
        if let OrderUpdate::Filled {
            id,
            side,
            price,
            qty,
            liquidity,
            ..
        } = update
        {
            report.record_fill(
                FillRecord {
                    ts,
                    id,
                    side,
                    price,
                    qty,
                    liquidity,
                    fee: 0,
                },
                fees,
            );
        }
        let u = &mut units[Path::of(update.id()) as usize];
        u.resp.push(ts + u.lat.order_response.sample(), update);
    }
}
