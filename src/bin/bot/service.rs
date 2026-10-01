//! Paper-trading service for the contrarian Pulse strategy (see
//! walkforward::LIVE_SIGNAL for the evidence) — never places orders.
//!
//! bar_task (each 15m close): sync closed bars + funding for every USDT
//! perpetual -> scores (top-100 universe by 24h turnover) -> 14-day
//! walk-forward -> step the persisted paper portfolio through every new bar
//! with the chosen params (missed bars replayed in order) -> publish the
//! current LONG (most bearish Pulse) / SHORT (most bullish Pulse) targets.
//! Console: 127.0.0.1:8787 (`/`, `/api/status`, `/api/signals`), polling only.

use anyhow::Result;
use bybit_mean_reversion_bot::engine::scores;
use bybit_mean_reversion_bot::engine::supervisor::{Health, Heartbeat};
use bybit_mean_reversion_bot::engine::walkforward::{self, XsReport};
use bybit_mean_reversion_bot::engine::xs::{self, XsParams, XsPortfolio};
use bybit_mean_reversion_bot::engine::{data, governor, keychain, Side, BARS, BAR_MS};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const PORT: u16 = 8787;
/// Bybit rules (account fee, margin tiers, measured order books) are re-measured this often.
const RULES_REFRESH: Duration = Duration::from_secs(3600);
const WF_DEADLINE: Duration = Duration::from_secs(900);
const PAPER_FILE: &str = "paper_xs.json";

#[derive(Debug, Clone, Serialize)]
pub struct SignalRow {
    pub symbol: String,
    /// Ranking value of the live signal (Pulse: pulse_long - pulse_short).
    pub signal: f64,
    /// 1 = lowest value (long side).
    pub rank: usize,
    /// Side this pair would hold at the next rebalance, if any.
    pub target: Option<Side>,
    /// Side held in the paper portfolio now, if any.
    pub held: Option<Side>,
    pub close: f64,
    pub turnover_24h: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub strategy: String,
    pub started_ms: i64,
    pub last_bar_ts: i64,
    pub next_rebalance_ts: i64,
    pub symbols: usize,
    pub universe: usize,
    pub sync_secs: f64,
    pub report: Option<XsReport>,
    pub params: Option<XsParams>,
    pub validated: bool,
    /// PASSED / FORWARD TEST / FAILED (flat).
    pub mode: String,
    pub paper_equity: f64,
    pub paper_start_equity: f64,
    pub paper_open_positions: usize,
    pub paper_trades: usize,
    pub paper_wins: usize,
    /// Equity change since the start: paper_realized + paper_open.
    pub paper_net: f64,
    /// Cash P&L: closed trades plus fees and funding already paid on open positions.
    pub paper_realized: f64,
    /// Mark-to-market of open positions at the last close (before exit fees).
    pub paper_open: f64,
    pub paper_max_drawdown_pct: f64,
    /// The real Bybit account's USDT wallet balance (read every bar, read-only).
    pub account_balance: f64,
    pub account_balance_ts: i64,
    /// Symbols with complete Bybit rules, and when they were last measured.
    pub rules_symbols: usize,
    pub rules_ts: i64,
    pub events: VecDeque<String>,
}

#[derive(Serialize, Deserialize)]
struct PaperState {
    portfolio: XsPortfolio,
    last_ts: i64,
    /// Params last used, so open positions stay managed (stops, rebalances)
    /// even on a bar where no params qualify.
    #[serde(default)]
    params: Option<XsParams>,
    /// Real account balance last mirrored; a change is a deposit or withdrawal.
    #[serde(default)]
    last_account_balance: Option<f64>,
}

struct App {
    creds: keychain::Credentials,
    rules_at: std::sync::Mutex<Option<Instant>>,
    dir: PathBuf,
    client: data::Client,
    cache: data::Cache,
    health: Health,
    status: RwLock<Status>,
    signals: RwLock<Vec<SignalRow>>,
    paper: RwLock<Option<PaperState>>,
    /// Day-by-day chronological forward test (forward_test_daily), computed once.
    research: RwLock<Option<serde_json::Value>>,
}

impl App {
    fn event(&self, level: &str, msg: String) {
        match level {
            "ERROR" => error!("{msg}"),
            "WARN" => warn!("{msg}"),
            _ => info!("{msg}"),
        }
        let line = format!(
            "{} {level} {msg}",
            chrono::Utc::now().format("%m-%d %H:%M:%S")
        );
        let mut s = self.status.write().unwrap_or_else(|p| p.into_inner());
        s.events.push_front(line);
        s.events.truncate(120);
    }

