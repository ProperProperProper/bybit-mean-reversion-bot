//! Screener scores per symbol per closed 15m bar. Everything at bar t uses
//! bars <= t only. Cross-sectional parts compare a symbol with the other
//! symbols in the universe at the SAME bar.
//!
//! * Volatility: high-low range as % over the last 1, 2, 4, 16, 96 bars
//!   (15m, 30m, 1h, 4h, 24h), each scaled by 1/sqrt(bars) so windows are
//!   comparable, averaged with weights 5:4:3:2:1 (recent intervals weigh more).
//! * Price action (signed): distance of close from EMA 8/21/55 in %, weights
//!   3:2:1 (recent pricing weighs more), divided by the 15m-equivalent volatility.
//! * Volume: Z-score of the bar's volume vs the previous 20 bars.
//! * Universe at t: the top `universe` symbols by rolling 24h turnover at t.
//! * volatility_score / volume_score / pa_strength: percentiles within the universe.
//! * rank_value = mean of those three; rank = position (1 = highest).
//! * price_action_score = percentile of the signed price action (0 bearish .. 100 bullish).
//! * trend_score = 50 + (price_action_score - 50) * rank_value/100, clamped to 1..100:
//!   strongly directional only when the pair is also highly ranked.

use super::{Bar, Market};
use serde::Serialize;

