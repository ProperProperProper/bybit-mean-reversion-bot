//! Read-only strategy research on three independent 14-day windows of real
//! Bybit data (every test is exactly 14 days):
//!   W1 2026-08-20..09-03, W2 09-03..09-17, W3 09-17..10-01 (UTC, 08:30).
//! For each signal family (screener scores, return, funding) and for all of
//! them together:
//!   - the full 14-day walk-forward inside every window (must pass in ALL three);
//!   - a chronological forward test: params the walk-forward picks at the end of
//!     window k trade window k+1, which they have never seen (at 1x and 2x costs).
//! Run `fetch_research_data` first (window_1.db, window_2.db, window_3.db).
use bybit_mean_reversion_bot::engine::{
    data::Cache, scores, walkforward, xs, Market, BARS, BAR_MS,
};
use std::time::{Duration, Instant};

const RESEARCH_FIRST_BAR: i64 = 1_789_633_800_000;

fn window(dir: &std::path::Path, file: &str, last: i64) -> anyhow::Result<Market> {
    let cache = Cache::open(dir.join(file).to_str().unwrap())?;
    let (symbols, _) = cache.contents()?;
    cache.market(&symbols, last)
}

/// Start equity: the real account's USDT wallet balance (read-only), or EQ=... to
/// study another size explicitly. Never a built-in default.
fn start_equity() -> anyhow::Result<f64> {
    if let Ok(v) = std::env::var("EQ") {
        return Ok(v.parse()?);
    }
    tokio::runtime::Runtime::new()?.block_on(async {
        let creds = bybit_mean_reversion_bot::engine::keychain::load()?;
        bybit_mean_reversion_bot::engine::data::Client::new()?
            .usdt_wallet_balance(&creds)
            .await
    })
}

fn main() -> anyhow::Result<()> {
    let eq = start_equity()?;
    println!("start equity {eq} USDT");
    let dir = bybit_mean_reversion_bot::engine::runtime_dir();
    let span = BARS as i64 * BAR_MS;
    let w3_last = RESEARCH_FIRST_BAR + span - BAR_MS;
    let markets = [
        window(&dir, "window_1.db", w3_last - 2 * span)?,
        window(&dir, "window_2.db", w3_last - span)?,
        window(&dir, "window_3.db", w3_last)?,
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
        for k in 0..2 {
            let Some(p) = &reports[k].params else {
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