    fn save_paper(&self) {
        let guard = self.paper.read().unwrap_or_else(|p| p.into_inner());
        if let Some(ps) = guard.as_ref() {
            let tmp = self.dir.join(format!("{PAPER_FILE}.tmp"));
            let ok = serde_json::to_string(ps)
                .map_err(std::io::Error::other)
                .and_then(|s| std::fs::write(&tmp, s))
                .and_then(|_| std::fs::rename(&tmp, self.dir.join(PAPER_FILE)));
            if let Err(e) = ok {
                error!("could not persist paper state: {e}");
            }
        }
    }
}

pub async fn serve(dir: PathBuf) -> Result<()> {
    info!("Bybit Mean Reversion Bot — contrarian Pulse, market-neutral (paper + signals only): 15m bars, top {} USDT perps, 14-day walk-forward, CPU target {}%",
        walkforward::UNIVERSE, governor::CPU_TARGET_PCT);
    governor::global();
    let paper: Option<PaperState> = std::fs::read_to_string(dir.join(PAPER_FILE))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let health = Health::default();
    // Read-only credentials for the account balance and fee rates; no guessing
    // without them (the service stops and launchd retries).
    let creds = keychain::load()?;
    let client = data::Client::new()?;
    let balance = client.usdt_wallet_balance(&creds).await?;
    info!("real account USDT wallet balance {balance:.2}");
    let app = Arc::new(App {
        creds,
        rules_at: std::sync::Mutex::new(None),
        client,
        cache: data::Cache::open(dir.join("data.db").to_str().unwrap_or("data.db"))?,
        dir,
        health: health.clone(),
        status: RwLock::new(Status {
            strategy: "Contrarian Pulse: short the strongest bullish Pulse, long the strongest bearish Pulse, market-neutral (paper only)".into(),
            started_ms: chrono::Utc::now().timestamp_millis(),
            universe: walkforward::UNIVERSE,
            ..Default::default()
        }),
        signals: RwLock::new(vec![]),
        paper: RwLock::new(paper),
        research: RwLock::new(None),
    });

    let a = app.clone();
    tokio::task::spawn_blocking(move || match forward_test_daily(&a.dir, balance) {
        Ok(v) => *a.research.write().unwrap_or_else(|e| e.into_inner()) = Some(v),
        Err(e) => {
            warn!("forward test chart unavailable: {e:#}");
            *a.research.write().unwrap_or_else(|e| e.into_inner()) =
                Some(serde_json::json!({ "error": format!("{e:#}") }));
        }
    });
    let a = app.clone();
    health.spawn("bar_task", Duration::from_secs(1200), move |hb| {
        let a = a.clone();
        async move { bar_task(a, hb).await }
    });
    let a = app.clone();
    health.spawn("console", Duration::from_secs(120), move |hb| {
        let a = a.clone();
        async move { console(a, hb).await }
    });

    let wd = health.clone();
    std::thread::Builder::new()
        .name("process-watchdog".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(60));
            for (name, t) in wd.snapshot() {
                if name == "bar_task" && t.seconds_since_heartbeat > 2400 {
                    error!(
                        "[watchdog] {name} silent for {}s; exiting so launchd restarts the service",
                        t.seconds_since_heartbeat
                    );
                    std::process::exit(1);
                }
            }
        })?;

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    app.save_paper();
    info!("shutdown");
    Ok(())
}

