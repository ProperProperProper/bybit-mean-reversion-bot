//! Cross-sectional, market-neutral portfolio over the top-50 USDT perpetuals.
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

// NOTE(agents): User requirement: below 5 USDT free (after margin committed ANYWHERE on the real
//               account, see `reserved`) nothing is opened or added; exits always run. Checked once
//               per rebalance and per add, not per leg, or a 10 USDT account could never open its
//               second leg.
/// No new position or add is placed while the free balance (after every
/// posted margin, including margin other positions hold on the real account)
/// is below this many USDT.
pub const MIN_ENTRY_BALANCE: f64 = 5.0;
/// Long and short gross notional may differ by at most this fraction.
pub const NEUTRAL_TOLERANCE: f64 = 0.02;
// NOTE(agents): Slots are sized at the decision close from marked equity; closing the old positions
//               costs fees and book cost before the new ones fill. Without this headroom the last
//               leg didn't fit and the whole rebalance was rejected (919 times in 192 real-data
//               runs).
/// Share of the free balance a rebalance commits: the rest covers closing the
/// outgoing positions (fees, book cost) between the decision and the fill.
const SLOT_HEADROOM: f64 = 0.99;

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
    /// Screener directional moving-average price-action percentile (bearish to bullish).
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
    // NOTE(agents): The live entry signal. It passed a pre-registered holdout (docs/validation.md,
    //               commits 5329e48/6988079). That holdout is now spent: do NOT re-tune CalmDip's
    //               definition on windows -3..0. Any change needs a fresh pre-registered holdout
    //               (windows before 2026-06-25).
    /// Mean of the volatility and 24h-return percentiles (unflipped = long the
    /// calmest coins that fell most). Both effects held in every research
    /// window on the full historical universe (examples/signal_ic.rs).
    CalmDip,
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
    /// Leverage of each position. Each of the 2 * `top` slots gets a budget of
    /// equity / (2 * top) (halved again when `risk.add_pct` must stay fundable)
    /// for margin + entry fee, so total exposure is about equity * gross_leverage.
    pub gross_leverage: f64,
    /// Intrabar stop: close the moment price touches this % against the entry.
    pub stop_pct: Option<f64>,
    /// Drawdown handling (all off by default; see `Risk`).
    #[serde(default)]
    pub risk: Risk,
    // NOTE(agents): The user wants LONG ONLY live. Market-neutral (long_only = false) stays for
    //               research comparison only; shorting the strongest coins got squeezed into
    //               liquidations on real data.
    /// Long only: hold the `top` lowest values (after `flip`), never short.
    #[serde(default)]
    pub long_only: bool,
    /// Entry filter: new positions only while it allows (see `Regime`).
    #[serde(default)]
    pub regime: Regime,
}

/// Market filter on entries, decided at the rebalance close. When it blocks,
/// the rebalance holds nothing (existing positions are closed as usual).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Regime {
    #[default]
    Off,
    /// BTCUSDT closes above its average close of the last `REGIME_BARS` bars.
    BtcTrend,
}

/// 24 hours of 15m bars: the BTC trend filter's averaging window.
pub const REGIME_BARS: usize = 96;

// NOTE(agents): Missing BTC history returns false (no entries) on purpose: never guess market
//               state. The BTC filter is NOT covered by the CalmDip holdout proof; it is the only
//               market-timing element.
/// Whether `r` allows entries at bar t. A missing BTC history blocks entries.
pub fn regime_allows(m: &Market, t: usize, r: Regime) -> bool {
    match r {
        Regime::Off => true,
        Regime::BtcTrend => {
            let Some(btc) = m.symbols.iter().position(|s| s == "BTCUSDT") else {
                return false;
            };
            if t + 1 < REGIME_BARS {
                return false;
            }
            let closes: Option<Vec<f64>> = (t + 1 - REGIME_BARS..=t)
                .map(|i| m.bars[btc][i].map(|b| b.close))
                .collect();
            closes.is_some_and(|c| c[c.len() - 1] > c.iter().sum::<f64>() / c.len() as f64)
        }
    }
}

