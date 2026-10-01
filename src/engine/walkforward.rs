//! 14-day walk-forward over closed 15m bars of Bybit USDT perpetuals, shared by
//! every engine (`run_wf`), plus the strategy grids.
//!
//! 1344 bars = 3 windows of 8 days in-sample (768) + 2 days out-of-sample
//! (192), stepping 2 days. Params are chosen on each in-sample window only
//! (and must survive all earlier data: no liquidation, drawdown <= 25%), then
//! judged on the next unseen 2 days. The params used for paper trading are
//! chosen the same way on the most recent 8 days.
//!
//! Gates: zero liquidations anywhere, out-of-sample profit factor > 1.2 and
//! net > 0, the net still > 0 without the single best window, at least
//! MIN_OOS_TRADES trades, final params profitable and liquidation-free over the
//! 14 days with drawdown <= 25%.

use super::governor;
use super::metrics::Metrics;
use super::scores::Score;
use super::{Market, BARS, BAR_MS};
use anyhow::{bail, Result};
use serde::Serialize;
use std::time::Instant;

pub const IS_BARS: usize = 768;
pub const OOS_BARS: usize = 192;
pub const WINDOW_STARTS: [usize; 3] = [0, 192, 384];
/// Top 100 Bybit USDT perpetuals by rolling 24h turnover at each bar (all leverage-capable).
pub const UNIVERSE: usize = 50;
pub const MIN_IS_TRADES: usize = 8;
pub const MIN_OOS_TRADES: usize = 8;
pub const MAX_DRAWDOWN_PCT: f64 = 25.0;
pub const MIN_PROFIT_FACTOR: f64 = 1.2;

fn objective(m: &Metrics) -> f64 {
    m.return_pct() - 0.5 * m.max_drawdown_pct
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_fit_14_days() {
        assert_eq!(BARS, 1344);
        assert_eq!(WINDOW_STARTS[2] + IS_BARS + OOS_BARS, BARS);
    }

    #[test]
    fn rejects_anything_but_14_days() {
        let ts: Vec<i64> = (0..100).map(|i| i * BAR_MS).collect();
        let r = run_wf(
            &ts,
            &[0u8],
            100.0,
            Instant::now() + std::time::Duration::from_secs(5),
            |_, _| Metrics::default(),
        );
        assert!(r.is_err());
    }

    /// Out-of-sample windows -27 / +20 / +241: the total is positive and PF is
    /// high, but without the best window it loses, so it must not PASS.
    #[test]
    fn a_single_lucky_window_cannot_pass() {
        let ts: Vec<i64> = (0..BARS as i64).map(|i| i * BAR_MS).collect();
        let window_net = |start: usize| match start {
            768 => -27.0,
            960 => 20.0,
            1152 => 241.0,
            _ => 50.0, // in-sample and full-period runs
        };
        let bt = |_: &u8, r: std::ops::Range<usize>| {
            let net = if r.end - r.start == OOS_BARS {
                window_net(r.start)
            } else {
                50.0
            };
            let (gp, gl) = if net > 0.0 {
                (net * 2.0, net)
            } else {
                (10.0, 10.0 - net)
            };
            Metrics {
                start_equity: 1000.0,
                end_equity: 1000.0 + net,
                trades: 20,
                wins: 12,
                gross_profit: gp,
                gross_loss: gl,
                ..Default::default()
            }
        };
        let r = run_wf(
            &ts,
            &[0u8],
            1000.0,
            Instant::now() + std::time::Duration::from_secs(5),
            bt,
        )
        .unwrap();
        assert_eq!(r.positive_windows, 2);
        assert!((r.net_without_best - (-7.0)).abs() < 1e-9);
        assert!(!r.passed);
        assert!(
            r.reasons.iter().any(|x| x.contains("rests on one window")),
            "{:?}",
            r.reasons
        );
        assert!(
            r.forward_ok(),
            "still positive overall: paper may forward-test, labelled as such"
        );
    }
}

// ---------------------------------------------------------------- cross-sectional (xs)

use super::xs::{self, Signal, XsParams};

/// The current paper strategy family. Historical selection results must be
/// rerun after accounting/data fixes; this constant is not a profitability claim.
pub const LIVE_SIGNAL: Signal = Signal::Pulse;

/// Current paper grid without an optional drawdown rule. Use the research
/// examples to compare alternatives on corrected accounting and untouched data.
pub fn live_grid() -> Vec<XsParams> {
    xs_family_grid(LIVE_SIGNAL)
}

/// Every ranking signal (screener scores, return, funding) in both directions.
pub const XS_SIGNALS: [Signal; 7] = [
    Signal::Return,
    Signal::TrendScore,
    Signal::PriceAction,
    Signal::Volatility,
    Signal::Rsi,
    Signal::Pulse,
    Signal::Funding,
];

