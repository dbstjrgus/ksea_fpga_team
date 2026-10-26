# hft-sim

A discrete-event backtester for low-latency trading strategies on L3 (market-by-order) data.
It replays historical order flow, models the latency between the exchange and your algorithm,
simulates where your orders sit in the queue, and reports why a strategy made or lost money.

- [Setup](#setup)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Interpreting results](#interpreting-results)
- [Writing your own strategy](#writing-your-own-strategy)
- [FPGA and hybrid strategies](#fpga-and-hybrid-strategies)
- [How the simulation works](#how-the-simulation-works)
- [Known gaps](#known-gaps)

## Setup

Requires Rust (stable). On macOS with Homebrew:

```sh
brew install rustup
rustup default stable
# Homebrew's rustup is keg-only; put it on your PATH (add this line to ~/.zshrc):
export PATH=/opt/homebrew/opt/rustup/bin:$PATH
```

Build (always use `--release` for real runs; debug builds are ~20× slower):

```sh
cargo build --release
cargo test --workspace      # optional: run the test suite
```

The binary is `./target/release/hft`.

## Quick start

```sh
# 1. Make 20M events of synthetic data (~760 MB)
./target/release/hft gen --out data/synth.mbo --events 20000000

# 2. Backtest the example market maker on it
./target/release/hft run --data data/synth.mbo

# 3. See how results change with latency
./target/release/hft sweep --data data/synth.mbo
```

> **Synthetic data only tests the engine.** Its price process is random, so strategy PnL on
> it means nothing. Use real data (below) to evaluate strategies.

## Commands

Run any command with `--help` for the full option list.

### `gen` — synthetic data

```sh
./target/release/hft gen --out data/synth.mbo --events 20000000 --seed 1
```

Same seed ⇒ same file. About 2µs between events on average, so 20M events ≈ 40s of market time.

### `itch` — convert real Nasdaq data

Converts a Nasdaq TotalView-ITCH 5.0 file (raw or `.gz`) into one `.mbo` file per symbol.
Nasdaq publishes sample day files at `emi.nasdaq.com/ITCH/` (several GB each).

```sh
./target/release/hft itch \
  --input 01302019.NASDAQ_ITCH50.gz \
  --symbols AAPL,MSFT,AMD \
  --date 20190130 \
  --out-dir data/
# → data/AAPL.mbo, data/MSFT.mbo, data/AMD.mbo
```

Converting is done once per day; every backtest then memory-maps the result.

### `run` — backtest one configuration

```sh
./target/release/hft run --data data/AAPL.mbo \
  --feed-ns 5000 --entry-ns 5000 --response-ns 5000 \
  --compute-ns 500 \
  --fills-csv data/fills.csv
```

| option | meaning | default |
|---|---|---|
| `--strategy` | `mm` (example market maker), `noop` (measures raw replay speed) or `taker` (example [hybrid FPGA/software](#fpga-and-hybrid-strategies) taker) | `mm` |
| `--feed-ns` | exchange → strategy market data delay | 5000 |
| `--entry-ns` | strategy → exchange order delay | 5000 |
| `--response-ns` | exchange → strategy ack/fill delay | 5000 |
| `--jitter-ns` | adds uniform random `[0, N]` ns to each network delay (seeded, reproducible) | 0 |
| `--compute-ns` | time your strategy is busy per callback | 500 |
| `--measure-compute` | charge the real measured callback time instead (not reproducible) | off |
| `--fpga` | run `mm`/`noop` as pipelined FPGA logic instead of software | off |
| `--hw-latency-ns` / `--hw-interval-ns` | FPGA pipeline: message in → order out, and how often it takes a new message | 100 / 4 |
| `--hw-feed-ns`, `--hw-entry-ns`, `--hw-response-ns` | network delays of the FPGA path | same as software |
| `--link-ns` | hybrid: one-way software ↔ FPGA delay (PCIe) | 500 |
| `--edge`, `--fair-tau-us` | `taker` parameters: how far through fair value to fire (1/10000 $), fair value time constant | 200 / 1000 |
| `--maker-fee` / `--taker-fee` | per share, in 1/10000 $; negative = rebate | -20 / 30 |
| `--qty`, `--max-position` | order size and position limit | 100 / 1000 |
| `--min-spread` | market maker: minimum spread to quote, in 1/10000 $ | 100 |
| `--bucket-ms` | bucket size for the PnL curve and drawdown | 1000 |
| `--fills-csv` | write every fill to a CSV | — |
| `--seed` | seed for latency jitter | 1 |

Units: prices are integer 1/10000 dollar (so `100` = 1 cent), times are nanoseconds.

### `sweep` — compare latencies

Runs the same strategy once per latency, in parallel, using that value for feed, entry and
response delay (on both paths of a hybrid strategy):

```sh
./target/release/hft sweep --data data/AAPL.mbo --latencies-ns 1000,5000,20000,100000
```

Takes the same strategy, fee and compute options as `run`.

## Interpreting results

Example output of `hft run` (synthetic data, 5µs one-way latency):

```
replay      20000000 events in 1.403s (14.3M events/s)
strategy    21752716 callbacks, 802266 orders, 598420 cancels
fills       352033 (30065313 maker / 3264117 taker shares)
position    626 final, 1000 max abs
fees        $-50338.28
net pnl     $-147402.05
tick→order  p50 10500ns, p99 11420ns, max 13032ns

markouts ($/share, + = in our favour)
             at fill      10µs     100µs       1ms      10ms     100ms
  maker      +0.0039   +0.0000   -0.0049   -0.0060   -0.0061   -0.0055
  taker      -0.0053   -0.0048   -0.0046   -0.0050   -0.0050   -0.0072
  all        +0.0030   -0.0005   -0.0048   -0.0059   -0.0060   -0.0056
pnl         spread capture $99333.20  +  post-fill moves $-297073.52  −  fees $-50338.28  =  $-147402.05
execution   fill ratio 41.5%, 0.75 cancels/order
inventory   avg |position| 478 shares, flat 0.1% of the time
risk        per-1s pnl mean $-3595.17, std $736.92, 0/41 positive; max drawdown $147402.05
```

### Summary block

| line | what it tells you |
|---|---|
| `replay` | Simulator speed. Not a result — it never affects the numbers below. |
| `strategy` | Callbacks = market data events delivered + order updates received. High cancels per order are normal for market makers. |
| `fills` | **Maker** shares rested on the book and were hit (you earn the rebate). **Taker** shares crossed the spread (you pay the fee). A "passive" strategy with many taker shares is sending orders based on stale prices. |
| `position` | Final and largest absolute inventory. If max equals your limit, the strategy spends time pinned there. |
| `fees` | Total fees; **negative means you received net rebates**. |
| `net pnl` | Cash from trades + final position marked to the last mid − fees. |
| `tick→order` | Time from a market event at the exchange to the order it triggered arriving at the exchange: feed + waiting for the strategy + compute + entry. p50 should be close to `feed + compute + entry`. **If p99 is much higher, your strategy is too slow for the message rate** and is processing a backlog. In a hybrid run this is the FPGA path's orders. |
| `slow t→o` | Hybrid only: tick→order for orders the software path sent itself. |
| `tick→param` | Hybrid only: market event → the software's reaction to it taking effect in the FPGA (software feed + compute + link). **How stale the FPGA's parameters are**; the FPGA fires on prices software has not seen yet. |

### Markouts — the most important table

For each fill, a markout is how far the mid moved from your fill price after a given time,
signed so **positive = good for you**, averaged per share.

- **`at fill`** is your edge versus the mid just before you traded. A maker quoting at the touch
  should see roughly +half the spread; a taker roughly −half the spread.
- **Later columns** show what happened next. If maker markouts drop from positive to negative,
  you are being **adversely selected**: you get filled mostly when the price is about to move
  through you.

Rules of thumb for per-share profitability at a horizon you would hold for:

- Maker fills: `markout − maker_fee` (rebate counts as positive). Here: −0.0060 + 0.0020 < 0 ⇒ losing.
- Taker fills: `markout − taker_fee` must be positive. A taker signal must predict a move larger
  than half the spread plus the fee.

### PnL attribution

```
spread capture  +  post-fill moves  −  fees  =  net pnl
```

- **Spread capture**: total edge versus mid at the moment of each fill (the `at fill` markout × shares).
- **Post-fill moves**: how the mid moved from each fill to the end of the run — adverse selection
  plus the cost of holding inventory.
- The three always add up exactly to net PnL.

A market maker typically wins on capture and fees and loses on post-fill moves; it is profitable
only if the first two outweigh the third.

### Execution, inventory, risk

| line | meaning |
|---|---|
| `fill ratio` | Filled shares ÷ submitted shares. |
| `cancels/order` | Exchanges and regulators watch order-to-trade ratios; very high values can be a problem live. |
| `avg \|position\|` | Time-weighted inventory. Large and rarely flat means you are carrying directional risk, not just making markets. |
| `risk` | PnL per bucket (`--bucket-ms`): mean, standard deviation, how many buckets were positive, and the worst peak-to-trough drawdown. Use more buckets (longer data or smaller `--bucket-ms`) before trusting these. |

### Reading a sweep

```
  one-way   orders   fill%  maker%   mo@fill    mo@1ms   capture $ post-fill $     fees $       net $
      1µs   907722   31.5%   98.6%   +0.0046   -0.0053   131811.61  -283271.93  -55161.73   -96298.59
      5µs   802266   41.5%   90.2%   +0.0030   -0.0059    99333.20  -297073.52  -50338.28  -147402.05
     20µs   601100   50.8%   68.6%   +0.0006   -0.0064    17353.82  -213997.20  -13128.36  -183515.03
    100µs   272047   47.0%   35.7%   -0.0027   -0.0073   -34686.94   -62247.08   15575.21  -112509.24
```

- `maker%` is the share of filled volume that was passive. Falling `maker%` and `mo@fill` with
  latency means your orders are increasingly based on prices that have already moved.
- `mo@fill` / `mo@1ms` are the "all fills" markouts at fill time and after 1ms.
- **Do not compare `net $` alone.** Above, 100µs loses less than 20µs only because it trades a
  third as much; each trade is worse. Compare per-share markouts.
- The latency at which the edge disappears tells you what infrastructure the strategy needs.

### Fills CSV

`--fills-csv` writes one row per fill: `ts_ns,order_id,side,price,qty,liquidity,fee`.
`ts_ns` is exchange time (nanoseconds since midnight), `price` and `fee` are in dollars.

## Writing your own strategy

Implement `hft_sim::Strategy` in `crates/hft-strategies`:

```rust
use hft_book::Book;
use hft_data::{MboEvent, Side};
use hft_sim::{Ctx, OrderUpdate, Strategy, TimeInForce};

pub struct MyAlgo { /* state */ }

impl Strategy for MyAlgo {
    // Called for every market event, after feed latency. `book` is your delayed view.
    fn on_market_data(&mut self, ctx: &mut Ctx, book: &Book, ev: &MboEvent) {
        if let Some(ask) = book.best_ask() {
            // Orders leave after this callback's compute time + entry latency.
            let _id = ctx.send(Side::Bid, ask, 100, TimeInForce::Ioc);
        }
    }

    // Acks, fills and cancels, after response latency.
    fn on_order_update(&mut self, ctx: &mut Ctx, book: &Book, update: &OrderUpdate) {}
}
```

Useful calls: `ctx.now()`, `ctx.position()`, `ctx.send(side, price, qty, tif)`, `ctx.cancel(id)`,
`book.best_bid()`, `book.best_ask()`, `book.levels(side)`, `book.level_qty(side, price)`.

Then register it in `crates/hft-cli/src/main.rs`: add a variant to `StrategyKind` and a match arm
in `simulate`. Run with `--strategy <name>`.

## FPGA and hybrid strategies

Two features model hardware trading logic.

**Pipelined compute.** Software handles one callback at a time, so a burst of messages queues
behind it. FPGA logic is a pipeline: each message takes a fixed latency to get through, but a
new one enters every few clock cycles. `ComputeLatency::Pipelined { latency, interval }` models
this; `--fpga` runs any single-path strategy that way. Compare a strategy both ways:

```sh
./target/release/hft run --data data/AAPL.mbo            # software: 500ns per callback
./target/release/hft run --data data/AAPL.mbo --fpga     # FPGA: 100ns latency, 4ns interval
```

The difference shows in `tick→order` p99: the software backlog disappears.

**Hybrid systems.** Most FPGA deployments split the work. Software (the *slow path*) computes
fair values and limits and writes them to the card; the FPGA (the *fast path*) watches the
feed and fires pre-armed orders the moment a condition is met. `hft_sim::run_hybrid` runs two
strategies this way:

- Each path has its own feed delay, book, order session, compute model and position.
- Exchange responses go only to the path that owns the order.
- The paths talk with `ctx.post(msg)`; a message leaves after the sender's compute time and
  arrives `link` later via `on_message`. Use it for parameter writes one way and fill or
  status notifications the other.

```rust
use hft_sim::{Ctx, HybridConfig, Strategy, run_hybrid};

enum Msg { Arm { buy_at: i64 }, Done }

struct Fpga { /* registers */ }
impl Strategy<Msg> for Fpga {
    // on_market_data: compare the book with armed registers and fire.
    // on_message: Msg::Arm from software updates the registers.
}

struct Software { /* models, state */ }
impl Strategy<Msg> for Software {
    // on_market_data: update models, ctx.post(Msg::Arm { .. }) when limits change.
    // on_message: Msg::Done from the FPGA, re-arm.
}

let report = run_hybrid(events, &mut Fpga { .. }, &mut Software { .. }, HybridConfig::default());
```

`HybridConfig::default()` uses the default network delays on both paths, `ComputeLatency::FPGA`
on the fast one, 500ns software compute and a 500ns link; replace them with your own figures.

The fast path is a Rust behavioral model of the hardware: the simulator enforces its timing,
not that it is synthesizable. Keep it to what the hardware actually does (compare registers
with the book, fire, disarm).

The example `taker` (`crates/hft-strategies/src/hybrid_taker.rs`): software keeps a moving
average of the mid as fair value and arms the FPGA with buy/sell thresholds `--edge` away; the
FPGA fires an IOC when the touch crosses one, disarms that side and tells software, which
re-arms it. Like `mm`, it exists to exercise the engine, not to make money.

## How the simulation works

Two clocks run side by side. The simulator's own wall-clock speed never affects results — only
the modeled delays do.

- **Exchange:** history is applied to the exchange book at its recorded time.
- **Strategy:** sees each event after the feed delay, in its own lagged book. Each callback keeps it
  busy for the compute time; events arriving meanwhile wait in line. Pipelined (FPGA) logic is
  busy only for its initiation interval; its output still leaves after the full latency.
- **Hybrid:** each path has its own feed, book, order session and compute; messages between them
  take the link delay.
- **Orders:** reach the exchange `compute + entry` after the callback started; acks and fills come
  back after the response delay. Messages on each link never reorder.
- **Ties:** at the same nanosecond, history goes first, so your order never beats it. Then
  arriving orders, then the fast path, then the slow path. Within a path: responses, then
  messages from the other path, then market data.

**Fill model.** Your orders do not change history (no market impact). A resting order joins the
back of its price level; everything already there is ahead of it. It fills when:

- an execution hits an order *behind* it at its price (FIFO, so everything ahead is gone);
- an execution prints through its price;
- an incoming historical order would have crossed it instead of resting.

Marketable orders take the visible liquidity on the exchange book when they arrive.

### Project layout

| crate | role |
|---|---|
| `hft-data` | 40-byte event record, memory-mapped file format, ITCH 5.0 converter, synthetic generator |
| `hft-book` | L3 order book with per-order queue position |
| `hft-sim` | event loop (single and hybrid), latency and FPGA pipeline models, fill model, `Strategy` trait, report and metrics |
| `hft-strategies` | example strategies (`MarketMaker`, `Noop`, hybrid `ArmedTaker` + `FairValueArmer`) |
| `hft-cli` | the `hft` command |

## Known gaps

- **No market impact:** liquidity you take is not removed, so repeated takes can reuse it.
- **Single venue:** Nasdaq's book only; no routing, NBBO, or other exchanges.
- **Hidden liquidity:** ITCH does not say which side a hidden execution was on, so hidden trades are ignored.
- **No capture timestamps in raw ITCH:** feed latency is measured from the exchange timestamp.
- **No auctions, halts or LULD:** use regular-hours data.
- **Your own orders at one price** fill in the order you sent them.
- **No wire-level feed timing:** FPGAs often fire part-way through a packet. Events carry no
  packet boundaries or byte offsets, so serialization delay and cut-through are not modeled;
  fold an average into `--hw-feed-ns`. Needs a pcap source with hardware timestamps.
- **Races at FPGA speeds:** history is fixed, so a competitor who in reality reacted to the same
  tick cannot react to you. Sub-microsecond race outcomes are the least trustworthy result;
  model exchange gateway jitter with `--jitter-ns` or `Latency::Empirical`.
- **End of data:** messages still in flight are dropped; the position is marked to the final mid.
