//! Read-only strategy research on three independent 14-day windows of real
//! Bybit data (every test is exactly 14 days):
//!   W1 2026-08-20..09-03, W2 09-03..09-17, W3 09-17..10-01 (UTC, 08:30).
//! For each signal family (screener scores, return, funding) and for all of
//! them together:
//!   - the full 14-day walk-forward inside every window (must pass in ALL three);
//!   - a chronological forward test: params the walk-forward picks at the end of
//!     window k trade window k+1, which they have never seen (at 1x and 2x costs).
//!
//! Run `fetch_research_data` first (window_1.db, window_2.db, window_3.db).
use bybit_mean_reversion_bot::engine::{research, scores, walkforward, xs, Market, BARS};
use std::time::{Duration, Instant};

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
    let mut families: Vec<(String, Vec<xs::XsParams>)> = walkforward::XS_SIGNALS
        .iter()
        .map(|&s| (format!("{s:?}"), walkforward::xs_family_grid(s)))
        .collect();
    families.push(("ALL".into(), walkforward::xs_grid()));

    for (name, grid) in &families {
        let started = Instant::now();
        let reports: Vec<_> = (0..3)
            .map(|k| {
                walkforward::run_xs_with(
                    &markets[k],
                    &sc[k],
                    eq,
                    Instant::now() + Duration::from_secs(7200),
                    grid,
                )
            })
            .collect::<anyhow::Result<_>>()?;
        println!(
            "\n=== {name} ({} combos, {:.0}s)",
            grid.len(),
            started.elapsed().as_secs_f64()
        );
        for (k, r) in reports.iter().enumerate() {
            println!(
                "  W{} walk-forward {:6}  OOS net {:+7.2}  PF {:5.2}  trades {:4}  liq {}{}",
                k + 1,
                if r.passed { "PASS" } else { "fail" },
                r.oos.net(),
                r.oos.profit_factor(),
                r.oos.trades,
                r.oos.liquidations,
                if r.passed {
                    String::new()
                } else {
                    format!("  ({})", r.reasons.join("; "))
                }
            );
        }
        for (k, report) in reports.iter().enumerate().take(2) {
            let Some(p) = &report.params else {
                println!("  forward W{}->W{}: no params qualified", k + 1, k + 2);
                continue;
            };
            let a = xs::backtest_costs(&all, &sc_all, (k + 1) * BARS..(k + 2) * BARS, p, eq, 1.0)
                .metrics();
            let b = xs::backtest_costs(&all, &sc_all, (k + 1) * BARS..(k + 2) * BARS, p, eq, 2.0)
                .metrics();
            println!("  forward W{}->W{}: net {:+6.1}% (2x costs {:+6.1}%)  PF {:5.2}  trades {:4}  maxDD {:5.1}%  liq {}  | {:?} flip {} lb {} hold {} top {} lev {} stop {:?}",
                k + 1, k + 2, a.return_pct(), b.return_pct(), a.profit_factor(), a.trades, a.max_drawdown_pct, a.liquidations,
                p.signal, p.flip, p.lookback, p.hold, p.top, p.gross_leverage, p.stop_pct);
        }
    }
    Ok(())
}
