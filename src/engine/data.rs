//! Bybit REST data (REST polling only, no WebSockets): public market data plus
//! one read-only signed call for the account's own fee rates:
//! USDT linear perpetual instruments, CLOSED 15m klines and settled funding,
//! cached in SQLite and assembled into a `Market` on a common timeline.

use super::keychain::Credentials;
use super::rules::Rules;
use super::{Bar, Market, BARS, BAR_MS, INTERVAL};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection};
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

fn num(v: &Value) -> f64 {
    match v {
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
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

    /// Trading USDT linear perpetuals, excluding any Bybit has scheduled for
    /// delisting (non-zero deliveryTime): those are never analysed or traded.
    pub async fn usdt_perpetuals(&self) -> Result<Vec<String>> {
        Ok(self
            .usdt_perpetual_lots()
            .await?
            .into_iter()
            .map(|(s, _)| s)
            .collect())
    }

    /// Same symbols as `usdt_perpetuals`, with each one's order rules
    /// (lotSizeFilter); None when Bybit's filter is missing or unparsable.
    pub async fn usdt_perpetual_lots(&self) -> Result<Vec<(String, Option<LotFilter>)>> {
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
                    && num(&i["deliveryTime"]) == 0.0
                {
                    let l = &i["lotSizeFilter"];
                    let lot = (|| {
                        Some(LotFilter {
                            qty_step: strict(&l["qtyStep"])?,
                            min_qty: strict(&l["minOrderQty"])?,
                            min_notional: strict(&l["minNotionalValue"])?,
                        })
                    })();
                    out.push((i["symbol"].as_str().unwrap_or_default().to_string(), lot));
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

    /// Maintenance-margin tiers per symbol: (position value limit, rate), ascending.
    pub async fn risk_limits(&self) -> Result<std::collections::HashMap<String, Vec<(f64, f64)>>> {
        let mut out: std::collections::HashMap<String, Vec<(f64, f64)>> = Default::default();
        let mut cursor = String::new();
        loop {
            let mut q = "category=linear".to_string();
            if !cursor.is_empty() {
                q.push_str(&format!("&cursor={cursor}"));
            }
            let r = self.get("/v5/market/risk-limit", &q).await?;
            for t in r["list"].as_array().cloned().unwrap_or_default() {
                if let (Some(s), Some(limit), Some(mmr)) = (
                    t["symbol"].as_str(),
                    strict(&t["riskLimitValue"]),
                    strict(&t["maintenanceMargin"]),
                ) {
                    out.entry(s.to_string()).or_default().push((limit, mmr));
                }
            }
            cursor = r["nextPageCursor"].as_str().unwrap_or_default().to_string();
            if cursor.is_empty() {
                break;
            }
        }
        for tiers in out.values_mut() {
            tiers.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        Ok(out)
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
        Ok((side("b"), side("a"), num(&r["ts"]) as i64))
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

    /// The real account's USDT wallet balance (Unified account; read-only).
    pub async fn usdt_wallet_balance(&self, creds: &Credentials) -> Result<f64> {
        let r = self
            .signed_get(
                creds,
                "/v5/account/wallet-balance",
                "accountType=UNIFIED&coin=USDT",
            )
            .await?;
        r["list"][0]["coin"]
            .as_array()
            .and_then(|c| c.iter().find(|x| x["coin"] == "USDT"))
            .and_then(|x| strict(&x["walletBalance"]))
            .ok_or_else(|| anyhow!("wallet-balance: no USDT walletBalance in the response"))
    }

    /// Signed GET (Bybit v5: HMAC-SHA256 of timestamp + key + recv_window + query).
    async fn signed_get(&self, creds: &Credentials, path: &str, query: &str) -> Result<Value> {
        use hmac::{Hmac, Mac};
        let ts = chrono::Utc::now().timestamp_millis().to_string();
        let recv = "5000";
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(creds.api_secret.as_bytes())?;
        mac.update(format!("{ts}{}{recv}{query}", creds.api_key).as_bytes());
        let sign = hex::encode(mac.finalize().into_bytes());
        let resp = self
            .http
            .get(format!("{BASE}{path}?{query}"))
            .header("X-BAPI-API-KEY", &creds.api_key)
            .header("X-BAPI-TIMESTAMP", &ts)
            .header("X-BAPI-RECV-WINDOW", recv)
            .header("X-BAPI-SIGN", sign)
            .send()
            .await?;
        let v: Value = serde_json::from_str(&resp.text().await?)?;
        if v["retCode"].as_i64() != Some(0) {
            bail!(
                "{path}: Bybit retCode {}: {}",
                v["retCode"],
                v["retMsg"].as_str().unwrap_or("?")
            );
        }
        Ok(v["result"].clone())
    }

    /// Full Bybit rules for every symbol that has all of them: order rules, the
    /// account's taker fee, margin tiers and a freshly measured order book.
    /// Symbols missing any piece are left out (never traded).
    pub async fn fetch_rules(
        &self,
        creds: &Credentials,
        lots: &[(String, Option<LotFilter>)],
    ) -> Result<Vec<(String, Rules)>> {
        let fees = self.taker_fees(creds).await?;
        let tiers = self.risk_limits().await?;
        let mut out = Vec::new();
        for (s, lot) in lots {
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
                    taker_fee: fee,
                    mm_tiers: t.clone(),
                    book,
                    book_ts: ts,
                },
            ));
        }
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
            let r = self.get("/v5/market/kline", &q).await?;
            let rows = r["list"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                break;
            }
            let mut oldest = i64::MAX;
            for k in &rows {
                let ts = num(&k[0]) as i64;
                oldest = oldest.min(ts);
                if ts + BAR_MS > now - 3_000 || ts < start || ts > end {
                    continue; // still forming / outside range
                }
                out.push((
                    ts,
                    Bar {
                        open: num(&k[1]),
                        high: num(&k[2]),
                        low: num(&k[3]),
                        close: num(&k[4]),
                        volume: num(&k[5]),
                        turnover: num(&k[6]),
                    },
                ));
            }
            if rows.len() < 1000 || oldest <= start {
                break;
            }
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
            let rows: Vec<(i64, f64)> = r["list"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|f| {
                    (
                        num(&f["fundingRateTimestamp"]) as i64,
                        num(&f["fundingRate"]),
                    )
                })
                .collect();
            let oldest = rows.iter().map(|x| x.0).min();
            let full = rows.len() >= 200;
            v.extend(rows);
            match oldest {
                Some(o) if full && o > start => page_end = o - 1,
                _ => break,
            }
        }
        v.sort_by_key(|x| x.0);
        v.dedup_by_key(|x| x.0);
        Ok(v)
    }
}

/// SQLite cache of bars and funding.
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
             DROP TABLE IF EXISTS lots;
             CREATE TABLE IF NOT EXISTS rules (symbol TEXT PRIMARY KEY, json TEXT NOT NULL);",
        )?;
        Ok(Cache(Arc::new(Mutex::new(c))))
    }

    fn with<R>(&self, f: impl FnOnce(&mut Connection) -> Result<R>) -> Result<R> {
        f(&mut self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }

    pub fn last_bar_ts(&self, symbol: &str) -> Result<Option<i64>> {
        self.with(|c| {
            Ok(
                c.query_row("SELECT MAX(ts) FROM bars WHERE symbol=?1", [symbol], |r| {
                    r.get(0)
                })?,
            )
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

    /// Store the current order rules (refreshed every bar by the service).
    pub fn put_rules(&self, rules: &[(String, Rules)]) -> Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
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

    /// (symbols, last bar ts) present in the cache.
    pub fn contents(&self) -> Result<(Vec<String>, i64)> {
        self.with(|c| {
            let mut st = c.prepare("SELECT DISTINCT symbol FROM bars ORDER BY symbol")?;
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
            Ok(())
        })
    }

    /// Market of exactly BARS bars ending at `last_ts` for `symbols`.
    pub fn market(&self, symbols: &[String], last_ts: i64) -> Result<Market> {
        let first = last_ts - (BARS as i64 - 1) * BAR_MS;
        let ts: Vec<i64> = (0..BARS as i64).map(|i| first + i * BAR_MS).collect();
        self.with(|c| {
            let mut bars = Vec::with_capacity(symbols.len());
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
                funding.push(sf.query_map(params![s, first, last_ts + BAR_MS], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<(i64, f64)>>>()?);
            }
            let mut sr = c.prepare("SELECT json FROM rules WHERE symbol=?1")?;
            let rules = symbols
                .iter()
                .map(|s| sr.query_row([s], |r| r.get::<_, String>(0)).ok().and_then(|j| serde_json::from_str(&j).ok()))
                .collect();
            Ok(Market { ts, symbols: symbols.to_vec(), bars, funding, rules })
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
        let since = cache
            .last_bar_ts(&s)?
            .map_or(earliest, |t| (t + BAR_MS).max(earliest));
        if since <= last_closed {
            let bars = client
                .klines_since(&s, since)
                .await
                .with_context(|| format!("klines {s}"))?;
            cache.put_bars(&s, &bars)?;
        }
        let last_f = cache.last_funding_ts(&s)?;
        if last_f.is_none_or(|t| now - t > 3_600_000) {
            let fsince = last_f.map_or(earliest, |t| t + 1);
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
