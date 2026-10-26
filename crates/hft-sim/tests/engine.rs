use hft_book::Book;
use hft_data::{MboEvent, Qty, Side, Ts};
use hft_sim::{
    ComputeLatency, Ctx, HybridConfig, Latency, LatencyConfig, Liquidity, OrderUpdate, Path,
    SimConfig, Strategy, TimeInForce, run, run_hybrid,
};

/// Sends one order on its first market data callback and records every update.
struct OneShot {
    order: Option<(Side, i64, Qty, TimeInForce)>,
    updates: Vec<(Ts, OrderUpdate)>,
}

impl OneShot {
    fn new(side: Side, price: i64, qty: Qty, tif: TimeInForce) -> Self {
        OneShot {
            order: Some((side, price, qty, tif)),
            updates: Vec::new(),
        }
    }

    fn fills(&self) -> Vec<(i64, Qty, Qty, Liquidity)> {
        self.updates
            .iter()
            .filter_map(|(_, u)| match *u {
                OrderUpdate::Filled {
                    price,
                    qty,
                    leaves,
                    liquidity,
                    ..
                } => Some((price, qty, leaves, liquidity)),
                _ => None,
            })
            .collect()
    }
}

impl Strategy for OneShot {
    fn on_market_data(&mut self, ctx: &mut Ctx, _: &Book, _: &MboEvent) {
        if let Some((side, price, qty, tif)) = self.order.take() {
            ctx.send(side, price, qty, tif);
        }
    }

    fn on_order_update(&mut self, ctx: &mut Ctx, _: &Book, update: &OrderUpdate) {
        self.updates.push((ctx.now(), *update));
    }
}

fn latency(feed: u64, entry: u64, response: u64, compute: u64) -> SimConfig {
    SimConfig {
        latency: LatencyConfig {
            feed: Latency::Constant(feed),
            order_entry: Latency::Constant(entry),
            order_response: Latency::Constant(response),
            compute: ComputeLatency::Fixed(compute),
        },
        ..Default::default()
    }
}

#[test]
fn resting_order_fills_only_after_queue_ahead_is_gone() {
    use Side::*;
    let events = [
        MboEvent::add(1_000, Bid, 1, 100, 100),
        // Strategy sees t=1000 at 1010, its order arrives at 1020. This add beats it.
        MboEvent::add(1_015, Bid, 2, 100, 50),
        MboEvent::add(2_000, Bid, 3, 100, 40),    // behind us
        MboEvent::fill(3_000, Bid, 1, 100, 100),  // ahead: we are not filled
        MboEvent::cancel(4_000, Bid, 2, 100, 50), // ahead: we are now first
        MboEvent::fill(5_000, Bid, 3, 100, 30),   // behind us executes ⇒ we were hit first
        MboEvent::add(6_000, Ask, 4, 99, 500),    // would have crossed our bid
        MboEvent::status(7_000, b'M'),
    ];
    let mut s = OneShot::new(Bid, 100, 100, TimeInForce::Gtc);
    let report = run(&events, &mut s, latency(10, 10, 10, 0));

    assert_eq!(
        s.fills(),
        [
            (100, 30, 70, Liquidity::Maker),
            (100, 70, 0, Liquidity::Maker)
        ]
    );
    assert_eq!(
        report.fills.iter().map(|f| f.ts).collect::<Vec<_>>(),
        [5_000, 6_000]
    );
    assert_eq!(report.position, 100);
    assert_eq!(report.maker_shares, 100);
    // Strategy learns of the first fill one response latency later.
    assert!(
        s.updates
            .iter()
            .any(|&(t, u)| t == 5_010 && matches!(u, OrderUpdate::Filled { qty: 30, .. }))
    );
}

#[test]
fn latency_decides_whether_a_take_succeeds() {
    use Side::*;
    let events = [
        MboEvent::add(1_000, Ask, 1, 101, 100),
        MboEvent::add(1_000, Bid, 2, 100, 100),
        MboEvent::cancel(1_005, Ask, 1, 101, 100), // liquidity pulled 5ns later
        MboEvent::status(10_000, b'M'),
    ];

    // Infinitely fast: the strategy reacts to the second event (the bid add at t=1000)
    // and its IOC arrives at t=1000, before the cancel.
    let mut fast = OneShot::new(Bid, 101, 100, TimeInForce::Ioc);
    run(&events, &mut fast, latency(0, 0, 0, 0));
    assert_eq!(fast.fills(), [(101, 100, 0, Liquidity::Taker)]);

    // 10ns of round trip is enough to miss it.
    let mut slow = OneShot::new(Bid, 101, 100, TimeInForce::Ioc);
    let report = run(&events, &mut slow, latency(5, 5, 5, 0));
    assert!(slow.fills().is_empty());
    assert!(
        slow.updates
            .iter()
            .any(|(_, u)| matches!(u, OrderUpdate::Canceled { qty: 100, .. }))
    );
    assert_eq!(report.tick_to_order_ns, [10]);
}

