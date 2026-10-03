//! Long-only paper-trading service — never places orders.
//!
//! bar_task (each 15m close): sync closed bars + funding for the universe
//! (top 20 USDT perpetuals by 24h turnover with complete rules) -> scores ->
//! step the persisted paper portfolio through every new bar with the settings
//! from the latest search (missed bars replayed in order) -> publish the
//! current LONG targets.
//! search_task: every parameter (walkforward::live_combo) over one 14-day
//! window, champion/challenger; next search 60 min after the last finished.
//! Console: 127.0.0.1:8787 (`/`, `/api/status`, `/api/signals`, `/api/research`),
//! polled by the page (no WebSocket).

use anyhow::Result;
use bybit_mean_reversion_bot::engine::scores;
use bybit_mean_reversion_bot::engine::supervisor::{Health, Heartbeat};
use bybit_mean_reversion_bot::engine::walkforward::{self, SearchReport};
use bybit_mean_reversion_bot::engine::xs::{self, XsParams, XsPortfolio};
use bybit_mean_reversion_bot::engine::{data, governor, keychain, research, Side, BARS, BAR_MS};
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
// NOTE(agents): User schedule (2026-10-03): the parameter search runs in its own task and starts
//               again SEARCH_PAUSE after the previous search FINISHED (not on the clock hour, not
//               per candle). The 15m bar loop never waits for it.
/// A search that runs longer is abandoned (the previous result stays in use).
const SEARCH_DEADLINE: Duration = Duration::from_secs(50 * 60);
/// Pause between the end of one search and the start of the next.
const SEARCH_PAUSE: Duration = Duration::from_secs(60 * 60);
/// Retry delay after a failed search.
const SEARCH_RETRY: Duration = Duration::from_secs(5 * 60);
const PAPER_FILE: &str = "paper_xs.json";