/// One 14-day forward test, day by day (14 days is the test limit): the live
/// walk-forward picks settings on the 14 days Sep 3 -> Sep 17 2026, then they
/// trade the next 14 days, Sep 17 -> Oct 1, which they never saw, from
/// the real account balance. The earlier window only supplies the screener's look-back
/// at the start, as the live bot has it. Real cached Bybit bars + funding only
/// (runtime window_2.db, window_3.db); errors if missing.
fn forward_test_daily(dir: &Path, start: f64) -> Result<serde_json::Value> {
    const RESEARCH_FIRST_BAR: i64 = 1_789_633_800_000;
    let span = BARS as i64 * BAR_MS;
    let w3_last = RESEARCH_FIRST_BAR + span - BAR_MS;
    let mut markets = Vec::new();
    for (file, last) in [("window_2.db", w3_last - span), ("window_3.db", w3_last)] {
        let path = dir.join(file);
        anyhow::ensure!(path.exists(), "research cache {file} not found");
        let cache = data::Cache::open(path.to_str().unwrap_or(file))?;
        let (symbols, _) = cache.contents()?;
        markets.push(cache.market(&symbols, last)?);
    }
    // Settings are chosen on the first 14-day window alone (exactly as live); the
    // next 14 days are traded on the joined timeline so the screener keeps its
    // look-back at the start (live it always has it): no artificial warm-up gap.
    let sc: Vec<_> = markets
        .iter()
        .map(|m| scores::compute(m, walkforward::UNIVERSE))
        .collect();
    let all = bybit_mean_reversion_bot::engine::Market::concat(&markets)?;
    let sc_all = scores::compute(&all, walkforward::UNIVERSE);
    let grid = walkforward::live_grid();
    let mut pf = XsPortfolio::new(start);
    let mut days: Vec<xs::DayRow> = Vec::new();
    let rep = walkforward::run_xs_with(
        &markets[0],
        &sc[0],
        start,
        Instant::now() + Duration::from_secs(600),
        &grid,
    )?;
    let p = rep
        .params
        .ok_or_else(|| anyhow::anyhow!("no settings qualified on the first 14 days"))?;
    xs::run_daily(&all, &sc_all, BARS..2 * BARS, &p, &mut pf, &mut days, true);
    let segments = vec![serde_json::json!({
        "chosen_on": [markets[0].ts[0], markets[0].ts[BARS - 1] + BAR_MS],
        "traded": [markets[1].ts[0], markets[1].ts[BARS - 1] + BAR_MS],
        "params": p, "start_equity": start, "end_equity": days.last().map_or(start, |d| d.equity),
    })];
    let m = pf.metrics();
    Ok(serde_json::json!({
        "start_equity": start, "end_equity": pf.equity, "net": pf.equity - start,
        "return_pct": (pf.equity / start - 1.0) * 100.0, "max_drawdown_pct": m.max_drawdown_pct,
        "trades": m.trades, "wins": m.wins, "profit_factor": m.profit_factor(), "liquidations": m.liquidations,
        "fees": pf.trades.iter().map(|t| t.fees).sum::<f64>(), "funding": pf.trades.iter().map(|t| t.funding).sum::<f64>(),
        "segments": segments, "days": days,
    }))
}

