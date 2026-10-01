//! Evaluate the current paper grid on a disposable snapshot of real cached data.
//!
//! Usage: validate_cached <snapshot-directory> <observed-equity> <output-json>
//! The snapshot must contain data.db and window_1.db through window_3.db.
//! No credentials, network calls, orders, or runtime-directory writes are used.
use anyhow::{ensure, Context, Result};
use bybit_mean_reversion_bot::engine::{
    data::Cache, research, scores, walkforward, xs, Market, BARS,
};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};

fn coverage(m: &Market) -> Value {
    let mut leading = 0;
    let mut internal = 0;
    let mut trailing = 0;
    for row in &m.bars {
        match (
            row.iter().position(Option::is_some),
            row.iter().rposition(Option::is_some),
        ) {
            (Some(first), Some(last)) => {
                leading += first;
                trailing += row.len() - last - 1;
                internal += row[first..=last].iter().filter(|b| b.is_none()).count();
            }
            _ => leading += row.len(),
        }
    }
    json!({"symbols":m.symbols.len(),"bars":m.ts.len(),"first_bar_ts":m.ts.first(),
        "last_bar_ts":m.ts.last(),"leading_missing":leading,"internal_missing":internal,
        "trailing_missing":trailing,"symbols_with_rules":m.rules.iter().flatten().count(),
        "funding_records":m.funding.iter().map(Vec::len).sum::<usize>()})
}

fn outcome(pf: &xs::XsPortfolio) -> Value {
    let m = pf.metrics();
    json!({"metrics":m,"return_pct":m.return_pct(),"profit_factor":m.profit_factor(),
        "fees":pf.trades.iter().map(|t|t.fees).sum::<f64>(),
        "funding":pf.trades.iter().map(|t|t.funding).sum::<f64>()})
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 3,
        "usage: validate_cached <snapshot-directory> <observed-equity> <output-json>"
    );
    let dir = Path::new(&args[0]);
    let equity: f64 = args[1].parse().context("observed equity")?;
    ensure!(
        equity.is_finite() && equity > 0.0,
        "observed equity must be positive and finite"
    );
    ensure!(dir.join("data.db").exists(), "snapshot data.db is missing");
    let cache = Cache::open(dir.join("data.db").to_str().context("snapshot path")?)?;
    let (symbols, last) = cache.contents()?;
    let live = cache.market(&symbols, last)?;
    let windows: Vec<Market> = (1..=3)
        .map(|k| research::load_window(dir, k))
        .collect::<Result<_>>()?;
    let grid = walkforward::live_grid();
    let mut reports = Vec::new();
    let mut rows = Vec::new();
    for (i, m) in std::iter::once(&live).chain(windows.iter()).enumerate() {
        let label = if i == 0 {
            "latest_cache".into()
        } else {
            format!("window_{i}")
        };
        eprintln!(
            "evaluating {label}: {} symbols, {} settings",
            m.symbols.len(),
            grid.len()
        );
        let sc = scores::compute(m, walkforward::UNIVERSE);
        let report = walkforward::run_xs_with(
            m,
            &sc,
            equity,
            Instant::now() + Duration::from_secs(600),
            &grid,
        )?;
        let final_outcome = report
            .params
            .as_ref()
            .map(|p| outcome(&xs::backtest(m, &sc, 0..BARS, p, equity)));
        rows.push(json!({"label":label,"coverage":coverage(m),"report":report,"final_settings_self_check":final_outcome}));
        if i > 0 {
            reports.push(report);
        }
    }
    let joined = Market::concat(&windows)?;
    let sc = scores::compute(&joined, walkforward::UNIVERSE);
    let mut forwards = Vec::new();
    for (k, report) in reports.iter().enumerate().take(2) {
        let Some(p) = &report.params else {
            forwards.push(
                json!({"from_window":k+1,"to_window":k+2,"error":"no qualifying parameters"}),
            );
            continue;
        };
        let range = (k + 1) * BARS..(k + 2) * BARS;
        let a = xs::backtest_costs(&joined, &sc, range.clone(), p, equity, 1.0);
        let b = xs::backtest_costs(&joined, &sc, range, p, equity, 2.0);
        forwards.push(json!({"from_window":k+1,"to_window":k+2,"params":p,"normal_costs":outcome(&a),"double_costs":outcome(&b)}));
    }
    let output = json!({"generated_utc":chrono::Utc::now().to_rfc3339(),"start_equity":equity,
        "equity_source":"caller-provided observed account balance; not fetched during this run",
        "grid":"current live_grid: Pulse family, both flips, no optional risk rule",
        "warning":"Current engine has outstanding audit defects. These are diagnostic simulation results, not validated profitability or executable live returns.",
        "walkforwards":rows,"chronological_forwards":forwards});
    std::fs::write(&args[2], serde_json::to_string_pretty(&output)?)?;
    eprintln!("saved {}", args[2]);
    Ok(())
}
