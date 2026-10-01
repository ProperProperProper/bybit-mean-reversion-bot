//! Cross-sectional, market-neutral portfolio over the top-100 USDT perpetuals.
//! Every `hold` bars (aligned to timestamps, so backtest and live rebalance at
//! the same closes) rank the universe by a causal `Signal` (screener scores,
//! return or settled funding), then hold LONG the `top` lowest and SHORT the
//! `top` highest, equal notional each. Orders execute at the next open; only
//! positions that change are traded. Isolated margin per position, real
//! funding, optional protective stop, pessimistic intrabar order.

use super::metrics::{Metrics, Trade};
use super::scores::Score;
use super::{Market, Side, BAR_MS};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What the universe is ranked by at each rebalance close. All are causal:
/// screener scores from closed bars up to t, funding already settled by bar t.
/// LONG the `top` lowest values, SHORT the `top` highest (`flip` reverses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Signal {
    /// Return over `lookback` bars (unflipped = reversal, flipped = momentum).
    #[default]
    Return,
    /// Screener trend score 1-100, bearish to bullish.
    TrendScore,
    /// Screener signed moving-average price-action strength.
    PriceAction,
    /// Screener multi-interval volatility score (unflipped = low-vol long).
    Volatility,
    /// RSI14 (unflipped = oversold long, overbought short).
    Rsi,
    /// Screener Pulse: pulse_long - pulse_short.
    Pulse,
    /// Average hourly funding rate of the last 3 settlements (unflipped = carry:
    /// long the most negative, short the most positive, paid on both legs).
    Funding,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XsParams {
    #[serde(default)]
    pub signal: Signal,
    #[serde(default)]
    pub flip: bool,
    pub lookback: usize,
    pub hold: usize,
    pub top: usize,
    /// Notional per position = equity * gross_leverage / (2 * top).
    pub gross_leverage: f64,
    /// Intrabar stop: close the moment price touches this % against the entry.
    pub stop_pct: Option<f64>,
    /// Drawdown handling (all off by default; see `Risk`).
    #[serde(default)]
    pub risk: Risk,
}

/// Drawdown handling, each rule decided on a 15m CLOSE and executed at the
/// next open, as a live bot would (no intrabar hindsight).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Risk {
    /// Exit a position whose close is this % against its average entry.
    #[serde(default)]
    pub close_stop_pct: Option<f64>,
    /// Same, for shorts only (short squeezes are the unbounded risk).
    #[serde(default)]
    pub short_stop_pct: Option<f64>,
    /// Exit a position whose close is this % in its favour (lock the snap-back).
    #[serde(default)]
    pub take_profit_pct: Option<f64>,
    /// Add once, the same size again, when a close is this % against the entry.
    #[serde(default)]
    pub add_pct: Option<f64>,
    /// Portfolio breaker: when marked equity closes this % below the equity at
    /// the last rebalance, close everything; no new positions until the next one.
    #[serde(default)]
    pub breaker_pct: Option<f64>,
    /// Size positions inversely to their recent volatility (same total size).
    #[serde(default)]
    pub vol_scaled: bool,
    /// Trade at half size while equity is this % or more below its peak.
    #[serde(default)]
    pub derisk_pct: Option<f64>,
}

