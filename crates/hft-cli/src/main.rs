use std::fs::File;
use std::io::{BufReader, Read};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use flate2::read::MultiGzDecoder;
use hft_data::MboEvent;
use hft_data::itch::{ItchNormalizer, normalize_stream};
use hft_data::store::{EventFile, EventWriter};
use hft_data::synthetic::{SyntheticConfig, generate};
use hft_sim::{
    ComputeLatency, DEFAULT_HORIZONS_NS, Fees, HybridConfig, Latency, LatencyConfig, Metrics,
    Report, SimConfig, run, run_hybrid,
};
use hft_strategies::{HybridTakerConfig, MarketMaker, MarketMakerConfig, Noop, hybrid_taker};

#[derive(Parser)]
#[command(name = "hft", about = "L3 market-by-order backtester")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a synthetic MBO file (for engine testing only, not strategy evaluation).
    Gen {
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 1_000_000)]
        events: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Convert a Nasdaq TotalView-ITCH 5.0 file (.gz or raw) into one MBO file per symbol.
    Itch {
        #[arg(long)]
        input: PathBuf,
        #[arg(long, value_delimiter = ',', required = true)]
        symbols: Vec<String>,
        /// Session date, YYYYMMDD.
        #[arg(long)]
        date: u32,
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Replay an MBO file through a strategy and evaluate it.
    Run(RunArgs),
    /// Run the same strategy at several latencies in parallel and compare.
    Sweep(SweepArgs),
}

#[derive(Clone, Copy, ValueEnum)]
enum StrategyKind {
    Mm,
    Noop,
    /// Hybrid: software arms take thresholds, FPGA logic fires on them.
    Taker,
}

#[derive(Args, Clone)]
struct StrategyArgs {
    #[arg(long, value_enum, default_value_t = StrategyKind::Mm)]
    strategy: StrategyKind,
    /// Order size.
    #[arg(long, default_value_t = 100)]
    qty: u32,
    /// Absolute position limit.
    #[arg(long, default_value_t = 1_000)]
    max_position: i64,
    /// Market maker minimum spread to quote, in 1/10000 $.
    #[arg(long, default_value_t = 100)]
    min_spread: i64,
    /// Maker fee per share in 1/10000 $ (negative = rebate).
    #[arg(long, default_value_t = -20, allow_negative_numbers = true)]
    maker_fee: i64,
    /// Taker fee per share in 1/10000 $.
    #[arg(long, default_value_t = 30)]
    taker_fee: i64,
    /// Taker: how far through fair value the touch must be to fire, in 1/10000 $.
    #[arg(long, default_value_t = 200)]
    edge: i64,
    /// Taker: time constant of the software fair value (moving average of the mid).
    #[arg(long, default_value_t = 1_000)]
    fair_tau_us: u64,
    /// Fixed software compute time charged per callback.
    #[arg(long, default_value_t = 500)]
    compute_ns: u64,
    /// Run a single-path strategy (mm, noop) as FPGA logic: --hw-latency-ns per message,
    /// a new one every --hw-interval-ns, instead of --compute-ns per callback.
    #[arg(long)]
    fpga: bool,
    /// FPGA pipeline latency, message in → order out.
    #[arg(long, default_value_t = 100)]
    hw_latency_ns: u64,
    /// FPGA initiation interval: how often the pipeline takes a new message.
    #[arg(long, default_value_t = 4)]
    hw_interval_ns: u64,
    /// Hybrid: one-way delay between software and FPGA (e.g. PCIe).
    #[arg(long, default_value_t = 500)]
    link_ns: u64,
    /// Adds uniform jitter in [0, N] ns to each network delay.
    #[arg(long, default_value_t = 0)]
    jitter_ns: u64,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Bucket size for the PnL curve and drawdown.
    #[arg(long, default_value_t = 1_000)]
    bucket_ms: u64,
}