#[derive(Debug, Clone, Serialize)]
pub struct SignalRow {
    pub symbol: String,
    /// Ranking value of the signal in use (negated when the settings buy the
    /// highest values, so the lowest is always bought first).
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
    pub report: Option<SearchReport>,
    pub params: Option<XsParams>,
    /// How the settings in use were chosen by the latest search.
    pub choice: Option<walkforward::Choice>,
    /// When the latest parameter search finished (ms; 0 = none yet).
    pub search_finished_ms: i64,
    /// TRADING (new best / kept last best) or NO SAFE SETTINGS.
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
    pub effective_pairs: usize,
    pub allocation_note: String,
    /// The real Bybit account's USDT wallet balance (read every bar, read-only).
    pub account_balance: f64,
    /// Part of it committed to other positions, orders and locks (never used here).
    pub account_reserved: f64,
    pub account_balance_ts: i64,
    /// Symbols with complete Bybit rules, and when they were last measured.
    pub rules_symbols: usize,
    pub rules_ts: i64,
    pub events: VecDeque<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct PaperState {
    #[serde(default = "legacy_version")]
    schema_version: u32,
    #[serde(default)]
    settlements_seen: std::collections::BTreeMap<String, f64>,
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

fn legacy_version() -> u32 {
    1
}

// NOTE(agents): Never silently reset the paper account: a missing file is a first run, anything
//               unreadable or invalid stops the service. To migrate state, write an explicit
//               migration, not a fallback.
fn load_paper(path: &Path) -> Result<Option<PaperState>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut state: PaperState = serde_json::from_str(&text)?;
    anyhow::ensure!(
        state.schema_version <= 2,
        "unsupported paper schema {}",
        state.schema_version
    );
    if state.schema_version < 2 {
        anyhow::ensure!(
            state.portfolio.positions.is_empty() && state.portfolio.trades.is_empty(),
            "legacy paper history needs replay migration; refusing to reset it"
        );
        state.schema_version = 2;
    }
    state.portfolio.validate_state()?;
    anyhow::ensure!(
        state.last_ts >= 0 && state.last_ts % BAR_MS == 0,
        "invalid paper checkpoint"
    );
    Ok(Some(state))
}

fn persist_paper(path: &Path, state: &PaperState) -> Result<()> {
    state.portfolio.validate_state()?;
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    serde_json::to_writer(&mut file, state)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

// NOTE(agents): Causality order: (1) replay missed bars with the PREVIOUS settings, (2) apply what
//               was observed now (wallet change, committed margin), (3) install
//               new settings and decide at the latest close. Anything learned now must never change
//               a replayed bar.
/// Replay only contiguous unseen bars with the previously known settings.
/// Later balance observations and reports must not resize historical orders.
fn advance_paper(
    ps: &mut PaperState,
    market: &bybit_mean_reversion_bot::engine::Market,
    sc: &[Vec<Option<scores::Score>>],
    next: &XsParams,
    allowed: bool,
    ready_ts: i64,
    observed: Option<data::AccountBalance>,
) -> Result<()> {
    anyhow::ensure!(!market.ts.is_empty(), "empty paper market");
    anyhow::ensure!(
        market.ts.windows(2).all(|w| w[1] - w[0] == BAR_MS),
        "noncontiguous paper timeline"
    );
    let mut candidate = ps.clone();
    if let Some(previous) = candidate.params.clone() {
        if let Some(&first) = market.ts.iter().find(|&&ts| ts > candidate.last_ts) {
            anyhow::ensure!(first==candidate.last_ts+BAR_MS,"paper checkpoint {} predates available history starting at {first}; recovery replay required",candidate.last_ts);
        }
        for (sym, name) in market.symbols.iter().enumerate() {
            for &(ts, rate) in &market.funding[sym] {
                if ts > candidate.last_ts {
                    continue;
                }
                let owned = candidate
                    .portfolio
                    .positions
                    .iter()
                    .any(|p| p.symbol == *name && p.entry_ts < ts)
                    || candidate
                        .portfolio
                        .trades
                        .iter()
                        .any(|p| p.symbol == *name && p.entry_ts < ts && p.exit_ts >= ts);
                if owned {
                    let key = format!("{name}:{ts}");
                    anyhow::ensure!(
                        candidate.settlements_seen.get(&key) == Some(&rate),
                        "late/revised funding {key}: replay recovery required"
                    );
                }
            }
        }
        for t in 0..market.ts.len() {
            if market.ts[t] > candidate.last_ts {
                let ts = market.ts[t];
                for pos in &candidate.portfolio.positions {
                    for &(f, rate) in &market.funding[pos.sym] {
                        if f == ts {
                            candidate
                                .settlements_seen
                                .insert(format!("{}:{f}", pos.symbol), rate);
                        }
                    }
                }
                candidate.portfolio.step(market, sc, t, &previous);
                candidate.portfolio.validate_state()?;
                for pos in &candidate.portfolio.positions {
                    if let Some(f) = pos.last_funding_ts {
                        if f == ts + BAR_MS {
                            if let Some(&(_, rate)) =
                                market.funding[pos.sym].iter().find(|(time, _)| *time == f)
                            {
                                candidate
                                    .settlements_seen
                                    .insert(format!("{}:{f}", pos.symbol), rate);
                            }
                        }
                    }
                }
                candidate.last_ts = ts;
            }
        }
    }
    if let Some(account) = observed {
        let balance = account.wallet;
        anyhow::ensure!(
            balance.is_finite() && balance >= 0.0,
            "invalid balance observation"
        );
        // Margin the real account has committed elsewhere is never free here.
        candidate.portfolio.reserved = account.reserved();
        if let Some(previous) = candidate.last_account_balance {
            let delta = balance - previous;
            if delta != 0.0 {
                candidate.portfolio.cash_flow(delta);
            }
        }
        candidate.last_account_balance = Some(balance);
    }
    let t = market.ts.len() - 1;
    candidate.last_ts = market.ts[t];
    if candidate.params.as_ref() != Some(next) || candidate.portfolio.entries_allowed != allowed {
        candidate.portfolio.install_entry_gate(allowed);
    }
    candidate.params = Some(next.clone());
    candidate.portfolio.decide_close(market, sc, t, next);
    candidate.portfolio.defer_new_decisions(ready_ts);
    *ps = candidate;
    Ok(())
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
    /// The newest 14-day market and the free balance to size from, for the search.
    latest: RwLock<Option<(Arc<bybit_mean_reversion_bot::engine::Market>, f64)>>,
    /// The latest completed parameter search.
    search: RwLock<Option<Search>>,
}

/// One completed parameter search (full 14-day window + champion/challenger).
#[derive(Clone)]
struct Search {
    report: SearchReport,
    chosen: Option<XsParams>,
    choice: walkforward::Choice,
    finished_ms: i64,
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

    fn save_paper(&self) -> Result<()> {
        let guard = self.paper.read().unwrap_or_else(|p| p.into_inner());
        if let Some(ps) = guard.as_ref() {
            persist_paper(&self.dir.join(PAPER_FILE), ps)?;
        }
        Ok(())
    }
}

pub async fn serve(dir: PathBuf) -> Result<()> {
    info!("Bybit Mean Reversion Bot — long only (paper + signals only): 15m bars, top {} USDT perps, full 14-day search over every parameter, CPU target {}%",
        walkforward::UNIVERSE, governor::CPU_TARGET_PCT);
    governor::global();
    let paper = load_paper(&dir.join(PAPER_FILE))?;
    let health = Health::default();
    // Read-only credentials for the account balance and fee rates; no guessing
    // without them (the service stops and launchd retries).
    let creds = keychain::load()?;
    let client = data::Client::new()?;
    let account = client.usdt_account(&creds).await?;
    let balance = account.wallet;
    info!(
        "real account USDT wallet balance {balance:.2} ({:.2} committed elsewhere)",
        account.reserved()
    );
    let app = Arc::new(App {
        creds,
        rules_at: std::sync::Mutex::new(None),
        client,
        cache: data::Cache::open(dir.join("data.db").to_str().unwrap_or("data.db"))?,
        dir,
        health: health.clone(),
        status: RwLock::new(Status {
            strategy: "Long only: signal and settings chosen by a search over every parameter on one 14-day window, two safety rules (paper only)".into(),
            started_ms: chrono::Utc::now().timestamp_millis(),
            universe: walkforward::UNIVERSE,
            ..Default::default()
        }),
        signals: RwLock::new(vec![]),
        paper: RwLock::new(paper),
        // NOTE(agents): The chart belongs to THIS process and current engine only.
        // Never deserialize an old chart or reuse a prior deployment's result.
        research: RwLock::new(None),
        latest: RwLock::new(None),
        search: RwLock::new(None),
    });

    let a = app.clone();
    health.spawn("bar_task", Duration::from_secs(1200), move |hb| {
        let a = a.clone();
        async move { bar_task(a, hb).await }
    });
    let a = app.clone();
    // Stall limit above SEARCH_DEADLINE: the task cannot beat while a search runs.
    health.spawn(
        "search_task",
        SEARCH_DEADLINE + Duration::from_secs(600),
        move |hb| {
            let a = a.clone();
            async move { search_task(a, hb).await }
        },
    );
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
    app.save_paper()?;
    info!("shutdown");
    Ok(())
}

/// One 14-day forward test, day by day (14 days is the test limit): the live
/// full-window search picks settings on the 14 days Sep 3 -> Sep 17 2026, then they
/// trade the next 14 days, Sep 17 -> Oct 1, which they never saw, from
/// the real account balance. The earlier window only supplies the screener's look-back
/// at the start, as the live bot has it. Real cached Bybit bars + funding only
/// (research windows 2 and 3, `engine::research`); errors if missing.
// NOTE(agents): This is a HISTORICAL forward simulation, not live paper P&L.
// deploy.sh deletes/refetches these inputs before starting a changed build.
// Selection uses window 2 only; window 3 must never affect parameter selection.
fn forward_test_daily(dir: &Path, start: f64) -> Result<serde_json::Value> {
    let markets = [
        research::load_window(dir, 2)?,
        research::load_window(dir, 3)?,
    ];
    // Settings are chosen on the first 14-day window alone (exactly as live); the
    // next 14 days are traded on the joined timeline so the screener keeps its
    // look-back at the start (live it always has it): no artificial warm-up gap.
    let sc: Vec<_> = markets
        .iter()
        .map(|m| scores::compute(m, walkforward::UNIVERSE))
        .collect();
    let all = bybit_mean_reversion_bot::engine::Market::concat(&markets)?;
    let sc_all = scores::compute(&all, walkforward::UNIVERSE);
    let mut pf = XsPortfolio::new(start);
    let mut days: Vec<xs::DayRow> = Vec::new();
    let rep = walkforward::search_full(
        &markets[0],
        &sc[0],
        start,
        Instant::now() + SEARCH_DEADLINE,
        walkforward::LIVE_COMBOS,
        walkforward::live_combo,
    )?;
    let p = rep
        .params
        .ok_or_else(|| anyhow::anyhow!("no settings qualified on the first 14 days"))?;
    xs::run_daily(&all, &sc_all, BARS..2 * BARS, &p, &mut pf, &mut days, true);
    // NOTE(agents): An incomplete or failed simulation is never a valid chart.
    // This checks terminal funding/exit failures as well as persisted values.
    pf.validate_state()?;
    anyhow::ensure!(
        pf.positions.is_empty(),
        "forward chart has unclosed positions"
    );
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
        "test_kind": "historical_simulation",
        "assumptions": [
            "Uses the paper engine; no exchange orders or actual fills.",
            "Today's eligible universe, fees, risk tiers and order books are applied to historical candles.",
            "Historical report latency, intrabar price order and funding-history completeness are not verified."
        ],
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
        // Candidates by turnover (trading, no delisting scheduled) and, hourly,
        // their full Bybit rules: order rules, the account's fees, margin tiers,
        // measured books. The universe is the top 20 with complete rules.
        let lots = app.client.usdt_perpetual_lots().await?;
        // NOTE(agents): The only list of tradeable tokens: trading, no delisting scheduled, maximum
        //               leverage above 1x. Everything else is hard-excluded before ranking,
        //               fetching or scoring; its stored rows are deleted, and a held one stops the
        //               bar for explicit recovery (see below).
        let trading: std::collections::HashSet<String> =
            lots.iter().map(|(s, _, _)| s.clone()).collect();
        let candidates = app
            .client
            .top_margin_tokens(&lots, walkforward::CANDIDATES)
            .await?;
        app.cache.put_instruments(&candidates)?;
        let stale = app
            .rules_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none_or(|t| t.elapsed() >= RULES_REFRESH);
        if stale {
            let rules = app.client.fetch_rules(&app.creds, &candidates).await?;
            app.cache.put_rules(&rules)?;

            *app.rules_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            let mut s = app.status.write().unwrap_or_else(|e| e.into_inner());
            s.rules_symbols = rules.len();
            s.rules_ts = chrono::Utc::now().timestamp_millis();
        }
        hb.beat();
        // The real account balance, every bar (deposits and withdrawals are
        // mirrored; margin committed elsewhere on the account is reserved).
        let account = app.client.usdt_account(&app.creds).await?;
        let balance = account.wallet;
        // NOTE(agents): Universe = top 20 by 24h turnover among the top 20 that have complete Bybit
        //               rules (user requirement: drop coins without margin data).
        let lots = data::universe(
            &candidates,
            &app.cache.rules_symbols()?,
            walkforward::UNIVERSE,
        );
        if lots.len() < walkforward::UNIVERSE {
            app.event(
                "WARN",
                format!(
                    "only {} of the top {} coins have complete Bybit rules",
                    lots.len(),
                    walkforward::CANDIDATES
                ),
            );
        }
        let mut symbols: Vec<String> = lots.into_iter().map(|(s, _, _)| s).collect();
        app.cache.put_universe(&symbols)?;
        // NOTE(agents): Strict exclusion includes stale held symbols. Purge their cached data
        // before any fetch; an excluded holding requires explicit recovery, never a fabricated exit.
        let purged = app.cache.retain_symbols(&trading)?;
        if purged > 0 {
            app.event(
                "INFO",
                format!("purged stored data of {purged} ineligible symbol(s)"),
            );
        }
        // Keep managing eligible held symbols even if turnover leaves the top 20.
        if let Some(ps) = app.paper.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            for pos in &ps.portfolio.positions {
                anyhow::ensure!(trading.contains(&pos.symbol),
                    "held symbol {} is delisted or lacks eligible leverage; excluded from fetching, recovery required", pos.symbol);
                if !symbols.contains(&pos.symbol) {
                    symbols.push(pos.symbol.clone());
                }
            }
        }
        symbols.sort();
        let hb2 = hb.clone();
        let last = data::sync(&app.client, &app.cache, &symbols, move |_, _| hb2.beat()).await?;
        if last < closed {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        let sync_secs = t0.elapsed().as_secs_f64();
        let mut market = app.cache.market(&symbols, last)?;
        // NOTE(agents): Skipping is fine for coins we don't hold. A HELD coin with incomplete data
        //               must fail the bar: never mark, fund or close a position on missing prices.
        // A symbol whose real data is incomplete this bar (a sync failure or an
        // exchange gap) sits out; a held one must be complete or the bar fails.
        let incomplete: Vec<(String, String)> = (0..market.symbols.len())
            .filter_map(|s| {
                let e = market.validate_symbol(s).err()?;
                Some((market.symbols[s].clone(), format!("{e:#}")))
            })
            .collect();
        if !incomplete.is_empty() {
            let held: Vec<String> = app
                .paper
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|ps| {
                    ps.portfolio
                        .positions
                        .iter()
                        .map(|p| p.symbol.clone())
                        .collect()
                })
                .unwrap_or_default();
            if let Some((_, e)) = incomplete.iter().find(|(s, _)| held.contains(s)) {
                anyhow::bail!("held symbol data incomplete: {e}");
            }
            symbols.retain(|s| incomplete.iter().all(|(x, _)| x != s));
            app.event(
                "WARN",
                format!(
                    "{} symbol(s) skipped this bar for incomplete real data (first: {})",
                    incomplete.len(),
                    incomplete[0].1
                ),
            );
            market = app.cache.market(&symbols, last)?;
        }
        let market = Arc::new(market);
        hb.beat();
        // NOTE(agents): Size the search from the FREE balance (minus margin committed elsewhere),
        //               like the live sizing, so the search and safety rules judge the account that
        //               would really trade.
        // The search sizes from the money actually in play: the paper account's
        // equity (which follows the real balance), else the real balance.
        let wf_equity = (app
            .paper
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(balance, |ps| {
                ps.portfolio.equity + ps.portfolio.unrealized()
            })
            - account.reserved())
        .max(0.0);
        let prev_params = app
            .paper
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|ps| ps.params.clone());
        let m2 = market.clone();
        let sc = tokio::task::spawn_blocking(move || scores::compute(&m2, walkforward::UNIVERSE))
            .await?;
        // Hand the newest data to the search task (it runs on its own schedule).
        *app.latest.write().unwrap_or_else(|e| e.into_inner()) = Some((market.clone(), wf_equity));
        hb.beat();
        last_done = last;
        let n = market.ts.len();
        let search = app.search.read().unwrap_or_else(|e| e.into_inner()).clone();
        let chosen = search.as_ref().and_then(|x| x.chosen.clone());
        // NOTE(agents): Paper trades the best SAFE settings from the latest search
        //               (champion/challenger, walkforward::choose_live). Entries stop only when no
        //               settings pass the safety rules (no liquidation, drawdown <= 25% over 14
        //               days) or before the first search finishes; held positions are then still
        //               managed with the last settings.
        let entries_allowed = chosen.is_some();
        if let Some(p) = chosen.clone().or(prev_params) {
            // Paper: same engine, every new bar in order.
            let new_lines = {
                let mut guard = app.paper.write().unwrap_or_else(|e| e.into_inner());
                let mut candidate = guard.clone().unwrap_or_else(|| PaperState {
                    schema_version: 2,
                    settlements_seen: Default::default(),
                    portfolio: XsPortfolio::new(balance),
                    last_ts: market.ts[n - 1] - BAR_MS,
                    params: None,
                    last_account_balance: Some(balance),
                });
                let ps = &mut candidate;
                // A change in the real balance (the account is flat: the bot never
                // trades) is a deposit or withdrawal: mirror it, not as profit.
                let mut flow = None;
                if let Some(prev) = ps.last_account_balance {
                    let delta = balance - prev;
                    if delta != 0.0 {
                        flow = Some(delta);
                    }
                }

                let (before_trades, before_pos): (usize, Vec<(String, Side, bool)>) = (
                    ps.portfolio.trades.len(),
                    ps.portfolio
                        .positions
                        .iter()
                        .map(|x| (x.symbol.clone(), x.side, x.added))
                        .collect(),
                );
                // The symbol list is re-fetched every bar: re-point positions by name.
                ps.portfolio.reindex(&market.symbols);
                advance_paper(
                    ps,
                    &market,
                    &sc,
                    &p,
                    entries_allowed,
                    chrono::Utc::now().timestamp_millis(),
                    Some(account),
                )?;
                persist_paper(&app.dir.join(PAPER_FILE), ps)?;
                let mut lines: Vec<String> = flow
                    .map(|d| format!("ACCOUNT observed external wallet change {d:+.8} USDT (now {balance:.8}): paper capital adjusted after replay; source unclassified"))
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
                            "PAPER add {:?} {}: closed {}% against, size doubled, average now {} (liquidation {:.6})",
                            x.side, x.symbol, p.risk.add_pct.unwrap_or_default(), x.entry, x.liquidation
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
                *guard = Some(candidate);
                lines
            };
            for l in new_lines {
                app.event("INFO", l);
            }
            app.save_paper()?;

            // Signals at the latest bar.
            let t = n - 1;
            let targets = if entries_allowed {
                app.paper
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|ps| ps.portfolio.executable_targets(&market, &sc, t, &p))
                    .unwrap_or_default()
            } else {
                Default::default()
            };
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