pub fn xs_grid() -> Vec<XsParams> {
    XS_SIGNALS.iter().flat_map(|&s| xs_family_grid(s)).collect()
}

/// Grid for one signal family (what research and the live service search);
/// lookback only varies where the signal uses it. Drawdown handling (`Risk`)
/// is off: examples/risk_variants.rs compares the options.
pub fn xs_family_grid(signal: Signal) -> Vec<XsParams> {
    let lookbacks: &[usize] = if signal == Signal::Return {
        &[16, 96]
    } else {
        &[0]
    };
    let mut out = Vec::new();
    for flip in [false, true] {
        for &lookback in lookbacks {
            for hold in [16usize, 32, 96] {
                for top in [5usize, 10] {
                    for gross_leverage in [1.0, 2.0] {
                        for stop_pct in [None, Some(20.0)] {
                            out.push(XsParams {
                                signal,
                                flip,
                                lookback,
                                hold,
                                top,
                                gross_leverage,
                                stop_pct,
                                risk: Default::default(),
                            });
                        }
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- generic walk-forward

/// One in-sample/out-of-sample window of a walk-forward.
#[derive(Debug, Clone, Serialize)]
pub struct WfWindow<P> {
    pub is_bars: (usize, usize),
    pub oos_bars: (usize, usize),
    pub params: Option<P>,
    pub in_sample: Option<Metrics>,
    pub out_of_sample: Option<Metrics>,
}

/// 14-day walk-forward result for any strategy with params `P`.
#[derive(Debug, Clone, Serialize)]
pub struct WfReport<P> {
    pub first_bar_ts: i64,
    pub last_bar_ts: i64,
    pub start_equity: f64,
    pub evaluated: usize,
    pub elapsed_ms: u128,
    pub windows: Vec<WfWindow<P>>,
    pub oos: Metrics,
    pub params: Option<P>,
    pub final_in_sample: Option<Metrics>,
    pub full_period: Option<Metrics>,
    /// Unseen windows that made money (out of WINDOW_STARTS.len()).
    pub positive_windows: usize,
    /// Out-of-sample net with the single best window removed: a pass must not
    /// rest on one lucky stretch.
    pub net_without_best: f64,
    pub passed: bool,
    pub reasons: Vec<String>,
}

pub type XsReport = WfReport<XsParams>;

impl<P> WfReport<P> {
    /// Weaker than `passed`: out-of-sample profitable with no liquidations, every
    /// window found params, final params survive the 14 days, but the profit
    /// factor gate may be missed. Paper keeps forward-testing in this state.
    pub fn forward_ok(&self) -> bool {
        self.oos.execution_error.is_none()
            && self.windows.iter().all(|w| w.params.is_some())
            && self.oos.trades >= MIN_OOS_TRADES
            && self.oos.liquidations == 0
            && self.oos.net() > 0.0
            && self.full_period.as_ref().is_some_and(|f| {
                f.execution_error.is_none()
                    && f.net() > 0.0
                    && f.liquidations == 0
                    && f.max_drawdown_pct <= MAX_DRAWDOWN_PCT
            })
    }
}

/// Best params on `range` by objective, among those that trade enough, never
/// liquidate, and survive all data up to range.end (no liquidation, DD cap).
fn wf_best<P: Clone>(
    grid: &[P],
    range: std::ops::Range<usize>,
    bt: &dyn Fn(&P, std::ops::Range<usize>) -> Metrics,
    deadline: Instant,
    evaluated: &mut usize,
) -> Result<Option<(P, Metrics)>> {
    let gov = governor::global();
    let mut best: Option<(f64, P, Metrics)> = None;
    for p in grid {
        gov.checkpoint(deadline)?;
        *evaluated += 1;
        let r = bt(p, range.clone());
        if r.execution_error.is_some() || r.liquidations > 0 || r.trades < MIN_IS_TRADES {
            continue;
        }
        let survival = bt(p, 0..range.end);
        if survival.execution_error.is_some()
            || survival.liquidations > 0
            || survival.max_drawdown_pct > MAX_DRAWDOWN_PCT
        {
            continue;
        }
        let score = objective(&r);
        if best.as_ref().is_none_or(|b| score > b.0) {
            best = Some((score, p.clone(), r));
        }
    }
    Ok(best.map(|(_, p, m)| (p, m)))
}

/// The 14-day walk-forward with the gates in this module's docs. `bt(p, range)`
/// must backtest params `p` over bar range `range` from the start equity, using
/// only data up to range.end (the engines are causal; see their tests).
pub fn run_wf<P: Clone>(
    ts: &[i64],
    grid: &[P],
    equity: f64,
    deadline: Instant,
    bt: impl Fn(&P, std::ops::Range<usize>) -> Metrics,
) -> Result<WfReport<P>> {
    if ts.len() != BARS || ts.windows(2).any(|w| w[1] - w[0] != BAR_MS) {
        bail!(
            "walk-forward needs exactly {BARS} contiguous 15m bars (14 days), got {}",
            ts.len()
        );
    }
    let started = Instant::now();
    let mut evaluated = 0;
    let mut windows = Vec::new();
    let mut oos = Metrics {
        start_equity: equity,
        end_equity: equity,
        ..Default::default()
    };
    for &s in &WINDOW_STARTS {
        let (is_r, oos_r) = (s..s + IS_BARS, s + IS_BARS..s + IS_BARS + OOS_BARS);
        let best = wf_best(grid, is_r.clone(), &bt, deadline, &mut evaluated)?;
        let (params, is_m, oos_m) = match best {
            Some((p, is_m)) => {
                let o = bt(&p, oos_r.clone());
                if o.execution_error.is_some() {
                    oos.execution_error = o.execution_error.clone();
                }
                oos.rejected_rebalances += o.rejected_rebalances;
                oos.trades += o.trades;
                oos.wins += o.wins;
                oos.gross_profit += o.gross_profit;
                oos.gross_loss += o.gross_loss;
                oos.liquidations += o.liquidations;
                oos.max_drawdown_pct = oos.max_drawdown_pct.max(o.max_drawdown_pct);
                oos.end_equity += o.net();
                (Some(p), Some(is_m), Some(o))
            }
            None => (None, None, None),
        };
        windows.push(WfWindow {
            is_bars: (is_r.start, is_r.end),
            oos_bars: (oos_r.start, oos_r.end),
            params,
            in_sample: is_m,
            out_of_sample: oos_m,
        });
    }
    let final_best = wf_best(grid, BARS - IS_BARS..BARS, &bt, deadline, &mut evaluated)?;
    let full = final_best.as_ref().map(|(p, _)| bt(p, 0..BARS));
    let nets: Vec<f64> = windows
        .iter()
        .map(|w| w.out_of_sample.as_ref().map_or(0.0, |o| o.net()))
        .collect();
    let positive_windows = nets.iter().filter(|&&n| n > 0.0).count();
    let best = nets.iter().cloned().fold(0.0, f64::max);
    let net_without_best = nets.iter().sum::<f64>() - best;
    let mut reasons = Vec::new();
    if let Some(e) = &oos.execution_error {
        reasons.push(format!("invalid out-of-sample execution: {e}"));
    }
    if windows.iter().any(|w| w.params.is_none()) {
        reasons.push("a window had no params that traded enough without liquidating".into());
    }
    if oos.liquidations > 0 {
        reasons.push(format!("{} out-of-sample liquidation(s)", oos.liquidations));
    }
    if net_without_best <= 0.0 {
        reasons.push(format!("profit rests on one window: out-of-sample net without the best window {net_without_best:+.2} (need > 0)"));
    }
    if oos.trades < MIN_OOS_TRADES {
        reasons.push(format!(
            "only {} out-of-sample trades (need {MIN_OOS_TRADES})",
            oos.trades
        ));
    }
    if oos.profit_factor() <= MIN_PROFIT_FACTOR {
        reasons.push(format!(
            "out-of-sample profit factor {:.2} (need > {MIN_PROFIT_FACTOR})",
            oos.profit_factor()
        ));
    }
    if oos.net() <= 0.0 {
        reasons.push(format!("out-of-sample net {:.2} (need > 0)", oos.net()));
    }
    match &full {
        Some(f) if f.execution_error.is_some() => {
            reasons.push(format!("invalid final execution: {:?}", f.execution_error))
        }
        None => reasons.push("no params qualified on the most recent 8 days".into()),
        Some(f) if f.liquidations > 0 => {
            reasons.push("final params liquidate within the 14 days".into())
        }
        Some(f) if f.max_drawdown_pct > MAX_DRAWDOWN_PCT => {
            reasons.push(format!("final params drawdown {:.1}%", f.max_drawdown_pct))
        }
        Some(f) if f.net() <= 0.0 => {
            reasons.push(format!("final params lose {:.2} over 14 days", f.net()))
        }
        _ => {}
    }
    Ok(WfReport {
        first_bar_ts: ts[0],
        last_bar_ts: ts[BARS - 1],
        start_equity: equity,
        evaluated,
        elapsed_ms: started.elapsed().as_millis(),
        windows,
        oos,
        params: final_best.as_ref().map(|(p, _)| p.clone()),
        final_in_sample: final_best.map(|(_, m)| m),
        full_period: full,
        positive_windows,
        net_without_best,
        passed: reasons.is_empty(),
        reasons,
    })
}

pub fn run_xs_with(
    m: &Market,
    scores: &[Vec<Option<Score>>],
    equity: f64,
    deadline: Instant,
    grid: &[XsParams],
) -> Result<XsReport> {
    m.validate()?;
    run_wf(&m.ts, grid, equity, deadline, |p, r| {
        xs::backtest(m, scores, r, p, equity).metrics()
    })
}
