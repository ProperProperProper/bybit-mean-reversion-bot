//! Read-only strategy research on three independent 14-day windows of real
//! Bybit data (every test is exactly 14 days):
//!   W1 2026-08-20..09-03, W2 09-03..09-17, W3 09-17..10-01 (UTC, 08:30).
//! For each signal family (screener scores, return, funding) and for all of
//! them together:
//!   - a full-window search inside every window (one 14-day window, no split;
//!     the best usable settings are in-sample by construction);
//!   - a chronological forward test: the settings chosen on window k trade
//!     window k+1, which they have never seen (at 1x and 2x costs).
//!
//! Run `fetch_research_data` first (window_1.db, window_2.db, window_3.db).
use bybit_mean_reversion_bot::engine::{research, scores, walkforward, xs, Market, BARS};
use std::time::{Duration, Instant};

// NOTE(agents): The research windows hold only today's top-20 coins (survivorship bias): long-only
//               results here are inflated. Compare with the buy-and-hold lines, and use signal_ic
//               for entry evidence.
fn main() -> anyhow::Result<()> {
    let eq = research::start_equity()?;
    println!("start equity {eq} USDT");
    let dir = bybit_mean_reversion_bot::engine::runtime_dir();
    let markets = [
        research::load_window(&dir, 1)?,
        research::load_window(&dir, 2)?,
        research::load_window(&dir, 3)?,
    ];
    let sc: Vec<_> = markets
        .iter()
        .map(|m| scores::compute(m, walkforward::UNIVERSE))
        .collect();
    // Forward tests trade one continuous market so indicators keep their history
    // across window edges (as live); settings are still chosen per 14-day window.
    let all = Market::concat(&markets)?;
    let sc_all = scores::compute(&all, walkforward::UNIVERSE);
    // Long-only families (the live mode) by default; NEUTRAL=1 compares the
    // market-neutral families instead.
    let neutral = std::env::var("NEUTRAL").is_ok();
    let mut families: Vec<(String, Vec<xs::XsParams>)> = walkforward::XS_SIGNALS
        .iter()
        .map(|&s| {
            let grid = if neutral {
                walkforward::xs_family_grid(s)
            } else {
                walkforward::long_family_grid(s)
            };
            (format!("{s:?}"), grid)
        })
        .collect();
    families.push((
        "ALL".into(),
        if neutral {
            walkforward::xs_grid()
        } else {
            walkforward::long_grid()
        },
    ));
    // Buy-and-hold references for each traded window: BTC alone, and every
    // coin of the window's universe equally weighted (open of the first bar to
    // close of the last, no costs).
    for (k, window) in markets.iter().enumerate().skip(1) {
        let range = k * BARS..(k + 1) * BARS;
        let hold = |s: usize| -> Option<f64> {
            Some(all.bars[s][range.end - 1]?.close / all.bars[s][range.start]?.open - 1.0)
        };
        let btc = all
            .symbols
            .iter()
            .position(|x| x == "BTCUSDT")
            .and_then(hold);
        let eq_weight: Vec<f64> = (0..all.symbols.len())
            .filter(|&s| window.symbols.contains(&all.symbols[s]))
            .filter_map(hold)
            .collect();
        println!(
            "buy and hold W{}: BTC {:+.1}%  equal-weight universe {:+.1}% ({} coins)",
            k + 1,
            btc.map_or(f64::NAN, |r| r * 100.0),
            eq_weight.iter().sum::<f64>() / eq_weight.len().max(1) as f64 * 100.0,
            eq_weight.len()
        );
    }

    for (name, grid) in &families {
        let started = Instant::now();
        let reports: Vec<_> = (0..3)
            .map(|k| {
                walkforward::search_full(
                    &markets[k],
                    &sc[k],
                    eq,
                    Instant::now() + Duration::from_secs(7200),
                    grid.len(),
                    |i| grid[i].clone(),
                )
            })
            .collect::<anyhow::Result<_>>()?;
        println!(
            "\n=== {name} ({} combos, {:.0}s)",
            grid.len(),
            started.elapsed().as_secs_f64()
        );
        for (k, r) in reports.iter().enumerate() {
            match &r.metrics {
                Some(m) => println!(
                    "  W{} best of {} usable: 14-day net {:+7.2} PF {:5.2} trades {:4} (in-sample: chosen on this window)",
                    k + 1,
                    r.usable,
                    m.net(),
                    m.profit_factor(),
                    m.trades
                ),
                None => println!("  W{}: no usable settings", k + 1),
            }
        }
        for (k, report) in reports.iter().enumerate().take(2) {
            let Some(p) = &report.params else {
                println!("  forward W{}->W{}: no usable settings", k + 1, k + 2);
                continue;
            };
            let a = xs::backtest_costs(&all, &sc_all, (k + 1) * BARS..(k + 2) * BARS, p, eq, 1.0)
                .metrics();
            let b = xs::backtest_costs(&all, &sc_all, (k + 1) * BARS..(k + 2) * BARS, p, eq, 2.0)
                .metrics();
            println!("  forward W{}->W{}: net {:+6.1}% (2x costs {:+6.1}%)  PF {:5.2}  trades {:4}  maxDD {:5.1}%  liq {}  | {:?} flip {} lb {} hold {} top {} lev {} stop {:?} {:?}",
                k + 1, k + 2, a.return_pct(), b.return_pct(), a.profit_factor(), a.trades, a.max_drawdown_pct, a.liquidations,
                p.signal, p.flip, p.lookback, p.hold, p.top, p.gross_leverage, p.stop_pct, p.regime);
        }
    }
    Ok(())
}