async fn bar_task(app: Arc<App>, hb: Heartbeat) -> Result<()> {
    let mut last_done = 0i64;
    loop {
        hb.beat();
        let now = chrono::Utc::now().timestamp_millis();
        let closed = (now - 5_000) / BAR_MS * BAR_MS - BAR_MS;
        if closed <= last_done {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        let t0 = Instant::now();
        // Symbols (trading, no delisting scheduled) and, hourly, their full Bybit
        // rules: order rules, the account's fees, margin tiers, measured books.
        let lots = app.client.usdt_perpetual_lots().await?;
        let stale = app
            .rules_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none_or(|t| t.elapsed() >= RULES_REFRESH);
        if stale {
            let rules = app.client.fetch_rules(&app.creds, &lots).await?;
            app.cache.put_rules(&rules)?;
            *app.rules_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            let mut s = app.status.write().unwrap_or_else(|e| e.into_inner());
            s.rules_symbols = rules.len();
            s.rules_ts = chrono::Utc::now().timestamp_millis();
        }
        hb.beat();
        // The real account balance, every bar (deposits and withdrawals are mirrored).
        let balance = app.client.usdt_wallet_balance(&app.creds).await?;
        let symbols: Vec<String> = lots.into_iter().map(|(s, _)| s).collect();
        let hb2 = hb.clone();
        let last = data::sync(&app.client, &app.cache, &symbols, move |_, _| hb2.beat()).await?;
        if last < closed {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        let sync_secs = t0.elapsed().as_secs_f64();
        let market = Arc::new(app.cache.market(&symbols, last)?);
        hb.beat();
        // The walk-forward sizes from the money actually in play: the paper
        // account's equity (which follows the real balance), else the real balance.
        let wf_equity = app
            .paper
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(balance, |ps| {
                ps.portfolio.equity + ps.portfolio.unrealized()
            });
        let m2 = market.clone();
        let (sc, report) = tokio::task::spawn_blocking(move || -> Result<_> {
            let sc = scores::compute(&m2, walkforward::UNIVERSE);
            let report = walkforward::run_xs_with(
                &m2,
                &sc,
                wf_equity,
                Instant::now() + WF_DEADLINE,
                &walkforward::live_grid(),
            )?;
            Ok((sc, report))
        })
        .await??;
        hb.beat();
        last_done = last;
        let n = market.len();

        let prev_params = app
            .paper
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|ps| ps.params.clone());
        if let Some(p) = report.params.clone().or(prev_params) {
            // Paper: same engine, every new bar in order. New positions only
            // while the walk-forward passes; otherwise it goes flat.
            let new_lines = {
                let mut guard = app.paper.write().unwrap_or_else(|e| e.into_inner());
                let ps = guard.get_or_insert_with(|| PaperState {
                    portfolio: XsPortfolio::new(balance),
                    last_ts: market.ts[n - 1] - BAR_MS,
                    params: None,
                    last_account_balance: Some(balance),
                });
                // A change in the real balance (the account is flat: the bot never
                // trades) is a deposit or withdrawal: mirror it, not as profit.
                let mut flow = None;
                if let Some(prev) = ps.last_account_balance {
                    let delta = balance - prev;
                    if delta.abs() >= 0.01 {
                        ps.portfolio.cash_flow(delta);
                        flow = Some(delta);
                    }
                }
                ps.last_account_balance = Some(balance);
                ps.params = Some(p.clone());
                ps.portfolio.entries_allowed = report.passed || report.forward_ok();
                let (before_trades, before_pos): (usize, Vec<(String, Side, bool)>) = (
                    ps.portfolio.trades.len(),
                    ps.portfolio
                        .positions
                        .iter()
                        .map(|x| (x.symbol.clone(), x.side, x.added))
                        .collect(),
                );
                // The symbol list is re-fetched every bar: re-point positions by name.
                ps.portfolio.reindex(&market.symbols, market.ts[n - 1]);
                for t in 0..n {
                    if market.ts[t] > ps.last_ts {
                        ps.portfolio.step(&market, &sc, t, &p);
                        ps.last_ts = market.ts[t];
                    }
                }
                let mut lines: Vec<String> = flow
                    .map(|d| format!("ACCOUNT balance changed {d:+.2} USDT (now {balance:.2}): mirrored into paper as a {}, not profit", if d > 0.0 { "deposit" } else { "withdrawal" }))
                    .into_iter()
                    .collect();
                lines.extend(ps.portfolio.trades[before_trades..].iter().map(|t| {
                    format!(
                        "PAPER close {:?} {} ({}) pnl {:+.2} USDT, held {}m",
                        t.side,
                        t.symbol,
                        t.reason,
                        t.pnl,
                        (t.exit_ts - t.entry_ts) / 60_000
                    )
                }));
                for x in &ps.portfolio.positions {
                    let before = before_pos.iter().find(|b| b.0 == x.symbol && b.1 == x.side);
                    if before.is_some_and(|b| !b.2) && x.added {
                        lines.push(format!(
                            "PAPER add {:?} {}: closed 10% against, size doubled, average now {} (liquidation {:.6})",
                            x.side, x.symbol, x.entry, x.liquidation
                        ));
                    }
                    if before.is_none() {
                        lines.push(format!(
                            "PAPER open {:?} {} @ {} ({:.1}x, stop {})",
                            x.side,
                            x.symbol,
                            x.entry,
                            x.leverage,
                            x.stop.map_or("-".to_string(), |s| format!("{s:.6}"))
                        ));
                    }
                }
                lines
            };
            for l in new_lines {
                app.event("INFO", l);
            }
            app.save_paper();

            // Signals at the latest bar.
            let t = n - 1;
            let targets = xs::targets(&market, &sc, t, &p);
            let held: Vec<(usize, Side)> = app
                .paper
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|ps| {
                    ps.portfolio
                        .positions
                        .iter()
                        .map(|x| (x.sym, x.side))
                        .collect()
                })
                .unwrap_or_default();
            let mut rows: Vec<SignalRow> = (0..market.symbols.len())
                .filter_map(|s| {
                    let score = sc[s][t]?;
                    Some(SignalRow {
                        symbol: market.symbols[s].clone(),
                        signal: xs::signal_value(&market, &sc, s, t, &p)?,
                        rank: 0,
                        target: targets.get(&s).copied(),
                        held: held.iter().find(|h| h.0 == s).map(|h| h.1),
                        close: market.bars[s][t]?.close,
                        turnover_24h: score.turnover_24h,
                    })
                })
                .collect();
            rows.sort_by(|a, b| a.signal.total_cmp(&b.signal));
            for (i, r) in rows.iter_mut().enumerate() {
                r.rank = i + 1;
            }
            *app.signals.write().unwrap_or_else(|e| e.into_inner()) = rows;
        }

        let hold = report.params.as_ref().map_or(16, |p| p.hold) as i64;
        let next_rebalance = ((last + BAR_MS) / (hold * BAR_MS) + 1) * hold * BAR_MS;
        let paper = app.paper.read().unwrap_or_else(|e| e.into_inner());
        let mut s = app.status.write().unwrap_or_else(|e| e.into_inner());
        s.last_bar_ts = last;
        s.account_balance = balance;
        s.account_balance_ts = chrono::Utc::now().timestamp_millis();
        s.next_rebalance_ts = next_rebalance;
        s.symbols = symbols.len();
        s.sync_secs = sync_secs;
        s.validated = report.passed;
        s.mode = if report.passed {
            "PASSED"
        } else if report.forward_ok() {
            "FORWARD TEST"
        } else {
            "FAILED (flat)"
        }
        .into();
        s.params = report.params.clone();
        if let Some(ps) = paper.as_ref() {
            let m = ps.portfolio.metrics();
            s.paper_equity = m.end_equity + m.open_unrealized;
            s.paper_start_equity = m.start_equity;
            s.paper_open_positions = ps.portfolio.positions.len();
            s.paper_trades = m.trades;
            s.paper_wins = m.wins;
            s.paper_net = m.net();
            s.paper_realized = m.end_equity - m.start_equity;
            s.paper_open = m.open_unrealized;
            s.paper_max_drawdown_pct = m.max_drawdown_pct;
        }
        let verdict = if report.passed {
            "PASSED".to_string()
        } else if report.forward_ok() {
            format!(
                "FORWARD TEST (paper keeps trading; {})",
                report.reasons.join("; ")
            )
        } else {
            format!("FAILED, paper goes flat: {}", report.reasons.join("; "))
        };
        let summary = format!(
            "bar {} | {} pairs | sync {:.0}s | walk-forward {} | paper equity {:.2}",
            chrono::DateTime::from_timestamp_millis(last)
                .map(|d| d.format("%m-%d %H:%M").to_string())
                .unwrap_or_default(),
            symbols.len(),
            sync_secs,
            verdict,
            s.paper_equity
        );
        s.report = Some(report);
        drop(s);
        drop(paper);
        app.event("INFO", summary);
    }
}