pub const WINDOWS: [usize; 5] = [1, 2, 4, 16, 96];
const WINDOW_WEIGHTS: [f64; 5] = [5.0, 4.0, 3.0, 2.0, 1.0];
const EMAS: [usize; 3] = [8, 21, 55];
const EMA_WEIGHTS: [f64; 3] = [3.0, 2.0, 1.0];
const VOLUME_LOOKBACK: usize = 20;
/// Bars of contiguous history needed before a symbol is scored.
pub const WARMUP: usize = 110;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Score {
    pub volatility_score: f64,
    pub volume_score: f64,
    pub pa_strength: f64,
    pub price_action_score: f64,
    pub rank_value: f64,
    pub rank: usize,
    pub trend_score: f64,
    pub rsi14: f64,
    pub turnover_24h: f64,
    /// Pulse score, 0-100: sigmoid(Z_v * V_R * D_M), V_R = BBW(15m)/BBW(4h),
    /// D_M = 1 + |slope%| in the side's direction else 0. Slope is the 5-bar OLS
    /// slope as % of price per bar (comparable across pairs); shorts mirror longs.
    /// None until 4h history (20 rolling 4h closes) exists.
    pub pulse_long: Option<f64>,
    pub pulse_short: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct Raw {
    vol: f64,
    pa: f64,
    vol_z: f64,
    rsi: f64,
    turnover_24h: f64,
    pulse: Option<(f64, f64)>,
}

/// Per-symbol raw features; None until WARMUP contiguous bars exist.
fn raw_features(bars: &[Option<Bar>]) -> Vec<Option<Raw>> {
    let n = bars.len();
    let mut out = vec![None; n];
    let mut run_start = 0usize; // start of the current contiguous run
    let mut emas = [0.0f64; 3];
    let (mut avg_gain, mut avg_loss) = (0.0, 0.0);
    for t in 0..n {
        let Some(b) = bars[t] else {
            run_start = t + 1;
            continue;
        };
        let len = t + 1 - run_start;
        if len == 1 {
            emas = [b.close; 3];
            avg_gain = 0.0;
            avg_loss = 0.0;
            continue;
        }
        let prev = bars[t - 1].expect("contiguous");
        for (i, p) in EMAS.iter().enumerate() {
            let k = 2.0 / (*p as f64 + 1.0);
            emas[i] = b.close * k + emas[i] * (1.0 - k);
        }
        let d = b.close - prev.close;
        avg_gain = (avg_gain * 13.0 + d.max(0.0)) / 14.0;
        avg_loss = (avg_loss * 13.0 + (-d).max(0.0)) / 14.0;
        if len < WARMUP {
            continue;
        }
        let w = |k: usize| &bars[t + 1 - k..=t];
        let mut vol = 0.0;
        for (i, k) in WINDOWS.iter().enumerate() {
            let win = w(*k);
            let hi = win.iter().map(|x| x.unwrap().high).fold(f64::MIN, f64::max);
            let lo = win.iter().map(|x| x.unwrap().low).fold(f64::MAX, f64::min);
            let base = bars[t - k].unwrap().close;
            vol += WINDOW_WEIGHTS[i] * (hi - lo) / base * 100.0 / (*k as f64).sqrt();
        }
        vol /= WINDOW_WEIGHTS.iter().sum::<f64>();
        let mut pa = 0.0;
        for i in 0..3 {
            pa += EMA_WEIGHTS[i] * (b.close / emas[i] - 1.0) * 100.0;
        }
        pa /= EMA_WEIGHTS.iter().sum::<f64>();
        let pa = if vol > 0.0 { pa / vol } else { 0.0 };
        let prior: Vec<f64> = bars[t - VOLUME_LOOKBACK..t]
            .iter()
            .map(|x| x.unwrap().volume)
            .collect();
        let mean = prior.iter().sum::<f64>() / VOLUME_LOOKBACK as f64;
        let sd =
            (prior.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / VOLUME_LOOKBACK as f64).sqrt();
        let vol_z = if sd > 0.0 {
            (b.volume - mean) / sd
        } else {
            0.0
        };
        let rsi = if avg_loss == 0.0 {
            100.0
        } else {
            100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
        };
        let turnover_24h = w(96).iter().map(|x| x.unwrap().turnover).sum::<f64>();
        let pulse = pulse_scores(bars, t, run_start, vol_z);
        out[t] = Some(Raw {
            vol,
            pa,
            vol_z,
            rsi,
            turnover_24h,
            pulse,
        });
    }
    out
}

/// Bollinger band width over `closes`: 4 * population sd / mean.
fn bbw(closes: &[f64]) -> Option<f64> {
    let n = closes.len() as f64;
    let mean = closes.iter().sum::<f64>() / n;
    let sd = (closes.iter().map(|c| (c - mean).powi(2)).sum::<f64>() / n).sqrt();
    (mean > 0.0).then(|| 4.0 * sd / mean)
}

fn sigmoid100(x: f64) -> f64 {
    100.0 / (1.0 + (-x).exp())
}

/// (long, short) Pulse scores at bar t using bars <= t only.
fn pulse_scores(
    bars: &[Option<Bar>],
    t: usize,
    run_start: usize,
    vol_z: f64,
) -> Option<(f64, f64)> {
    const N: usize = 20;
    const H4: usize = 16;
    if t + 1 < run_start + (N - 1) * H4 + 1 || t < N {
        return None;
    }
    let close = |i: usize| bars[i].unwrap().close;
    let bbw15 = bbw(&(t + 1 - N..=t).map(close).collect::<Vec<_>>())?;
    let bbw4h = bbw(&(0..N).map(|i| close(t - i * H4)).collect::<Vec<_>>())?;
    if bbw4h <= 1e-12 {
        return None;
    }
    let vr = bbw15 / bbw4h;
    let ys: Vec<f64> = (t - 4..=t).map(close).collect();
    let mean = ys.iter().sum::<f64>() / 5.0;
    let slope = ys
        .iter()
        .enumerate()
        .map(|(i, y)| (i as f64 - 2.0) * (y - mean))
        .sum::<f64>()
        / 10.0
        / mean
        * 100.0;
    let dm_long = if slope > 0.0 { 1.0 + slope } else { 0.0 };
    let dm_short = if slope < 0.0 { 1.0 - slope } else { 0.0 };
    Some((
        sigmoid100(vol_z * vr * dm_long),
        sigmoid100(vol_z * vr * dm_short),
    ))
}

/// Percentile (0-100) of `x` within `all` (share of values <= x).
fn percentile(sorted: &[f64], x: f64) -> f64 {
    let le = sorted.partition_point(|v| *v <= x);
    if sorted.len() <= 1 {
        50.0
    } else {
        (le as f64 - 1.0) / (sorted.len() as f64 - 1.0) * 100.0
    }
}

/// Scores `[symbol][bar]`, None outside the universe or during warmup.
pub fn compute(m: &Market, universe: usize) -> Vec<Vec<Option<Score>>> {
    let raws: Vec<Vec<Option<Raw>>> = m
        .bars
        .iter()
        .enumerate()
        .map(|(s, bars)| {
            if let Some(launch) = m.listing_times.get(s).copied().flatten() {
                let first_full = ((launch + crate::engine::BAR_MS - 1) / crate::engine::BAR_MS)
                    * crate::engine::BAR_MS;
                let eligible: Vec<Option<Bar>> = bars
                    .iter()
                    .zip(&m.ts)
                    .map(|(bar, ts)| if *ts < first_full { None } else { *bar })
                    .collect();
                raw_features(&eligible)
            } else {
                raw_features(bars)
            }
        })
        .collect();
    let mut out = vec![vec![None; m.ts.len()]; m.symbols.len()];
    for t in 0..m.ts.len() {
        let mut members: Vec<(usize, Raw)> = (0..m.symbols.len())
            .filter_map(|s| {
                if !m.entry_eligible.get(s).copied().unwrap_or(true) {
                    return None;
                }
                m.rules(s)?;
                if let Some(launch) = m.listing_times.get(s).copied().flatten() {
                    let first_full = ((launch + crate::engine::BAR_MS - 1) / crate::engine::BAR_MS)
                        * crate::engine::BAR_MS;
                    if m.ts[t] < first_full + (WARMUP as i64 - 1) * crate::engine::BAR_MS {
                        return None;
                    }
                }
                raws[s][t].map(|r| (s, r))
            })
            .collect();
        members.sort_by(|a, b| b.1.turnover_24h.total_cmp(&a.1.turnover_24h));
        members.truncate(universe);
        if members.len() < 2 {
            continue;
        }
        let sorted = |f: &dyn Fn(&Raw) -> f64| {
            let mut v: Vec<f64> = members.iter().map(|(_, r)| f(r)).collect();
            v.sort_by(f64::total_cmp);
            v
        };
        let (vols, zs, strengths, pas) = (
            sorted(&|r| r.vol),
            sorted(&|r| r.vol_z),
            sorted(&|r| r.pa.abs()),
            sorted(&|r| r.pa),
        );
        let mut scored: Vec<(usize, Score)> = members
            .iter()
            .map(|(s, r)| {
                let (vs, zs_, ps) = (
                    percentile(&vols, r.vol),
                    percentile(&zs, r.vol_z),
                    percentile(&strengths, r.pa.abs()),
                );
                let rank_value = (vs + zs_ + ps) / 3.0;
                let pas_ = percentile(&pas, r.pa);
                (
                    *s,
                    Score {
                        volatility_score: vs,
                        volume_score: zs_,
                        pa_strength: ps,
                        price_action_score: pas_,
                        rank_value,
                        rank: 0,
                        trend_score: (50.0 + (pas_ - 50.0) * rank_value / 100.0).clamp(1.0, 100.0),
                        rsi14: r.rsi,
                        turnover_24h: r.turnover_24h,
                        pulse_long: r.pulse.map(|p| p.0),
                        pulse_short: r.pulse.map(|p| p.1),
                    },
                )
            })
            .collect();
        scored.sort_by(|a, b| b.1.rank_value.total_cmp(&a.1.rank_value));
        for (i, (s, mut sc)) in scored.into_iter().enumerate() {
            sc.rank = i + 1;
            out[s][t] = Some(sc);
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::BAR_MS;

    #[test]
    fn listing_time_excludes_prelisting_history_and_requires_warmup() {
        let series: Vec<f64> = (0..250).map(|i| 100.0 + (i as f64 * 0.1).sin()).collect();
        let mut m = market(&[series.clone(), series]);
        m.listing_times = vec![Some(100 * BAR_MS + 1); 2];
        let sc = compute(&m, 100);
        assert!(sc[0][..210].iter().all(Option::is_none));
        assert!(sc[0][210].is_some());
    }

    pub fn market(series: &[Vec<f64>]) -> Market {
        let n = series[0].len();
        let mut m = Market {
            marks: Vec::new(),
            listing_times: Vec::new(),
            entry_eligible: Vec::new(),
            ts: (0..n as i64).map(|i| i * BAR_MS).collect(),
            symbols: (0..series.len()).map(|i| format!("S{i}USDT")).collect(),
            bars: series
                .iter()
                .enumerate()
                .map(|(k, s)| {
                    s.iter()
                        .enumerate()
                        .map(|(i, &c)| {
                            let o = if i == 0 { c } else { s[i - 1] };
                            Some(Bar {
                                open: o,
                                high: o.max(c) * 1.001,
                                low: o.min(c) * 0.999,
                                close: c,
                                volume: 100.0 + (i % 7) as f64 * 10.0,
                                turnover: (1000.0 + k as f64) * c,
                            })
                        })
                        .collect()
                })
                .collect(),
            funding: vec![vec![]; series.len()],
            rules: vec![Some(crate::engine::rules::Rules::test_liquid()); series.len()],
        };
        m.marks = m.bars.clone();
        m
    }

    #[test]
    fn scores_never_look_ahead() {
        let a: Vec<f64> = (0..300)
            .map(|i| 100.0 + (i as f64 * 0.11).sin() * 3.0)
            .collect();
        let b: Vec<f64> = (0..300)
            .map(|i| 50.0 + (i as f64 * 0.07).cos() * 2.0 + i as f64 * 0.01)
            .collect();
        let c: Vec<f64> = (0..300).map(|i| 10.0 + (i as f64 * 0.05).sin()).collect();
        let full = compute(&market(&[a.clone(), b.clone(), c.clone()]), 10);
        let pre = compute(
            &market(&[a[..200].to_vec(), b[..200].to_vec(), c[..200].to_vec()]),
            10,
        );
        for s in 0..3 {
            assert_eq!(full[s][..200], pre[s][..]);
        }
    }

    #[test]
    fn rally_is_bullish_and_selloff_bearish() {
        let flat: Vec<f64> = (0..200)
            .map(|i| 100.0 + (i as f64 * 0.3).sin() * 0.2)
            .collect();
        let mut up = flat.clone();
        let mut down = flat.clone();
        for i in 180..200 {
            up[i] = up[179] * (1.0 + 0.004 * (i - 179) as f64);
            down[i] = down[179] * (1.0 - 0.004 * (i - 179) as f64);
        }
        let sc = compute(&market(&[flat, up, down]), 10);
        let (u, d) = (sc[1][199].unwrap(), sc[2][199].unwrap());
        assert!(u.trend_score > 50.0 && d.trend_score < 50.0, "{u:?} {d:?}");
        assert!(u.price_action_score > d.price_action_score);
    }

    #[test]
    fn pulse_is_causal_and_mirrors_for_shorts() {
        let base: Vec<f64> = (0..400)
            .map(|i| 100.0 + (i as f64 * 0.05).sin() * 2.0)
            .collect();
        let mut up = base.clone();
        let mut down = base.clone();
        for i in 395..400 {
            up[i] = up[394] * (1.0 + 0.006 * (i - 394) as f64);
            down[i] = down[394] * (1.0 - 0.006 * (i - 394) as f64);
        }
        let mut m = market(&[base.clone(), up, down]);
        for s in 1..3 {
            for i in 395..400 {
                if let Some(b) = m.bars[s][i].as_mut() {
                    b.volume *= 20.0;
                }
            }
        }
        let sc = compute(&m, 10);
        let (u, d) = (sc[1][399].unwrap(), sc[2][399].unwrap());
        assert!(
            u.pulse_long.unwrap() > 60.0 && u.pulse_short.unwrap() == 50.0,
            "{u:?}"
        );
        assert!(
            d.pulse_short.unwrap() > 60.0 && d.pulse_long.unwrap() == 50.0,
            "{d:?}"
        );
        // No 4h history yet -> no Pulse score.
        assert!(sc[0][200].unwrap().pulse_long.is_none());
        // Causal: identical before the burst whatever happens later.
        let pre = compute(
            &market(&[
                base[..390].to_vec(),
                base[..390].to_vec(),
                base[..390].to_vec(),
            ]),
            10,
        );
        assert_eq!(
            compute(&market(&[base.clone(), base.clone(), base.clone()]), 10)[0][..390],
            pre[0][..]
        );
    }

    #[test]
    fn universe_is_limited_by_turnover() {
        let s: Vec<Vec<f64>> = (0..5)
            .map(|k| {
                (0..150)
                    .map(|i| 10.0 + k as f64 + (i as f64 * 0.1).sin())
                    .collect()
            })
            .collect();
        let sc = compute(&market(&s), 3);
        let scored = (0..5).filter(|&k| sc[k][149].is_some()).count();
        assert_eq!(scored, 3);
    }
}
