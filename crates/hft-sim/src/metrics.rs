//! Post-run evaluation: markouts, PnL attribution, execution quality and risk.

use std::fmt;

use hft_data::{PRICE_SCALE, Price, Ts};

use crate::report::{Report, dollars};
use crate::strategy::Liquidity;

/// 0 (at fill), 10µs, 100µs, 1ms, 10ms, 100ms.
pub const DEFAULT_HORIZONS_NS: [u64; 6] = [0, 10_000, 100_000, 1_000_000, 10_000_000, 100_000_000];

/// Average signed mid move after fills, in dollars per share. Positive means the price
/// moved in our favour (we bought below / sold above where the mid ended up).
#[derive(Clone, Copy, Debug, Default)]
pub struct Markout {
    pub horizon_ns: u64,
    pub maker: Option<f64>,
    pub taker: Option<f64>,
    pub all: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub markouts: Vec<Markout>,
    /// Σ edge versus the mid just before each fill ($). What the strategy "earned" at the
    /// moment of trading.
    pub spread_capture: f64,
    /// Σ mid moves from each fill to the final mark ($): adverse selection plus inventory.
    pub post_fill_moves: f64,
    pub fees: f64,
    pub net_pnl: f64,
    /// Filled shares / submitted shares.
    pub fill_ratio: f64,
    pub cancels_per_order: f64,
    /// Time-weighted average absolute position (shares).
    pub avg_abs_position: f64,
    /// Fraction of the session spent flat.
    pub time_flat: f64,
    pub bucket_ns: u64,
    /// Change in marked-to-market PnL per bucket ($).
    pub bucket_pnl: Vec<f64>,
    /// Peak-to-trough drop of the equity curve, sampled at bucket ends ($).
    pub max_drawdown: f64,
}

/// Mid strictly before `ts`.
fn mid_before(mids: &[(Ts, Price)], ts: Ts) -> Option<Price> {
    let i = mids.partition_point(|m| m.0 < ts);
    i.checked_sub(1).map(|i| mids[i].1)
}

/// Mid as of `ts` (including changes at `ts`).
fn mid_at(mids: &[(Ts, Price)], ts: Ts) -> Option<Price> {
    let i = mids.partition_point(|m| m.0 <= ts);
    i.checked_sub(1).map(|i| mids[i].1)
}

impl Metrics {
    pub fn compute(report: &Report, horizons: &[u64], bucket_ns: u64) -> Metrics {
        let mids = &report.mids;
        let fills = &report.fills;
        let mut m = Metrics {
            fees: dollars(report.fees),
            net_pnl: dollars(report.pnl()),
            fill_ratio: (report.maker_shares + report.taker_shares) as f64
                / report.shares_sent.max(1) as f64,
            cancels_per_order: report.cancels_sent as f64 / report.orders_sent.max(1) as f64,
            bucket_ns,
            ..Default::default()
        };
        let (Some(&(start, _)), Some(&(end, _))) = (mids.first(), mids.last()) else {
            return m;
        };

        // Markouts. Fills whose horizon runs past the data are left out.
        for &h in horizons {
            // [maker, taker] × (Σ signed move × qty, Σ qty)
            let mut acc = [(0i64, 0i64); 2];
            for f in fills.iter().filter(|f| f.ts + h <= end) {
                let later = if h == 0 {
                    mid_before(mids, f.ts)
                } else {
                    mid_at(mids, f.ts + h)
                };
                if let Some(mid) = later {
                    let k = (f.liquidity == Liquidity::Taker) as usize;
                    acc[k].0 += f.side.sign() * (mid - f.price) * f.qty as i64;
                    acc[k].1 += f.qty as i64;
                }
            }
            let avg =
                |(v, q): (i64, i64)| (q > 0).then(|| v as f64 / q as f64 / PRICE_SCALE as f64);
            m.markouts.push(Markout {
                horizon_ns: h,
                maker: avg(acc[0]),
                taker: avg(acc[1]),
                all: avg((acc[0].0 + acc[1].0, acc[0].1 + acc[1].1)),
            });
        }

        // Attribution: trading PnL = edge at fill + moves after fill.
        let mark = report.mark_price.unwrap_or(0);
        let (mut capture, mut moves) = (0i64, 0i64);
        for f in fills {
            let mid = mid_before(mids, f.ts).unwrap_or(f.price);
            let sign = f.side.sign() * f.qty as i64;
            capture += sign * (mid - f.price);
            moves += sign * (mark - mid);
        }
        m.spread_capture = dollars(capture);
        m.post_fill_moves = dollars(moves);

        // Inventory over time.
        let (mut pos, mut last_t, mut area, mut flat) = (0i64, start, 0u128, 0u64);
        for f in fills {
            let t = f.ts.clamp(start, end);
            area += pos.unsigned_abs() as u128 * (t - last_t) as u128;
            if pos == 0 {
                flat += t - last_t;
            }
            pos += f.side.sign() * f.qty as i64;
            last_t = t;
        }
        area += pos.unsigned_abs() as u128 * (end - last_t) as u128;
        if pos == 0 {
            flat += end - last_t;
        }
        let span = (end - start).max(1);
        m.avg_abs_position = area as f64 / span as f64;
        m.time_flat = flat as f64 / span as f64;

        // Equity curve per bucket.
        let (mut cash, mut pos, mut fees, mut fi) = (0i64, 0i64, 0i64, 0);
        let (mut prev, mut peak) = (0.0f64, 0.0f64);
        let mut t = start;
        while t < end {
            let t_end = (t + bucket_ns).min(end);
            while fi < fills.len() && fills[fi].ts <= t_end {
                let f = &fills[fi];
                pos += f.side.sign() * f.qty as i64;
                cash -= f.side.sign() * f.price * f.qty as i64;
                fees += f.fee;
                fi += 1;
            }
            let equity = dollars(cash + pos * mid_at(mids, t_end).unwrap() - fees);
            m.bucket_pnl.push(equity - prev);
            prev = equity;
            peak = peak.max(equity);
            m.max_drawdown = m.max_drawdown.max(peak - equity);
            t = t_end;
        }
        m
    }

