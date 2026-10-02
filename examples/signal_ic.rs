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
//! Usage: `signal_ic [WINDOW_DIR]`. With a directory (window_1.db..window_3.db
//! holding candles for every perpetual that traded then, e.g. a backup made
//! before the top-50 refetch), the universe is the top 50 by turnover at each
//! bar among all of them: no bias towards today's survivors. Holding periods
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

fn measure(m: &Market, sc: &[Vec<Option<scores::Score>>], p: &xs::XsParams, h: usize) -> Stat {
    let mut st = Stat {
        ic: vec![],
        low_excess: vec![],
        high_excess: vec![],
    };
    let mut t = 0;
    while t + h < m.ts.len() {
        let mut rows: Vec<(f64, f64)> = (0..m.symbols.len())
            .filter_map(|s| {
                let v = xs::signal_value(m, sc, s, t, p)?;
                let r = m.bars[s][t + h]?.close / m.bars[s][t + 1]?.open - 1.0;
                r.is_finite().then_some((v, r))
            })
            .collect();
        if rows.len() >= 2 * PICK {
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
            "top 50 by turnover at each bar among every perpetual that traded then"
        } else {
            "top 50 among today's tradeable coins (biased towards recent winners)"
        }
    );
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
                };
                let cells: Vec<String> = stats
                    .iter()
                    .chain(std::iter::once(&pooled))
                    .map(|st| {
                        format!(
                            "IC {:+.3} t {:+4.1} low {:+5.2}% (t {:+4.1}) high {:+5.2}% (t {:+4.1}) n {}",
                            mean(&st.ic),
                            tstat(&st.ic),
                            mean(&st.low_excess) * 100.0,
                            tstat(&st.low_excess),
                            mean(&st.high_excess) * 100.0,
                            tstat(&st.high_excess),
                            st.ic.len()
                        )
                    })
                    .collect();
                let labels = windows.iter().map(|k| format!("W{k}")).chain(["pooled".into()]);
                let row: Vec<String> = labels.zip(&cells).map(|(l, c)| format!("{l} {c}")).collect();
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