/// Counts callbacks and when they started.
#[derive(Default)]
struct Recorder(Vec<Ts>);

impl Strategy for Recorder {
    fn on_market_data(&mut self, ctx: &mut Ctx, _: &Book, _: &MboEvent) {
        self.0.push(ctx.now());
    }
}

#[test]
fn busy_strategy_falls_behind_the_feed() {
    let events: Vec<_> = (0..5)
        .map(|k| MboEvent::add(1_000 + k, Side::Bid, k + 1, 100, 1))
        .chain([MboEvent::status(1_000_000, b'M')])
        .collect();
    let mut r = Recorder::default();
    run(&events, &mut r, latency(100, 0, 0, 50));
    // Events arrive 1ns apart but each callback takes 50ns. The last event ends the
    // run before its delivery, so it is never seen.
    assert_eq!(r.0, [1_100, 1_150, 1_200, 1_250, 1_300]);
}

/// Joins the touch on both sides; a minimal stand-in for a real strategy.
#[derive(Default)]
struct Joiner {
    bid: Option<u64>,
    ask: Option<u64>,
}

impl Strategy for Joiner {
    fn on_market_data(&mut self, ctx: &mut Ctx, book: &Book, _: &MboEvent) {
        if self.bid.is_none()
            && let Some(p) = book.best_bid()
        {
            self.bid = Some(ctx.send(Side::Bid, p, 100, TimeInForce::Gtc));
        }
        if self.ask.is_none()
            && let Some(p) = book.best_ask()
        {
            self.ask = Some(ctx.send(Side::Ask, p, 100, TimeInForce::Gtc));
        }
    }

    fn on_order_update(&mut self, _: &mut Ctx, _: &Book, update: &OrderUpdate) {
        if let OrderUpdate::Filled { id, leaves: 0, .. } | OrderUpdate::Canceled { id, .. } =
            *update
        {
            if self.bid == Some(id) {
                self.bid = None;
            }
            if self.ask == Some(id) {
                self.ask = None;
            }
        }
    }
}

#[test]
fn runs_are_deterministic_with_jitter() {
    use hft_data::synthetic::{SyntheticConfig, generate};
    let mut events = Vec::new();
    generate(
        &SyntheticConfig {
            events: 100_000,
            ..Default::default()
        },
        |ev| {
            events.push(ev);
            Ok(())
        },
    )
    .unwrap();

    let cfg = || SimConfig {
        latency: LatencyConfig {
            feed: Latency::uniform(2_000, 8_000, 1),
            order_entry: Latency::uniform(2_000, 8_000, 2),
            order_response: Latency::uniform(2_000, 8_000, 3),
            compute: ComputeLatency::Sampled(Latency::uniform(100, 900, 4)),
        },
        ..Default::default()
    };
    let a = run(&events, &mut Joiner::default(), cfg());
    let b = run(&events, &mut Joiner::default(), cfg());
    assert!(!a.fills.is_empty());
    assert_eq!(a.fills, b.fills);
    assert_eq!(a.pnl(), b.pnl());
    assert_eq!(a.tick_to_order_ns, b.tick_to_order_ns);
}

#[test]
fn pnl_attribution_adds_up() {
    use hft_data::synthetic::{SyntheticConfig, generate};
    use hft_sim::{DEFAULT_HORIZONS_NS, Metrics};
    let mut events = Vec::new();
    generate(
        &SyntheticConfig {
            events: 100_000,
            ..Default::default()
        },
        |ev| {
            events.push(ev);
            Ok(())
        },
    )
    .unwrap();
    let r = run(&events, &mut Joiner::default(), SimConfig::default());
    let m = Metrics::compute(&r, &DEFAULT_HORIZONS_NS, 10_000_000);
    assert!(!r.fills.is_empty());
    assert!((m.spread_capture + m.post_fill_moves - m.fees - m.net_pnl).abs() < 1e-6);
    // The equity curve ends at the same marked PnL.
    assert!((m.bucket_pnl.iter().sum::<f64>() - m.net_pnl).abs() < 1e-6);
}

/// Sends an IOC on every market data callback.
#[derive(Default)]
struct Burst(Vec<Ts>);

impl Strategy for Burst {
    fn on_market_data(&mut self, ctx: &mut Ctx, _: &Book, _: &MboEvent) {
        self.0.push(ctx.now());
        ctx.send(Side::Bid, 1, 1, TimeInForce::Ioc);
    }
}

