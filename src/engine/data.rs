//! Bybit REST data (REST polling only, no WebSockets). Public: USDT linear
//! perpetual instruments, CLOSED 15m klines, settled funding, risk limits and
//! order books. Signed and read-only: the account's fee rates and wallet balance.
//! Every number is parsed strictly: a missing or garbled field drops that row
//! (or fails the call), it is never replaced by a default. Cached in SQLite and
//! assembled into a `Market` on a common timeline.

use super::keychain::Credentials;
use super::rules::{MarginTier, Rules};
use super::{Bar, Market, BARS, BAR_MS, INTERVAL};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BASE: &str = "https://api.bybit.com";
/// ~14 requests/second, well inside Bybit's public limits.
const PACE: Duration = Duration::from_millis(70);

pub struct Client {
    http: reqwest::Client,
    last: tokio::sync::Mutex<tokio::time::Instant>,
}

/// Bybit lotSizeFilter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LotFilter {
    pub qty_step: f64,
    pub min_qty: f64,
    pub min_notional: f64,
    pub max_market_qty: f64,
}

/// A number Bybit sent (string or number); None if missing or unparsable,
/// never a default.
fn strict(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

impl Client {
    pub fn new() -> Result<Self> {
        Ok(Client {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .build()?,
            last: tokio::sync::Mutex::new(tokio::time::Instant::now()),
        })
    }

    async fn get(&self, path: &str, query: &str) -> Result<Value> {
        let url = format!("{BASE}{path}?{query}");
        let mut err = anyhow!("{path}: no attempt");
        for attempt in 0..5u32 {
            {
                let mut last = self.last.lock().await;
                let next = *last + PACE;
                tokio::time::sleep_until(next).await;
                *last = tokio::time::Instant::now();
            }
            match self.http.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let code = v["retCode"].as_i64().unwrap_or(-1);
                    if status.as_u16() == 429 || code == 10006 || code == 10018 {
                        err = anyhow!("{path}: rate limited");
                        tokio::time::sleep(Duration::from_secs(2u64 << attempt)).await;
                        continue;
                    }
                    if code != 0 {
                        bail!(
                            "{path}: Bybit retCode {code}: {}",
                            v["retMsg"].as_str().unwrap_or("?")
                        );
                    }
                    return Ok(v["result"].clone());
                }
                Err(e) => {
                    err = anyhow!("{path}: {e}");
                    tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
                }
            }
        }
        Err(err)
    }

    /// Trading USDT linear perpetuals with parsed lot filters, excluding
    /// scheduled delistings. An incomplete filter returns None for that symbol.
    pub async fn usdt_perpetual_lots(&self) -> Result<Vec<(String, Option<LotFilter>, i64)>> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let mut q = "category=linear&limit=1000".to_string();
            if !cursor.is_empty() {
                q.push_str(&format!("&cursor={cursor}"));
            }
            let r = self.get("/v5/market/instruments-info", &q).await?;
            for i in r["list"].as_array().cloned().unwrap_or_default() {
                if i["quoteCoin"] == "USDT"
                    && i["status"] == "Trading"
                    && i["contractType"] == "LinearPerpetual"
                    && strict(&i["deliveryTime"]) == Some(0.0)
                    && i["isPreListing"] == false
                    && i["underlyingTicker"].as_str() == Some("")
                    && matches!(i["symbolType"].as_str(), Some("" | "innovation"))
                {
                    let l = &i["lotSizeFilter"];
                    let lot = (|| {
                        Some(LotFilter {
                            qty_step: strict(&l["qtyStep"])?,
                            min_qty: strict(&l["minOrderQty"])?,
                            min_notional: strict(&l["minNotionalValue"])?,
                            max_market_qty: strict(&l["maxMktOrderQty"])?,
                        })
                    })();
                    let symbol = i["symbol"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| anyhow!("instrument: missing symbol"))?;
                    let launch = i["launchTime"]
                        .as_str()
                        .and_then(|s| s.parse::<i64>().ok())
                        .filter(|t| *t > 0)
                        .ok_or_else(|| anyhow!("instrument {symbol}: invalid launchTime"))?;
                    out.push((symbol.to_string(), lot, launch));
                }
            }
            cursor = r["nextPageCursor"].as_str().unwrap_or_default().to_string();
            if cursor.is_empty() {
                break;
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Current top margin-eligible token perpetuals by public 24h turnover.
    pub async fn top_margin_tokens(
        &self,
        instruments: &[(String, Option<LotFilter>, i64)],
        count: usize,
    ) -> Result<Vec<(String, Option<LotFilter>, i64)>> {
        let r = self.get("/v5/market/tickers", "category=linear").await?;
        let rows = r["list"]
            .as_array()
            .ok_or_else(|| anyhow!("missing ticker list"))?;
        let mut ranked = Vec::new();
        // An instrument without a ticker or a valid turnover cannot be ranked
        // this bar (e.g. just listed); it is left out rather than guessed.
        for item in instruments.iter().filter(|(_, lot, _)| lot.is_some()) {
            let turnover = rows
                .iter()
                .find(|r| r["symbol"].as_str() == Some(item.0.as_str()))
                .and_then(|row| strict(&row["turnover24h"]))
                .filter(|v| *v >= 0.0);
            if let Some(turnover) = turnover {
                ranked.push((turnover, item.clone()));
            }
        }
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1 .0.cmp(&b.1 .0)));
        anyhow::ensure!(
            ranked.len() >= count,
            "only {} eligible margin tokens",
            ranked.len()
        );
        Ok(ranked.into_iter().take(count).map(|(_, i)| i).collect())
    }

    /// Maintenance-margin tiers per symbol: (position value limit, rate), ascending.
    pub async fn risk_limits(&self) -> Result<std::collections::HashMap<String, Vec<MarginTier>>> {
        let mut out: std::collections::HashMap<String, Vec<(MarginTier, Option<f64>)>> =
            Default::default();
        let mut invalid = std::collections::HashSet::new();
        let mut cursor = String::new();
        loop {
            let mut q = "category=linear".to_string();
            if !cursor.is_empty() {
                q.push_str(&format!("&cursor={cursor}"));
            }
            let r = self.get("/v5/market/risk-limit", &q).await?;
            for t in r["list"].as_array().cloned().unwrap_or_default() {
                let symbol = t["symbol"]
                    .as_str()
                    .ok_or_else(|| anyhow!("risk tier missing symbol"))?;
                let Some(tier) = parse_tier(&t) else {
                    invalid.insert(symbol.to_string());
                    continue;
                };
                out.entry(symbol.to_string()).or_default().push(tier);
            }
            cursor = r["nextPageCursor"].as_str().unwrap_or_default().to_string();
            if cursor.is_empty() {
                break;
            }
        }
        Ok(out
            .into_iter()
            .filter(|(symbol, _)| !invalid.contains(symbol))
            .filter_map(|(symbol, tiers)| Some((symbol, with_deductions(tiers)?)))
            .collect())
    }

    /// The current order book (500 levels per side): (bids, asks, ts ms), best first.
    pub async fn orderbook(&self, symbol: &str) -> Result<(Vec<(f64, f64)>, Vec<(f64, f64)>, i64)> {
        let r = self
            .get(
                "/v5/market/orderbook",
                &format!("category=linear&symbol={symbol}&limit=500"),
            )
            .await?;
        let side = |k: &str| -> Vec<(f64, f64)> {
            r[k].as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|l| Some((strict(&l[0])?, strict(&l[1])?)))
                .collect()
        };
        let ts =
            strict(&r["ts"]).ok_or_else(|| anyhow!("orderbook {symbol}: no timestamp"))? as i64;
        Ok((side("b"), side("a"), ts))
    }

    /// The account's taker fee per linear symbol (signed, read-only request).
    pub async fn taker_fees(
        &self,
        creds: &Credentials,
    ) -> Result<std::collections::HashMap<String, f64>> {
        let r = self
            .signed_get(creds, "/v5/account/fee-rate", "category=linear")
            .await?;
        Ok(r["list"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|f| {
                Some((
                    f["symbol"].as_str()?.to_string(),
                    strict(&f["takerFeeRate"])?,
                ))
            })
            .collect())
    }

    /// The real account's USDT wallet and the part of it already committed to
    /// open positions, open orders and locks (read-only).
    pub async fn usdt_account(&self, creds: &Credentials) -> Result<AccountBalance> {
        let r = self
            .signed_get(
                creds,
                "/v5/account/wallet-balance",
                "accountType=UNIFIED&coin=USDT",
            )
            .await?;
        let coin = r["list"][0]["coin"]
            .as_array()
            .and_then(|c| c.iter().find(|x| x["coin"] == "USDT"))
            .ok_or_else(|| anyhow!("wallet-balance: no USDT coin in the response"))?;
        parse_account(coin)
    }

    /// Signed GET (Bybit v5: HMAC-SHA256 of timestamp + key + recv_window + query),
    /// re-signed and retried like `get` on network errors and rate limits.
    async fn signed_get(&self, creds: &Credentials, path: &str, query: &str) -> Result<Value> {
        use hmac::{Hmac, Mac};
        let mut err = anyhow!("{path}: no attempt");
        for attempt in 0..5u32 {
            let ts = chrono::Utc::now().timestamp_millis().to_string();
            let recv = "5000";
            let mut mac = Hmac::<sha2::Sha256>::new_from_slice(creds.api_secret.as_bytes())?;
            mac.update(format!("{ts}{}{recv}{query}", creds.api_key).as_bytes());
            let sign = hex::encode(mac.finalize().into_bytes());
            let sent = self
                .http
                .get(format!("{BASE}{path}?{query}"))
                .header("X-BAPI-API-KEY", &creds.api_key)
                .header("X-BAPI-TIMESTAMP", &ts)
                .header("X-BAPI-RECV-WINDOW", recv)
                .header("X-BAPI-SIGN", sign)
                .send()
                .await;
            let body = match sent {
                Ok(resp) if resp.status().as_u16() == 429 => {
                    err = anyhow!("{path}: rate limited");
                    tokio::time::sleep(Duration::from_secs(2u64 << attempt)).await;
                    continue;
                }
                Ok(resp) => match resp.text().await {
                    Ok(body) => body,
                    Err(e) => {
                        err = anyhow!("{path}: {e}");
                        tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
                        continue;
                    }
                },
                Err(e) => {
                    err = anyhow!("{path}: {e}");
                    tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
                    continue;
                }
            };
            let v: Value = serde_json::from_str(&body)?;
            match v["retCode"].as_i64() {
                Some(0) => return Ok(v["result"].clone()),
                Some(10006 | 10018) => {
                    err = anyhow!("{path}: rate limited");
                    tokio::time::sleep(Duration::from_secs(2u64 << attempt)).await;
                }
                _ => bail!(
                    "{path}: Bybit retCode {}: {}",
                    v["retCode"],
                    v["retMsg"].as_str().unwrap_or("?")
                ),
            }
        }
        Err(err)
    }

    /// Full Bybit rules for every symbol that has all of them: order rules, the
    /// account's taker fee, margin tiers and a freshly measured order book.
    /// Symbols missing any piece are left out (never traded).
    pub async fn fetch_rules(
        &self,
        creds: &Credentials,
        lots: &[(String, Option<LotFilter>, i64)],
    ) -> Result<Vec<(String, Rules)>> {
        let fees = self.taker_fees(creds).await?;
        let tiers = self.risk_limits().await?;
        let mut out = Vec::new();
        for (s, lot, _) in lots {
            let (Some(l), Some(&fee), Some(t)) = (lot, fees.get(s), tiers.get(s)) else {
                continue;
            };
            let Ok((bids, asks, ts)) = self.orderbook(s).await else {
                continue;
            };
            let book = super::rules::measure_book(&bids, &asks);
            if book.is_empty() || t.is_empty() {
                continue;
            }
            out.push((
                s.clone(),
                Rules {
                    qty_step: l.qty_step,
                    min_qty: l.min_qty,
                    min_notional: l.min_notional,
                    max_market_qty: l.max_market_qty,
                    taker_fee: fee,
                    mm_tiers: t.clone(),
                    book,
                    book_ts: ts,
                },
            ));
        }
        // A refresh replaces the stored snapshot: one that lost most symbols
        // (an API change or outage) must fail instead of wiping the rules.
        anyhow::ensure!(
            out.len() * 2 >= lots.iter().filter(|(_, lot, _)| lot.is_some()).count(),
            "rules measured for only {} of {} symbols; keeping the previous snapshot",
            out.len(),
            lots.len()
        );
        Ok(out)
    }

    /// Closed 15m bars with open time >= since (oldest first); never the forming bar.
    pub async fn klines_since(&self, symbol: &str, since: i64) -> Result<Vec<(i64, Bar)>> {
        self.klines_range(symbol, since, i64::MAX).await
    }

    /// Closed 15m bars with open time in [start, end] (oldest first); never the forming bar.
    pub async fn klines_range(
        &self,
        symbol: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<(i64, Bar)>> {
        self.price_range(symbol, start, end, false).await
    }

    pub async fn mark_range(&self, symbol: &str, start: i64, end: i64) -> Result<Vec<(i64, Bar)>> {
        self.price_range(symbol, start, end, true).await
    }

    async fn price_range(
        &self,
        symbol: &str,
        start: i64,
        end: i64,
        mark: bool,
    ) -> Result<Vec<(i64, Bar)>> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut out: Vec<(i64, Bar)> = Vec::new();
        let mut page_end: Option<i64> = (end < now).then_some(end);
        loop {
            let mut q = format!(
                "category=linear&symbol={symbol}&interval={INTERVAL}&limit=1000&start={start}"
            );
            if let Some(e) = page_end {
                q.push_str(&format!("&end={e}"));
            }
            let r = self
                .get(
                    if mark {
                        "/v5/market/mark-price-kline"
                    } else {
                        "/v5/market/kline"
                    },
                    &q,
                )
                .await?;
            let rows = r["list"]
                .as_array()
                .ok_or_else(|| anyhow!("{symbol}: missing kline list"))?;
            if rows.is_empty() {
                break;
            }
            let mut oldest = i64::MAX;
            for k in rows {
                let ts = k[0]
                    .as_str()
                    .and_then(|s| s.parse::<i64>().ok())
                    .ok_or_else(|| anyhow!("{symbol}: invalid kline timestamp"))?;
                oldest = oldest.min(ts);
                if ts + BAR_MS > now - 3_000 || ts < start || ts > end {
                    continue; // still forming / outside range
                }
                // A row with any missing or garbled field is dropped (the bar becomes a
                // gap), never filled with zeros.
                let bar = (|| {
                    Some(Bar {
                        open: strict(&k[1])?,
                        high: strict(&k[2])?,
                        low: strict(&k[3])?,
                        close: strict(&k[4])?,
                        volume: if mark { 0.0 } else { strict(&k[5])? },
                        turnover: if mark { 0.0 } else { strict(&k[6])? },
                    })
                })();
                let b = bar
                    .filter(|b| {
                        b.open > 0.0
                            && b.low > 0.0
                            && b.high >= b.open.max(b.close).max(b.low)
                            && b.low <= b.open.min(b.close)
                            && b.volume >= 0.0
                            && b.turnover >= 0.0
                    })
                    .ok_or_else(|| anyhow!("{symbol}: invalid OHLC at {ts}"))?;
                anyhow::ensure!(ts % BAR_MS == 0, "{symbol}: unaligned kline");
                out.push((ts, b));
            }
            if rows.len() < 1000 || oldest <= start {
                break;
            }
            anyhow::ensure!(
                page_end.is_none_or(|e| oldest <= e),
                "{symbol}: kline pagination did not progress"
            );
            page_end = Some(oldest - 1);
        }
        out.sort_by_key(|x| x.0);
        out.dedup_by_key(|x| x.0);
        Ok(out)
    }

    pub async fn funding_since(&self, symbol: &str, since: i64) -> Result<Vec<(i64, f64)>> {
        self.funding_range(symbol, since, chrono::Utc::now().timestamp_millis())
            .await
    }

    /// Settled funding in [start, end]. Paginated: Bybit returns at most 200
    /// records, newest first, and 1h-funding symbols settle 336 times in 14 days.
    pub async fn funding_range(
        &self,
        symbol: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<(i64, f64)>> {
        let mut v: Vec<(i64, f64)> = Vec::new();
        let mut page_end = end;
        loop {
            let r = self.get("/v5/market/funding/history",
                &format!("category=linear&symbol={symbol}&startTime={start}&endTime={page_end}&limit=200")).await?;
            let raw = r["list"]
                .as_array()
                .ok_or_else(|| anyhow!("funding {symbol}: missing list"))?;
            let rows = raw
                .iter()
                .map(|f| {
                    let ts = f["fundingRateTimestamp"]
                        .as_str()
                        .and_then(|s| s.parse::<i64>().ok())
                        .ok_or_else(|| anyhow!("funding {symbol}: invalid timestamp"))?;
                    let rate = strict(&f["fundingRate"])
                        .ok_or_else(|| anyhow!("funding {symbol}: invalid rate at {ts}"))?;
                    anyhow::ensure!(
                        ts >= start && ts <= page_end,
                        "funding {symbol}: out-of-range timestamp"
                    );
                    Ok((ts, rate))
                })
                .collect::<Result<Vec<_>>>()?;
            let oldest = rows.iter().map(|x| x.0).min();
            let full = rows.len() >= 200;
            v.extend(rows);
            match oldest {
                Some(o) if full && o > start => {
                    anyhow::ensure!(o <= page_end, "funding pagination did not progress");
                    page_end = o - 1
                }
                _ => break,
            }
        }
        v.sort_by_key(|x| x.0);
        v.dedup_by_key(|x| x.0);
        Ok(v)
    }
}

