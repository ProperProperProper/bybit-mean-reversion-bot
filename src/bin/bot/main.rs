//! Bybit Mean Reversion Bot: contrarian Pulse cross-sectional strategy on Bybit USDT perps
//! (paper + signals only; never places orders).
//!
//!   bot backtest     sync real data and print the live strategy's 14-day walk-forward
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
    let lots = client
        .top_margin_tokens(&lots, walkforward::UNIVERSE)
        .await?;
    cache.put_instruments(&lots)?;
    eprintln!("real account free balance {balance:.2} USDT; measuring Bybit rules (fees, margin tiers, order books)...");
    cache.put_rules(&client.fetch_rules(&creds, &lots).await?)?;
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
        walkforward::run_xs_with(
            &market,
            &sc,
            balance,
            Instant::now() + Duration::from_secs(1800),
            &walkforward::live_grid(),
        )
    })
    .await??;
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprintln!(
        "walk-forward {:.1}s, {} configs evaluated",
        t1.elapsed().as_secs_f64(),
        report.evaluated
    );
    Ok(())
}