#[derive(Args)]
struct RunArgs {
    #[arg(long)]
    data: PathBuf,
    #[command(flatten)]
    s: StrategyArgs,
    /// Exchange → strategy market data delay.
    #[arg(long, default_value_t = 5_000)]
    feed_ns: u64,
    /// Strategy → exchange order delay.
    #[arg(long, default_value_t = 5_000)]
    entry_ns: u64,
    /// Exchange → strategy response delay.
    #[arg(long, default_value_t = 5_000)]
    response_ns: u64,
    /// FPGA path market data delay [default: --feed-ns].
    #[arg(long)]
    hw_feed_ns: Option<u64>,
    /// FPGA path order delay [default: --entry-ns].
    #[arg(long)]
    hw_entry_ns: Option<u64>,
    /// FPGA path response delay [default: --response-ns].
    #[arg(long)]
    hw_response_ns: Option<u64>,
    /// Charge each software callback's measured wall-clock time instead of --compute-ns.
    #[arg(long)]
    measure_compute: bool,
    /// Write every fill to this CSV.
    #[arg(long)]
    fills_csv: Option<PathBuf>,
}

#[derive(Args)]
struct SweepArgs {
    #[arg(long)]
    data: PathBuf,
    #[command(flatten)]
    s: StrategyArgs,
    /// One-way delays to try; each is used for feed, entry and response on every path.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "1000,2000,5000,10000,20000,50000,100000"
    )]
    latencies_ns: Vec<u64>,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Gen { out, events, seed } => {
            let mut w = EventWriter::create(&out, "SYNTH", 0)?;
            generate(
                &SyntheticConfig {
                    seed,
                    events,
                    ..Default::default()
                },
                |ev| w.push(&ev),
            )?;
            println!("wrote {} events to {}", w.finish()?, out.display());
        }
        Cmd::Itch {
            input,
            symbols,
            date,
            out_dir,
        } => convert_itch(input, symbols, date, out_dir)?,
        Cmd::Run(args) => run_backtest(args)?,
        Cmd::Sweep(args) => sweep(args)?,
    }
    Ok(())
}

fn convert_itch(input: PathBuf, symbols: Vec<String>, date: u32, out_dir: PathBuf) -> Result<()> {
    let symbols: Vec<String> = symbols.iter().map(|s| s.trim().to_uppercase()).collect();
    std::fs::create_dir_all(&out_dir)?;
    let mut writers = symbols
        .iter()
        .map(|s| EventWriter::create(out_dir.join(format!("{s}.mbo")), s, date))
        .collect::<std::io::Result<Vec<_>>>()?;

    let file = File::open(&input).with_context(|| format!("opening {}", input.display()))?;
    let reader: Box<dyn Read> = if input.extension().is_some_and(|e| e == "gz") {
        Box::new(BufReader::with_capacity(1 << 20, MultiGzDecoder::new(file)))
    } else {
        Box::new(BufReader::with_capacity(1 << 20, file))
    };

    let mut norm = ItchNormalizer::new(&symbols);
    normalize_stream(reader, &mut norm, &mut |out, ev| writers[out].push(&ev))
        .context("reading ITCH")?;
    println!("read {} ITCH messages", norm.messages);
    for (symbol, w) in symbols.iter().zip(writers) {
        println!("{symbol}: {} events", w.finish()?);
    }
    Ok(())
}

fn open(path: &PathBuf) -> Result<EventFile> {
    let file = EventFile::open(path).with_context(|| format!("opening {}", path.display()))?;
    let h = file.header();
    println!("{} {} — {} events", h.symbol(), h.date, h.count);
    Ok(file)
}

fn net(base: u64, jitter: u64, seed: u64) -> Latency {
    match jitter {
        0 => Latency::Constant(base),
        j => Latency::uniform(base, base + j, seed),
    }
}

/// Feed, entry and response delays of one path.
#[derive(Clone, Copy)]
struct Network {
    feed: u64,
    entry: u64,
    response: u64,
}

impl Network {
    fn same(ns: u64) -> Self {
        Network {
            feed: ns,
            entry: ns,
            response: ns,
        }
    }

    /// Jitter seeds start at `seed`; paths of a hybrid run get distinct ones.
    fn config(self, s: &StrategyArgs, seed: u64, compute: ComputeLatency) -> LatencyConfig {
        LatencyConfig {
            feed: net(self.feed, s.jitter_ns, seed),
            order_entry: net(self.entry, s.jitter_ns, seed.wrapping_add(1)),
            order_response: net(self.response, s.jitter_ns, seed.wrapping_add(2)),
            compute,
        }
    }
}