/// One maintenance-margin tier and its published deduction. Bybit sends an
/// empty `mmDeduction` for every lowest tier and for whole symbols on some
/// risk-limit tables; any other missing or garbled field invalidates the tier.
fn parse_tier(t: &Value) -> Option<(MarginTier, Option<f64>)> {
    let published = if t["mmDeduction"].as_str() == Some("") {
        None
    } else {
        Some(strict(&t["mmDeduction"]).filter(|v| *v >= 0.0)?)
    };
    Some((
        MarginTier {
            limit: strict(&t["riskLimitValue"]).filter(|v| *v > 0.0)?,
            rate: strict(&t["maintenanceMargin"]).filter(|v| *v >= 0.0 && *v < 1.0)?,
            deduction: 0.0,
            max_leverage: strict(&t["maxLeverage"]).filter(|v| *v > 0.0)?,
        },
        published,
    ))
}

/// Tiered maintenance margin is continuous at every tier boundary, which fixes
/// each deduction: d[i] = d[i-1] + limit[i-1] * (rate[i] - rate[i-1]). Bybit's
/// published values equal this exactly (BTC/ETH/SOL/DOGE, 2026-10-01); a
/// published value that disagrees invalidates the symbol (None).
fn with_deductions(mut tiers: Vec<(MarginTier, Option<f64>)>) -> Option<Vec<MarginTier>> {
    tiers.sort_by(|a, b| a.0.limit.total_cmp(&b.0.limit));
    let mut out: Vec<MarginTier> = Vec::with_capacity(tiers.len());
    for (mut tier, published) in tiers {
        tier.deduction = out.last().map_or(0.0, |prev| {
            prev.deduction + prev.limit * (tier.rate - prev.rate)
        });
        if let Some(d) = published {
            if (d - tier.deduction).abs() > 1e-6 * tier.deduction.abs().max(1.0) {
                return None;
            }
        }
        out.push(tier);
    }
    Some(out)
}