async fn console(app: Arc<App>, hb: Heartbeat) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", PORT)).await?;
    info!("console on http://127.0.0.1:{PORT}");
    loop {
        hb.beat();
        let Ok(Ok((mut sock, _))) =
            tokio::time::timeout(Duration::from_secs(20), listener.accept()).await
        else {
            continue;
        };
        let app = app.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf)).await
            else {
                return;
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (ctype, body) = if path.starts_with("/api/status") {
                let mut v =
                    serde_json::to_value(&*app.status.read().unwrap_or_else(|e| e.into_inner()))
                        .unwrap_or_default();
                v["cpu_pct"] = governor::global().cpu_pct().into();
                v["tasks"] = serde_json::to_value(app.health.snapshot()).unwrap_or_default();
                v["now_ms"] = chrono::Utc::now().timestamp_millis().into();
                if let Some(ps) = app.paper.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    v["paper_positions"] =
                        serde_json::to_value(&ps.portfolio.positions).unwrap_or_default();
                    v["paper_recent_trades"] = serde_json::to_value(
                        ps.portfolio
                            .trades
                            .iter()
                            .rev()
                            .take(40)
                            .collect::<Vec<_>>(),
                    )
                    .unwrap_or_default();
                }
                ("application/json", v.to_string())
            } else if path.starts_with("/api/research") {
                (
                    "application/json",
                    app.research
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .map_or("null".into(), |v| v.to_string()),
                )
            } else if path.starts_with("/api/signals") {
                (
                    "application/json",
                    serde_json::to_string(&*app.signals.read().unwrap_or_else(|e| e.into_inner()))
                        .unwrap_or_default(),
                )
            } else {
                ("text/html; charset=utf-8", PAGE.to_string())
            };
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ =
                tokio::time::timeout(Duration::from_secs(5), sock.write_all(resp.as_bytes())).await;
        });
    }
}

