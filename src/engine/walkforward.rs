//! The parameter search over closed 15m bars of Bybit USDT perpetuals, plus the
//! strategy grids.
//!
//! User rule (2026-10-03): ONE window of exactly 14 days (1,344 bars), never
//! split into in-sample and out-of-sample parts. `search_full` backtests every
//! combination over the whole window; usable settings pass the two safety rules
//! (no liquidation, drawdown <= 25%) with at least `MIN_TRADES` trades, and the
//! best by `objective` challenges the settings paper is using (`choose_live`).
//! The honest check without splitting a window is chronological: choose on one
//! 14-day window, trade the next, never-seen one (dashboard chart, research).

use super::governor;
use super::metrics::Metrics;
use super::scores::Score;
use super::{Market, BARS};
use anyhow::{bail, Result};
use serde::Serialize;
use std::time::Instant;

/// The top 20 Bybit token USDT perpetuals by 24h turnover that have complete
/// Bybit rules (lot filter, account fee, margin tiers, measured order book).
pub const UNIVERSE: usize = 20;
/// Measure only the selected top 20. Missing rules reduce the count; do not
/// scan additional candidates beyond the user's cap.
pub const CANDIDATES: usize = UNIVERSE;
pub const MAX_DRAWDOWN_PCT: f64 = 25.0;

fn objective(m: &Metrics) -> f64 {
    m.return_pct() - 0.5 * m.max_drawdown_pct
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_grid_covers_every_parameter_value_once() {
        assert_eq!(LIVE_COMBOS, 2_949_120);
        // Every index decodes to a distinct combination (checked on a dense
        // stride plus both ends); every listed value of every parameter occurs.
        let picks: Vec<usize> = (0..LIVE_COMBOS)
            .step_by(97)
            .chain([LIVE_COMBOS - 1])
            .collect();
        let mut keys: Vec<String> = picks
            .iter()
            .map(|&i| format!("{:?}", live_combo(i)))
            .collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), picks.len());
        let all: Vec<XsParams> = (0..LIVE_COMBOS).step_by(7).map(live_combo).collect();
        assert!(all.iter().all(|p| p.long_only));
        for (s, lb, flip) in signal_variants() {
            assert!(all
                .iter()
                .any(|p| p.signal == s && p.lookback == lb && p.flip == flip));
        }
        for h in HOLDS {
            assert!(all.iter().any(|p| p.hold == h));
        }
        assert!(all.iter().any(|p| p.risk.vol_scaled) && all.iter().any(|p| !p.risk.vol_scaled));
        assert!(all
            .iter()
            .any(|p| p.risk.add_pct.is_some() && p.risk.breaker_pct.is_some()));
    }

    #[test]
    fn champion_is_kept_unless_the_new_best_scores_higher() {
        // Challenger better: switch.
        assert_eq!(
            pick(Some(("new", 2.0)), Some(("old", 1.0))),
            (Some("new"), Choice::NewBest)
        );
        // Challenger not better (lower or equal): keep the settings in use.
        assert_eq!(
            pick(Some(("new", 1.0)), Some(("old", 2.0))),
            (Some("old"), Choice::KeptLastBest)
        );
        assert_eq!(
            pick(Some(("new", 2.0)), Some(("old", 2.0))),
            (Some("old"), Choice::KeptLastBest)
        );
        // The search found nothing usable: keep the champion.
        assert_eq!(
            pick(None, Some(("old", -5.0))),
            (Some("old"), Choice::KeptLastBest)
        );
        // The champion fails the safety rules now: the challenger takes over.
        assert_eq!(
            pick(Some(("new", -1.0)), None),
            (Some("new"), Choice::NewBest)
        );
        // Same settings found again: kept, not "new".
        assert_eq!(
            pick(Some(("same", 3.0)), Some(("same", 3.0))),
            (Some("same"), Choice::KeptLastBest)
        );
        // Nothing passes the safety rules: no settings, paper opens nothing.
        assert_eq!(pick::<&str>(None, None), (None, Choice::NoneSafe));
    }
}