/// The first `n` of `candidates` (ranked by turnover) that have complete Bybit
/// rules: coins without margin tiers, fee, lot filter or a measurable book are
/// left out and the next coin by turnover takes the place.
pub fn universe(
    candidates: &[(String, Option<LotFilter>, i64)],
    measured: &std::collections::HashSet<String>,
    n: usize,
) -> Vec<(String, Option<LotFilter>, i64)> {
    candidates
        .iter()
        .filter(|(s, _, _)| measured.contains(s))
        .take(n)
        .cloned()
        .collect()
}

/// The real account's USDT wallet balance and how much of it is committed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccountBalance {
    pub wallet: f64,
    /// Initial margin of open positions and orders plus locked funds. None when
    /// Bybit does not report them (portfolio margin): treat as all committed.
    pub committed: Option<f64>,
}

impl AccountBalance {
    /// USDT this bot must never count as free.
    pub fn reserved(&self) -> f64 {
        self.committed
            .map_or(self.wallet, |c| c.clamp(0.0, self.wallet.max(0.0)))
    }
}

fn parse_account(coin: &Value) -> Result<AccountBalance> {
    let wallet = strict(&coin["walletBalance"])
        .ok_or_else(|| anyhow!("wallet-balance: no USDT walletBalance in the response"))?;
    let part = |k: &str| strict(&coin[k]).filter(|v| *v >= 0.0);
    let committed = match (
        part("totalPositionIM"),
        part("totalOrderIM"),
        part("locked"),
    ) {
        (Some(p), Some(o), Some(l)) => Some(p + o + l),
        _ => None,
    };
    Ok(AccountBalance { wallet, committed })
}