impl XsParams {
    /// Positions held at a full rebalance: `top` longs, plus `top` shorts
    /// unless long only.
    pub fn slots(&self) -> usize {
        if self.long_only {
            self.top
        } else {
            2 * self.top
        }
    }
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
    Rebalance,
    /// The token is no longer trading normally (delisted or delisting
    /// scheduled): close at the next open, never overridden.
    Delisting,
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
    #[serde(default)]
    pub last_funding_ts: Option<i64>,
    pub mark: f64,
    #[serde(default)]
    pub next_action: Option<NextAction>,
    #[serde(default)]
    pub action_not_before: i64,
    /// Already added to once.
    #[serde(default)]
    pub added: bool,
    /// The account's Bybit taker fee for this symbol at entry.
    pub fee_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XsPortfolio {
    pub equity: f64,
    /// A failed execution/data precondition invalidates the simulation, never a fake fill.
    #[serde(default)]
    pub execution_error: Option<String>,
    #[serde(default)]
    pub rejected_rebalances: usize,
    #[serde(default)]
    pub effective_top: usize,
    #[serde(default)]
    pub allocation_note: String,
    /// USDT the real account has committed elsewhere (other positions' and
    /// orders' initial margin, locked funds): never available to this bot.
    #[serde(default)]
    pub reserved: f64,
    start_equity: f64,
    pub positions: Vec<XsPosition>,
    /// Target set decided at the last rebalance close, executed at the next open.
    /// Carries symbol names so it survives the live symbol list changing.
    #[serde(default)]
    pending_targets: Option<Vec<(usize, String, Side)>>,
    #[serde(default)]
    pending_params: Option<XsParams>,
    #[serde(default)]
    pending_not_before: i64,
    /// Margin + entry fee per slot fixed at the decision close, so the fill
    /// uses exactly the sizes whose lot rounding was checked then.
    #[serde(default)]
    pending_slot: f64,
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
        Signal::PriceAction => sc.price_action_score,
        Signal::Volatility => sc.volatility_score,
        Signal::Rsi => sc.rsi14,
        Signal::Pulse => sc.pulse_long? - sc.pulse_short?,
        Signal::Funding => funding_hourly(&m.funding[s], m.ts[t])?,
        Signal::CalmDip => (sc.volatility_score + sc.return_score) / 2.0,
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
    ranked_targets(m, scores, t, p, None)
}

fn ranked_targets(
    m: &Market,
    scores: &[Vec<Option<Score>>],
    t: usize,
    p: &XsParams,
    budget: Option<(f64, f64)>,
) -> BTreeMap<usize, Side> {
    let mut out = BTreeMap::new();
    if p.top == 0 {
        return out;
    }
    let mut ranked: Vec<(usize, f64)> = (0..m.symbols.len())
        .filter_map(|s| {
            let value = signal_value(m, scores, s, t, p)?;
            if let Some((margin, cm)) = budget {
                let (bar, r) = (m.bars[s][t]?, m.rules(s)?);
                let notional = (margin / (1.0 / p.gross_leverage + r.taker_fee * cm))
                    .min(r.order_notional_cap(bar.close)?);
                for side in [Side::Long, Side::Short] {
                    let cost = r.book_cost(notional, side == Side::Long)?;
                    let price = bar.close * (1.0 + side.sign() * cost * cm);
                    let qty = r.order_qty(notional / price, price)?;
                    // NOTE(agents): Keep this check at decision time with the SAME budget the fill
                    //               will use (`pending_slot`); checking a different size let
                    //               rounding unbalance fills.
                    // Lot rounding must not unbalance the legs: a contract whose
                    // step is coarse for this slot size cannot be held neutrally.
                    if qty * price < notional * (1.0 - NEUTRAL_TOLERANCE / 2.0)
                        || !r.leverage_allowed(qty * price, p.gross_leverage)
                    {
                        return None;
                    }
                }
            }
            Some((s, value))
        })
        .collect();
    if ranked.len() < 2 * p.top {
        return out;
    }
    ranked.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    for &(s, _) in &ranked[..p.top] {
        out.insert(s, Side::Long);
    }
    if !p.long_only {
        for &(s, _) in &ranked[ranked.len() - p.top..] {
            out.insert(s, Side::Short);
        }
    }
    out
}

// NOTE(agents): Balance-aware sizing for every account size: tries the configured basket, then
//               smaller ones, until each slot passes the real lot, minimum, depth and leverage
//               checks. Returns the slot budget so execution reuses it exactly.
/// Reduce the basket until the balance can fund the contracts' actual lot rules.
fn funded_targets(
    m: &Market,
    scores: &[Vec<Option<Score>>],
    t: usize,
    p: &XsParams,
    equity: f64,
    cost_mult: f64,
    scale: f64,
) -> (BTreeMap<usize, Side>, usize, f64) {
    if equity < MIN_ENTRY_BALANCE {
        return (BTreeMap::new(), 0, 0.0);
    }
    let adds = if p.risk.add_pct.is_some() { 2.0 } else { 1.0 };
    for top in (1..=p.top.min(m.symbols.len() / 2)).rev() {
        let mut adaptive = p.clone();
        adaptive.top = top;
        let budget = equity * scale * SLOT_HEADROOM / adaptive.slots() as f64 / adds;
        let target = ranked_targets(m, scores, t, &adaptive, Some((budget, cost_mult)));
        if target.len() == adaptive.slots() {
            return (target, top, budget);
        }
    }
    (BTreeMap::new(), 0, 0.0)
}

impl XsPortfolio {
    pub fn new(equity: f64) -> Self {
        XsPortfolio {
            equity,
            execution_error: None,
            rejected_rebalances: 0,
            effective_top: 0,
            allocation_note: String::new(),
            reserved: 0.0,
            start_equity: equity,
            positions: vec![],
            pending_targets: None,
            pending_params: None,
            pending_not_before: 0,
            pending_slot: 0.0,
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
    pub fn reindex(&mut self, symbols: &[String]) {
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
                    self.execution_error = Some(format!(
                        "{}: held symbol missing; exchange settlement data required",
                        self.positions[i].symbol
                    ));
                    return;
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

    /// A close must fit the measured book. Extrapolated or nonpositive prices
    /// invalidate the simulation rather than becoming invented fills.
    fn exit_price(&self, m: &Market, i: usize, reference: f64) -> Option<f64> {
        let pos = &self.positions[i];
        let cost = m
            .rules(pos.sym)?
            .book_cost(pos.qty * reference, pos.side == Side::Short)?;
        let price = reference * (1.0 - pos.side.sign() * cost * self.cost_mult);
        (price.is_finite() && price > 0.0).then_some(price)
    }

    fn close_market(
        &mut self,
        m: &Market,
        i: usize,
        ts: i64,
        reference: f64,
        reason: &str,
    ) -> bool {
        if let Some(price) = self.exit_price(m, i, reference) {
            self.close(i, ts, price, reason);
            true
        } else {
            self.execution_error = Some(format!(
                "{}: {reason} cannot fill within measured book depth",
                self.positions[i].symbol
            ));
            false
        }
    }

    // NOTE(agents): Bybit rule: funding is valued at the MARK price and debited from free balance
    //               first, then the isolated margin, which moves the liquidation price. Never value
    //               it at traded prices.
    /// Funding consumes free cash first, then this position's isolated margin.
    fn charge_funding(&mut self, m: &Market, i: usize, ts: i64, rate: f64, mark: f64) {
        if self.positions[i].last_funding_ts == Some(ts) {
            return;
        }
        let cost = self.positions[i].side.sign() * rate * self.positions[i].qty * mark;
        let debit = (cost - self.available().max(0.0)).max(0.0);
        self.equity -= cost;
        let pos = &mut self.positions[i];
        pos.funding += cost;
        pos.last_funding_ts = Some(ts);
        pos.margin = (pos.margin - debit).max(0.0);
        if let Some(price) = m.rules(pos.sym).and_then(|r| {
            r.liquidation_price(pos.side, pos.qty, pos.entry, pos.margin, self.cost_mult)
        }) {
            pos.liquidation = price;
        } else {
            self.execution_error = Some(format!(
                "{}: no valid liquidation tier after funding",
                pos.symbol
            ));
        }
    }

    fn liquidate(&mut self, i: usize, ts: i64) {
        let (liq, margin) = (self.positions[i].liquidation, self.positions[i].margin);
        self.liquidations += 1;
        self.close(i, ts, liq, "Liquidated");
        if let Some(tr) = self.trades.last_mut() {
            let floor = -margin - tr.fees - tr.funding;
            if tr.pnl < floor {
                self.equity += floor - tr.pnl;
                tr.pnl = floor;
            }
        }
    }

    // NOTE(agents): User requirement: delisted/delisting tokens are hard-excluded. The service
    //               calls this AFTER replaying missed bars so the exit fills at the next open,
    //               never at a replayed (historical) bar.
    /// Queue an exit at the next open for every held token not in `trading`
    /// (Bybit's tokens that trade with no delisting scheduled). Returns them.
    pub fn exit_delisting(&mut self, trading: &std::collections::HashSet<String>) -> Vec<String> {
        let mut out = Vec::new();
        for pos in &mut self.positions {
            if !trading.contains(&pos.symbol) && pos.next_action != Some(NextAction::Delisting) {
                pos.next_action = Some(NextAction::Delisting);
                pos.action_not_before = 0;
                out.push(pos.symbol.clone());
            }
        }
        out
    }

    /// Cancel queued entries when the report changes; retain risk exits.
    pub fn install_entry_gate(&mut self, allowed: bool) {
        self.entries_allowed = allowed;
        self.pending_targets = None;
        self.pending_params = None;
        self.pending_slot = 0.0;
        self.effective_top = 0;
        self.allocation_note = if allowed {
            "settings changed: waiting for the next rebalance close".into()
        } else {
            "entry gate disabled".into()
        };
        if !allowed {
            for pos in &mut self.positions {
                if pos.next_action == Some(NextAction::Add) {
                    pos.next_action = None;
                }
            }
        }
    }

    /// Free balance: equity minus this bot's posted margins and the real
    /// account's margin committed elsewhere.
    pub fn available(&self) -> f64 {
        self.equity - self.reserved - self.positions.iter().map(|x| x.margin).sum::<f64>()
    }

    /// Market order at bar t's open spending `budget` USDT on margin + fee, under
    /// Bybit lot rules, the measured order-book cost for this size, the account's
    /// taker fee and the real margin tier. Not placed when the symbol has no
    /// rules, the book is too thin, or the account lacks the margin (as Bybit).
    fn open(&mut self, m: &Market, t: usize, sym: usize, side: Side, budget: f64, p: &XsParams) {
        let (Some(b), Some(r), Some(mark)) = (m.bars[sym][t], m.rules(sym), m.marks[sym][t]) else {
            return;
        };
        if budget <= 0.0 {
            return;
        }
        let (buy, leverage, cm) = (side == Side::Long, p.gross_leverage, self.cost_mult);
        // Size so that margin + entry fee == budget (the book cost is not a separate
        // payment: it is already in the worse fill price).
        let Some(cap) = r.order_notional_cap(b.open) else {
            return;
        };
        let notional = (budget / (1.0 / leverage + r.taker_fee * cm)).min(cap);
        let (Some(cost), Some(_)) = (r.book_cost(notional, buy), r.book_cost(notional, !buy))
        else {
            return;
        };
        let entry = b.open * (1.0 + side.sign() * cost * cm);
        let Some(qty) = r.order_qty(notional / entry, entry) else {
            return;
        };
        if !r.leverage_allowed(qty * entry, leverage) {
            return;
        }
        let Some(liquidation) = r.liquidation_price(side, qty, entry, qty * entry / leverage, cm)
        else {
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
            liquidation,
            stop: p.stop_pct.map(|s| entry * (1.0 - side.sign() * s / 100.0)),
            fees: fee,
            funding: 0.0,
            last_funding_ts: None,
            mark: mark.open,
            next_action: None,
            action_not_before: 0,
            added: false,
            fee_rate: r.taker_fee,
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
        if !r.leverage_allowed((qty0 + q) * px, self.positions[i].leverage) {
            return;
        }
        let entry = (self.positions[i].entry * qty0 + px * q) / (qty0 + q);
        let margin = self.positions[i].margin + q * px / self.positions[i].leverage;
        let Some(liquidation) = r.liquidation_price(side, qty0 + q, entry, margin, self.cost_mult)
        else {
            return;
        };
        let fee = q * px * r.taker_fee * self.cost_mult;
        if self.available() < MIN_ENTRY_BALANCE
            || q * px / self.positions[i].leverage + fee > self.available() + 1e-9
        {
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
        pos.liquidation = liquidation;
        pos.stop = p.stop_pct.map(|st| pos.entry * (1.0 - s * st / 100.0));
    }

    pub fn step(&mut self, m: &Market, scores: &[Vec<Option<Score>>], t: usize, p: &XsParams) {
        if self.execution_error.is_some() {
            return;
        }
        if p.hold == 0
            || p.top == 0
            || !p.gross_leverage.is_finite()
            || p.gross_leverage <= 0.0
            || !self.cost_mult.is_finite()
            || self.cost_mult <= 0.0
        {
            self.execution_error = Some("invalid strategy parameters or cost multiplier".into());
            return;
        }
        let ts = m.ts[t];
        // Held positions need actual mark data before any state mutation.
        if let Some(pos) = self.positions.iter().find(|pos| {
            m.marks
                .get(pos.sym)
                .and_then(|r| r.get(t))
                .copied()
                .flatten()
                .is_none()
        }) {
            self.execution_error = Some(format!("{}: missing mark candle at {ts}", pos.symbol));
            return;
        }
        let mut i = 0;
        while i < self.positions.len() {
            let sym = self.positions[i].sym;
            let mark = m.marks[sym][t].expect("prechecked").open;
            for &(_, rate) in m.funding[sym].iter().filter(|(f, _)| *f == ts) {
                self.charge_funding(m, i, ts, rate, mark);
            }
            if self.execution_error.is_some() {
                return;
            }
            // NOTE(agents): Order matters. Funding at this open, then mark-price gap liquidation,
            //               THEN queued stops/exits/adds. Reordering lets a queued stop book a loss
            //               beyond the isolated margin with zero liquidations (old audit finding
            //               1).
            // Gap liquidation precedes every queued discretionary action.
            if self.positions[i].side.sign() * (mark - self.positions[i].liquidation) <= 0.0 {
                self.liquidate(i, ts);
            } else {
                i += 1;
            }
        }
        let rebalancing = self.pending_targets.is_some() && ts >= self.pending_not_before;
        // 0. Actions decided at the previous close execute at this open. An add
        //    that coincides with a rebalance is skipped: the rebalance decides.
        if std::mem::take(&mut self.flatten_next) {
            self.pending_targets = None;
            let mut i = 0;
            while i < self.positions.len() {
                let sym = self.positions[i].sym;
                match m.bars[sym][t] {
                    Some(b) => {
                        if !self.close_market(m, i, ts, b.open, "Breaker") {
                            return;
                        }
                    }
                    None => {
                        self.flatten_next = true;
                        i += 1;
                    }
                }
            }
        }
        let mut i = 0;
        while i < self.positions.len() {
            let sym = self.positions[i].sym;
            if ts < self.positions[i].action_not_before {
                i += 1;
                continue;
            }
            let action = self.positions[i].next_action.take();
            match (action, m.bars[sym][t]) {
                (Some(NextAction::Stop), Some(b)) => {
                    if !self.close_market(m, i, ts, b.open, "Stop (close)") {
                        return;
                    }
                    continue;
                }
                (Some(NextAction::TakeProfit), Some(b)) => {
                    if !self.close_market(m, i, ts, b.open, "Take profit (close)") {
                        return;
                    }
                    continue;
                }
                (Some(NextAction::Rebalance), Some(b)) => {
                    if !self.close_market(m, i, ts, b.open, "Rebalance") {
                        return;
                    }
                    continue;
                }
                (Some(NextAction::Delisting), Some(b)) => {
                    if !self.close_market(m, i, ts, b.open, "Delisting") {
                        return;
                    }
                    continue;
                }
                (Some(action), None) => self.positions[i].next_action = Some(action),
                (Some(NextAction::Add), Some(_)) if !rebalancing && self.entries_allowed => {
                    self.add(m, t, i, p)
                }
                _ => {}
            }
            i += 1;
        }
        // 1. Rebalance decided at the previous close, executed at this open.
        if rebalancing {
            let execution_data_missing = self.pending_targets.as_ref().is_some_and(|target| {
                target
                    .iter()
                    .any(|(sym, _, _)| m.bars[*sym][t].is_none() || m.marks[*sym][t].is_none())
            });
            if execution_data_missing {
                self.pending_not_before = ts + BAR_MS;
            } else if let Some(target) = self.pending_targets.take() {
                let execution = self.pending_params.take().unwrap_or_else(|| p.clone());
                let p = &execution;
                // NOTE(agents): Positions are always fully closed and reopened at a rebalance (no
                //               partial keep); simpler and keeps every leg at the decided slot
                //               size.
                // Every position closes so both sides are re-sized to neutral.
                let mut i = 0;
                while i < self.positions.len() {
                    match m.bars[self.positions[i].sym][t] {
                        Some(b) => {
                            if !self.close_market(m, i, ts, b.open, "Rebalance") {
                                return;
                            }
                        }
                        None => {
                            self.positions[i].next_action = Some(NextAction::Rebalance);
                            i += 1;
                        }
                    }
                }
                // Each slot's budget (margin + entry fee) was fixed at the decision
                // so the base order AND its possible add both fit in the account.
                let base = std::mem::take(&mut self.pending_slot);
                // Volatility weights from bars up to the decision close (t - 1),
                // mean 1 within each side so both sides keep equal notional.
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
                let side_mean = |side: Side| {
                    let known: Vec<f64> = target
                        .iter()
                        .zip(&inv)
                        .filter(|((_, _, s), _)| *s == side)
                        .filter_map(|(_, w)| *w)
                        .collect();
                    if known.is_empty() {
                        1.0
                    } else {
                        known.iter().sum::<f64>() / known.len() as f64
                    }
                };
                let means = [side_mean(Side::Long), side_mean(Side::Short)];
                let can_enter = self.entries_allowed
                    && !self.flatten_next
                    && self.positions.is_empty()
                    && !target.is_empty();
                if can_enter && self.available() < MIN_ENTRY_BALANCE {
                    self.allocation_note = format!(
                        "free balance {:.2} USDT below the {MIN_ENTRY_BALANCE} USDT minimum: no entries",
                        self.available()
                    );
                } else if can_enter {
                    let mut candidate = self.clone();
                    for (k, &(sym, _, side)) in target.iter().enumerate() {
                        let mean = means[usize::from(side == Side::Short)];
                        let w = inv[k].map_or(1.0, |x| x / mean);
                        candidate.open(m, t, sym, side, base * w, p);
                    }
                    let long = candidate
                        .positions
                        .iter()
                        .filter(|p| p.side == Side::Long)
                        .map(|p| p.qty * p.entry)
                        .sum::<f64>();
                    let short = candidate
                        .positions
                        .iter()
                        .filter(|p| p.side == Side::Short)
                        .map(|p| p.qty * p.entry)
                        .sum::<f64>();
                    let gross = long + short;
                    // NOTE(agents): All legs or none: a partial fill would leave an unintended
                    //               basket (one-sided when market-neutral). Fills are simulated on
                    //               a clone and committed only if complete.
                    if candidate.positions.len() == target.len()
                        && gross > 0.0
                        && (p.long_only || (long - short).abs() / gross <= NEUTRAL_TOLERANCE)
                    {
                        *self = candidate;
                    } else {
                        self.rejected_rebalances += 1;
                    }
                }
                self.rebalance_equity = self.equity + self.unrealized();
            }
        }
        // 2. Funding, liquidation and the intrabar stop (worst case first), then
        //    the close-based rules, executed at the next open.
        let mut i = 0;
        while i < self.positions.len() {
            let (sym, side) = (self.positions[i].sym, self.positions[i].side);
            let mark = m.marks[sym][t].expect("held marks validated at entry");
            if let Some(&(_, rate)) = m.funding[sym]
                .iter()
                .find(|(f, _)| *f > ts && *f < ts + BAR_MS)
            {
                // Non-boundary settlement prices cannot be recovered exactly from
                // 15m candles. Refuse that data rather than use an opening estimate.
                self.execution_error = Some(format!(
                    "{}: intrabar funding at {ts} requires finer mark data (rate {rate})",
                    self.positions[i].symbol
                ));
                return;
            }
            let Some(b) = m.bars[sym][t] else {
                i += 1;
                continue;
            };
            let pos = &self.positions[i];
            let s = side.sign();
            let adverse = if s > 0.0 { b.low } else { b.high };
            let mark_adverse = if s > 0.0 { mark.low } else { mark.high };
            let through = |level: f64, price: f64| s * (price - level) <= 0.0;
            // Mark liquidation is checked first when both mark liquidation and
            // traded-price stop occur inside one candle: their ordering is unknown.
            if through(pos.liquidation, mark.open) || through(pos.liquidation, mark_adverse) {
                self.liquidate(i, ts);
                continue;
            }
            if let Some(st) = pos.stop {
                if through(st, b.open) {
                    if !self.close_market(m, i, ts, b.open, "Stop (gap)") {
                        return;
                    }
                    continue;
                }
                if through(st, adverse) {
                    if !self.close_market(m, i, ts, st, "Stop") {
                        return;
                    }
                    continue;
                }
            }
            let r = p.risk;
            let pos = &mut self.positions[i];
            pos.mark = mark.close;
            let against = -s * (b.close - pos.entry) / pos.entry * 100.0;
            let pending_exit = matches!(
                pos.next_action,
                Some(
                    NextAction::Stop
                        | NextAction::TakeProfit
                        | NextAction::Rebalance
                        | NextAction::Delisting
                )
            );
            if pending_exit {
                i += 1;
                continue;
            }
            pos.action_not_before = 0;
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
        let end = ts + BAR_MS;
        for i in 0..self.positions.len() {
            let sym = self.positions[i].sym;
            // NOTE(agents): Do not turn this back into an error: the live market always loads
            //               funding stamped at the newest close, whose mark price (next open)
            //               doesn't exist yet. Erroring here permanently stopped the paper account.
            // The newest loaded bar's closing settlement is charged when the next
            // bar (and its real mark open) is processed.
            if t + 1 == m.ts.len() {
                break;
            }
            for &(_, rate) in m.funding[sym].iter().filter(|(f, _)| *f == end) {
                let mark =
                    m.ts.get(t + 1)
                        .filter(|&&time| time == end)
                        .and_then(|_| m.marks[sym].get(t + 1))
                        .copied()
                        .flatten()
                        .map(|b| b.open);
                let Some(mark) = mark else {
                    self.execution_error = Some(format!(
                        "{}: funding at end boundary {end} needs its real mark opening price",
                        self.positions[i].symbol
                    ));
                    return;
                };
                self.charge_funding(m, i, end, rate, mark);
            }
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
        self.decide_close(m, scores, t, p);
        self.peak = self.peak.max(marked);
        if self.peak > 0.0 {
            self.max_dd = self.max_dd.max((self.peak - marked) / self.peak * 100.0);
        }
    }

    // NOTE(agents): Decisions use only data up to bar t and fill at a later open
    //               (`defer_new_decisions` live). Targets, parameters and slot budget are frozen
    //               here; `install_entry_gate` cancels them if the settings or the gate change.
    /// Queue a decision using only information available at this close.
    pub fn decide_close(
        &mut self,
        m: &Market,
        scores: &[Vec<Option<Score>>],
        t: usize,
        p: &XsParams,
    ) {
        if p.hold > 0 && is_rebalance(m.ts[t], p.hold) {
            self.pending_not_before = 0;
            let marked = self.equity + self.unrealized();
            let usable = marked - self.reserved;
            let scale =
                if p.risk.derisk_pct.is_some_and(|d| {
                    self.peak > 0.0 && (self.peak - marked) / self.peak * 100.0 >= d
                }) {
                    0.5
                } else {
                    1.0
                };
            let regime = regime_allows(m, t, p.regime);
            let (target, top, slot) = if self.entries_allowed && regime {
                funded_targets(m, scores, t, p, usable, self.cost_mult, scale)
            } else {
                (BTreeMap::new(), 0, 0.0)
            };
            self.effective_top = top;
            self.pending_slot = slot;
            self.allocation_note = if !self.entries_allowed {
                "entry gate disabled".into()
            } else if !regime {
                "regime filter: BTC below its 24h average, no entries".into()
            } else if usable < MIN_ENTRY_BALANCE {
                format!("free balance {usable:.2} USDT below the {MIN_ENTRY_BALANCE} USDT minimum: no entries")
            } else if top == 0 {
                "balance, minimum orders or measured liquidity cannot fund the basket".into()
            } else if top < p.top {
                format!(
                    "balance-adjusted basket: {top} per side (configured maximum {})",
                    p.top
                )
            } else {
                format!("{top} funded per side")
            };
            let mut execution = p.clone();
            if top > 0 {
                execution.top = top;
            }
            self.pending_params = Some(execution);
            self.pending_targets = Some(
                target
                    .into_iter()
                    .map(|(s, side)| (s, m.symbols[s].clone(), side))
                    .collect(),
            );
        }
    }

    pub fn executable_targets(
        &self,
        m: &Market,
        scores: &[Vec<Option<Score>>],
        t: usize,
        p: &XsParams,
    ) -> BTreeMap<usize, Side> {
        if !self.entries_allowed || !regime_allows(m, t, p.regime) {
            return BTreeMap::new();
        }
        funded_targets(
            m,
            scores,
            t,
            p,
            self.equity + self.unrealized() - self.reserved,
            self.cost_mult,
            1.0,
        )
        .0
    }

    /// Live reports/data received after a candle closes can first fill at a
    /// future opening boundary. Backtests leave this delay unset.
    pub fn defer_new_decisions(&mut self, ready_ts: i64) {
        let earliest = ((ready_ts + BAR_MS - 1) / BAR_MS) * BAR_MS;
        if self.pending_targets.is_some() && self.pending_not_before == 0 {
            self.pending_not_before = earliest;
        }
        for pos in &mut self.positions {
            if pos.next_action.is_some() && pos.action_not_before == 0 {
                pos.action_not_before = earliest;
            }
        }
    }

    pub fn validate_state(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            [
                self.equity,
                self.start_equity,
                self.peak,
                self.max_dd,
                self.cost_mult
            ]
            .iter()
            .all(|v| v.is_finite())
                && self.start_equity > 0.0
                && self.cost_mult > 0.0,
            "invalid persisted account values"
        );
        anyhow::ensure!(
            self.execution_error.is_none(),
            "paper execution failed: {:?}",
            self.execution_error
        );
        let mut symbols = std::collections::HashSet::new();
        for pos in &self.positions {
            anyhow::ensure!(
                symbols.insert(&pos.symbol),
                "duplicate persisted position {}",
                pos.symbol
            );
            anyhow::ensure!(
                [
                    pos.entry,
                    pos.qty,
                    pos.margin,
                    pos.leverage,
                    pos.liquidation,
                    pos.fees,
                    pos.funding,
                    pos.mark,
                    pos.fee_rate,
                ]
                .iter()
                .all(|v| v.is_finite())
                    && pos.entry > 0.0
                    && pos.qty > 0.0
                    && pos.margin >= 0.0
                    && pos.leverage > 0.0
                    && pos.mark > 0.0
                    && pos.fees >= 0.0
                    && pos.fee_rate >= 0.0,
                "invalid persisted position {}",
                pos.symbol
            );
        }
        Ok(())
    }

    /// Close every position at its last mark (bar close) with its measured
    /// order-book cost and fee.
    pub fn close_all(&mut self, m: &Market, ts: i64, reason: &str) {
        if self.execution_error.is_some() {
            return;
        }
        while !self.positions.is_empty() {
            let i = self.positions.len() - 1;
            let final_bar = m.ts.partition_point(|t| *t + BAR_MS <= ts).checked_sub(1);
            let reference = final_bar
                .and_then(|t| m.bars[self.positions[i].sym][t])
                .map(|b| b.close);
            let Some(reference) = reference else {
                self.execution_error = Some("no traded close for final exit".into());
                return;
            };
            if !self.close_market(m, i, ts, reference, reason) {
                return;
            }
        }
        self.peak = self.peak.max(self.equity);
        if self.peak > 0.0 {
            self.max_dd = self
                .max_dd
                .max((self.peak - self.equity) / self.peak * 100.0);
        }
    }

    pub fn unrealized(&self) -> f64 {
        self.positions
            .iter()
            .map(|p| p.side.sign() * (p.mark - p.entry) * p.qty)
            .sum()
    }

    pub fn metrics(&self) -> Metrics {
        let mut m = Metrics {
            execution_error: self.execution_error.clone(),
            rejected_rebalances: self.rejected_rebalances,
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
    fn mark_gap_liquidates_before_queued_stop() {
        let mut m = flat_with(&[]);
        let p = risk_params(Risk::default());
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 200.0, &p);
        pf.positions[0].next_action = Some(NextAction::Stop);
        m.marks[0][101] = Some(super::super::Bar {
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 0.0,
            turnover: 0.0,
        });
        pf.step(&m, &[vec![None; 200]], 101, &p);
        assert!(pf.positions.is_empty());
        assert_eq!(pf.trades[0].reason, "Liquidated");
        assert_eq!(pf.metrics().liquidations, 1);
    }

    #[test]
    fn funding_uses_mark_and_debits_isolated_margin() {
        let mut m = flat_with(&[]);
        let r = m.rules[0].as_mut().unwrap();
        r.taker_fee = 0.0;
        for b in &mut r.book {
            b.1 = 0.0;
            b.2 = 0.0;
        }
        let p = risk_params(Risk::default());
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 1000.0, &p);
        let (qty, margin, liq) = (
            pf.positions[0].qty,
            pf.positions[0].margin,
            pf.positions[0].liquidation,
        );
        let mark = m.marks[0][101].as_mut().unwrap();
        mark.open = 110.0;
        mark.high = 110.0;
        m.funding[0] = vec![(101 * BAR_MS, 0.1)];
        pf.step(&m, &[vec![None; 200]], 101, &p);
        let cost = qty * 110.0 * 0.1;
        assert!((pf.positions[0].funding - cost).abs() < 1e-9);
        assert!((pf.positions[0].margin - (margin - cost)).abs() < 1e-9);
        assert!(pf.positions[0].liquidation > liq);
        assert!((pf.available()).abs() < 1e-9);
    }

    #[test]
    fn mark_data_is_required_and_intrabar_funding_is_rejected() {
        let mut m = flat_with(&[]);
        let p = risk_params(Risk::default());
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 200.0, &p);
        let cash = pf.equity;
        m.marks[0][101] = None;
        pf.step(&m, &[vec![None; 200]], 101, &p);
        assert!(pf
            .execution_error
            .as_ref()
            .unwrap()
            .contains("missing mark"));
        assert_eq!(pf.equity, cash);
        m.marks[0][101] = m.marks[0][100];
        m.funding[0] = vec![(101 * BAR_MS + 1, 0.01)];
        pf.execution_error = None;
        pf.step(&m, &[vec![None; 200]], 101, &p);
        assert!(pf
            .execution_error
            .as_ref()
            .unwrap()
            .contains("intrabar funding"));
    }

    fn pending_pair(pf: &mut XsPortfolio, p: &XsParams) {
        pf.pending_targets = Some(vec![
            (0, "S0USDT".into(), Side::Long),
            (1, "S1USDT".into(), Side::Short),
        ]);
        pf.pending_params = Some(p.clone());
        pf.pending_slot = pf.equity * SLOT_HEADROOM / 2.0;
    }

    #[test]
    fn paired_entries_wait_for_missing_candle_and_use_captured_parameters() {
        let mut m = market(&[vec![100.0; 200], vec![100.0; 200]]);
        let p = risk_params(Risk::default());
        let mut pf = XsPortfolio::new(1000.0);
        pending_pair(&mut pf, &p);
        m.bars[1][101] = None;
        pf.step(&m, &vec![vec![None; 200]; 2], 101, &p);
        assert!(pf.positions.is_empty());
        assert!(pf.pending_targets.is_some());
        let mut later = p.clone();
        later.gross_leverage = 10.0;
        pf.step(&m, &vec![vec![None; 200]; 2], 102, &later);
        assert_eq!(pf.positions.len(), 2);
        assert!(pf.positions.iter().all(|x| x.leverage == p.gross_leverage));
    }

    #[test]
    fn disabled_gate_and_report_delay_prevent_stale_entries() {
        let m = market(&[vec![100.0; 200], vec![100.0; 200]]);
        let p = risk_params(Risk::default());
        let mut pf = XsPortfolio::new(1000.0);
        pending_pair(&mut pf, &p);
        pf.install_entry_gate(false);
        assert_eq!(pf.allocation_note, "entry gate disabled");
        pf.step(&m, &vec![vec![None; 200]; 2], 101, &p);
        assert!(pf.positions.is_empty());
        pf.install_entry_gate(true);
        assert!(pf.allocation_note.starts_with("settings changed"));
        pending_pair(&mut pf, &p);
        pf.defer_new_decisions(101 * BAR_MS + 1);
        pf.step(&m, &vec![vec![None; 200]; 2], 101, &p);
        assert!(pf.positions.is_empty());
        pf.step(&m, &vec![vec![None; 200]; 2], 102, &p);
        assert_eq!(pf.positions.len(), 2);
    }

    #[test]
    fn incomplete_pair_does_not_leave_one_sided_orders_or_fees() {
        let mut m = market(&[vec![100.0; 200], vec![100.0; 200]]);
        let p = risk_params(Risk::default());
        m.rules[1].as_mut().unwrap().min_qty = 1000.0;
        let mut pf = XsPortfolio::new(1000.0);
        pending_pair(&mut pf, &p);
        pf.step(&m, &vec![vec![None; 200]; 2], 101, &p);
        assert!(pf.positions.is_empty());
        assert_eq!(pf.equity, 1000.0);
        assert_eq!(pf.rejected_rebalances, 1);
    }

    #[test]
    fn thin_book_exit_is_invalid_instead_of_negative_fill() {
        let mut m = flat_with(&[]);
        let p = risk_params(Risk::default());
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 200.0, &p);
        m.rules[0].as_mut().unwrap().book = vec![(1.0, 0.5, 0.5)];
        pf.close_all(&m, 101 * BAR_MS, "End of test");
        assert!(pf.execution_error.is_some());
        assert_eq!(pf.positions.len(), 1);
        assert!(pf.trades.is_empty());
    }

    #[test]
    fn final_exit_uses_traded_close_and_never_future_data() {
        let mut m = flat_with(&[]);
        let p = risk_params(Risk::default());
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 200.0, &p);
        pf.positions[0].mark = 200.0;
        for b in m.bars[0][101..].iter_mut().flatten() {
            b.close = 500.0;
            b.high = 500.0;
        }
        pf.close_all(&m, 101 * BAR_MS, "End of test");
        assert!(pf.trades[0].exit < 100.0 && pf.trades[0].exit > 99.0);
    }

    #[test]
    fn allocation_adapts_to_small_and_large_balances() {
        let series: Vec<Vec<f64>> = (0..12)
            .map(|s| {
                (0..200)
                    .map(|t| 100.0 + (s as f64 - 5.5) * t as f64 * 0.0001)
                    .collect()
            })
            .collect();
        let mut m = market(&series);
        for r in m.rules.iter_mut().flatten() {
            r.qty_step = 0.001;
            r.min_qty = 0.001;
        }
        let scores = sc(&m);
        let p = XsParams {
            signal: Signal::Return,
            flip: false,
            lookback: 16,
            hold: 16,
            top: 5,
            gross_leverage: 2.0,
            stop_pct: None,
            risk: Risk::default(),
            long_only: false,
            regime: Regime::Off,
        };
        for equity in [0.1, 1.0, 10.0, 100.0, 1000.0, 1_000_000.0] {
            let mut pf = XsPortfolio::new(equity);
            pf.decide_close(&m, &scores, 175, &p);
            pf.step(&m, &scores, 176, &p);
            assert!(
                pf.execution_error.is_none(),
                "balance {equity}: {:?}",
                pf.execution_error
            );
            if equity < 5.01 {
                assert!(pf.positions.is_empty());
                assert_eq!(pf.effective_top, 0);
            } else {
                assert!(
                    !pf.positions.is_empty(),
                    "balance {equity}: {}",
                    pf.allocation_note
                );
            }
            assert!(pf.available() >= -1e-8);
            assert!(pf.positions.len() <= 2 * p.top);
            for pos in &pf.positions {
                assert!(pos.qty * pos.entry >= 5.0);
                assert!(pos.qty * pos.entry <= 3200.0 + 1e-8);
            }
            if equity == 10.0 {
                assert!(pf.effective_top < p.top);
            }
        }
    }

    #[test]
    fn no_entries_or_adds_below_the_free_balance_floor() {
        let series: Vec<Vec<f64>> = (0..12)
            .map(|s| {
                (0..200)
                    .map(|t| 100.0 + (s as f64 - 5.5) * t as f64 * 0.0001)
                    .collect()
            })
            .collect();
        let m = market(&series);
        let scores = sc(&m);
        let p = XsParams {
            signal: Signal::Return,
            flip: false,
            lookback: 16,
            hold: 16,
            top: 1,
            gross_leverage: 1.0,
            stop_pct: None,
            risk: Risk::default(),
            long_only: false,
            regime: Regime::Off,
        };
        // 100 USDT wallet, 96 committed to other positions: 4 free, no trade.
        let mut pf = XsPortfolio::new(100.0);
        pf.reserved = 96.0;
        pf.decide_close(&m, &scores, 175, &p);
        assert_eq!(pf.effective_top, 0);
        assert!(
            pf.allocation_note.contains("below the 5 USDT minimum"),
            "{}",
            pf.allocation_note
        );
        pf.step(&m, &scores, 176, &p);
        assert!(pf.positions.is_empty());
        // 20 free: one neutral pair opens, sized from the free balance only.
        let mut pf = XsPortfolio::new(100.0);
        pf.reserved = 80.0;
        pf.decide_close(&m, &scores, 175, &p);
        pf.step(&m, &scores, 176, &p);
        assert_eq!(pf.positions.len(), 2);
        let posted: f64 = pf.positions.iter().map(|x| x.margin + x.fees).sum();
        assert!(posted <= 20.0 + 1e-9, "{posted}");
        // An add is refused once the free balance is under the floor.
        let a = flat_with(&[]);
        let rp = risk_params(Risk {
            add_pct: Some(1.0),
            ..Default::default()
        });
        let mut pf = XsPortfolio::new(100.0);
        pf.open(&a, 100, 0, Side::Long, 96.0, &rp);
        let qty = pf.positions[0].qty;
        pf.add(&a, 101, 0, &rp);
        assert_eq!(pf.positions[0].qty, qty, "4 USDT free: add refused");
    }

    #[test]
    fn coarse_lot_steps_are_left_out_instead_of_unbalancing_the_basket() {
        let series: Vec<Vec<f64>> = (0..12)
            .map(|s| {
                (0..200)
                    .map(|t| 100.0 + (s as f64 - 5.5) * t as f64 * 0.0001)
                    .collect()
            })
            .collect();
        let mut m = market(&series);
        // Symbol 0 (a long candidate) trades in steps of 0.5 (~50 USDT): a 75
        // USDT slot would round down to 50 and unbalance the basket.
        if let Some(r) = m.rules[0].as_mut() {
            r.qty_step = 0.5;
            r.min_qty = 0.5;
        }
        let scores = sc(&m);
        let p = XsParams {
            signal: Signal::Return,
            flip: false,
            lookback: 16,
            hold: 16,
            top: 2,
            gross_leverage: 1.0,
            stop_pct: None,
            risk: Risk::default(),
            long_only: false,
            regime: Regime::Off,
        };
        let mut pf = XsPortfolio::new(300.0);
        pf.decide_close(&m, &scores, 175, &p);
        pf.step(&m, &scores, 176, &p);
        assert_eq!(pf.rejected_rebalances, 0);
        assert_eq!(pf.positions.len(), 4);
        assert!(pf.positions.iter().all(|x| x.sym != 0));
        let side = |s: Side| -> f64 {
            pf.positions
                .iter()
                .filter(|x| x.side == s)
                .map(|x| x.qty * x.entry)
                .sum()
        };
        let (l, sh) = (side(Side::Long), side(Side::Short));
        assert!((l - sh).abs() / (l + sh) <= NEUTRAL_TOLERANCE);
    }

    fn long_params(flip: bool, regime: Regime) -> XsParams {
        XsParams {
            signal: Signal::Return,
            flip,
            lookback: 16,
            hold: 16,
            top: 3,
            gross_leverage: 1.0,
            stop_pct: None,
            risk: Risk::default(),
            long_only: true,
            regime,
        }
    }

    #[test]
    fn long_only_buys_the_ranked_end_and_never_shorts() {
        // Symbol s drifts by (s - 5.5): 0..5 fall, 6..11 rise.
        let series: Vec<Vec<f64>> = (0..12)
            .map(|s| {
                (0..200)
                    .map(|t| 100.0 + (s as f64 - 5.5) * t as f64 * 0.0001)
                    .collect()
            })
            .collect();
        let m = market(&series);
        let scores = sc(&m);
        for (flip, expected) in [(false, [0, 1, 2]), (true, [9, 10, 11])] {
            let p = long_params(flip, Regime::Off);
            let mut pf = XsPortfolio::new(300.0);
            pf.decide_close(&m, &scores, 175, &p);
            pf.step(&m, &scores, 176, &p);
            assert_eq!(pf.rejected_rebalances, 0);
            let mut held: Vec<usize> = pf.positions.iter().map(|x| x.sym).collect();
            held.sort();
            assert_eq!(held, expected, "flip {flip}");
            assert!(pf.positions.iter().all(|x| x.side == Side::Long));
            // Each of the 3 slots gets a third of 99% of the balance.
            for x in &pf.positions {
                assert!((x.margin + x.fees - 300.0 * SLOT_HEADROOM / 3.0).abs() < 1.0);
            }
        }
    }

    #[test]
    fn btc_trend_filter_blocks_entries_below_its_24h_average() {
        let rising: Vec<f64> = (0..200).map(|t| 100.0 + t as f64 * 0.01).collect();
        let falling: Vec<f64> = (0..200).map(|t| 100.0 - t as f64 * 0.01).collect();
        for (btc, allowed) in [(rising, true), (falling, false)] {
            let mut series: Vec<Vec<f64>> = (0..12)
                .map(|s| {
                    (0..200)
                        .map(|t| 100.0 + (s as f64 - 5.5) * t as f64 * 0.0001)
                        .collect()
                })
                .collect();
            series[11] = btc;
            let mut m = market(&series);
            m.symbols[11] = "BTCUSDT".into();
            let scores = sc(&m);
            assert_eq!(regime_allows(&m, 175, Regime::BtcTrend), allowed);
            let p = long_params(false, Regime::BtcTrend);
            let mut pf = XsPortfolio::new(300.0);
            pf.decide_close(&m, &scores, 175, &p);
            pf.step(&m, &scores, 176, &p);
            assert_eq!(pf.positions.len(), if allowed { 3 } else { 0 });
            if !allowed {
                assert!(
                    pf.allocation_note.contains("BTC below"),
                    "{}",
                    pf.allocation_note
                );
            }
        }
        // Without BTC history the filter never guesses: no entries.
        let m = market(&[vec![100.0; 200], vec![100.0; 200]]);
        assert!(!regime_allows(&m, 150, Regime::BtcTrend));
        assert!(regime_allows(&m, 150, Regime::Off));
    }

    #[test]
    fn delisting_tokens_exit_at_the_next_open() {
        let m = market(&[vec![100.0; 200], vec![100.0; 200]]);
        let p = risk_params(Risk {
            add_pct: Some(1.0),
            ..Default::default()
        });
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 200.0, &p);
        pf.open(&m, 100, 1, Side::Long, 200.0, &p);
        let trading: std::collections::HashSet<String> = ["S1USDT".to_string()].into();
        assert_eq!(pf.exit_delisting(&trading), vec!["S0USDT".to_string()]);
        assert!(pf.exit_delisting(&trading).is_empty(), "queued once");
        pf.step(&m, &vec![vec![None; 200]; 2], 101, &p);
        assert_eq!(pf.positions.len(), 1);
        assert_eq!(pf.positions[0].symbol, "S1USDT");
        assert_eq!(pf.trades[0].reason, "Delisting");
    }

    #[test]
    fn settlement_at_the_newest_close_waits_for_the_next_bar() {
        let mut m = flat_with(&[]);
        let p = risk_params(Risk::default());
        // A settlement stamped at the close of the last loaded bar.
        m.funding[0] = vec![(200 * BAR_MS, 0.001)];
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 199, 0, Side::Long, 200.0, &p);
        pf.step(&m, &[vec![None; 200]], 199, &p);
        assert!(pf.execution_error.is_none(), "{:?}", pf.execution_error);
        assert_eq!(pf.positions[0].funding, 0.0);
        // Once that bar exists, the settlement is charged at its mark open.
        let mut next = flat_with(&[]);
        next.ts = (1..201).map(|i| i * BAR_MS).collect();
        next.funding[0] = m.funding[0].clone();
        pf.step(&next, &[vec![None; 200]], 199, &p);
        assert!(pf.execution_error.is_none());
        let pos = &pf.positions[0];
        assert!((pos.funding - 0.001 * pos.qty * 100.0).abs() < 1e-9);
    }

    #[test]
    fn boundary_funding_is_paid_by_outgoing_holdings() {
        let mut m = flat_with(&[]);
        m.funding[0] = vec![(101 * BAR_MS, 0.01)];
        let sc = vec![vec![None; 200]];
        let p = risk_params(Risk::default());
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 200.0, &p);
        let qty = pf.positions[0].qty;
        pf.pending_targets = Some(vec![(0, "AUSDT".into(), Side::Short)]);
        pf.step(&m, &sc, 101, &p);
        assert!((pf.trades[0].funding - qty).abs() < 1e-9);
        assert!(
            pf.positions.is_empty(),
            "unpaired replacement must be rejected"
        );
        assert_eq!(pf.rejected_rebalances, 1);
        pf.open(&m, 101, 0, Side::Short, 200.0, &p);
        assert_eq!(pf.positions[0].funding, 0.0);
    }