fn simulate(
    events: &[MboEvent],
    s: &StrategyArgs,
    sw: Network,
    hw: Network,
    sw_compute: ComputeLatency,
) -> (Report, Metrics) {
    let fees = Fees {
        maker_per_share: s.maker_fee,
        taker_per_share: s.taker_fee,
    };
    let hw_compute = ComputeLatency::Pipelined {
        latency: s.hw_latency_ns,
        interval: s.hw_interval_ns,
    };
    // A single-path strategy runs on one path with the first jitter seeds.
    let single = || SimConfig {
        latency: if s.fpga {
            hw.config(s, s.seed, hw_compute.clone())
        } else {
            sw.config(s, s.seed, sw_compute.clone())
        },
        fees,
    };
    let report = match s.strategy {
        StrategyKind::Mm => {
            let mm = MarketMakerConfig {
                qty: s.qty,
                max_position: s.max_position,
                min_spread: s.min_spread,
            };
            run(events, &mut MarketMaker::new(mm), single())
        }
        StrategyKind::Noop => run(events, &mut Noop, single()),
        StrategyKind::Taker => {
            let (mut fast, mut slow) = hybrid_taker(HybridTakerConfig {
                edge: s.edge,
                tau_ns: s.fair_tau_us * 1_000,
                tick: 100,
                qty: s.qty,
                max_position: s.max_position,
            });
            let cfg = HybridConfig {
                fast: hw.config(s, s.seed.wrapping_add(3), hw_compute.clone()),
                slow: sw.config(s, s.seed, sw_compute.clone()),
                link: Latency::Constant(s.link_ns),
                fees,
            };
            run_hybrid(events, &mut fast, &mut slow, cfg)
        }
    };
    let metrics = Metrics::compute(&report, &DEFAULT_HORIZONS_NS, s.bucket_ms * 1_000_000);
    (report, metrics)
}

fn run_backtest(a: RunArgs) -> Result<()> {
    let file = open(&a.data)?;
    let sw = Network {
        feed: a.feed_ns,
        entry: a.entry_ns,
        response: a.response_ns,
    };
    let hw = Network {
        feed: a.hw_feed_ns.unwrap_or(a.feed_ns),
        entry: a.hw_entry_ns.unwrap_or(a.entry_ns),
        response: a.hw_response_ns.unwrap_or(a.response_ns),
    };
    let sw_compute = if a.measure_compute {
        ComputeLatency::Measured
    } else {
        ComputeLatency::Fixed(a.s.compute_ns)
    };
    let (report, metrics) = simulate(file.events(), &a.s, sw, hw, sw_compute);
    println!("{report}\n\n{metrics}");
    if let Some(path) = a.fills_csv {
        report.write_fills_csv(&path)?;
        println!("\nfills written to {}", path.display());
    }
    Ok(())
}

fn sweep(a: SweepArgs) -> Result<()> {
    let file = open(&a.data)?;
    let events = file.events();
    let s = &a.s;
    // Runs are independent and single-threaded, so the sweep parallelizes across them.
    let results: Vec<(u64, Report, Metrics)> = std::thread::scope(|scope| {
        let handles: Vec<_> = a
            .latencies_ns
            .iter()
            .map(|&ns| {
                scope.spawn(move || {
                    let path = Network::same(ns);
                    let compute = ComputeLatency::Fixed(s.compute_ns);
                    let (r, m) = simulate(events, s, path, path, compute);
                    (ns, r, m)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let cell = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:+.4}"));
    println!(
        "\n{:>9} {:>8} {:>7} {:>7} {:>9} {:>9} {:>11} {:>11} {:>10} {:>11}",
        "one-way",
        "orders",
        "fill%",
        "maker%",
        "mo@fill",
        "mo@1ms",
        "capture $",
        "post-fill $",
        "fees $",
        "net $"
    );
    for (ns, r, m) in &results {
        let filled = (r.maker_shares + r.taker_shares).max(1) as f64;
        println!(
            "{:>7}µs {:>8} {:>6.1}% {:>6.1}% {:>9} {:>9} {:>11.2} {:>11.2} {:>10.2} {:>11.2}",
            ns / 1_000,
            r.orders_sent,
            100.0 * m.fill_ratio,
            100.0 * r.maker_shares as f64 / filled,
            cell(m.markout(0).and_then(|x| x.all)),
            cell(m.markout(1_000_000).and_then(|x| x.all)),
            m.spread_capture,
            m.post_fill_moves,
            m.fees,
            m.net_pnl,
        );
    }
    Ok(())
}