#[test]
fn pipelined_logic_keeps_up_with_a_burst() {
    let events: Vec<_> = (0..5)
        .map(|k| MboEvent::add(1_000 + k, Side::Bid, k + 1, 100, 1))
        .chain([MboEvent::status(1_000_000, b'M')])
        .collect();

    // A CPU busy 50ns per callback falls further behind with every event.
    let mut cpu = Burst::default();
    let report = run(&events, &mut cpu, latency(100, 0, 0, 50));
    assert_eq!(report.tick_to_order_ns, [150, 199, 248, 297, 346]);

    // A pipeline with the same 50ns latency takes a new message every 1ns.
    let mut cfg = latency(100, 0, 0, 0);
    cfg.latency.compute = ComputeLatency::Pipelined {
        latency: 50,
        interval: 1,
    };
    let mut fpga = Burst::default();
    let report = run(&events, &mut fpga, cfg);
    assert_eq!(fpga.0, [1_100, 1_101, 1_102, 1_103, 1_104]);
    assert_eq!(report.tick_to_order_ns, [150; 5]);
}

/// Hybrid software side: on its first callback, arms the FPGA with a buy limit and
/// sends a non-marketable IOC of its own.
#[derive(Default)]
struct Arming {
    sent: bool,
    updates: Vec<(Ts, OrderUpdate)>,
}

impl Strategy<i64> for Arming {
    fn on_market_data(&mut self, ctx: &mut Ctx<i64>, _: &Book, _: &MboEvent) {
        if !std::mem::replace(&mut self.sent, true) {
            ctx.post(102);
            ctx.send(Side::Bid, 99, 10, TimeInForce::Ioc);
        }
    }

    fn on_order_update(&mut self, ctx: &mut Ctx<i64>, _: &Book, update: &OrderUpdate) {
        self.updates.push((ctx.now(), *update));
    }
}

/// Hybrid FPGA side: buys once when the best ask is at or below the armed limit.
#[derive(Default)]
struct Trigger {
    limit: Option<i64>,
    armed_at: Option<Ts>,
    updates: Vec<(Ts, OrderUpdate)>,
}

impl Strategy<i64> for Trigger {
    fn on_market_data(&mut self, ctx: &mut Ctx<i64>, book: &Book, _: &MboEvent) {
        if let (Some(limit), Some(ask)) = (self.limit, book.best_ask())
            && ask <= limit
        {
            ctx.send(Side::Bid, limit, 50, TimeInForce::Ioc);
            self.limit = None;
        }
    }

    fn on_order_update(&mut self, ctx: &mut Ctx<i64>, _: &Book, update: &OrderUpdate) {
        self.updates.push((ctx.now(), *update));
    }

    fn on_message(&mut self, ctx: &mut Ctx<i64>, _: &Book, limit: i64) {
        self.limit = Some(limit);
        self.armed_at = Some(ctx.now());
    }
}

#[test]
fn hybrid_fast_path_fires_on_parameters_from_the_slow_path() {
    use Side::*;
    let events = [
        MboEvent::add(1_000, Ask, 1, 102, 50),
        MboEvent::add(1_000, Bid, 2, 100, 50),
        MboEvent::add(2_000, Ask, 3, 102, 50),
        MboEvent::status(10_000, b'M'),
    ];
    let cfg = HybridConfig {
        fast: LatencyConfig {
            feed: Latency::Constant(10),
            order_entry: Latency::Constant(10),
            order_response: Latency::Constant(10),
            compute: ComputeLatency::Pipelined {
                latency: 5,
                interval: 1,
            },
        },
        slow: latency(100, 10, 10, 20).latency,
        link: Latency::Constant(30),
        ..Default::default()
    };
    let (mut fast, mut slow) = (Trigger::default(), Arming::default());
    let report = run_hybrid(&events, &mut fast, &mut slow, cfg);

    // Software sees t=1000 at 1100 and is done at 1120; the write lands at 1150. The
    // FPGA saw the same ask at 1010, before it was armed, so it waits for the next event.
    assert_eq!(fast.armed_at, Some(1_150));
    assert_eq!(report.tick_to_param_ns, [150]);
    // It sees t=2000 at 2010 and its order arrives 5 + 10ns later.
    assert_eq!(report.tick_to_order_ns, [25]);
    assert_eq!(report.slow_tick_to_order_ns, [130]);
    assert_eq!(report.fills.len(), 1);
    assert_eq!((report.fills[0].ts, report.fills[0].qty), (2_025, 50));

    // Each path hears only about its own orders, after its own response delay. Both
    // responses land together; the FPGA takes the second one a pipeline interval later,
    // the CPU only after a full callback.
    assert!(matches!(
        fast.updates.as_slice(),
        [
            (2_035, OrderUpdate::Accepted { .. }),
            (2_036, OrderUpdate::Filled { qty: 50, .. })
        ]
    ));
    assert!(matches!(
        slow.updates.as_slice(),
        [
            (1_140, OrderUpdate::Accepted { .. }),
            (1_160, OrderUpdate::Canceled { qty: 10, .. })
        ]
    ));
    assert_eq!(Path::of(fast.updates[0].1.id()), Path::Fast);
    assert_eq!(Path::of(slow.updates[0].1.id()), Path::Slow);
}