        let hold = chosen.as_ref().map_or(16, |p| p.hold) as i64;
        let choice = search.as_ref().map(|x| x.choice);
        let next_rebalance = ((last + BAR_MS) / (hold * BAR_MS) + 1) * hold * BAR_MS;
        let paper = app.paper.read().unwrap_or_else(|e| e.into_inner());
        let mut s = app.status.write().unwrap_or_else(|e| e.into_inner());
        s.last_bar_ts = last;
        s.account_balance = balance;
        s.account_reserved = account.reserved();
        s.account_balance_ts = chrono::Utc::now().timestamp_millis();
        s.next_rebalance_ts = next_rebalance;
        s.symbols = symbols.len();
        s.sync_secs = sync_secs;
        s.choice = choice;
        s.params = chosen.clone();
        s.search_finished_ms = search.as_ref().map_or(0, |x| x.finished_ms);
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
            if ps.portfolio.entries_allowed {
                s.effective_pairs = ps.portfolio.effective_top;
                s.allocation_note = ps.portfolio.allocation_note.clone();
            } else {
                s.effective_pairs = 0;
                s.allocation_note =
                    "no entries: no settings pass the safety rules (no liquidation, drawdown <= 25%)"
                        .into();
            }
        }
        s.mode = match choice {
            None => "SEARCHING (first search running)",
            Some(walkforward::Choice::NewBest) => "TRADING (new best settings)",
            Some(walkforward::Choice::KeptLastBest) => "TRADING (kept last best settings)",
            Some(walkforward::Choice::NoneSafe) if s.paper_open_positions > 0 => {
                "NO SAFE SETTINGS (exits pending)"
            }
            Some(walkforward::Choice::NoneSafe) => "NO SAFE SETTINGS (flat)",
        }
        .into();
        let verdict = match search.as_ref().map(|x| &x.report) {
            None => "no search finished yet".to_string(),
            Some(r) => format!(
                "last search: {} of {} combinations, {} usable",
                r.evaluated, r.combos, r.usable
            ),
        };
        let summary = format!(
            "bar {} | {} pairs | sync {:.0}s | {} | {} | paper equity {:.2}",
            chrono::DateTime::from_timestamp_millis(last)
                .map(|d| d.format("%m-%d %H:%M").to_string())
                .unwrap_or_default(),
            symbols.len(),
            sync_secs,
            s.mode,
            verdict,
            s.paper_equity
        );
        s.report = search.map(|x| x.report);
        drop(s);
        drop(paper);
        app.event("INFO", summary);
    }
}