// ---------------------------------------------------------------- grids

use super::xs::{self, Regime, Signal, XsParams};

// NOTE(agents): User rule (2026-10-03): "test all". The live search varies EVERY strategy
//               parameter: all 8 signals in both directions (Return at two lookbacks), hold,
//               basket size, leverage, intrabar stop, take-profit, close-based stop, averaging
//               down, drawdown breaker, half-size after drawdown, volatility sizing and the BTC
//               filter. Value lists are sized so one search takes ~20 min (about 0.4 ms per
//               14-day backtest); adding values multiplies the time. Long only is a user rule.
const HOLDS: [usize; 8] = [8, 16, 24, 32, 64, 96, 144, 192];
const TOPS: [usize; 5] = [1, 2, 3, 4, 5];
const LEVERAGES: [f64; 4] = [1.0, 2.0, 3.0, 5.0];
const STOPS: [Option<f64>; 4] = [None, Some(5.0), Some(10.0), Some(20.0)];
const TAKE_PROFITS: [Option<f64>; 4] = [None, Some(5.0), Some(10.0), Some(40.0)];
const CLOSE_STOPS: [Option<f64>; 2] = [None, Some(10.0)];
const ADDS: [Option<f64>; 2] = [None, Some(10.0)];
const BREAKERS: [Option<f64>; 2] = [None, Some(15.0)];
const DERISKS: [Option<f64>; 2] = [None, Some(10.0)];
const VOL_SCALED: [bool; 2] = [false, true];
const REGIMES: [Regime; 2] = [Regime::Off, Regime::BtcTrend];

/// (signal, lookback, flip) for every signal: unflipped buys the lowest values,
/// flipped the highest; `Return` at each of its lookbacks. 18 variants.
fn signal_variants() -> Vec<(Signal, usize, bool)> {
    XS_SIGNALS
        .iter()
        .flat_map(|&s| {
            lookbacks(s)
                .iter()
                .flat_map(move |&lb| [false, true].map(|flip| (s, lb, flip)))
        })
        .collect()
}

/// Combinations in the live grid: 18 × 8 × 5 × 4 × 4 × 4 × 2⁶ = 2,949,120.
pub const LIVE_COMBOS: usize = 18
    * HOLDS.len()
    * TOPS.len()
    * LEVERAGES.len()
    * STOPS.len()
    * TAKE_PROFITS.len()
    * CLOSE_STOPS.len()
    * ADDS.len()
    * BREAKERS.len()
    * DERISKS.len()
    * VOL_SCALED.len()
    * REGIMES.len();

/// Live-grid combination `i` (0..LIVE_COMBOS), decoded in mixed radix so the
/// grid is never materialised. Not a profitability claim (docs/validation.md).
pub fn live_combo(i: usize) -> XsParams {
    let mut r = i;
    let mut take = |n: usize| {
        let d = r % n;
        r /= n;
        d
    };
    let regime = REGIMES[take(REGIMES.len())];
    let vol_scaled = VOL_SCALED[take(VOL_SCALED.len())];
    let derisk_pct = DERISKS[take(DERISKS.len())];
    let breaker_pct = BREAKERS[take(BREAKERS.len())];
    let add_pct = ADDS[take(ADDS.len())];
    let close_stop_pct = CLOSE_STOPS[take(CLOSE_STOPS.len())];
    let take_profit_pct = TAKE_PROFITS[take(TAKE_PROFITS.len())];
    let stop_pct = STOPS[take(STOPS.len())];
    let gross_leverage = LEVERAGES[take(LEVERAGES.len())];
    let top = TOPS[take(TOPS.len())];
    let hold = HOLDS[take(HOLDS.len())];
    let (signal, lookback, flip) = signal_variants()[take(18)];
    XsParams {
        signal,
        flip,
        lookback,
        hold,
        top,
        gross_leverage,
        stop_pct,
        risk: xs::Risk {
            close_stop_pct,
            take_profit_pct,
            add_pct,
            breaker_pct,
            vol_scaled,
            derisk_pct,
            short_stop_pct: None,
        },
        long_only: true,
        regime,
    }
}