    #[test]
    fn missing_candle_does_not_erase_funding() {
        let mut m = flat_with(&[]);
        m.bars[0][101] = None;
        m.funding[0] = vec![(101 * BAR_MS, 0.01)];
        let p = risk_params(Risk::default());
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 200.0, &p);
        let qty = pf.positions[0].qty;
        let mark = pf.positions[0].mark;
        pf.step(&m, &[vec![None; 200]], 101, &p);
        assert!((pf.positions[0].funding - 0.01 * qty * mark).abs() < 1e-9);
    }

    #[test]
    fn queued_exits_survive_missing_candles() {
        for action in [
            NextAction::Stop,
            NextAction::TakeProfit,
            NextAction::Rebalance,
        ] {
            let mut m = flat_with(&[]);
            m.bars[0][101] = None;
            let sc = vec![vec![None; 200]];
            let p = risk_params(Risk::default());
            let mut pf = XsPortfolio::new(1000.0);
            pf.open(&m, 100, 0, Side::Long, 200.0, &p);
            pf.positions[0].next_action = Some(action);
            pf.step(&m, &sc, 101, &p);
            assert_eq!(pf.positions[0].next_action, Some(action));
            pf.step(&m, &sc, 102, &p);
            assert!(pf.positions.is_empty());
            assert_eq!(pf.trades[0].exit_ts, 102 * BAR_MS);
        }
    }

