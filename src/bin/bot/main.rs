//! Bybit Mean Reversion Bot: long-only strategy on Bybit USDT perps, settings from a
//! full 14-day search over every parameter
//! (paper + signals only; never places orders).
//!
//!   bot backtest     sync real data and print the live parameter search (one 14-day window)
//!                    (same grid, gates, real Bybit rules and real account balance as `serve`)
//!   bot serve        paper-trading service + dashboard on 127.0.0.1:8787

use anyhow::Result;
use bybit_mean_reversion_bot::engine::{data, runtime_dir, scores, walkforward};
use std::time::{Duration, Instant};

mod service;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match std::env::args().nth(1).as_deref() {
        Some("backtest") => backtest().await,
        Some("serve") => service::serve(runtime_dir()).await,
        _ => {
            eprintln!("usage: bot <backtest|serve>");
            std::process::exit(2);
        }
    }
}

async fn backtest() -> Result<()> {
    let dir = runtime_dir();
    let client = data::Client::new()?;
    let cache = data::Cache::open(dir.join("data.db").to_str().unwrap_or("data.db"))?;
    let creds = bybit_mean_reversion_bot::engine::keychain::load()?;
    let account = client.usdt_account(&creds).await?;
    // Size from the free balance, as the service does.
    let balance = account.wallet - account.reserved();
    let lots = client.usdt_perpetual_lots().await?;
    let candidates = client
        .top_margin_tokens(&lots, walkforward::CANDIDATES)
        .await?;
    cache.put_instruments(&candidates)?;
    eprintln!("real account free balance {balance:.2} USDT; measuring Bybit rules (fees, margin tiers, order books)...");
    cache.put_rules(&client.fetch_rules(&creds, &candidates).await?)?;
    let lots = data::universe(&candidates, &cache.rules_symbols()?, walkforward::UNIVERSE);
    let symbols: Vec<String> = lots.into_iter().map(|(s, _, _)| s).collect();
    cache.put_universe(&symbols)?;
    eprintln!(
        "{} USDT perpetuals; syncing closed 15m bars + funding...",
        symbols.len()
    );
    let t0 = Instant::now();
    let last = data::sync(&client, &cache, &symbols, |i, n| {
        if i % 50 == 0 || i == n {
            eprintln!("  synced {i}/{n}");
        }
    })
    .await?;
    eprintln!("sync done in {:.0}s", t0.elapsed().as_secs_f64());
    let market = cache.market(&symbols, last)?;
    let t1 = Instant::now();
    let report = tokio::task::spawn_blocking(move || {
        let sc = scores::compute(&market, walkforward::UNIVERSE);
        walkforward::search_full(
            &market,
            &sc,
            balance,
            Instant::now() + Duration::from_secs(3600),
            walkforward::LIVE_COMBOS,
            walkforward::live_combo,
        )
    })
    .await??;
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprintln!(
        "search {:.1}s: {} of {} combinations backtested over 14 days, {} usable",
        t1.elapsed().as_secs_f64(),
        report.evaluated,
        report.combos,
        report.usable
    );
    Ok(())
}