/// The parameter search: every live-grid combination (`walkforward::LIVE_COMBOS`)
/// backtested over the full 14 days, then champion/challenger against the
/// settings in use.
/// It starts again `SEARCH_PAUSE` after each search finishes.
async fn search_task(app: Arc<App>, hb: Heartbeat) -> Result<()> {
    loop {
        hb.beat();
        let latest = app.latest.read().unwrap_or_else(|e| e.into_inner()).clone();
        let Some((market, equity)) = latest else {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        // The champion: the settings the latest search chose, else paper's.
        let champion = app
            .search
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|x| x.chosen.clone())
            .or_else(|| {
                app.paper
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .and_then(|ps| ps.params.clone())
            });
        let combos = walkforward::LIVE_COMBOS;
        let started = Instant::now();
        let result = tokio::task::spawn_blocking(move || -> Result<_> {
            let sc = scores::compute(&market, walkforward::UNIVERSE);
            let report = walkforward::search_full(
                &market,
                &sc,
                equity,
                Instant::now() + SEARCH_DEADLINE,
                combos,
                walkforward::live_combo,
            )?;
            let (chosen, choice) = walkforward::choose_live(
                &market,
                &sc,
                equity,
                report.params.as_ref(),
                champion.as_ref(),
            );
            Ok((report, chosen, choice))
        })
        .await?;
        hb.beat();
        let pause = match result {
            Ok((report, chosen, choice)) => {
                app.event(
                    "INFO",
                    format!(
                        "SEARCH {combos} combinations, {} evaluations in {:.0}s: {:?}; next search in {} min",
                        report.evaluated,
                        started.elapsed().as_secs_f64(),
                        choice,
                        SEARCH_PAUSE.as_secs() / 60
                    ),
                );
                *app.search.write().unwrap_or_else(|e| e.into_inner()) = Some(Search {
                    report,
                    chosen,
                    choice,
                    finished_ms: chrono::Utc::now().timestamp_millis(),
                });
                // The dashboard's chronological check (choose on research window 2, trade
                // window 3) is built once, after the first live search, so the two full
                // searches don't share the CPU budget.
                let unbuilt = app
                    .research
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none();
                if unbuilt {
                    let a = app.clone();
                    let chart =
                        tokio::task::spawn_blocking(move || forward_test_daily(&a.dir, equity))
                            .await?;
                    let value = chart.unwrap_or_else(|e| {
                        warn!("forward test chart unavailable: {e:#}");
                        serde_json::json!({ "error": format!("{e:#}") })
                    });
                    *app.research.write().unwrap_or_else(|e| e.into_inner()) = Some(value);
                    hb.beat();
                }
                SEARCH_PAUSE
            }
            Err(e) => {
                app.event(
                    "WARN",
                    format!("SEARCH failed ({e:#}); previous settings stay in use; retry in 5 min"),
                );
                SEARCH_RETRY
            }
        };
        let until = Instant::now() + pause;
        while Instant::now() < until {
            hb.beat();
            tokio::time::sleep(Duration::from_secs(30).min(until - Instant::now())).await;
        }
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
<section><b>Next rebalance targets</b> <span class=k id=sighelp></span><table id=sig></table></section>
<section><b>Live event log</b><pre id=ev></pre></section>
<section><b>Recent paper trades</b><table id=tr></table></section>
<section><b>Forward test, day by day</b> <span class=k>(historical simulation: settings chosen on the preceding 14 days, then tested on these 14 using the paper engine; current universe and execution measurements, simulated fills)</span><p class=k>Daily equity change includes realised and unrealised P&amp;L, fees and funding. Closed wins counts only profitable trades closed that day; equity can rise with zero closed wins.</p><div id=fsum class=k></div><div id=fchart></div><table id=fdays></table></section>
<section><b>Parameter search (one 14-day window, every combination; next search 60 min after the last one finished)</b><pre id=wf></pre></section>
<script>
const f=(x,d=2)=>x==null||isNaN(x)?'-':Number(x).toFixed(d);
const t=ms=>ms?new Date(ms).toLocaleString():'-';
async function tick(){try{
const s=await (await fetch('/api/status')).json(), g=await (await fetch('/api/signals')).json();
document.getElementById('strat').textContent=s.strategy+' — last update '+t(s.last_bar_ts+900000);
const sg=x=>(x>=0?'+':'')+f(x), pc=x=>sg(x)+'%', st=s.paper_start_equity||1;
const pu=p=>(p.side==="Long"?1:-1)*(p.mark-p.entry)*p.qty-p.fees-p.funding;
const cards=[['Real account balance',f(s.account_balance)+' USDT <span class=k>(Bybit, read-only, '+f(s.account_reserved)+' committed elsewhere, '+t(s.account_balance_ts)+')</span>'],['Paper equity',f(s.paper_equity)+' USDT <span class=k>('+pc((s.paper_equity/st-1)*100)+' from '+f(st,0)+')</span>'],['Realised P&L <span class=k title="closed trades + fees and funding already paid">ⓘ</span>','<span class='+(s.paper_realized>=0?'ok':'bad')+'>'+sg(s.paper_realized)+' USDT ('+pc(s.paper_realized/st*100)+')</span>'],['Open P&L <span class=k title="open positions marked at the last close">ⓘ</span>','<span class='+(s.paper_open>=0?'ok':'bad')+'>'+sg(s.paper_open)+' USDT ('+pc(s.paper_open/st*100)+')</span>'],['Open positions',s.paper_open_positions],['Dynamic allocation',s.effective_pairs+' pairs <span class=k>'+s.allocation_note+'</span>'],
['Closed trades / wins',s.paper_trades+' / '+s.paper_wins],['Max drawdown',f(s.paper_max_drawdown_pct,1)+'%'],
['Paper settings',s.report?'<span class='+((s.mode||'').startsWith('TRADING')?'ok':'bad')+'>'+s.mode+'</span>':'…'],
['Parameter search',s.search_finished_ms?'last finished '+t(s.search_finished_ms)+', next about '+t(s.search_finished_ms+3600000):'first search running'],['Last 15m bar',t(s.last_bar_ts+900000)],['Pairs scanned',s.symbols+' (top '+s.universe+')'],['Bybit rules',s.rules_symbols+' coins (fees, margin tiers, order books) measured '+t(s.rules_ts)],['CPU',f(s.cpu_pct,0)+'%'],
['Tasks',Object.entries(s.tasks||{}).map(([k,v])=>k+(v.running?' ✓':' ✗')).join(' ')]];
document.getElementById('sighelp').textContent='(at the next rebalance: long the '+s.effective_pairs+(s.params?' '+(s.params.flip?'highest':'lowest')+' '+s.params.signal+' values':' coins ranked first')+', among the top '+s.universe+' token USDT perps by 24h turnover; no entries below 5 USDT free balance)';
document.getElementById('cards').innerHTML=cards.map(c=>`<div class=card><span class=k>${c[0]}</span><b>${c[1]}</b></div>`).join('');
document.getElementById('nextreb').textContent=s.next_rebalance_ts?'next rebalance at '+t(s.next_rebalance_ts)+(s.params?' (every '+s.params.hold*15+' min)':''):'';
document.getElementById('pos').innerHTML='<tr><th>Symbol<th>Side<th>Entry<th>Mark<th>P&L USDT (after its fees + funding)<th>P&L % (margin)<th>Stop<th>Lev<th>Opened</tr>'+(s.paper_positions||[]).map(p=>{
 const u=pu(p),pl=u/p.margin*100;return `<tr><td>${p.symbol}<td class=${p.side}>${p.side}<td>${p.entry}<td>${p.mark}<td class=${u>=0?"ok":"bad"}>${sg(u)}<td class=${u>=0?"ok":"bad"}>${pc(pl)}<td>${f(p.stop,6)}<td>${f(p.leverage,1)}<td>${t(p.entry_ts)}</tr>`}).join('');
const tg=g.filter(r=>r.target||r.held);
document.getElementById('sig').innerHTML='<tr><th>Symbol<th>Target<th>Held<th>Signal value (sign flipped when buying the highest)<th>Rank (1 = bought first)<th>Close<th>24h turnover</tr>'+
tg.map(r=>`<tr><td>${r.symbol}<td class=${r.target||''}>${r.target||'-'}<td class=${r.held||''}>${r.held||'-'}<td>${f(r.signal)}<td>${r.rank}<td>${r.close}<td>${f(r.turnover_24h/1e6,1)}M</tr>`).join('');
document.getElementById('ev').textContent=(s.events||[]).join('\n');
document.getElementById('tr').innerHTML='<tr><th>Symbol<th>Side<th>Entry<th>Exit<th>P&L USDT<th>P&L % (margin)<th>Reason<th>Closed</tr>'+(s.paper_recent_trades||[]).map(x=>`<tr><td>${x.symbol}<td class=${x.side}>${x.side}<td>${x.entry}<td>${x.exit}<td class=${x.pnl>=0?"ok":"bad"}>${sg(x.pnl)}<td class=${x.pnl>=0?"ok":"bad"}>${pc(x.r*100)}<td>${x.reason}<td>${t(x.exit_ts)}</tr>`).join('');
const r=s.report||{},b=r.metrics||{},bn=(b.end_equity||0)+(b.open_unrealized||0)-(b.start_equity||0);
const desc=p=>{if(!p)return 'none';const k=p.risk||{},x=[];if(k.take_profit_pct!=null)x.push('take-profit '+k.take_profit_pct+'%');if(k.close_stop_pct!=null)x.push('close-stop '+k.close_stop_pct+'%');if(k.add_pct!=null)x.push('add at -'+k.add_pct+'%');if(k.breaker_pct!=null)x.push('breaker '+k.breaker_pct+'%');if(k.derisk_pct!=null)x.push('half size after -'+k.derisk_pct+'%');if(k.vol_scaled)x.push('volatility-sized');
return `${p.signal}${p.signal==='Return'?' over '+p.lookback*15/60+'h':''} ${p.flip?'(follow)':'(contrarian)'}: ${p.long_only?'long the '+p.top+' lowest only':'long the '+p.top+' lowest, short the '+p.top+' highest'}${p.regime==='BtcTrend'?', only while BTC is above its 24h average':''}, rebalance every ${p.hold*15/60}h, ${p.gross_leverage}x, stop ${p.stop_pct==null?'none':p.stop_pct+'%'}${x.length?', '+x.join(', '):''}`};
document.getElementById('wf').textContent=r.combos?`parameter search: ${r.evaluated} of ${r.combos} combinations backtested over the full 14 days (one window, no split) in ${f(r.elapsed_ms/1000,0)} s; ${r.usable} pass the safety rules (no liquidation, drawdown ≤ 25%, at least 8 trades)
best found: ${desc(r.params)}
  its 14 days: ${sg(bn)} USDT (${pc(bn/(b.start_equity||1)*100)}), ${b.trades} trades, ${b.wins} wins, profit factor ${b.gross_loss>0?f(b.gross_profit/b.gross_loss):'-'}, max drawdown ${f(b.max_drawdown_pct,1)}%  (chosen on these same 14 days: not a forecast; see the forward test below)
settings paper uses: ${desc(s.params)} (${s.choice==='NewBest'?'new best':s.choice==='KeptLastBest'?'kept: nothing scored higher':'none pass the safety rules'})
real closed 15m Bybit bars ${t(r.first_bar_ts)} → ${t(r.last_bar_ts+900000)}`:'waiting for the first search…';
}catch(e){}}
// NOTE(agents): Clear old geometry before loading. An unavailable/error result
// must never leave a previous successful chart visible beside a new status.
async function research(){try{
document.getElementById('fchart').innerHTML='';
document.getElementById('fdays').innerHTML='';
const r=await (await fetch('/api/research')).json();
if(!r){document.getElementById('fsum').textContent='computing from the cached real data…';setTimeout(research,10000);return}
if(r.error){document.getElementById('fsum').textContent='unavailable: '+r.error;return}
// NOTE(agents): Keep execution assumptions visible; a simulated historical
// chart must never be presented as actual live fills or realised profit.
const F=(x,d=2)=>Number(x).toFixed(d),S=x=>(x>=0?'+':'')+F(x),P=x=>S(x)+'%',D=ms=>new Date(ms).toISOString().slice(5,10);
const days=r.days,pos=days.filter(d=>d.pnl>0).length;
const best=days.reduce((a,d)=>d.pnl>a.pnl?d:a,days[0]),worst=days.reduce((a,d)=>d.pnl<a.pnl?d:a,days[0]);
document.getElementById('fsum').innerHTML=`${F(r.start_equity,0)} → <b>${F(r.end_equity)} USDT</b> = <b class=${r.net>=0?'ok':'bad'}>${S(r.net)} USDT (${P(r.return_pct)})</b> over ${F((r.segments[0].traded[1]-r.segments[0].traded[0])/864e5,0)} days (${new Date(r.segments[0].traded[0]).toISOString().slice(0,16).replace("T"," ")} → ${new Date(r.segments[0].traded[1]).toISOString().slice(0,16).replace("T"," ")} UTC) · ${pos} of ${days.length} UTC dates up · best day ${D(best.day_ts)} ${S(best.pnl)} (${P(best.pnl_pct)}) · worst day ${D(worst.day_ts)} ${S(worst.pnl)} (${P(worst.pnl_pct)}) · max drawdown ${F(r.max_drawdown_pct,1)}% · ${r.trades} trades, ${r.wins} wins · profit factor ${F(r.profit_factor)} · fees ${F(r.fees)} · funding ${S(-r.funding)} · liquidations ${r.liquidations}<br>`+
 r.segments.map((s,i)=>`segment ${i+1}: settings chosen on ${D(s.chosen_on[0])}–${D(s.chosen_on[1])}, traded ${D(s.traded[0])}–${D(s.traded[1])}: ${S(s.end_equity-s.start_equity)} USDT (${P((s.end_equity/s.start_equity-1)*100)}) — ${s.params.signal} ${s.params.flip?'follow':'contrarian'}, top ${s.params.top}, every ${s.params.hold/4}h, ${s.params.gross_leverage}x, stop ${s.params.stop_pct??'none'}`).join('<br>');
const assumptions=document.createElement('div');
assumptions.className='k';
assumptions.textContent=(r.assumptions||[]).join(' ');
document.getElementById('fsum').appendChild(assumptions);
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
document.getElementById('fchart').innerHTML=g+`<text x=${L} y=${by0-bH/2-4} fill=#666>daily equity change USDT (includes unrealised P&amp;L)</text></svg>`;
document.getElementById('fdays').innerHTML='<tr><th>Day (UTC)<th>Equity change USDT<th>Equity change %<th>End-of-day equity<th>Trades closed<th>Closed wins</tr>'+days.map(d=>`<tr><td>${new Date(d.day_ts).toISOString().slice(0,10)}<td class=${d.pnl>=0?'ok':'bad'}>${S(d.pnl)}<td class=${d.pnl>=0?'ok':'bad'}>${P(d.pnl_pct)}<td>${F(d.equity)}<td>${d.trades}<td>${d.wins}</tr>`).join('');
}catch(e){document.getElementById('fsum').textContent='chart error: '+e}}
research();
// NOTE(agents): Reload on process restart: the page embeds strategy labels and
// chart code. Polling new numbers into an old page would mix code versions.
let boot=null;
async function guard(){try{const s=await (await fetch('/api/status')).json();if(boot&&s.started_ms!==boot)location.reload();boot=s.started_ms;}catch(e){}}
tick();guard();setInterval(tick,5000);setInterval(guard,15000);
</script>"#;

#[cfg(test)]
mod causal_tests {
    use super::*;
    #[test]
    fn new_report_does_not_change_replayed_bars() {
        let mut market = bybit_mean_reversion_bot::engine::Market {
            marks: Vec::new(),
            listing_times: Vec::new(),
            entry_eligible: Vec::new(),
            ts: (0..200).map(|i| i * BAR_MS).collect(),
            symbols: vec!["A".into(), "B".into()],
            bars: [-0.1, 0.1]
                .iter()
                .map(|drift| {
                    (0..200)
                        .map(|i| {
                            let price = 100.0 * (1.0 + drift * i as f64 / 200.0);
                            Some(bybit_mean_reversion_bot::engine::Bar {
                                open: price,
                                high: price,
                                low: price,
                                close: price,
                                volume: 1.0,
                                turnover: 100.0,
                            })
                        })
                        .collect()
                })
                .collect(),
            rules: vec![
                Some(bybit_mean_reversion_bot::engine::rules::Rules {
                    qty_step: 0.0,
                    min_qty: 0.0,
                    min_notional: 5.0,
                    max_market_qty: 1e12,
                    taker_fee: 0.00055,
                    mm_tiers: vec![bybit_mean_reversion_bot::engine::rules::MarginTier {
                        limit: 1e12,
                        rate: 0.01,
                        deduction: 0.0,
                        max_leverage: 100.0
                    }],
                    book: vec![(6400.0, 0.0002, 0.0002)],
                    book_ts: 0
                });
                2
            ],
            funding: vec![vec![]; 2],
        };
        market.marks = market.bars.clone();
        let sc = scores::compute(&market, 100);
        let old = XsParams {
            signal: xs::Signal::Return,
            flip: false,
            lookback: 1,
            hold: 2,
            top: 1,
            gross_leverage: 1.0,
            stop_pct: None,
            risk: xs::Risk::default(),
            long_only: false,
            regime: xs::Regime::Off,
        };
        let mut next = old.clone();
        next.hold = 4;
        let mut ps = PaperState {
            schema_version: 2,
            settlements_seen: Default::default(),
            portfolio: XsPortfolio::new(1000.0),
            last_ts: 175 * BAR_MS,
            params: Some(old.clone()),
            last_account_balance: None,
        };
        ps.portfolio.entries_allowed = false;
        let mut expected = ps.portfolio.clone();
        for t in 176..200 {
            expected.step(&market, &sc, t, &old);
        }
        expected.install_entry_gate(true);
        expected.decide_close(&market, &sc, 199, &next);
        expected.defer_new_decisions(market.ts[199] + BAR_MS);
        advance_paper(
            &mut ps,
            &market,
            &sc,
            &next,
            true,
            market.ts[199] + BAR_MS,
            None,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&ps.portfolio).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(ps.params.unwrap().hold, 4);
        assert_eq!(ps.last_ts, 199 * BAR_MS);
    }
}