    pub fn markout(&self, horizon_ns: u64) -> Option<&Markout> {
        self.markouts.iter().find(|x| x.horizon_ns == horizon_ns)
    }
}

pub fn fmt_ns(ns: u64) -> String {
    match ns {
        0 => "at fill".into(),
        n if n >= 1_000_000_000 && n % 1_000_000_000 == 0 => format!("{}s", n / 1_000_000_000),
        n if n >= 1_000_000 && n % 1_000_000 == 0 => format!("{}ms", n / 1_000_000),
        n if n >= 1_000 && n % 1_000 == 0 => format!("{}µs", n / 1_000),
        n => format!("{n}ns"),
    }
}

impl fmt::Display for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cell = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:+.4}"));
        writeln!(f, "markouts ($/share, + = in our favour)")?;
        write!(f, "  {:<8}", "")?;
        for m in &self.markouts {
            write!(f, "{:>10}", fmt_ns(m.horizon_ns))?;
        }
        writeln!(f)?;
        for (name, pick) in [
            (
                "maker",
                (|m: &Markout| m.maker) as fn(&Markout) -> Option<f64>,
            ),
            ("taker", |m| m.taker),
            ("all", |m| m.all),
        ] {
            write!(f, "  {name:<8}")?;
            for m in &self.markouts {
                write!(f, "{:>10}", cell(pick(m)))?;
            }
            writeln!(f)?;
        }
        writeln!(
            f,
            "pnl         spread capture ${:.2}  +  post-fill moves ${:.2}  −  fees ${:.2}  =  ${:.2}",
            self.spread_capture, self.post_fill_moves, self.fees, self.net_pnl
        )?;
        writeln!(
            f,
            "execution   fill ratio {:.1}%, {:.2} cancels/order",
            100.0 * self.fill_ratio,
            self.cancels_per_order
        )?;
        writeln!(
            f,
            "inventory   avg |position| {:.0} shares, flat {:.1}% of the time",
            self.avg_abs_position,
            100.0 * self.time_flat
        )?;
        let n = self.bucket_pnl.len();
        if n > 0 {
            let mean = self.bucket_pnl.iter().sum::<f64>() / n as f64;
            let var = self
                .bucket_pnl
                .iter()
                .map(|x| (x - mean).powi(2))
                .sum::<f64>()
                / n as f64;
            let up = self.bucket_pnl.iter().filter(|x| **x > 0.0).count();
            write!(
                f,
                "risk        per-{} pnl mean ${mean:.2}, std ${:.2}, {up}/{n} positive; max drawdown ${:.2}",
                fmt_ns(self.bucket_ns),
                var.sqrt(),
                self.max_drawdown
            )?;
        }
        Ok(())
    }
}
