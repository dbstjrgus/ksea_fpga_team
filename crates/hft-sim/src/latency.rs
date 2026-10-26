use hft_data::rng::Rng;

/// A source of one-way delays in nanoseconds.
#[derive(Clone, Debug)]
pub enum Latency {
    Constant(u64),
    /// Uniform in `[lo, hi]`.
    Uniform {
        lo: u64,
        hi: u64,
        rng: Rng,
    },
    /// Resampled from observed delays, e.g. captured `ts_recv - ts_exch` or order round trips.
    Empirical {
        samples: Vec<u64>,
        rng: Rng,
    },
}

impl Latency {
    pub fn uniform(lo: u64, hi: u64, seed: u64) -> Self {
        assert!(lo <= hi, "uniform latency needs lo <= hi");
        Latency::Uniform {
            lo,
            hi,
            rng: Rng::new(seed),
        }
    }

    pub fn empirical(samples: Vec<u64>, seed: u64) -> Self {
        assert!(!samples.is_empty(), "empirical latency needs samples");
        Latency::Empirical {
            samples,
            rng: Rng::new(seed),
        }
    }

    #[inline]
    pub fn sample(&mut self) -> u64 {
        match self {
            Latency::Constant(ns) => *ns,
            Latency::Uniform { lo, hi, rng } => *lo + rng.below(*hi - *lo + 1),
            Latency::Empirical { samples, rng } => {
                samples[rng.below(samples.len() as u64) as usize]
            }
        }
    }
}

/// How long the strategy is busy per callback. Orders it sends leave after this.
#[derive(Clone, Debug)]
pub enum ComputeLatency {
    /// Same charge every callback. Deterministic.
    Fixed(u64),
    /// Sampled charge, e.g. from a distribution profiled offline. Deterministic for a seed.
    Sampled(Latency),
    /// The callback's measured wall-clock time. Realistic but not reproducible, and it
    /// includes simulator overhead and cache effects that production would not have.
    Measured,
    /// Pipelined hardware (FPGA) logic: every message takes `latency` ns to get through,
    /// but a new one can enter every `interval` ns, so a burst does not queue behind
    /// whole callbacks the way it does on a CPU.
    Pipelined { latency: u64, interval: u64 },
}

impl ComputeLatency {
    /// Rough FPGA figures: 100ns through the pipeline, one message per 4ns (250 MHz).
    pub const FPGA: ComputeLatency = ComputeLatency::Pipelined {
        latency: 100,
        interval: 4,
    };
}

#[derive(Clone, Debug)]
pub struct LatencyConfig {
    /// Exchange → strategy market data, added to each event's `ts_recv`.
    pub feed: Latency,
    /// Strategy → exchange order messages.
    pub order_entry: Latency,
    /// Exchange → strategy acks, fills and cancel confirmations.
    pub order_response: Latency,
    pub compute: ComputeLatency,
}

impl Default for LatencyConfig {
    /// Rough colocated-equities figures; replace with measurements from your own setup.
    fn default() -> Self {
        LatencyConfig {
            feed: Latency::Constant(5_000),
            order_entry: Latency::Constant(5_000),
            order_response: Latency::Constant(5_000),
            compute: ComputeLatency::Fixed(500),
        }
    }
}

impl LatencyConfig {
    /// The default network delays with [`ComputeLatency::FPGA`] logic.
    pub fn fpga() -> Self {
        LatencyConfig {
            compute: ComputeLatency::FPGA,
            ..Default::default()
        }
    }
}