const PAGE: &str = r#"<!doctype html><title>Bybit Mean Reversion Bot — live console</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>body{font:14px -apple-system,system-ui,sans-serif;margin:16px;background:#fafafa;color:#222}
h1{font-size:18px;margin:0 0 6px}section{background:#fff;border:1px solid #ddd;border-radius:8px;padding:12px;margin:10px 0;overflow-x:auto}
.k{color:#666}.ok{color:#0a7d2c}.bad{color:#b00020}.warn{color:#b26a00}.Long{color:#0a7d2c}.Short{color:#b00020}pre{white-space:pre-wrap;font-size:12px;margin:0;max-height:420px;overflow:auto}
table{border-collapse:collapse;font-size:12px}td,th{padding:2px 8px;text-align:right}th{color:#666;font-weight:500}td:first-child,th:first-child{text-align:left}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:8px}.card{background:#f4f4f4;border-radius:6px;padding:8px}.card b{display:block;font-size:18px}</style>
<h1>Bybit Mean Reversion Bot — live console <span class=k>(paper + signals only, never trades)</span></h1>
<div class=k id=strat></div>
<section><div class=grid id=cards></div></section>
<section><b>Paper positions</b> <span class=k id=nextreb></span><table id=pos></table></section>
<section><b>Next rebalance targets</b> <span class=k>(at the next rebalance: long the 5 most bearish Pulse, short the 5 most bullish, among the top 100 USDT perps by 24h turnover)</span><table id=sig></table></section>
<section><b>Live event log</b><pre id=ev></pre></section>
<section><b>Recent paper trades</b><table id=tr></table></section>
<section><b>Forward test, day by day</b> <span class=k>(14 days of real Bybit data the settings never saw: chosen on the previous 14 days, then traded these 14; same engine, fees, slippage, funding and lot rules as paper)</span><div id=fsum class=k></div><div id=fchart></div><table id=fdays></table></section>
<section><b>Walk-forward (14 days of 15m bars, re-run every bar)</b><pre id=wf></pre></section>
<script>
const f=(x,d=2)=>x==null||isNaN(x)?'-':Number(x).toFixed(d);
const t=ms=>ms?new Date(ms).toLocaleString():'-';
async function tick(){try{
const s=await (await fetch('/api/status')).json(), g=await (await fetch('/api/signals')).json();
document.getElementById('strat').textContent=s.strategy+' — last update '+t(s.now_ms);
const sg=x=>(x>=0?'+':'')+f(x), pc=x=>sg(x)+'%', st=s.paper_start_equity||1;
const pu=p=>(p.side==="Long"?1:-1)*(p.mark-p.entry)*p.qty-p.fees-p.funding;
const cards=[['Real account balance',f(s.account_balance)+' USDT <span class=k>(Bybit, read-only, '+t(s.account_balance_ts)+')</span>'],['Paper equity',f(s.paper_equity)+' USDT <span class=k>('+pc((s.paper_equity/st-1)*100)+' from '+f(st,0)+')</span>'],['Realised P&L <span class=k title="closed trades + fees and funding already paid">ⓘ</span>','<span class='+(s.paper_realized>=0?'ok':'bad')+'>'+sg(s.paper_realized)+' USDT ('+pc(s.paper_realized/st*100)+')</span>'],['Open P&L <span class=k title="open positions marked at the last close">ⓘ</span>','<span class='+(s.paper_open>=0?'ok':'bad')+'>'+sg(s.paper_open)+' USDT ('+pc(s.paper_open/st*100)+')</span>'],['Open positions',s.paper_open_positions],
['Closed trades / wins',s.paper_trades+' / '+s.paper_wins],['Max drawdown',f(s.paper_max_drawdown_pct,1)+'%'],
['Walk-forward',s.report?'<span class='+(s.validated?'ok':s.mode==='FORWARD TEST'?'warn':'bad')+'>'+s.mode+'</span>':'…'],
['Last 15m bar',t(s.last_bar_ts+900000)],['Pairs scanned',s.symbols+' (top '+s.universe+')'],['Bybit rules',s.rules_symbols+' coins (fees, margin tiers, order books) measured '+t(s.rules_ts)],['CPU',f(s.cpu_pct,0)+'%'],
['Tasks',Object.entries(s.tasks||{}).map(([k,v])=>k+(v.running?' ✓':' ✗')).join(' ')]];
document.getElementById('cards').innerHTML=cards.map(c=>`<div class=card><span class=k>${c[0]}</span><b>${c[1]}</b></div>`).join('');
document.getElementById('nextreb').textContent=s.next_rebalance_ts?'next rebalance at '+t(s.next_rebalance_ts)+(s.params?' (every '+s.params.hold*15+' min)':''):'';
document.getElementById('pos').innerHTML='<tr><th>Symbol<th>Side<th>Entry<th>Mark<th>P&L USDT (after its fees + funding)<th>P&L % (margin)<th>Stop<th>Lev<th>Opened</tr>'+(s.paper_positions||[]).map(p=>{
 const u=pu(p),pl=u/p.margin*100;return `<tr><td>${p.symbol}<td class=${p.side}>${p.side}<td>${p.entry}<td>${p.mark}<td class=${u>=0?"ok":"bad"}>${sg(u)}<td class=${u>=0?"ok":"bad"}>${pc(pl)}<td>${f(p.stop,6)}<td>${f(p.leverage,1)}<td>${t(p.entry_ts)}</tr>`}).join('');
const tg=g.filter(r=>r.target||r.held);
document.getElementById('sig').innerHTML='<tr><th>Symbol<th>Target<th>Held<th>Pulse long−short<th>Rank (1 = most bearish)<th>Close<th>24h turnover</tr>'+
tg.map(r=>`<tr><td>${r.symbol}<td class=${r.target||''}>${r.target||'-'}<td class=${r.held||''}>${r.held||'-'}<td>${f(r.signal)}<td>${r.rank}<td>${r.close}<td>${f(r.turnover_24h/1e6,1)}M</tr>`).join('');
document.getElementById('ev').textContent=(s.events||[]).join('\n');
document.getElementById('tr').innerHTML='<tr><th>Symbol<th>Side<th>Entry<th>Exit<th>P&L USDT<th>P&L % (margin)<th>Reason<th>Closed</tr>'+(s.paper_recent_trades||[]).map(x=>`<tr><td>${x.symbol}<td class=${x.side}>${x.side}<td>${x.entry}<td>${x.exit}<td class=${x.pnl>=0?"ok":"bad"}>${sg(x.pnl)}<td class=${x.pnl>=0?"ok":"bad"}>${pc(x.r*100)}<td>${x.reason}<td>${t(x.exit_ts)}</tr>`).join('');
const r=s.report||{},o=r.oos||{},full=r.full_period||{},eq0=r.start_equity||1,on=(o.end_equity||0)-(o.start_equity||0);
const desc=p=>p?`${p.signal}${p.signal==='Return'?' over '+p.lookback*15/60+'h':''} ${p.flip?'(follow)':'(contrarian)'}: long the ${p.top} lowest, short the ${p.top} highest, rebalance every ${p.hold*15/60}h, ${p.gross_leverage}x, stop ${p.stop_pct==null?'none':p.stop_pct+'%'}`:'none';
const wn=w=>w.out_of_sample?w.out_of_sample.end_equity-w.out_of_sample.start_equity:null;
document.getElementById('wf').textContent=r.windows?`verdict: ${r.passed?'PASSED':(s.mode==='FORWARD TEST'?'FORWARD TEST — not validated, paper trades to gather evidence':'FAILED — no new positions')}${r.reasons&&r.reasons.length?'\nwhy: '+r.reasons.join('; '):''}
settings now: ${desc(r.params)}

UNSEEN-DATA TEST (the number that counts): 3 separate 2-day windows the settings never saw, each starting from ${f(eq0,0)} USDT
  per window: ${r.windows.map((w,i)=>'W'+(i+1)+' '+(wn(w)==null?'no trade':sg(wn(w))+' USDT ('+pc(wn(w)/eq0*100)+')')).join('  |  ')}
  windows in profit: ${r.positive_windows} of ${r.windows.length}   |   total without the best window: ${sg(r.net_without_best)} USDT
  total: ${sg(on)} USDT (${pc(on/eq0/r.windows.length*100)} per window on average), ${o.trades} trades, ${o.wins} wins (${f(o.trades?o.wins/o.trades*100:0,0)}%), profit factor ${o.gross_loss>0?f(o.gross_profit/o.gross_loss):'-'}, liquidations ${o.liquidations}, worst drawdown inside a window ${f(o.max_drawdown_pct,1)}%

SELF-CHECK (not a forecast — the settings were chosen on 8 of these 14 days): ${full.trades} trades, ${sg((full.end_equity||0)-(full.start_equity||0))} USDT (${pc(((full.end_equity||0)/(full.start_equity||1)-1)*100)}), max drawdown ${f(full.max_drawdown_pct,1)}% — only used to reject settings that liquidate or fall > 25%
${r.evaluated} backtests in ${r.elapsed_ms} ms, real closed 15m Bybit bars ${t(r.first_bar_ts)} → ${t(r.last_bar_ts+900000)}`:'waiting for the first closed bar…';
}catch(e){}}
async function research(){try{
const r=await (await fetch('/api/research')).json();
if(!r){document.getElementById('fsum').textContent='computing from the cached real data…';setTimeout(research,10000);return}
if(r.error){document.getElementById('fsum').textContent='unavailable: '+r.error;return}
const F=(x,d=2)=>Number(x).toFixed(d),S=x=>(x>=0?'+':'')+F(x),P=x=>S(x)+'%',D=ms=>new Date(ms).toISOString().slice(5,10);
const days=r.days,pos=days.filter(d=>d.pnl>0).length;
const best=days.reduce((a,d)=>d.pnl>a.pnl?d:a,days[0]),worst=days.reduce((a,d)=>d.pnl<a.pnl?d:a,days[0]);
document.getElementById('fsum').innerHTML=`${F(r.start_equity,0)} → <b>${F(r.end_equity)} USDT</b> = <b class=${r.net>=0?'ok':'bad'}>${S(r.net)} USDT (${P(r.return_pct)})</b> over ${F((r.segments[0].traded[1]-r.segments[0].traded[0])/864e5,0)} days (${new Date(r.segments[0].traded[0]).toISOString().slice(0,16).replace("T"," ")} → ${new Date(r.segments[0].traded[1]).toISOString().slice(0,16).replace("T"," ")} UTC) · ${pos} of ${days.length} UTC dates up · best day ${D(best.day_ts)} ${S(best.pnl)} (${P(best.pnl_pct)}) · worst day ${D(worst.day_ts)} ${S(worst.pnl)} (${P(worst.pnl_pct)}) · max drawdown ${F(r.max_drawdown_pct,1)}% · ${r.trades} trades, ${r.wins} wins · profit factor ${F(r.profit_factor)} · fees ${F(r.fees)} · funding ${S(-r.funding)} · liquidations ${r.liquidations}<br>`+
 r.segments.map((s,i)=>`segment ${i+1}: settings chosen on ${D(s.chosen_on[0])}–${D(s.chosen_on[1])}, traded ${D(s.traded[0])}–${D(s.traded[1])}: ${S(s.end_equity-s.start_equity)} USDT (${P((s.end_equity/s.start_equity-1)*100)}) — ${s.params.signal} ${s.params.flip?'follow':'contrarian'}, top ${s.params.top}, every ${s.params.hold/4}h, ${s.params.gross_leverage}x, stop ${s.params.stop_pct??'none'}`).join('<br>');
const W=960,H=320,L=60,R=10,T=10,eqH=170,gap=20,bH=H-T-eqH-gap-24,n=days.length,bw=(W-L-R)/n;
const eqs=[r.start_equity,...days.map(d=>d.equity)],lo=Math.min(...eqs),hi=Math.max(...eqs);
const ey=v=>T+eqH-(v-lo)/(hi-lo||1)*eqH, mx=Math.max(...days.map(d=>Math.abs(d.pnl)))||1, by0=T+eqH+gap+bH/2, by=v=>by0-v/mx*bH/2;
let g=`<svg viewBox="0 0 ${W} ${H}" style="width:100%;max-width:${W}px;height:auto;font:11px system-ui">`;
for(const v of [lo,(lo+hi)/2,hi]) g+=`<line x1=${L} x2=${W-R} y1=${ey(v)} y2=${ey(v)} stroke=#eee /><text x=${L-4} y=${ey(v)+4} text-anchor=end fill=#888>${F(v,0)}</text>`;
g+=`<line x1=${L} x2=${W-R} y1=${ey(r.start_equity)} y2=${ey(r.start_equity)} stroke=#bbb stroke-dasharray=4 />`;
g+=`<polyline fill=none stroke=#2463eb stroke-width=2 points="${eqs.map((v,i)=>(L+i*bw)+','+ey(v)).join(' ')}"/>`;
g+=`<text x=${L} y=${T+12} fill=#2463eb>equity USDT (end of each UTC day)</text>`;
g+=`<line x1=${L} x2=${W-R} y1=${by0} y2=${by0} stroke=#bbb /><text x=${L-4} y=${by0+4} text-anchor=end fill=#888>0</text><text x=${L-4} y=${by(mx)+4} text-anchor=end fill=#888>${S(mx)}</text><text x=${L-4} y=${by(-mx)+4} text-anchor=end fill=#888>${S(-mx)}</text>`;
days.forEach((d,i)=>{const x=L+i*bw+bw*0.15,y=Math.min(by(d.pnl),by0),h=Math.abs(by(d.pnl)-by0);
 g+=`<rect x=${x} y=${y} width=${bw*0.7} height=${Math.max(h,1)} fill=${d.pnl>=0?'#0a7d2c':'#b00020'}><title>${D(d.day_ts)}: ${S(d.pnl)} USDT (${P(d.pnl_pct)}), equity ${F(d.equity)}, ${d.trades} trades / ${d.wins} wins</title></rect>`;
 if(i%2==0) g+=`<text x=${L+i*bw+bw/2} y=${H-6} text-anchor=middle fill=#888>${D(d.day_ts)}</text>`;});
const sw=r.segments.length>1?days.findIndex(d=>d.day_ts>=r.segments[1].traded[0]-86400000):-1;
if(sw>0) g+=`<line x1=${L+sw*bw+bw/2} x2=${L+sw*bw+bw/2} y1=${T} y2=${H-20} stroke=#b26a00 stroke-dasharray=3 /><text x=${L+sw*bw+bw/2+4} y=${T+24} fill=#b26a00>settings re-chosen</text>`;
document.getElementById('fchart').innerHTML=g+`<text x=${L} y=${by0-bH/2-4} fill=#666>daily P&L USDT</text></svg>`;
document.getElementById('fdays').innerHTML='<tr><th>Day (UTC)<th>P&L USDT<th>P&L %<th>Equity<th>Trades closed<th>Wins</tr>'+days.map(d=>`<tr><td>${new Date(d.day_ts).toISOString().slice(0,10)}<td class=${d.pnl>=0?'ok':'bad'}>${S(d.pnl)}<td class=${d.pnl>=0?'ok':'bad'}>${P(d.pnl_pct)}<td>${F(d.equity)}<td>${d.trades}<td>${d.wins}</tr>`).join('');
}catch(e){document.getElementById('fsum').textContent='chart error: '+e}}
research();
let boot=null;
async function guard(){try{const s=await (await fetch('/api/status')).json();if(boot&&s.started_ms!==boot)location.reload();boot=s.started_ms;}catch(e){}}
tick();guard();setInterval(tick,5000);setInterval(guard,15000);
</script>"#;