/// Action decided at a close, executed at the next open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NextAction {
    Stop,
    TakeProfit,
    Add,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XsPosition {
    pub sym: usize,
    pub symbol: String,
    pub side: Side,
    pub entry_ts: i64,
    /// Average entry price (changes if `Risk::add_pct` adds to the position).
    pub entry: f64,
    pub qty: f64,
    pub margin: f64,
    pub leverage: f64,
    pub liquidation: f64,
    pub stop: Option<f64>,
    pub fees: f64,
    pub funding: f64,
    pub mark: f64,
    #[serde(default)]
    pub next_action: Option<NextAction>,
    /// Already added to once.
    #[serde(default)]
    pub added: bool,
    /// The account's Bybit taker fee for this symbol at entry.
    pub fee_rate: f64,
    /// Measured order-book cost of closing this size at entry (used only if
    /// the symbol's rules disappear, e.g. delisting).
    pub exit_cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XsPortfolio {
    pub equity: f64,
    start_equity: f64,
    pub positions: Vec<XsPosition>,
    /// Target set decided at the last rebalance close, executed at the next open.
    /// Carries symbol names so it survives the live symbol list changing.
    #[serde(default)]
    pending_targets: Option<Vec<(usize, String, Side)>>,
    pub trades: Vec<Trade>,
    peak: f64,
    max_dd: f64,
    liquidations: usize,
    /// Cost multiplier on the measured fee and order-book cost (1 = as measured);
    /// >1 only for cost stress tests.
    #[serde(default = "one")]
    pub cost_mult: f64,
    /// False while the walk-forward is not passing: no new positions, and the
    /// next rebalance closes everything (go flat rather than trade unvalidated).
    #[serde(default = "yes")]
    pub entries_allowed: bool,
    /// Marked equity right after the last rebalance (breaker reference).
    #[serde(default)]
    rebalance_equity: f64,
    /// Breaker tripped at a close: close everything at the next open.
    #[serde(default)]
    flatten_next: bool,
}

fn one() -> f64 {
    1.0
}

fn yes() -> bool {
    true
}

/// Standard deviation of 15m log returns over the 96 bars ending at bar `t`
/// (None with fewer than 48 returns).
pub fn realized_vol(m: &Market, sym: usize, t: usize) -> Option<f64> {
    let from = t.saturating_sub(96);
    let r: Vec<f64> = (from + 1..=t)
        .filter_map(|i| Some((m.bars[sym][i]?.close / m.bars[sym][i - 1]?.close).ln()))
        .filter(|x| x.is_finite())
        .collect();
    if r.len() < 48 {
        return None;
    }
    let mean = r.iter().sum::<f64>() / r.len() as f64;
    let v = (r.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / r.len() as f64).sqrt();
    (v > 0.0).then_some(v)
}

/// True when bar t's close is a rebalance point (timestamp-aligned).
pub fn is_rebalance(ts: i64, hold: usize) -> bool {
    (ts + BAR_MS) % (hold as i64 * BAR_MS) == 0
}

/// Average hourly rate of the last 3 funding settlements at or before `ts`
/// (intervals differ per symbol: 1h, 4h or 8h, so normalise per hour).
fn funding_hourly(f: &[(i64, f64)], ts: i64) -> Option<f64> {
    let n = f.partition_point(|x| x.0 <= ts);
    if n < 2 {
        return None;
    }
    let from = n.saturating_sub(3).max(1);
    let rates: Vec<f64> = (from..n)
        .filter_map(|i| {
            let hours = (f[i].0 - f[i - 1].0) as f64 / 3_600_000.0;
            (hours > 0.0).then(|| f[i].1 / hours)
        })
        .collect();
    (!rates.is_empty()).then(|| rates.iter().sum::<f64>() / rates.len() as f64)
}

/// Ranking value of symbol s at bar t (None = not rankable now).
pub fn signal_value(
    m: &Market,
    scores: &[Vec<Option<Score>>],
    s: usize,
    t: usize,
    p: &XsParams,
) -> Option<f64> {
    let sc = scores[s][t]?;
    let v = match p.signal {
        Signal::Return => {
            let (now, then) = (
                m.bars[s][t]?.close,
                m.bars[s][t.checked_sub(p.lookback)?]?.close,
            );
            if then <= 0.0 {
                return None;
            }
            now / then - 1.0
        }
        Signal::TrendScore => sc.trend_score,
        Signal::PriceAction => sc.pa_strength,
        Signal::Volatility => sc.volatility_score,
        Signal::Rsi => sc.rsi14,
        Signal::Pulse => sc.pulse_long? - sc.pulse_short?,
        Signal::Funding => funding_hourly(&m.funding[s], m.ts[t])?,
    };
    v.is_finite().then_some(if p.flip { -v } else { v })
}

/// Target sides at bar t: LONG the `top` lowest signal values, SHORT the `top` highest.
pub fn targets(
    m: &Market,
    scores: &[Vec<Option<Score>>],
    t: usize,
    p: &XsParams,
) -> BTreeMap<usize, Side> {
    let mut out = BTreeMap::new();
    let mut ranked: Vec<(usize, f64)> = (0..m.symbols.len())
        .filter_map(|s| Some((s, signal_value(m, scores, s, t, p)?)))
        .collect();
    if ranked.len() < 2 * p.top {
        return out;
    }
    ranked.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    for &(s, _) in &ranked[..p.top] {
        out.insert(s, Side::Long);
    }
    for &(s, _) in &ranked[ranked.len() - p.top..] {
        out.insert(s, Side::Short);
    }
    out
}

impl XsPortfolio {
    pub fn new(equity: f64) -> Self {
        XsPortfolio {
            equity,
            start_equity: equity,
            positions: vec![],
            pending_targets: None,
            trades: vec![],
            peak: equity,
            max_dd: 0.0,
            liquidations: 0,
            cost_mult: 1.0,
            entries_allowed: true,
            rebalance_equity: equity,
            flatten_next: false,
        }
    }

    pub fn with_cost_mult(mut self, k: f64) -> Self {
        self.cost_mult = k;
        self
    }

    /// Mirror a deposit (+) or withdrawal (-) on the real account: equity changes
    /// and so does the P&L baseline, so cash moved is never counted as profit or
    /// loss. Later positions are sized from the new equity.
    pub fn cash_flow(&mut self, delta: f64) {
        self.equity += delta;
        self.start_equity += delta;
        self.peak += delta;
        self.rebalance_equity += delta;
    }

    /// Point positions and pending targets at `symbols` by NAME. The live symbol
    /// list is re-fetched every bar, so indices shift when Bybit lists a coin;
    /// a held position whose symbol is gone (delisted or delisting scheduled)
    /// is closed at its last mark and never traded again.
    pub fn reindex(&mut self, symbols: &[String], ts: i64) {
        let idx: std::collections::HashMap<&str, usize> = symbols
            .iter()
            .enumerate()
            .map(|(i, s)| (s.as_str(), i))
            .collect();
        let mut i = 0;
        while i < self.positions.len() {
            match idx.get(self.positions[i].symbol.as_str()) {
                Some(&s) => {
                    self.positions[i].sym = s;
                    i += 1;
                }
                None => {
                    let mark = self.positions[i].mark;
                    self.close(i, ts, mark, "Delisted");
                }
            }
        }
        if let Some(pending) = self.pending_targets.as_mut() {
            pending.retain_mut(|(s, name, _)| idx.get(name.as_str()).map(|&n| *s = n).is_some());
        }
    }

    /// Close position i at `price` (already including any market-order cost),
    /// paying the position's own Bybit taker fee.
    fn close(&mut self, i: usize, ts: i64, price: f64, reason: &str) {
        let p = self.positions.remove(i);
        let gross = p.side.sign() * (price - p.entry) * p.qty;
        let fee = price * p.qty * p.fee_rate * self.cost_mult;
        self.equity += gross - fee;
        let fees = p.fees + fee;
        let pnl = gross - fees - p.funding;
        let risk = p.margin.max(1e-9);
        self.trades.push(Trade {
            symbol: p.symbol,
            side: p.side,
            entry_ts: p.entry_ts,
            exit_ts: ts,
            entry: p.entry,
            exit: price,
            qty: p.qty,
            leverage: p.leverage,
            fees,
            funding: p.funding,
            pnl,
            r: pnl / risk,
            reason: reason.into(),
        });
    }

    /// Fill price of a market order closing position i when the price is
    /// `reference`: the measured order-book cost for that size (a long sells
    /// into the bids, a short buys from the asks). Beyond the measured depth the
    /// cost of the deepest measured size is scaled up linearly (worse, never better).
    fn exit_price(&self, m: &Market, i: usize, reference: f64) -> f64 {
        let pos = &self.positions[i];
        let notional = pos.qty * reference;
        let buy = pos.side == Side::Short;
        let cost = m.rules(pos.sym).map_or(pos.exit_cost, |r| {
            r.book_cost(notional, buy).unwrap_or_else(|| {
                r.book.last().map_or(pos.exit_cost, |&(n, b, s)| {
                    (if buy { b } else { s }) * notional / n
                })
            })
        });
        reference * (1.0 - pos.side.sign() * cost * self.cost_mult)
    }

    /// Margin not yet posted (isolated margin: balance minus position margins).
    fn available(&self) -> f64 {
        self.equity - self.positions.iter().map(|x| x.margin).sum::<f64>()
    }

    /// Market order at bar t's open spending `budget` USDT on margin + fee: Bybit lot rules, the measured order-book cost for this size, the
    /// account's taker fee, the real margin tier. Not placed when the symbol has no
    /// rules, the book is too thin, or the account lacks the margin (as Bybit).
    fn open(&mut self, m: &Market, t: usize, sym: usize, side: Side, budget: f64, p: &XsParams) {
        let (Some(b), Some(r)) = (m.bars[sym][t], m.rules(sym)) else {
            return;
        };
        if budget <= 0.0 {
            return;
        }
        let (buy, leverage, cm) = (side == Side::Long, p.gross_leverage, self.cost_mult);
        // Size so that margin + entry fee == budget (the book cost is not a separate
        // payment: it is already in the worse fill price).
        let notional = budget / (1.0 / leverage + r.taker_fee * cm);
        let (Some(cost), Some(exit_cost)) =
            (r.book_cost(notional, buy), r.book_cost(notional, !buy))
        else {
            return;
        };
        let entry = b.open * (1.0 + side.sign() * cost * cm);
        let Some(qty) = r.order_qty(notional / entry, entry) else {
            return;
        };
        let Some(mmr) = r.mmr(qty * entry) else {
            return;
        };
        let fee = qty * entry * r.taker_fee * cm;
        if qty * entry / leverage + fee > self.available() + 1e-9 {
            return; // Bybit would reject: not enough available balance
        }
        self.equity -= fee;
        self.positions.push(XsPosition {
            sym,
            symbol: m.symbols[sym].clone(),
            side,
            entry_ts: m.ts[t],
            entry,
            qty,
            margin: qty * entry / leverage,
            leverage,
            liquidation: entry * (1.0 - side.sign() * (1.0 / leverage - mmr)),
            stop: p.stop_pct.map(|s| entry * (1.0 - side.sign() * s / 100.0)),
            fees: fee,
            funding: 0.0,
            mark: entry,
            next_action: None,
            added: false,
            fee_rate: r.taker_fee,
            exit_cost,
        });
    }

    /// Add the position's current size again at bar t's open (same rules as
    /// `open`); entry, margin, liquidation and stop move to the new average.
    fn add(&mut self, m: &Market, t: usize, i: usize, p: &XsParams) {
        let (sym, side, qty0) = (
            self.positions[i].sym,
            self.positions[i].side,
            self.positions[i].qty,
        );
        let (Some(b), Some(r)) = (m.bars[sym][t], m.rules(sym)) else {
            return;
        };
        let s = side.sign();
        let Some(cost) = r.book_cost(qty0 * b.open, side == Side::Long) else {
            return;
        };
        let px = b.open * (1.0 + s * cost * self.cost_mult);
        let Some(q) = r.order_qty(qty0, px) else {
            return;
        };
        let Some(mmr) = r.mmr((qty0 + q) * px) else {
            return;
        };
        let fee = q * px * r.taker_fee * self.cost_mult;
        if q * px / self.positions[i].leverage + fee > self.available() + 1e-9 {
            return; // Bybit would reject: not enough available balance
        }
        self.equity -= fee;
        let pos = &mut self.positions[i];
        let qty = pos.qty + q;
        pos.entry = (pos.entry * pos.qty + px * q) / qty;
        pos.qty = qty;
        pos.margin += q * px / pos.leverage;
        pos.fees += fee;
        pos.added = true;
        pos.liquidation = pos.entry * (1.0 - s * (1.0 / pos.leverage - mmr));
        pos.stop = p.stop_pct.map(|st| pos.entry * (1.0 - s * st / 100.0));
    }

    pub fn step(&mut self, m: &Market, scores: &[Vec<Option<Score>>], t: usize, p: &XsParams) {
        let ts = m.ts[t];
        let rebalancing = self.pending_targets.is_some();
        // 0. Actions decided at the previous close execute at this open. An add
        //    that coincides with a rebalance is skipped: the rebalance decides.
        if std::mem::take(&mut self.flatten_next) {
            let mut i = 0;
            while i < self.positions.len() {
                let sym = self.positions[i].sym;
                match m.bars[sym][t] {
                    Some(b) => {
                        let px = self.exit_price(m, i, b.open);
                        self.close(i, ts, px, "Breaker")
                    }
                    None => i += 1,
                }
            }
        }
        let mut i = 0;
        while i < self.positions.len() {
            let sym = self.positions[i].sym;
            let action = self.positions[i].next_action.take();
            match (action, m.bars[sym][t]) {
                (Some(NextAction::Stop), Some(b)) => {
                    self.close(i, ts, self.exit_price(m, i, b.open), "Stop (close)");
                    continue;
                }
                (Some(NextAction::TakeProfit), Some(b)) => {
                    self.close(i, ts, self.exit_price(m, i, b.open), "Take profit (close)");
                    continue;
                }
                (Some(NextAction::Add), Some(_)) if !rebalancing => self.add(m, t, i, p),
                _ => {}
            }
            i += 1;
        }
        // 1. Rebalance decided at the previous close, executed at this open.
        if let Some(target) = self.pending_targets.take() {
            let mut i = 0;
            while i < self.positions.len() {
                let (sym, side) = (self.positions[i].sym, self.positions[i].side);
                let keep = target.iter().any(|&(s, _, sd)| s == sym && sd == side);
                match (keep, m.bars[sym][t]) {
                    (false, Some(b)) => {
                        let px = self.exit_price(m, i, b.open);
                        self.close(i, ts, px, "Rebalance");
                    }
                    _ => i += 1,
                }
            }
            let marked = self.equity + self.unrealized();
            let derisk = p
                .risk
                .derisk_pct
                .is_some_and(|d| self.peak > 0.0 && (self.peak - marked) / self.peak * 100.0 >= d);
            // Each slot's budget (margin + entry fee) is fixed so the base
            // order AND its possible add both fit in the account: equity / (2 * top)
            // / (1 + adds).
            let adds = if p.risk.add_pct.is_some() { 1.0 } else { 0.0 };
            let base =
                self.equity / (2 * p.top) as f64 / (1.0 + adds) * if derisk { 0.5 } else { 1.0 };
            // Volatility weights from bars up to the decision close (t - 1), mean 1.
            let inv: Vec<Option<f64>> = target
                .iter()
                .map(|&(sym, _, _)| {
                    if p.risk.vol_scaled {
                        realized_vol(m, sym, t.saturating_sub(1)).map(|v| 1.0 / v)
                    } else {
                        Some(1.0)
                    }
                })
                .collect();
            let known: Vec<f64> = inv.iter().flatten().copied().collect();
            let mean = if known.is_empty() {
                1.0
            } else {
                known.iter().sum::<f64>() / known.len() as f64
            };
            for (k, &(sym, _, side)) in target.iter().enumerate() {
                if self.positions.iter().any(|x| x.sym == sym) {
                    continue;
                }
                let w = inv[k].map_or(1.0, |x| x / mean);
                self.open(m, t, sym, side, base * w, p);
            }
            self.rebalance_equity = self.equity + self.unrealized();
        }
        // 2. Funding, liquidation and the intrabar stop (worst case first), then
        //    the close-based rules, executed at the next open.
        let mut i = 0;
        while i < self.positions.len() {
            let (sym, side) = (self.positions[i].sym, self.positions[i].side);
            let Some(b) = m.bars[sym][t] else {
                i += 1;
                continue;
            };
            for &(_, rate) in m.funding[sym]
                .iter()
                .filter(|(f, _)| *f >= ts && *f < ts + BAR_MS)
            {
                let pos = &mut self.positions[i];
                let cost = side.sign() * rate * pos.qty * b.open;
                pos.funding += cost;
                self.equity -= cost;
            }
            let pos = &self.positions[i];
            let s = side.sign();
            let adverse = if s > 0.0 { b.low } else { b.high };
            let through = |level: f64, price: f64| s * (price - level) <= 0.0;
            let stop_first = pos.stop.is_some_and(|st| s * (st - pos.liquidation) > 0.0);
            if through(pos.liquidation, b.open)
                || (through(pos.liquidation, adverse) && !stop_first)
            {
                let (liq, margin) = (pos.liquidation, pos.margin);
                self.liquidations += 1;
                self.close(i, ts, liq, "Liquidated");
                if let Some(tr) = self.trades.last_mut() {
                    let floor = -margin - tr.fees - tr.funding;
                    if tr.pnl < floor {
                        self.equity += floor - tr.pnl;
                        tr.pnl = floor;
                    }
                }
                continue;
            }
            if let Some(st) = pos.stop {
                if through(st, b.open) {
                    let px = self.exit_price(m, i, b.open);
                    self.close(i, ts, px, "Stop (gap)");
                    continue;
                }
                if through(st, adverse) {
                    let px = self.exit_price(m, i, st);
                    self.close(i, ts, px, "Stop");
                    continue;
                }
            }
            let r = p.risk;
            let pos = &mut self.positions[i];
            pos.mark = b.close;
            let against = -s * (b.close - pos.entry) / pos.entry * 100.0;
            pos.next_action = if r.close_stop_pct.is_some_and(|c| against >= c)
                || (s < 0.0 && r.short_stop_pct.is_some_and(|c| against >= c))
            {
                Some(NextAction::Stop)
            } else if r.take_profit_pct.is_some_and(|tp| -against >= tp) {
                Some(NextAction::TakeProfit)
            } else if !pos.added && r.add_pct.is_some_and(|a| against >= a) {
                Some(NextAction::Add)
            } else {
                None
            };
            i += 1;
        }
        let marked = self.equity + self.unrealized();
        if let Some(bk) = p.risk.breaker_pct {
            if !self.positions.is_empty()
                && self.rebalance_equity > 0.0
                && marked <= self.rebalance_equity * (1.0 - bk / 100.0)
            {
                self.flatten_next = true;
            }
        }
        // 3. Rebalance decision at aligned closes.
        if is_rebalance(ts, p.hold) {
            let target = if self.entries_allowed {
                targets(m, scores, t, p)
            } else {
                BTreeMap::new()
            };
            self.pending_targets = Some(
                target
                    .into_iter()
                    .map(|(s, side)| (s, m.symbols[s].clone(), side))
                    .collect(),
            );
        }
        self.peak = self.peak.max(marked);
        if self.peak > 0.0 {
            self.max_dd = self.max_dd.max((self.peak - marked) / self.peak * 100.0);
        }
    }

    /// Close every position at its last mark (bar close) with its measured
    /// order-book cost and fee.
    pub fn close_all(&mut self, m: &Market, ts: i64, reason: &str) {
        while !self.positions.is_empty() {
            let i = self.positions.len() - 1;
            let px = self.exit_price(m, i, self.positions[i].mark);
            self.close(i, ts, px, reason);
        }
    }

    pub fn unrealized(&self) -> f64 {
        self.positions
            .iter()
            .map(|p| p.side.sign() * (p.mark - p.entry) * p.qty)
            .sum()
    }

    pub fn pending_targets(&self) -> Option<&[(usize, String, Side)]> {
        self.pending_targets.as_deref()
    }

    pub fn metrics(&self) -> Metrics {
        let mut m = Metrics {
            start_equity: self.start_equity,
            end_equity: self.equity,
            open_unrealized: self.unrealized(),
            trades: self.trades.len(),
            liquidations: self.liquidations,
            max_drawdown_pct: self.max_dd,
            ..Default::default()
        };
        for t in &self.trades {
            if t.pnl > 0.0 {
                m.wins += 1;
                m.gross_profit += t.pnl
            } else {
                m.gross_loss -= t.pnl
            }
        }
        if !self.trades.is_empty() {
            m.avg_r = self.trades.iter().map(|t| t.r).sum::<f64>() / self.trades.len() as f64;
        }
        m
    }
}

pub const DAY_MS: i64 = 86_400_000;

/// One UTC day of a backtest: marked equity at the day's last closed bar.
#[derive(Debug, Clone, Serialize)]
pub struct DayRow {
    /// 00:00 UTC of the day.
    pub day_ts: i64,
    pub equity: f64,
    pub pnl: f64,
    pub pnl_pct: f64,
    /// Positions closed that day and how many made money.
    pub trades: usize,
    pub wins: usize,
}

/// Step `pf` over `range` with `p`, appending one row per UTC day (equity marked
/// to the last close, fees and funding included). With `close_at_end` the
/// positions still open at the end are closed at the last close (realised);
/// without it they carry into the next call, as they would live when settings
/// change.
pub fn run_daily(
    m: &Market,
    scores: &[Vec<Option<Score>>],
    range: std::ops::Range<usize>,
    p: &XsParams,
    pf: &mut XsPortfolio,
    out: &mut Vec<DayRow>,
    close_at_end: bool,
) {
    let Some(last) = range.clone().last() else {
        return;
    };
    let mut prev = pf.equity + pf.unrealized();
    let mut seen = pf.trades.len();
    for t in range {
        pf.step(m, scores, t, p);
        let end = m.ts[t] + BAR_MS;
        if end % DAY_MS == 0 || t == last {
            if t == last && close_at_end {
                pf.close_all(m, end, "End of test");
            }
            let equity = pf.equity + pf.unrealized();
            let closed = &pf.trades[seen..];
            out.push(DayRow {
                day_ts: (end - 1) / DAY_MS * DAY_MS,
                equity,
                pnl: equity - prev,
                pnl_pct: if prev > 0.0 {
                    (equity / prev - 1.0) * 100.0
                } else {
                    0.0
                },
                trades: closed.len(),
                wins: closed.iter().filter(|x| x.pnl > 0.0).count(),
            });
            prev = equity;
            seen = pf.trades.len();
        }
    }
}

pub fn backtest(
    m: &Market,
    scores: &[Vec<Option<Score>>],
    range: std::ops::Range<usize>,
    p: &XsParams,
    equity: f64,
) -> XsPortfolio {
    backtest_costs(m, scores, range, p, equity, 1.0)
}

/// Backtest with a fee/slippage multiplier. Positions still open at the end are
/// closed at the last close (with fees), so net and profit factor always come
/// from the same closed trades.
pub fn backtest_costs(
    m: &Market,
    scores: &[Vec<Option<Score>>],
    range: std::ops::Range<usize>,
    p: &XsParams,
    equity: f64,
    cost_mult: f64,
) -> XsPortfolio {
    let mut pf = XsPortfolio::new(equity).with_cost_mult(cost_mult);
    let Some(last) = range.clone().last() else {
        return pf;
    };
    for t in range {
        pf.step(m, scores, t, p);
    }
    pf.close_all(m, m.ts[last] + BAR_MS, "End of test");
    pf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::scores::tests::market;

    fn sc(m: &Market) -> Vec<Vec<Option<Score>>> {
        crate::engine::scores::compute(m, 100)
    }

    #[test]
    fn longs_losers_and_shorts_winners() {
        let mk = |drift: f64| {
            (0..200)
                .map(|i| 100.0 * (1.0 + drift * i as f64 / 200.0))
                .collect::<Vec<f64>>()
        };
        let m = market(&[mk(-0.2), mk(-0.1), mk(0.0), mk(0.1), mk(0.2)]);
        let s = sc(&m);
        let p = XsParams {
            signal: Signal::Return,
            flip: false,
            lookback: 16,
            hold: 16,
            top: 1,
            gross_leverage: 1.0,
            stop_pct: None,
            risk: Risk::default(),
        };
        let t = targets(&m, &s, 190, &p);
        assert_eq!(t.get(&0), Some(&Side::Long));
        assert_eq!(t.get(&4), Some(&Side::Short));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn rebalance_is_timestamp_aligned_and_executes_next_open() {
        assert!(is_rebalance(15 * BAR_MS, 16));
        assert!(!is_rebalance(14 * BAR_MS, 16));
        let mk = |drift: f64| {
            (0..200)
                .map(|i| 100.0 * (1.0 + drift * i as f64 / 200.0))
                .collect::<Vec<f64>>()
        };
        let m = market(&[mk(-0.2), mk(-0.1), mk(0.0), mk(0.1), mk(0.2)]);
        let s = sc(&m);
        let p = XsParams {
            signal: Signal::Return,
            flip: false,
            lookback: 16,
            hold: 16,
            top: 1,
            gross_leverage: 1.0,
            stop_pct: None,
            risk: Risk::default(),
        };
        let mut pf = XsPortfolio::new(100.0);
        for t in 160..=175 {
            pf.step(&m, &s, t, &p);
        }
        // bar 175 is a rebalance close (ts+BAR = 176*BAR, 176 % 16 == 0): nothing held yet.
        assert!(pf.positions.is_empty());
        pf.step(&m, &s, 176, &p);
        assert_eq!(pf.positions.len(), 2);
        assert!(pf.positions.iter().all(|x| x.entry_ts == m.ts[176]));
    }

    /// Step without the end-of-test close (positions stay open).
    fn stepped(
        m: &Market,
        s: &[Vec<Option<Score>>],
        range: std::ops::Range<usize>,
        p: &XsParams,
    ) -> XsPortfolio {
        let mut pf = XsPortfolio::new(100.0);
        for t in range {
            pf.step(m, s, t, p);
        }
        pf
    }

    #[test]
    fn backtest_closes_everything_at_the_end() {
        let m = market(&wavy(200));
        let s = sc(&m);
        let p = XsParams {
            signal: Signal::Return,
            flip: false,
            lookback: 16,
            hold: 16,
            top: 2,
            gross_leverage: 1.0,
            stop_pct: None,
            risk: Risk::default(),
        };
        let pf = backtest(&m, &s, 150..199, &p, 100.0);
        assert!(pf.positions.is_empty());
        let r = pf.metrics();
        assert!(
            ((r.gross_profit - r.gross_loss) - r.net()).abs() < 1e-9,
            "net must equal closed-trade P&L"
        );
    }

    /// One symbol, flat at 100, with chosen (open, high, low, close) bars.
    fn flat_with(changes: &[(usize, (f64, f64, f64, f64))]) -> Market {
        let mut bars: Vec<Option<crate::engine::Bar>> = (0..200)
            .map(|_| {
                Some(crate::engine::Bar {
                    open: 100.0,
                    high: 100.0,
                    low: 100.0,
                    close: 100.0,
                    volume: 1.0,
                    turnover: 1.0,
                })
            })
            .collect();
        for &(t, (o, h, l, c)) in changes {
            bars[t] = Some(crate::engine::Bar {
                open: o,
                high: h,
                low: l,
                close: c,
                volume: 1.0,
                turnover: 1.0,
            });
        }
        Market {
            ts: (0..200).map(|i| i * BAR_MS).collect(),
            symbols: vec!["AUSDT".into()],
            bars: vec![bars],
            funding: vec![vec![]],
            rules: vec![Some(crate::engine::rules::Rules::test_liquid()); 3],
            ..Default::default()
        }
    }

    fn risk_params(risk: Risk) -> XsParams {
        XsParams {
            signal: Signal::Pulse,
            flip: false,
            lookback: 0,
            hold: 96,
            top: 1,
            gross_leverage: 2.0,
            stop_pct: None,
            risk,
        }
    }

    /// Close-based stop: a wick through 10% that closes back is ignored; a CLOSE
    /// 10% against exits at the NEXT open, not at the close.
    #[test]
    fn close_stop_ignores_wicks_and_exits_next_open() {
        let m = flat_with(&[
            (101, (100.0, 100.5, 85.0, 99.0)),
            (102, (99.0, 99.5, 88.0, 89.0)),
            (103, (88.0, 90.0, 87.0, 89.5)),
        ]);
        let s = vec![vec![None; 200]];
        let p = risk_params(Risk {
            close_stop_pct: Some(10.0),
            ..Default::default()
        });
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 200.0, &p);
        pf.step(&m, &s, 100, &p);
        pf.step(&m, &s, 101, &p);
        assert_eq!(pf.positions[0].next_action, None, "-15% wick but close -1%");
        pf.step(&m, &s, 102, &p);
        assert_eq!(
            pf.positions.len(),
            1,
            "decided at the close, not filled at it"
        );
        assert_eq!(pf.positions[0].next_action, Some(NextAction::Stop));
        pf.step(&m, &s, 103, &p);
        let tr = pf.trades.last().unwrap();
        assert_eq!(tr.reason, "Stop (close)");
        assert!(
            (tr.exit - 88.0 * (1.0 - crate::engine::rules::TEST_COST)).abs() < 1e-9
                && tr.exit_ts == m.ts[103]
        );
    }

    #[test]
    fn short_stop_only_stops_shorts() {
        let m = flat_with(&[
            (101, (100.0, 116.0, 99.0, 115.0)),
            (102, (115.0, 116.0, 114.0, 115.0)),
        ]);
        let s = vec![vec![None; 200]];
        let p = risk_params(Risk {
            short_stop_pct: Some(15.0),
            ..Default::default()
        });
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Short, 200.0, &p);
        pf.step(&m, &s, 100, &p);
        pf.step(&m, &s, 101, &p);
        pf.step(&m, &s, 102, &p);
        assert_eq!(pf.trades.last().unwrap().reason, "Stop (close)");
        let m2 = flat_with(&[
            (101, (100.0, 101.0, 84.0, 85.0)),
            (102, (85.0, 86.0, 84.0, 85.0)),
        ]);
        let mut pf2 = XsPortfolio::new(1000.0);
        pf2.open(&m2, 100, 0, Side::Long, 200.0, &p);
        for t in 100..=102 {
            pf2.step(&m2, &s, t, &p);
        }
        assert_eq!(pf2.positions.len(), 1, "a long 15% down is not stopped");
    }

    #[test]
    fn take_profit_on_close_exits_next_open() {
        let m = flat_with(&[
            (101, (100.0, 112.0, 99.0, 111.0)),
            (102, (110.0, 111.0, 109.0, 110.0)),
        ]);
        let s = vec![vec![None; 200]];
        let p = risk_params(Risk {
            take_profit_pct: Some(10.0),
            ..Default::default()
        });
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 200.0, &p);
        for t in 100..=102 {
            pf.step(&m, &s, t, &p);
        }
        let tr = pf.trades.last().unwrap();
        assert_eq!(tr.reason, "Take profit (close)");
        assert!(
            (tr.exit - 110.0 * (1.0 - crate::engine::rules::TEST_COST)).abs() < 1e-9,
            "fills at the next open, not the high"
        );
    }

    #[test]
    fn add_once_averages_the_entry() {
        let m = flat_with(&[
            (101, (100.0, 100.0, 89.0, 90.0)),
            (102, (90.0, 90.0, 90.0, 90.0)),
            (103, (80.0, 80.0, 79.0, 79.5)),
        ]);
        let s = vec![vec![None; 200]];
        let p = risk_params(Risk {
            add_pct: Some(10.0),
            ..Default::default()
        });
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 200.0, &p);
        for t in 100..=103 {
            pf.step(&m, &s, t, &p);
        }
        let pos = &pf.positions[0];
        assert!(pos.added);
        let first = 100.0 * (1.0 + crate::engine::rules::TEST_COST);
        let second = 90.0 * (1.0 + crate::engine::rules::TEST_COST);
        assert!(
            (pos.entry - (first + second) / 2.0).abs() < 0.01,
            "same size again at the next open (90)"
        );
        assert_eq!(pos.next_action, None, "never adds twice");
    }

    #[test]
    fn breaker_flattens_after_equity_falls_from_the_rebalance() {
        let m = flat_with(&[
            (101, (100.0, 100.0, 60.0, 60.0)),
            (102, (60.0, 60.0, 60.0, 60.0)),
        ]);
        let s = vec![vec![None; 200]];
        let p = risk_params(Risk {
            breaker_pct: Some(10.0),
            ..Default::default()
        });
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 400.0, &p);
        pf.step(&m, &s, 100, &p);
        pf.step(&m, &s, 101, &p);
        assert!(pf.flatten_next, "-40% on 400 notional = -16% equity");
        pf.step(&m, &s, 102, &p);
        assert!(pf.positions.is_empty());
        assert_eq!(pf.trades.last().unwrap().reason, "Breaker");
    }

    #[test]
    fn realized_vol_uses_only_past_bars() {
        let m = market(&wavy(300));
        let v = realized_vol(&m, 0, 200).unwrap();
        let mut later = m.clone();
        for b in later.bars[0].iter_mut().skip(201).flatten() {
            b.close *= 3.0;
        }
        assert_eq!(realized_vol(&later, 0, 200), Some(v));
    }

    /// Orders the account cannot fund are rejected, as on Bybit.
    #[test]
    fn rejects_orders_beyond_available_margin() {
        let m = flat_with(&[]);
        let p = risk_params(Risk::default());
        let mut pf = XsPortfolio::new(100.0);
        pf.open(&m, 100, 0, Side::Long, 90.0, &p);
        assert_eq!(pf.positions.len(), 1);
        let used = pf.positions[0].margin + pf.positions[0].fees;
        assert!(
            (used - 90.0).abs() < 1e-6,
            "margin + fee == budget ({used})"
        );
        pf.open(&m, 100, 0, Side::Short, 20.0, &p);
        assert_eq!(pf.positions.len(), 1, "only ~10 USDT available: rejected");
    }

    /// With adds on, each slot keeps room for its add: base + add == the slot.
    #[test]
    fn sizing_reserves_room_for_the_add() {
        let m = market(&wavy(200));
        let s = sc(&m);
        let mut p = risk_params(Risk::default());
        p.signal = Signal::Return;
        p.lookback = 16;
        p.hold = 16;
        p.top = 2;
        // Bar 159 closes a rebalance (hold 16); positions open at bar 160, before any add.
        let plain = stepped(&m, &s, 150..161, &p);
        p.risk.add_pct = Some(10.0);
        let with_add = stepped(&m, &s, 150..161, &p);
        assert!(!plain.positions.is_empty());
        let total = |pf: &XsPortfolio| pf.positions.iter().map(|x| x.margin).sum::<f64>();
        assert!((total(&with_add) * 2.0 - total(&plain)).abs() / total(&plain) < 0.01);
    }

    #[test]
    fn cash_flows_resize_but_are_not_profit() {
        let mut pf = XsPortfolio::new(100.0);
        pf.cash_flow(50.0);
        assert_eq!(pf.equity, 150.0);
        assert_eq!(pf.metrics().net(), 0.0, "a deposit is not profit");
        pf.cash_flow(-30.0);
        assert_eq!(pf.metrics().net(), 0.0, "a withdrawal is not a loss");
    }

    #[test]
    fn daily_rows_add_up_to_the_backtest() {
        let m = market(&wavy(400));
        let s = sc(&m);
        let p = XsParams {
            signal: Signal::Return,
            flip: false,
            lookback: 16,
            hold: 16,
            top: 2,
            gross_leverage: 1.0,
            stop_pct: None,
            risk: Risk::default(),
        };
        let bt = backtest(&m, &s, 120..400, &p, 100.0);
        let mut pf = XsPortfolio::new(100.0);
        let mut days = Vec::new();
        run_daily(&m, &s, 120..400, &p, &mut pf, &mut days, true);
        assert!(days.len() >= 3);
        assert!((days.last().unwrap().equity - bt.equity).abs() < 1e-9);
        assert!((days.iter().map(|d| d.pnl).sum::<f64>() - (bt.equity - 100.0)).abs() < 1e-9);
        assert_eq!(
            days.iter().map(|d| d.trades).sum::<usize>(),
            bt.trades.len()
        );
        assert!(days.windows(2).all(|w| w[1].day_ts - w[0].day_ts == DAY_MS));
    }

    /// Funding carry: long the most negative hourly funding, short the most
    /// positive, normalising 1h vs 8h intervals; only settled funding counts.
    #[test]
    fn funding_carry_ranks_hourly_rate_from_settled_funding_only() {
        let mut m = market(&wavy(200));
        let s = sc(&m);
        let h = 3_600_000;
        let t = 190;
        let now = m.ts[t];
        for f in m.funding.iter_mut() {
            *f = vec![(now - 16 * h, 0.0001), (now - 8 * h, 0.0001), (now, 0.0001)];
        }
        // Symbol 1: 1h funding at -0.0002/h -> most negative per hour.
        m.funding[1] = (0..4).map(|i| (now - (3 - i) * h, -0.0002)).collect();
        // Symbol 2: 8h funding 0.004 = 0.0005/h -> most positive.
        m.funding[2] = vec![(now - 16 * h, 0.004), (now - 8 * h, 0.004), (now, 0.004)];
        // Symbol 3: a huge rate settling AFTER bar t must be ignored.
        m.funding[3].push((now + BAR_MS, 1.0));
        let p = XsParams {
            signal: Signal::Funding,
            flip: false,
            lookback: 16,
            hold: 16,
            top: 1,
            gross_leverage: 1.0,
            stop_pct: None,
            risk: Risk::default(),
        };
        let tg = targets(&m, &s, t, &p);
        assert_eq!(tg.get(&1), Some(&Side::Long));
        assert_eq!(tg.get(&2), Some(&Side::Short));
        assert_eq!(tg.len(), 2);
    }

    fn wavy(n: usize) -> Vec<Vec<f64>> {
        (0..8)
            .map(|k| {
                (0..n)
                    .map(|i| {
                        100.0 + ((i as f64) * (0.03 + k as f64 * 0.011)).sin() * (3.0 + k as f64)
                    })
                    .collect()
            })
            .collect()
    }

    /// No repaint / lookahead: everything decided and filled through bar t is
    /// identical whether or not bars after t exist, or are wildly different.
    #[test]
    fn decisions_never_depend_on_future_bars() {
        let p = XsParams {
            signal: Signal::Return,
            flip: false,
            lookback: 16,
            hold: 16,
            top: 2,
            gross_leverage: 2.0,
            stop_pct: Some(20.0),
            risk: Risk::default(),
        };
        let full = wavy(400);
        let mut altered = full.clone();
        for s in altered.iter_mut() {
            for (i, x) in s.iter_mut().enumerate().skip(301) {
                *x *= 1.0 + (i as f64 * 0.7).sin() * 0.3;
            }
        }
        let cut: Vec<Vec<f64>> = full.iter().map(|s| s[..=300].to_vec()).collect();
        let run = |series: &[Vec<f64>]| {
            let m = market(series);
            let s = sc(&m);
            let pf = backtest(&m, &s, 120..301, &p, 100.0);
            (
                serde_json::to_string(&pf).unwrap(),
                (120..301)
                    .map(|t| targets(&m, &s, t, &p))
                    .collect::<Vec<_>>(),
            )
        };
        let (a, b, c) = (run(&full), run(&altered), run(&cut));
        assert!(
            !a.0.contains("\"trades\":[]"),
            "test needs trades to be meaningful"
        );
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    /// Walk-forward not passing: no new positions; the next rebalance goes flat.
    #[test]
    fn unvalidated_goes_flat_at_next_rebalance() {
        let m = market(&wavy(220));
        let s = sc(&m);
        let p = XsParams {
            signal: Signal::Return,
            flip: false,
            lookback: 16,
            hold: 16,
            top: 2,
            gross_leverage: 1.0,
            stop_pct: None,
            risk: Risk::default(),
        };
        let mut pf = stepped(&m, &s, 150..180, &p);
        assert!(!pf.positions.is_empty());
        pf.entries_allowed = false;
        for t in 180..219 {
            pf.step(&m, &s, t, &p);
        }
        assert!(pf.positions.is_empty());
        assert!(pf.trades.iter().all(|t| t.entry_ts < m.ts[180]));
    }

    /// Live: a new listing shifts symbol indices; positions must follow by name.
    #[test]
    fn reindex_follows_symbol_names_and_settles_delisted() {
        let m = market(&wavy(200));
        let s = sc(&m);
        let p = XsParams {
            signal: Signal::Return,
            flip: false,
            lookback: 16,
            hold: 16,
            top: 2,
            gross_leverage: 1.0,
            stop_pct: None,
            risk: Risk::default(),
        };
        let pf0 = stepped(&m, &s, 150..199, &p);
        assert_eq!(pf0.positions.len(), 4);
        let mut pf = pf0.clone();
        let mut shifted = vec!["AAANEWUSDT".to_string()];
        shifted.extend(m.symbols.iter().cloned());
        pf.reindex(&shifted, m.ts[199]);
        for x in &pf.positions {
            assert_eq!(shifted[x.sym], x.symbol);
        }
        let gone = pf0.positions[0].symbol.clone();
        let mut pf = pf0.clone();
        let fewer: Vec<String> = m.symbols.iter().filter(|x| **x != gone).cloned().collect();
        pf.reindex(&fewer, m.ts[199]);
        assert_eq!(pf.positions.len(), 3);
        assert_eq!(pf.trades.last().unwrap().reason, "Delisted");
        assert_eq!(pf.trades.last().unwrap().symbol, gone);
    }
}
