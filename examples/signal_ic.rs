//! Does an entry signal pick coins that beat the other coins? Read-only, on the
//! three real research windows (`fetch_research_data` first), no network.
//!
//! Every 4 hours (from the screener warm-up on), for each signal and holding
//! period h: the rank correlation (Spearman) across the universe between the
//! signal at close t and each coin's return from the next open to the close h
//! bars later, relative to the universe average (so market moves cancel).
//! Also the average excess return of the 5 coins a long-only entry buys
//! (lowest values; the flipped direction is the negative of the highest).
//! A useful entry signal keeps the same sign in every window.
//!
//! Usage: `signal_ic [WINDOW_DIR]`. With a directory (window_k.db holding
//! candles for every eligible token, e.g. `fetch_research_data` HOLDOUT
//! output), the universe is the top 50 by turnover at each
//! bar among the stored symbols. Excluding delisted symbols still introduces
//! survivorship bias. Holding periods
//! do not overlap (one sample every h bars).
//!
//! `WINDOWS=-3,-2,-1,0` picks the windows (default 1,2,3); `SIGNAL=CalmDip`
//! restricts the study to one signal (a pre-registered holdout tests only the
//! signal it registered). The last column pools every sample.
use bybit_mean_reversion_bot::engine::{research, scores, walkforward, xs, Market};

const HORIZONS: [usize; 3] = [16, 32, 96];
const PICK: usize = 5;

fn ranks(v: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|&a, &b| v[a].total_cmp(&v[b]));
    let mut r = vec![0.0; v.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
            j += 1;
        }
        for &k in &idx[i..=j] {
            r[k] = (i + j) as f64 / 2.0;
        }
        i = j + 1;
    }
    r
}

fn spearman(a: &[f64], b: &[f64]) -> Option<f64> {
    let (ra, rb) = (ranks(a), ranks(b));
    let n = a.len() as f64;
    let (ma, mb) = (ra.iter().sum::<f64>() / n, rb.iter().sum::<f64>() / n);
    let cov: f64 = ra.iter().zip(&rb).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let va: f64 = ra.iter().map(|x| (x - ma).powi(2)).sum();
    let vb: f64 = rb.iter().map(|y| (y - mb).powi(2)).sum();
    (va > 0.0 && vb > 0.0).then(|| cov / (va * vb).sqrt())
}

struct Stat {
    ic: Vec<f64>,
    low_excess: Vec<f64>,
    high_excess: Vec<f64>,
    incomplete: usize,
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len().max(1) as f64
}

fn tstat(v: &[f64]) -> f64 {
    let m = mean(v);
    let sd = (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len().max(2) - 1) as f64).sqrt();
    if sd > 0.0 {
        m / sd * (v.len() as f64).sqrt()
    } else {
        0.0
    }
}

// NOTE(agents): Freeze membership using the decision-time signal BEFORE reading future prices.
//               Missing endpoints invalidate the whole sample; never replace a missing selected
//               coin with the next-ranked survivor. Report exclusions: complete-case samples can
//               still be biased. Non-overlapping holds do not establish statistical independence.
fn complete_returns(
    values: &[(usize, f64)],
    returns: impl Fn(usize) -> Option<f64>,
) -> Option<Vec<(f64, f64)>> {
    values
        .iter()
        .map(|&(s, v)| {
            let r = returns(s)?;
            r.is_finite().then_some((v, r))
        })
        .collect()
}