    #[test]
    fn breaker_and_rebalance_exits_survive_missing_candles() {
        for breaker in [false, true] {
            let mut m = flat_with(&[]);
            m.bars[0][101] = None;
            let sc = vec![vec![None; 200]];
            let p = risk_params(Risk::default());
            let mut pf = XsPortfolio::new(1000.0);
            pf.open(&m, 100, 0, Side::Long, 200.0, &p);
            if breaker {
                pf.flatten_next = true;
            } else {
                pf.pending_targets = Some(vec![]);
            }
            pf.step(&m, &sc, 101, &p);
            pf.step(&m, &sc, 102, &p);
            assert!(pf.positions.is_empty());
        }
    }

    #[test]
    fn final_closing_costs_are_in_drawdown() {
        let m = flat_with(&[]);
        let p = risk_params(Risk::default());
        let mut pf = XsPortfolio::new(1000.0);
        pf.open(&m, 100, 0, Side::Long, 200.0, &p);
        pf.step(&m, &[vec![None; 200]], 100, &p);
        let before = pf.metrics().max_drawdown_pct;
        pf.close_all(&m, 101 * BAR_MS, "End of test");
        assert!(pf.metrics().max_drawdown_pct > before);
        assert!((pf.metrics().max_drawdown_pct - (1000.0 - pf.equity) / 10.0).abs() < 1e-9);
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
            long_only: false,
            regime: Regime::Off,
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
            long_only: false,
            regime: Regime::Off,
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
            long_only: false,
            regime: Regime::Off,
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
    type Ohlc = (f64, f64, f64, f64);

    fn flat_with(changes: &[(usize, Ohlc)]) -> Market {
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
            marks: vec![bars.clone()],
            listing_times: Vec::new(),
            entry_eligible: Vec::new(),
            ts: (0..200).map(|i| i * BAR_MS).collect(),
            symbols: vec!["AUSDT".into()],
            bars: vec![bars],
            funding: vec![vec![]],
            rules: vec![Some(crate::engine::rules::Rules::test_liquid()); 3],
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
            long_only: false,
            regime: Regime::Off,
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
            long_only: false,
            regime: Regime::Off,
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
            long_only: false,
            regime: Regime::Off,
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
            long_only: false,
            regime: Regime::Off,
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
            long_only: false,
            regime: Regime::Off,
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
    fn reindex_follows_names_and_refuses_invented_delisting_fills() {
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
            long_only: false,
            regime: Regime::Off,
        };
        let pf0 = stepped(&m, &s, 150..199, &p);
        assert_eq!(pf0.positions.len(), 4);
        let mut pf = pf0.clone();
        let mut shifted = vec!["AAANEWUSDT".to_string()];
        shifted.extend(m.symbols.iter().cloned());
        pf.reindex(&shifted);
        for x in &pf.positions {
            assert_eq!(shifted[x.sym], x.symbol);
        }
        let gone = pf0.positions[0].symbol.clone();
        let mut pf = pf0.clone();
        let fewer: Vec<String> = m.symbols.iter().filter(|x| **x != gone).cloned().collect();
        pf.reindex(&fewer);
        assert_eq!(pf.positions.len(), 4);
        assert!(pf.execution_error.as_ref().unwrap().contains(&gone));
        assert_eq!(pf.trades.len(), pf0.trades.len());
    }
}