/// Every ranking signal (screener scores, return, funding).
pub const XS_SIGNALS: [Signal; 8] = [
    Signal::Return,
    Signal::TrendScore,
    Signal::PriceAction,
    Signal::Volatility,
    Signal::Rsi,
    Signal::Pulse,
    Signal::Funding,
    Signal::CalmDip,
];

/// Lookbacks searched: only `Return` uses one.
fn lookbacks(signal: Signal) -> &'static [usize] {
    if signal == Signal::Return {
        &[16, 96]
    } else {
        &[0]
    }
}

/// Long-only grid over every signal.
pub fn long_grid() -> Vec<XsParams> {
    XS_SIGNALS
        .iter()
        .flat_map(|&s| long_family_grid(s))
        .collect()
}

/// Long-only grid for one entry signal: unflipped buys the lowest values
/// (contrarian), flipped the highest (momentum). 3 or 5 coins, a 10% stop or
/// none, the BTC trend filter off or on.
pub fn long_family_grid(signal: Signal) -> Vec<XsParams> {
    let mut out = Vec::new();
    for flip in [false, true] {
        for &lookback in lookbacks(signal) {
            for hold in [16usize, 32, 96] {
                for top in [3usize, 5] {
                    for gross_leverage in [1.0, 2.0] {
                        for stop_pct in [None, Some(10.0)] {
                            for regime in [Regime::Off, Regime::BtcTrend] {
                                out.push(XsParams {
                                    signal,
                                    flip,
                                    lookback,
                                    hold,
                                    top,
                                    gross_leverage,
                                    stop_pct,
                                    risk: Default::default(),
                                    long_only: true,
                                    regime,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// Market-neutral grid over every signal (research comparison).
pub fn xs_grid() -> Vec<XsParams> {
    XS_SIGNALS.iter().flat_map(|&s| xs_family_grid(s)).collect()
}

/// Market-neutral grid for one signal family (research comparison).
pub fn xs_family_grid(signal: Signal) -> Vec<XsParams> {
    let mut out = Vec::new();
    for flip in [false, true] {
        for &lookback in lookbacks(signal) {
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
                                long_only: false,
                                regime: Regime::Off,
                            });
                        }
                    }
                }
            }
        }
    }
    out
}

/// How this bar's paper settings were chosen (champion/challenger).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Choice {
    /// The search found settings that beat the ones in use (or none were in use).
    NewBest,
    /// Nothing scored higher: the settings already in use are kept.
    KeptLastBest,
    /// Neither the new best nor the settings in use pass the safety rules.
    NoneSafe,
}

// NOTE(agents): User design (2026-10-03): paper always trades the best known settings. The
//               search's best (challenger) replaces the settings in use (champion) only if it
//               scores strictly higher on the SAME latest data; otherwise the champion stays.
//               The safety rules are not optional: settings that liquidate or exceed the
//               drawdown cap over the 14 days are never used, champion included. The
//               walk-forward verdict is reported as information, not a trade gate.
/// Pick between the challenger and the champion by score; `None` scores are
/// unusable. A tie keeps the champion (no churn for equal evidence).
pub fn pick<P: Clone + PartialEq>(
    challenger: Option<(P, f64)>,
    champion: Option<(P, f64)>,
) -> (Option<P>, Choice) {
    match (challenger, champion) {
        (Some((c, cs)), Some((k, ks))) if c != k && cs > ks => (Some(c), Choice::NewBest),
        (_, Some((k, _))) => (Some(k), Choice::KeptLastBest),
        (Some((c, _)), None) => (Some(c), Choice::NewBest),
        (None, None) => (None, Choice::NoneSafe),
    }
}

/// Minimum closed trades over the 14 days for settings to be usable.
pub const MIN_TRADES: usize = 8;

// NOTE(agents): User rule (2026-10-03): the live search uses ONE 14-day window, never split into
//               in-sample/out-of-sample parts. Usable = the two safety rules (no liquidation,
//               drawdown <= MAX_DRAWDOWN_PCT) plus at least MIN_TRADES trades and no execution
//               error, all over the full 14 days.
fn usable(r: &Metrics) -> bool {
    r.execution_error.is_none()
        && r.liquidations == 0
        && r.max_drawdown_pct <= MAX_DRAWDOWN_PCT
        && r.trades >= MIN_TRADES
}

/// Score of `p` for live use: its objective over the full 14 days if usable
/// there (`usable`). `None` = not usable now.
pub fn live_score(
    m: &Market,
    scores: &[Vec<Option<Score>>],
    equity: f64,
    p: &XsParams,
) -> Option<f64> {
    let r = xs::backtest(m, scores, 0..BARS, p, equity).metrics();
    usable(&r).then(|| objective(&r))
}

/// One full-window parameter search.
#[derive(Debug, Clone, Serialize)]
pub struct SearchReport {
    pub first_bar_ts: i64,
    pub last_bar_ts: i64,
    pub start_equity: f64,
    /// Combinations in the grid, and how many were backtested before the deadline.
    pub combos: usize,
    pub evaluated: usize,
    /// Combinations that passed the safety rules and the minimum trade count.
    pub usable: usize,
    pub elapsed_ms: u128,
    /// The best usable settings by `objective` over the 14 days, with their metrics.
    pub params: Option<XsParams>,
    pub metrics: Option<Metrics>,
}

/// Backtest combinations `0..combos` (built by `combo(i)`, so millions never sit
/// in memory) over the full 14 days (`BARS`, no split) and keep the best usable
/// one by `objective`.
pub fn search_full(
    m: &Market,
    scores: &[Vec<Option<Score>>],
    equity: f64,
    deadline: Instant,
    combos: usize,
    combo: impl Fn(usize) -> XsParams,
) -> Result<SearchReport> {
    m.validate()?;
    if m.ts.len() != BARS {
        bail!(
            "search needs exactly {BARS} bars (14 days), got {}",
            m.ts.len()
        );
    }
    let started = Instant::now();
    let gov = governor::global();
    let (mut evaluated, mut usable_n) = (0, 0);
    let mut best: Option<(f64, XsParams, Metrics)> = None;
    for i in 0..combos {
        gov.checkpoint(deadline)?;
        evaluated += 1;
        let p = combo(i);
        let r = xs::backtest(m, scores, 0..BARS, &p, equity).metrics();
        if !usable(&r) {
            continue;
        }
        usable_n += 1;
        let score = objective(&r);
        if best.as_ref().is_none_or(|b| score > b.0) {
            best = Some((score, p, r));
        }
    }
    Ok(SearchReport {
        first_bar_ts: m.ts[0],
        last_bar_ts: m.ts[BARS - 1],
        start_equity: equity,
        combos,
        evaluated,
        usable: usable_n,
        elapsed_ms: started.elapsed().as_millis(),
        params: best.as_ref().map(|b| b.1.clone()),
        metrics: best.map(|b| b.2),
    })
}

/// The settings paper trades this bar: the search's best (`challenger`) or
/// the settings in use (`champion`), each re-scored on the current market.
pub fn choose_live(
    m: &Market,
    scores: &[Vec<Option<Score>>],
    equity: f64,
    challenger: Option<&XsParams>,
    champion: Option<&XsParams>,
) -> (Option<XsParams>, Choice) {
    let score = |p: Option<&XsParams>| {
        p.and_then(|p| live_score(m, scores, equity, p).map(|s| (p.clone(), s)))
    };
    pick(score(challenger), score(champion))
}