/// SQLite cache of bars, funding and Bybit rules.
#[derive(Clone)]
pub struct Cache(Arc<Mutex<Connection>>);

impl Cache {
    pub fn open(path: &str) -> Result<Self> {
        let c = Connection::open(path)?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS bars (symbol TEXT, ts INTEGER, open REAL, high REAL, low REAL, close REAL,
                volume REAL, turnover REAL, PRIMARY KEY (symbol, ts));
             CREATE TABLE IF NOT EXISTS funding (symbol TEXT, ts INTEGER, rate REAL, PRIMARY KEY (symbol, ts));
             CREATE TABLE IF NOT EXISTS marks (symbol TEXT, ts INTEGER, open REAL, high REAL, low REAL, close REAL, PRIMARY KEY (symbol, ts));
             CREATE TABLE IF NOT EXISTS rules (symbol TEXT PRIMARY KEY, json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS instruments (symbol TEXT PRIMARY KEY, launch_ts INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS first_trades (symbol TEXT PRIMARY KEY, ts INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS universe (symbol TEXT PRIMARY KEY);",
        )?;
        Ok(Cache(Arc::new(Mutex::new(c))))
    }

    fn with<R>(&self, f: impl FnOnce(&mut Connection) -> Result<R>) -> Result<R> {
        f(&mut self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Persist exchange listing timestamps independently of optional trading rules.
    pub fn put_instruments(&self, instruments: &[(String, Option<LotFilter>, i64)]) -> Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            for (symbol, _, launch) in instruments {
                anyhow::ensure!(*launch > 0, "invalid listing timestamp for {symbol}");
                tx.execute(
                    "INSERT OR REPLACE INTO instruments VALUES (?1,?2)",
                    params![symbol, launch],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Persist the explicitly selected live/research universe, without deleting historical data.
    pub fn put_universe(&self, symbols: &[String]) -> Result<()> {
        anyhow::ensure!(!symbols.is_empty(), "empty token universe");
        self.with(|c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM universe", [])?;
            for symbol in symbols {
                tx.execute("INSERT INTO universe VALUES (?1)", [symbol])?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Open time of the symbol's first possible candle: its launch rounded up to a
    /// bar, or later when Bybit's complete history begins after the launch (the
    /// pre-trading period of a new listing). None when the launch is unknown.
    fn listing_start(c: &Connection, symbol: &str) -> Result<Option<i64>> {
        let launch: Option<i64> = c
            .query_row(
                "SELECT launch_ts FROM instruments WHERE symbol=?1",
                [symbol],
                |r| r.get(0),
            )
            .optional()?;
        let first: Option<i64> = c
            .query_row(
                "SELECT ts FROM first_trades WHERE symbol=?1",
                [symbol],
                |r| r.get(0),
            )
            .optional()?;
        Ok(launch.map(|t| (((t + BAR_MS - 1) / BAR_MS) * BAR_MS).max(first.unwrap_or(0))))
    }

    /// First candle at or after `earliest` that is missing from `table`
    /// (`last + BAR_MS` when complete): internal gaps are retried, not just bars
    /// newer than the newest cached one. Never infers listing from cached bars.
    fn first_missing(&self, table: &str, symbol: &str, earliest: i64, last: i64) -> Result<i64> {
        self.with(|c| {
            let mut expected =
                Self::listing_start(c, symbol)?.map_or(earliest, |t| earliest.max(t));
            let mut st = c.prepare(&format!(
                "SELECT ts FROM {table} WHERE symbol=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts"
            ))?;
            for t in st.query_map(params![symbol, earliest, last], |r| r.get::<_, i64>(0))? {
                let t = t?;
                if t < expected {
                    continue;
                }
                if t > expected {
                    return Ok(expected);
                }
                expected = t + BAR_MS;
            }
            Ok(expected)
        })
    }

    pub fn bars_since(&self, symbol: &str, earliest: i64, last: i64) -> Result<i64> {
        self.first_missing("bars", symbol, earliest, last)
    }

    pub fn marks_since(&self, symbol: &str, earliest: i64, last: i64) -> Result<i64> {
        self.first_missing("marks", symbol, earliest, last)
    }

    /// `fetched` is Bybit's complete traded history from `since`. When `since` is
    /// the listing boundary of a symbol listed inside the window and Bybit's
    /// first candle comes later, trading began there: record it.
    pub fn note_first_trade(
        &self,
        symbol: &str,
        since: i64,
        earliest: i64,
        fetched: &[(i64, Bar)],
    ) -> Result<()> {
        let Some(&(first, _)) = fetched.first() else {
            return Ok(());
        };
        self.with(|c| {
            let start = Self::listing_start(c, symbol)?;
            if start == Some(since) && since >= earliest && first > since {
                c.execute(
                    "INSERT OR REPLACE INTO first_trades VALUES (?1,?2)",
                    params![symbol, first],
                )?;
            }
            Ok(())
        })
    }

    pub fn put_marks(&self, symbol: &str, bars: &[(i64, Bar)]) -> Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            for (ts, b) in bars {
                tx.execute(
                    "INSERT OR REPLACE INTO marks VALUES (?1,?2,?3,?4,?5,?6)",
                    params![symbol, ts, b.open, b.high, b.low, b.close],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn last_funding_ts(&self, symbol: &str) -> Result<Option<i64>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT MAX(ts) FROM funding WHERE symbol=?1",
                [symbol],
                |r| r.get(0),
            )?)
        })
    }

    pub fn put_bars(&self, symbol: &str, bars: &[(i64, Bar)]) -> Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            {
                let mut st =
                    tx.prepare("INSERT OR REPLACE INTO bars VALUES (?1,?2,?3,?4,?5,?6,?7,?8)")?;
                for (ts, b) in bars {
                    st.execute(params![
                        symbol, ts, b.open, b.high, b.low, b.close, b.volume, b.turnover
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn put_funding(&self, symbol: &str, f: &[(i64, f64)]) -> Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            {
                let mut st = tx.prepare("INSERT OR REPLACE INTO funding VALUES (?1,?2,?3)")?;
                for (ts, r) in f {
                    st.execute(params![symbol, ts, r])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Replace the complete Bybit rules snapshot atomically (refreshed hourly).
    pub fn put_rules(&self, rules: &[(String, Rules)]) -> Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            // This is a complete refresh snapshot. Omitted symbols must lose
            // entry eligibility instead of retaining old fees/books indefinitely.
            tx.execute("DELETE FROM rules", [])?;
            {
                let mut st = tx.prepare("INSERT OR REPLACE INTO rules VALUES (?1,?2)")?;
                for (s, r) in rules {
                    st.execute(params![s, serde_json::to_string(r)?])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Symbols with stored Bybit rules.
    pub fn rules_symbols(&self) -> Result<std::collections::HashSet<String>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT symbol FROM rules")?;
            let rows = st.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// (symbols, last bar ts) present in the cache.
    pub fn contents(&self) -> Result<(Vec<String>, i64)> {
        self.with(|c| {
            let mut st = c.prepare("SELECT DISTINCT symbol FROM bars WHERE NOT EXISTS (SELECT 1 FROM universe) OR symbol IN (SELECT symbol FROM universe) ORDER BY symbol")?;
            let syms = st
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            let last: i64 = c.query_row("SELECT MAX(ts) FROM bars", [], |r| r.get(0))?;
            Ok((syms, last))
        })
    }

    /// Drop data older than `keep_from` (the cache only ever needs ~14 days + warmup).
    pub fn prune(&self, keep_from: i64) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM bars WHERE ts < ?1", [keep_from])?;
            c.execute("DELETE FROM funding WHERE ts < ?1", [keep_from])?;
            c.execute("DELETE FROM marks WHERE ts < ?1", [keep_from])?;
            Ok(())
        })
    }

    /// Market of exactly BARS bars ending at `last_ts` for `symbols`.
    pub fn market(&self, symbols: &[String], last_ts: i64) -> Result<Market> {
        let first = last_ts - (BARS as i64 - 1) * BAR_MS;
        let ts: Vec<i64> = (0..BARS as i64).map(|i| first + i * BAR_MS).collect();
        self.with(|c| {
            let mut bars = Vec::with_capacity(symbols.len());
            let mut marks = Vec::with_capacity(symbols.len());
            let mut funding = Vec::with_capacity(symbols.len());
            let mut sb = c.prepare("SELECT ts, open, high, low, close, volume, turnover FROM bars WHERE symbol=?1 AND ts BETWEEN ?2 AND ?3")?;
            let mut sf = c.prepare("SELECT ts, rate FROM funding WHERE symbol=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts")?;
            for s in symbols {
                let mut row = vec![None; BARS];
                for r in sb.query_map(params![s, first, last_ts], |r| {
                    Ok((r.get::<_, i64>(0)?, Bar { open: r.get(1)?, high: r.get(2)?, low: r.get(3)?, close: r.get(4)?, volume: r.get(5)?, turnover: r.get(6)? }))
                })? {
                    let (t, b) = r?;
                    let idx = ((t - first) / BAR_MS) as usize;
                    if (t - first) % BAR_MS == 0 && idx < BARS {
                        row[idx] = Some(b);
                    }
                }
                bars.push(row);
                let mut row = vec![None; BARS];
                let mut sm = c.prepare("SELECT ts,open,high,low,close FROM marks WHERE symbol=?1 AND ts BETWEEN ?2 AND ?3")?;
                for r in sm.query_map(params![s, first, last_ts], |r| Ok((r.get::<_,i64>(0)?,Bar {open:r.get(1)?,high:r.get(2)?,low:r.get(3)?,close:r.get(4)?,volume:0.0,turnover:0.0})))? {
                    let (t,b)=r?; let idx=((t-first)/BAR_MS) as usize;
                    if (t-first)%BAR_MS==0 && idx<BARS { row[idx]=Some(b); }
                }
                marks.push(row);
                funding.push(sf.query_map(params![s, first, last_ts + BAR_MS], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<(i64, f64)>>>()?);
            }
            let mut sr = c.prepare("SELECT json FROM rules WHERE symbol=?1")?;
            let rules = symbols
                .iter()
                .map(|s| sr.query_row([s], |r| r.get::<_, String>(0)).ok().and_then(|j| serde_json::from_str(&j).ok()))
                .collect();
            let listing_times = symbols.iter().map(|s| Self::listing_start(c, s)).collect::<Result<Vec<Option<i64>>>>()?;
            let active:Vec<String> = c.prepare("SELECT symbol FROM universe")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
            let entry_eligible = symbols.iter().map(|s|active.is_empty() || active.contains(s)).collect();
            Ok(Market { ts, symbols: symbols.to_vec(), bars, marks, funding, rules, listing_times, entry_eligible })
        })
    }
}

/// Bring the cache up to date for all symbols (incremental, up to 6 requests in
/// flight, globally paced), returning the last fully closed bar timestamp. A
/// symbol that fails is skipped for this bar; more than 20% failing is an error.
pub async fn sync(
    client: &Client,
    cache: &Cache,
    symbols: &[String],
    mut progress: impl FnMut(usize, usize),
) -> Result<i64> {
    use futures_util::{stream, StreamExt};
    let now = chrono::Utc::now().timestamp_millis();
    let last_closed = (now - 3_000) / BAR_MS * BAR_MS - BAR_MS;
    let earliest = last_closed - (BARS as i64 - 1) * BAR_MS;
    let one = |s: String| async move {
        let since = cache.bars_since(&s, earliest, last_closed)?;
        if since <= last_closed {
            let bars = client
                .klines_since(&s, since)
                .await
                .with_context(|| format!("klines {s}"))?;
            cache.put_bars(&s, &bars)?;
            cache.note_first_trade(&s, since, earliest, &bars)?;
            let remaining = cache.bars_since(&s, earliest, last_closed)?;
            if remaining <= last_closed {
                bail!("klines {s}: missing closed candle at {remaining} after backfill");
            }
        }
        let msince = cache.marks_since(&s, earliest, last_closed)?;
        if msince <= last_closed {
            cache.put_marks(&s, &client.mark_range(&s, msince, last_closed).await?)?;
        }
        let last_f = cache.last_funding_ts(&s)?;
        {
            let fsince = last_f.map_or(earliest, |t| earliest.max(t - 8 * 3_600_000));
            let f = client
                .funding_since(&s, fsince)
                .await
                .with_context(|| format!("funding {s}"))?;
            cache.put_funding(&s, &f)?;
        }
        anyhow::Ok(())
    };
    let mut results = stream::iter(symbols.iter().cloned())
        .map(one)
        .buffer_unordered(6);
    let (mut done, mut failed) = (0usize, Vec::new());
    while let Some(r) = results.next().await {
        done += 1;
        if let Err(e) = r {
            failed.push(format!("{e:#}"));
        }
        progress(done, symbols.len());
    }
    if failed.len() * 5 > symbols.len() {
        bail!(
            "sync: {} of {} symbols failed (first: {})",
            failed.len(),
            symbols.len(),
            failed[0]
        );
    }
    if !failed.is_empty() {
        log::warn!(
            "sync: {} symbol(s) skipped this bar (first: {})",
            failed.len(),
            failed[0]
        );
    }
    cache.prune(earliest - 2 * 86_400_000)?;
    Ok(last_closed)
}

#[cfg(test)]
mod gap_tests {
    use super::*;

    #[test]
    fn deductions_follow_bybit_tiers() {
        // First three BTCUSDT tiers from /v5/market/risk-limit on 2026-10-01.
        let tier = |limit: &str, mm: &str, lowest: i64, d: &str| {
            serde_json::json!({"symbol":"BTCUSDT","riskLimitValue":limit,"maintenanceMargin":mm,
                "isLowestRisk":lowest,"maxLeverage":"100.00","mmDeduction":d})
        };
        let rows = [
            tier("300000", "0.0033", 1, ""),
            tier("2000000", "0.005", 0, "510"),
            tier("2600000", "0.0056", 0, "1710"),
        ];
        let parsed: Vec<_> = rows.iter().map(|t| parse_tier(t).unwrap()).collect();
        let tiers = with_deductions(parsed.clone()).unwrap();
        let d: Vec<f64> = tiers.iter().map(|t| t.deduction).collect();
        assert!(
            (d[0] - 0.0).abs() < 1e-9
                && (d[1] - 510.0).abs() < 1e-6
                && (d[2] - 1710.0).abs() < 1e-6,
            "{d:?}"
        );
        // Symbols that publish no deductions at all get the same values.
        let blank: Vec<_> = parsed.iter().map(|(t, _)| (t.clone(), None)).collect();
        assert_eq!(with_deductions(blank).unwrap(), tiers);
        // A published value inconsistent with the tiers rejects the symbol.
        let mut wrong = parsed.clone();
        wrong[2].1 = Some(9999.0);
        assert!(with_deductions(wrong).is_none());
        assert!(parse_tier(&tier("300000", "abc", 1, "")).is_none());
    }

    /// CTUSDT on 2026-10-01: launchTime 1790844328000, but Bybit's first 15m
    /// candle opens at 1790846100000, one bar after the launch boundary.
    #[test]
    fn new_listing_starts_at_bybits_first_candle() {
        let cache = Cache::open(":memory:").unwrap();
        let (launch, boundary, first) = (1_790_844_328_000, 1_790_845_200_000, 1_790_846_100_000);
        let earliest = boundary - 10 * BAR_MS;
        cache
            .put_instruments(&[("CTUSDT".into(), None, launch)])
            .unwrap();
        let b = Bar {
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
            turnover: 1.0,
        };
        let fetched = [(first, b), (first + BAR_MS, b)];
        cache.put_bars("CTUSDT", &fetched).unwrap();
        let last = first + BAR_MS;
        assert_eq!(
            cache.bars_since("CTUSDT", earliest, last).unwrap(),
            boundary
        );
        // A later start inside the fetch is not a listing fact.
        cache
            .note_first_trade("CTUSDT", boundary + BAR_MS, earliest, &fetched)
            .unwrap();
        assert_eq!(
            cache.bars_since("CTUSDT", earliest, last).unwrap(),
            boundary
        );
        cache
            .note_first_trade("CTUSDT", boundary, earliest, &fetched)
            .unwrap();
        assert_eq!(
            cache.bars_since("CTUSDT", earliest, last).unwrap(),
            last + BAR_MS
        );
        let m = cache.market(&["CTUSDT".into()], last).unwrap();
        assert_eq!(m.listing_times, vec![Some(first)]);
        // A symbol listed before the window keeps a leading gap as a real gap.
        cache
            .put_instruments(&[("OLD".into(), None, earliest - 100 * BAR_MS)])
            .unwrap();
        cache.put_bars("OLD", &[(earliest + BAR_MS, b)]).unwrap();
        cache
            .note_first_trade("OLD", earliest, earliest, &[(earliest + BAR_MS, b)])
            .unwrap();
        assert_eq!(cache.bars_since("OLD", earliest, last).unwrap(), earliest);
    }

    #[test]
    fn universe_skips_coins_without_rules() {
        let cache = Cache::open(":memory:").unwrap();
        let r = crate::engine::rules::Rules::test_liquid();
        cache
            .put_rules(&[("BTCUSDT".into(), r.clone()), ("SOLUSDT".into(), r)])
            .unwrap();
        let ranked: Vec<(String, Option<LotFilter>, i64)> = ["BTCUSDT", "VVVUSDT", "SOLUSDT"]
            .iter()
            .map(|s| (s.to_string(), None, 1))
            .collect();
        let picked: Vec<String> = universe(&ranked, &cache.rules_symbols().unwrap(), 2)
            .into_iter()
            .map(|x| x.0)
            .collect();
        assert_eq!(picked, ["BTCUSDT", "SOLUSDT"]);
    }

    #[test]
    fn account_balance_reserves_committed_margin() {
        let a = parse_account(&serde_json::json!({"coin":"USDT","walletBalance":"100",
            "totalPositionIM":"30.5","totalOrderIM":"2","locked":"0.5"}))
        .unwrap();
        assert_eq!(a.wallet, 100.0);
        assert!((a.reserved() - 33.0).abs() < 1e-12);
        // Portfolio margin reports "" for the IM fields: nothing is assumed free.
        let pm = parse_account(&serde_json::json!({"coin":"USDT","walletBalance":"100",
            "totalPositionIM":"","totalOrderIM":"","locked":"0"}))
        .unwrap();
        assert_eq!(pm.reserved(), 100.0);
        assert!(parse_account(&serde_json::json!({"coin":"USDT"})).is_err());
    }
    #[test]
    fn rules_refresh_removes_omitted_symbols() {
        let cache = Cache::open(":memory:").unwrap();
        let r = crate::engine::rules::Rules::test_liquid();
        cache.put_rules(&[("A".into(), r.clone())]).unwrap();
        cache.put_rules(&[("B".into(), r.clone())]).unwrap();
        let m = cache.market(&["A".into(), "B".into()], BAR_MS).unwrap();
        assert!(m.rules[0].is_none());
        assert_eq!(m.rules[1], Some(r));
        cache.put_rules(&[]).unwrap();
        assert!(cache.market(&["B".into()], BAR_MS).unwrap().rules[0].is_none());
    }

    #[test]
    fn incremental_sync_retries_internal_gaps_and_missing_tail() {
        let cache = Cache::open(":memory:").unwrap();
        let b = Bar {
            open: 100.0,
            high: 100.0,
            low: 100.0,
            close: 100.0,
            volume: 1.0,
            turnover: 1.0,
        };
        cache
            .put_bars("A", &[(BAR_MS, b), (3 * BAR_MS, b)])
            .unwrap();
        assert_eq!(
            cache.bars_since("A", 0, 4 * BAR_MS).unwrap(),
            0,
            "unknown listing must retry leading gap"
        );
        cache
            .put_instruments(&[("A".into(), None, BAR_MS)])
            .unwrap();
        assert_eq!(cache.bars_since("A", 0, 4 * BAR_MS).unwrap(), 2 * BAR_MS);
        cache.put_bars("A", &[(2 * BAR_MS, b)]).unwrap();
        assert_eq!(cache.bars_since("A", 0, 4 * BAR_MS).unwrap(), 4 * BAR_MS);
        assert_eq!(cache.bars_since("NEW", 0, 4 * BAR_MS).unwrap(), 0);
        cache
            .put_instruments(&[("NEW".into(), None, 2 * BAR_MS + 1)])
            .unwrap();
        assert_eq!(cache.bars_since("NEW", 0, 4 * BAR_MS).unwrap(), 3 * BAR_MS);
        let m = cache.market(&["NEW".into()], 4 * BAR_MS).unwrap();
        assert_eq!(
            m.listing_times,
            vec![Some(3 * BAR_MS)],
            "launch rounded up to its first bar"
        );
    }
}
