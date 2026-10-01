//! The trading engine for Bybit USDT perpetuals on closed 15-minute bars.
//!
//! * `data` — real Bybit REST data: closed bars, settled funding, instruments,
//!   risk limits, order books, the account's fee rates and balance (read-only),
//!   cached in SQLite.
//! * `rules` — per-symbol Bybit trading rules: lot rules, the account's taker
//!   fee, margin tiers and the measured order-book cost of each order size.
//! * `scores` — the screener: volatility, price action, volume, activity rank,
//!   trend score and Pulse, computed causally from closed bars.
//! * `xs` — the strategy: cross-sectional, market-neutral ranking (live:
//!   contrarian Pulse, `walkforward::LIVE_SIGNAL`), with optional drawdown rules.
//! * `walkforward` — the 14-day walk-forward gate and the strategy grids.
//! * `metrics` — trade records and performance metrics.
//! * `governor`, `supervisor`, `keychain` — CPU cap, task restarts, credentials.
//!
//! Decisions are made at a bar's close and filled at the next open (or at a
//! level inside a later bar, adverse first): no lookahead, no repainting; the
//! same engine drives backtest, walk-forward and paper trading.

pub mod data;
pub mod governor;
pub mod keychain;
pub mod metrics;
pub mod rules;
pub mod scores;
pub mod supervisor;
pub mod walkforward;
pub mod xs;

/// Runtime data folder (bars, rules, research windows, paper account, logs). It
/// lives outside ~/Documents: macOS privacy protection blocks launchd jobs there.
pub fn runtime_dir() -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Library/Application Support/BybitMeanReversionBot");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Locked: 15-minute bars, 14 days of data for every test.
pub const INTERVAL: &str = "15";
pub const BAR_MS: i64 = 15 * 60 * 1000;
pub const TEST_DAYS: i64 = 14;
pub const BARS: usize = (TEST_DAYS * 24 * 4) as usize; // 1344

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Bar {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Base-asset volume.
    pub volume: f64,
    /// Quote (USDT) turnover.
    pub turnover: f64,
}

/// All symbols on one common timeline of closed bars. `bars[s][t]` is None when
/// symbol `s` has no bar at `ts[t]` (not listed yet, or a gap).
#[derive(Debug, Clone, Default)]
pub struct Market {
    pub ts: Vec<i64>,
    pub symbols: Vec<String>,
    pub bars: Vec<Vec<Option<Bar>>>,
    /// Real settled funding (timestamp ms, rate) per symbol, ascending.
    pub funding: Vec<Vec<(i64, f64)>>,
    /// Bybit trading rules per symbol (`rules::Rules`: order rules, account fee,
    /// margin tiers, measured order-book cost). None = unknown: not traded.
    pub rules: Vec<Option<rules::Rules>>,
}

impl Market {
    /// The symbol's Bybit rules; None means the symbol must not be traded.
    pub fn rules(&self, sym: usize) -> Option<&rules::Rules> {
        self.rules.get(sym).and_then(|r| r.as_ref())
    }

    /// Join back-to-back markets into one continuous timeline (union of symbols;
    /// a symbol missing from one part has no bars there). Indicators computed on
    /// the result carry their history across the joins, as they do live.
    pub fn concat(parts: &[Market]) -> anyhow::Result<Market> {
        for w in parts.windows(2) {
            let (a, b) = (w[0].ts.last(), w[1].ts.first());
            anyhow::ensure!(
                matches!((a, b), (Some(a), Some(b)) if b - a == BAR_MS),
                "markets are not back-to-back"
            );
        }
        let mut symbols: Vec<String> = parts
            .iter()
            .flat_map(|m| m.symbols.iter().cloned())
            .collect();
        symbols.sort();
        symbols.dedup();
        let mut out = Market {
            ts: parts.iter().flat_map(|m| m.ts.iter().copied()).collect(),
            symbols,
            ..Default::default()
        };
        for s in &out.symbols {
            let (mut bars, mut funding) = (Vec::with_capacity(out.ts.len()), Vec::new());
            for m in parts {
                match m.symbols.iter().position(|x| x == s) {
                    Some(i) => {
                        bars.extend_from_slice(&m.bars[i]);
                        for &f in &m.funding[i] {
                            if funding.last().is_none_or(|l: &(i64, f64)| f.0 > l.0) {
                                funding.push(f);
                            }
                        }
                    }
                    None => bars.extend(std::iter::repeat_n(None, m.ts.len())),
                }
            }
            out.bars.push(bars);
            out.funding.push(funding);
            // The most recent part's rules (they are measured, newest is best).
            out.rules.push(parts.iter().rev().find_map(|m| {
                let i = m.symbols.iter().position(|x| x == s)?;
                m.rules.get(i).cloned().flatten()
            }));
        }
        Ok(out)
    }

    pub fn len(&self) -> usize {
        self.ts.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ts.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub fn sign(self) -> f64 {
        match self {
            Side::Long => 1.0,
            Side::Short => -1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(m: &Market, r: std::ops::Range<usize>) -> Market {
        Market {
            ts: m.ts[r.clone()].to_vec(),
            symbols: m.symbols.clone(),
            bars: m.bars.iter().map(|b| b[r.clone()].to_vec()).collect(),
            funding: m.funding.clone(),
            rules: m.rules.clone(),
        }
    }

    /// Joined windows behave exactly like one continuous series: scores at the
    /// join carry their history instead of restarting a warm-up.
    #[test]
    fn concat_is_continuous() {
        let series: Vec<Vec<f64>> = (0..4)
            .map(|k| {
                (0..500)
                    .map(|i| 100.0 + ((i as f64) * (0.04 + k as f64 * 0.01)).sin() * 5.0)
                    .collect()
            })
            .collect();
        let full = scores::tests::market(&series);
        let joined = Market::concat(&[slice(&full, 0..200), slice(&full, 200..500)]).unwrap();
        assert_eq!(joined.ts, full.ts);
        assert_eq!(scores::compute(&joined, 100), scores::compute(&full, 100));
        assert!(
            Market::concat(&[slice(&full, 0..200), slice(&full, 201..500)]).is_err(),
            "gaps are rejected"
        );
    }
}