fn measure(m: &Market, sc: &[Vec<Option<scores::Score>>], p: &xs::XsParams, h: usize) -> Stat {
    let mut st = Stat {
        ic: vec![],
        low_excess: vec![],
        high_excess: vec![],
        incomplete: 0,
    };
    let mut t = 0;
    while t + h < m.ts.len() {
        let values: Vec<(usize, f64)> = (0..m.symbols.len())
            .filter_map(|s| xs::signal_value(m, sc, s, t, p).map(|v| (s, v)))
            .collect();
        if values.len() >= 2 * PICK {
            let Some(mut rows) = complete_returns(&values, |s| {
                let entry = m.bars[s][t + 1]?.open;
                let exit = m.bars[s][t + h]?.close;
                (entry.is_finite() && entry > 0.0 && exit.is_finite() && exit > 0.0)
                    .then_some(exit / entry - 1.0)
            }) else {
                st.incomplete += 1;
                t += h;
                continue;
            };
            let avg = rows.iter().map(|x| x.1).sum::<f64>() / rows.len() as f64;
            for x in &mut rows {
                x.1 -= avg;
            }
            let (v, r): (Vec<f64>, Vec<f64>) = rows.iter().copied().unzip();
            if let Some(ic) = spearman(&v, &r) {
                st.ic.push(ic);
            }
            rows.sort_by(|a, b| a.0.total_cmp(&b.0));
            st.low_excess
                .push(mean(&rows[..PICK].iter().map(|x| x.1).collect::<Vec<_>>()));
            st.high_excess.push(mean(
                &rows[rows.len() - PICK..]
                    .iter()
                    .map(|x| x.1)
                    .collect::<Vec<_>>(),
            ));
        }
        t += h;
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_future_price_cannot_replace_a_decision_time_member() {
        let values: Vec<_> = (0..12).map(|i| (i, i as f64)).collect();
        assert!(complete_returns(&values, |i| (i != 0).then_some(0.01)).is_none());
        assert!(complete_returns(&values, |i| (i != 11).then_some(0.01)).is_none());
        assert_eq!(complete_returns(&values, |_| Some(0.01)).unwrap().len(), 12);
        assert!(complete_returns(&values, |_| Some(f64::NAN)).is_none());
    }
}

// NOTE(agents): This is how entry signals get proven. Workflow: explore on research windows, then
//               commit the exact signal and pass criteria (pre-register) BEFORE fetching a fresh
//               holdout, then run once with SIGNAL= set. Never iterate on a holdout; once used it
//               is spent.
fn main() -> anyhow::Result<()> {
    let historical = std::env::args().nth(1).map(std::path::PathBuf::from);
    let dir = historical
        .clone()
        .unwrap_or_else(bybit_mean_reversion_bot::engine::runtime_dir);
    let windows: Vec<i64> = std::env::var("WINDOWS")
        .unwrap_or_else(|_| "1,2,3".into())
        .split(',')
        .map(|k| k.trim().parse())
        .collect::<Result<_, _>>()?;
    let only: Option<xs::Signal> = std::env::var("SIGNAL")
        .ok()
        .map(|s| serde_json::from_str(&format!("\"{s}\"")))
        .transpose()?;
    let markets: Vec<Market> = windows
        .iter()
        .map(|&k| research::load_window(&dir, k))
        .collect::<anyhow::Result<_>>()?;
    let sc: Vec<_> = markets
        .iter()
        .map(|m| scores::compute_with(m, walkforward::UNIVERSE, historical.is_none()))
        .collect();
    println!(
        "universe: {}",
        if historical.is_some() {
            "top 50 by turnover at each bar among stored symbols (delisted exclusions cause survivorship bias)"
        } else {
            "top 50 among today's tradeable coins (biased towards recent winners)"
        }
    );
    println!("Incomplete decision-time universes are excluded in full; remaining results are conditional on coverage. t-stat assumes independent samples.");
    println!("IC = mean Spearman(signal, next-open-to-close excess return); t = IC t-stat");
    println!("buy low / buy high = mean excess return (%) of the {PICK} lowest / highest values\n");
    for &signal in walkforward::XS_SIGNALS
        .iter()
        .filter(|s| only.is_none_or(|o| o == **s))
    {
        let lookbacks: &[usize] = if signal == xs::Signal::Return {
            &[4, 16, 96]
        } else {
            &[0]
        };
        for &lookback in lookbacks {
            let p = xs::XsParams {
                signal,
                flip: false,
                lookback,
                hold: 16,
                top: PICK,
                gross_leverage: 1.0,
                stop_pct: None,
                risk: Default::default(),
                long_only: true,
                regime: xs::Regime::Off,
            };
            for h in HORIZONS {
                let stats: Vec<Stat> = markets
                    .iter()
                    .zip(&sc)
                    .map(|(m, s)| measure(m, s, &p, h))
                    .collect();
                let pooled = Stat {
                    ic: stats.iter().flat_map(|x| x.ic.clone()).collect(),
                    low_excess: stats.iter().flat_map(|x| x.low_excess.clone()).collect(),
                    high_excess: stats.iter().flat_map(|x| x.high_excess.clone()).collect(),
                    incomplete: stats.iter().map(|x| x.incomplete).sum(),
                };
                let cells: Vec<String> = stats
                    .iter()
                    .chain(std::iter::once(&pooled))
                    .map(|st| {
                        format!(
                            "IC {:+.3} t {:+4.1} low {:+5.2}% (t {:+4.1}) high {:+5.2}% (t {:+4.1}) n {} incomplete {}",
                            mean(&st.ic),
                            tstat(&st.ic),
                            mean(&st.low_excess) * 100.0,
                            tstat(&st.low_excess),
                            mean(&st.high_excess) * 100.0,
                            tstat(&st.high_excess),
                            st.ic.len(),
                            st.incomplete
                        )
                    })
                    .collect();
                let labels = windows
                    .iter()
                    .map(|k| format!("W{k}"))
                    .chain(["pooled".into()]);
                let row: Vec<String> = labels
                    .zip(&cells)
                    .map(|(l, c)| format!("{l} {c}"))
                    .collect();
                println!(
                    "{:<12} lb {:>2} h {:>2} | {}",
                    format!("{signal:?}"),
                    lookback,
                    h,
                    row.join(" | ")
                );
            }
        }
    }
    Ok(())
}
