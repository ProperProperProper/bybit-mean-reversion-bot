//! Read-only: drawdown handling for contrarian Pulse, compared on the two 14-day
//! forward tests (the walk-forward picks the other settings on one 14-day window
//! exactly as live; they then trade the next unseen 14 days, with the screener's
//! look-back carried over). Every rule is decided on a 15m close and executed at
//! the next open. The list was fixed before looking at any result; each rule is
//! tested on its own. Start equity = the real account balance (or EQ=...);
//! real Bybit data and rules (window_{1,2,3}.db).
use bybit_mean_reversion_bot::engine::xs::{Risk, XsParams};
use bybit_mean_reversion_bot::engine::{research, scores, walkforward, xs, Market, BARS};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let eq = research::start_equity()?;
    println!("start equity {eq:.2} USDT");
    let dir = bybit_mean_reversion_bot::engine::runtime_dir();
    let markets: Vec<Market> = (1..=3)
        .map(|k| research::load_window(&dir, k))
        .collect::<anyhow::Result<_>>()?;
    let sc: Vec<_> = markets
        .iter()
        .map(|m| scores::compute(m, walkforward::UNIVERSE))
        .collect();
    let all = Market::concat(&markets)?;
    let sc_all = scores::compute(&all, walkforward::UNIVERSE);
    // The plain family grid; each variant sets its own drawdown rule.
    let live = walkforward::xs_family_grid(walkforward::LIVE_SIGNAL);
    let none = Risk::default();
    let variants: Vec<(&str, Risk, bool)> = vec![
        ("no drawdown rule (live)", none, false),
        (
            "close stop 10%",
            Risk {
                close_stop_pct: Some(10.0),
                ..none
            },
            false,
        ),
        (
            "close stop 15%",
            Risk {
                close_stop_pct: Some(15.0),
                ..none
            },
            false,
        ),
        (
            "short-only stop 15% (squeeze guard)",
            Risk {
                short_stop_pct: Some(15.0),
                ..none
            },
            false,
        ),
        (
            "take profit 10% (lock the snap-back)",
            Risk {
                take_profit_pct: Some(10.0),
                ..none
            },
            false,
        ),
        (
            "portfolio breaker 10%",
            Risk {
                breaker_pct: Some(10.0),
                ..none
            },
            false,
        ),
        (
            "volatility-scaled sizing",
            Risk {
                vol_scaled: true,
                ..none
            },
            false,
        ),
        (
            "half size while 10% below peak",
            Risk {
                derisk_pct: Some(10.0),
                ..none
            },
            false,
        ),
        (
            "add once at 10% against",
            Risk {
                add_pct: Some(10.0),
                ..none
            },
            false,
        ),
        ("1x leverage (reference)", none, true),
    ];
    println!(
        "{:42} | {:^52} | {:^52}",
        "rule", "forward test 1 (Sep 3-17)", "forward test 2 (Sep 17-Oct 1)"
    );
    let rows: Vec<(&str, Vec<XsParams>)> = variants
        .into_iter()
        .map(|(name, risk, one_x)| {
            let grid = live
                .iter()
                .filter(|p| !one_x || p.gross_leverage == 1.0)
                .map(|p| XsParams { risk, ..p.clone() })
                .collect();
            (name, grid)
        })
        .collect();
    for (name, grid) in rows {
        let mut line = format!("{name:42}");
        for k in 0..2 {
            let rep = walkforward::run_xs_with(
                &markets[k],
                &sc[k],
                eq,
                Instant::now() + Duration::from_secs(600),
                &grid,
            )?;
            match rep.params {
                Some(p) => {
                    let range = (k + 1) * BARS..(k + 2) * BARS;
                    let a = xs::backtest_costs(&all, &sc_all, range.clone(), &p, eq, 1.0).metrics();
                    let b = xs::backtest_costs(&all, &sc_all, range, &p, eq, 2.0).metrics();
                    line += &format!(
                        " | {:+6.1}% (2x costs {:+6.1}%) DD {:4.1}% PF {:4.2} {}x",
                        a.return_pct(),
                        b.return_pct(),
                        a.max_drawdown_pct,
                        a.profit_factor(),
                        p.gross_leverage
                    );
                }
                None => line += &format!(" | {:52}", "no settings qualified"),
            }
        }
        // The three 14-day walk-forwards: out-of-sample net per window (unseen 2-day stretches).
        let wf: Vec<String> = (0..3)
            .map(|k| {
                walkforward::run_xs_with(
                    &markets[k],
                    &sc[k],
                    eq,
                    Instant::now() + Duration::from_secs(600),
                    &grid,
                )
                .map(|r| format!("{:+.1}%", r.oos.net() / eq * 100.0))
                .unwrap_or_else(|_| "-".into())
            })
            .collect();
        println!("{line} | walk-forward OOS {}", wf.join(" / "));
    }
    Ok(())
}
