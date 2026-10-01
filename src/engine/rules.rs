//! Per-symbol trading rules, all taken from Bybit, never assumed:
//! * order rules (`lotSizeFilter`: qty step, min qty, min order value),
//! * the account's own taker fee (`/v5/account/fee-rate`, read-only, signed),
//! * maintenance-margin tiers (`/v5/market/risk-limit`),
//! * the cost of a market order of a given size, measured by walking the real
//!   order book (`/v5/market/orderbook`, 500 levels per side).
//!
//! Bybit publishes no historical order books, so backtests use the most recent
//! measurement (`book_ts` says when); the live service re-measures every hour.
//! A symbol without rules is not traded, and an order larger than the measured
//! book depth is not filled.

use serde::{Deserialize, Serialize};

/// Order sizes (USDT notional) at which the book cost is measured.
pub const BOOK_POINTS: [f64; 10] = [
    10.0, 25.0, 50.0, 100.0, 200.0, 400.0, 800.0, 1600.0, 3200.0, 6400.0,
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarginTier {
    pub limit: f64,
    pub rate: f64,
    pub deduction: f64,
    pub max_leverage: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rules {
    pub qty_step: f64,
    pub min_qty: f64,
    pub min_notional: f64,
    pub max_market_qty: f64,
    /// The account's taker fee rate (e.g. 0.00055).
    pub taker_fee: f64,
    /// Maintenance-margin tiers: (position value limit USDT, rate), ascending.
    pub mm_tiers: Vec<MarginTier>,
    /// Measured market-order cost: (notional USDT, buy cost, sell cost) as a
    /// fraction of the mid price, for each BOOK_POINTS size the book could fill.
    pub book: Vec<(f64, f64, f64)>,
    /// When the order book was measured (ms).
    pub book_ts: i64,
}

impl Rules {
    /// Quantity Bybit accepts for an order of about `qty` at `price`: rounded
    /// down to the step; None below the minimum qty or order value.
    pub fn order_qty(&self, qty: f64, price: f64) -> Option<f64> {
        if !qty.is_finite()
            || !price.is_finite()
            || price <= 0.0
            || qty <= 0.0
            || self.qty_step < 0.0
        {
            return None;
        }
        let qty = qty.min(self.max_market_qty);
        let q = if self.qty_step > 0.0 {
            (qty / self.qty_step + 1e-9).floor() * self.qty_step
        } else {
            qty
        };
        (q > 0.0 && q >= self.min_qty && q * price >= self.min_notional).then_some(q)
    }

    /// Leave half of the measured book capacity as a closing-liquidity reserve.
    /// Large balances keep unallocated cash instead of assuming infinite depth.
    pub fn order_notional_cap(&self, reference: f64) -> Option<f64> {
        let depth = self.book.last()?.0;
        Some((depth * 0.5).min(self.max_market_qty * reference))
    }

    /// Market-order cost (fraction of price) for `notional` USDT, buying or
    /// selling, interpolated between measured sizes; None if the measured book
    /// was too thin for that size.
    pub fn book_cost(&self, notional: f64, buy: bool) -> Option<f64> {
        if !notional.is_finite()
            || notional <= 0.0
            || self.book.iter().any(|&(n, b, s)| {
                !n.is_finite()
                    || n <= 0.0
                    || !b.is_finite()
                    || !s.is_finite()
                    || !(0.0..1.0).contains(&b)
                    || !(0.0..1.0).contains(&s)
            })
            || self.book.windows(2).any(|w| w[0].0 >= w[1].0)
        {
            return None;
        }
        let pick = |p: &(f64, f64, f64)| if buy { p.1 } else { p.2 };
        let first = self.book.first()?;
        if notional <= first.0 {
            return Some(pick(first));
        }
        for w in self.book.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if notional <= b.0 {
                let f = (notional - a.0) / (b.0 - a.0);
                return Some(pick(a) + f * (pick(b) - pick(a)));
            }
        }
        None
    }

    /// Isolated-margin simulation threshold with the actual tier deduction and
    /// a closing-fee reserve. Mark prices trigger this threshold; this is not a
    /// model of Bybit's unified/cross-margin account engine.
    pub fn liquidation_price(
        &self,
        side: super::Side,
        qty: f64,
        entry: f64,
        margin: f64,
        fee_mult: f64,
    ) -> Option<f64> {
        if !qty.is_finite()
            || qty <= 0.0
            || !entry.is_finite()
            || entry <= 0.0
            || !margin.is_finite()
            || margin < 0.0
        {
            return None;
        }
        let fee = self.taker_fee * fee_mult;
        let mut lower = 0.0;
        for tier in &self.mm_tiers {
            let denominator = qty * (1.0 - side.sign() * (tier.rate + fee));
            if denominator <= 0.0 {
                return None;
            }
            let price = (qty * entry - side.sign() * (margin + tier.deduction)) / denominator;
            if price <= 0.0 && side == super::Side::Long {
                return Some(0.0);
            }
            let value = qty * price;
            if value > lower && value <= tier.limit {
                return Some(price);
            }
            lower = tier.limit;
        }
        None
    }

    pub fn leverage_allowed(&self, value: f64, leverage: f64) -> bool {
        self.mm_tiers
            .iter()
            .find(|t| value <= t.limit)
            .is_some_and(|t| leverage.is_finite() && leverage > 0.0 && leverage <= t.max_leverage)
    }
}

/// UNIT TESTS ONLY: a deep, uniform book (0.02% cost at every size), a 0.055%
/// fee and one 1% margin tier, so engine tests can trade synthetic series.
#[cfg(test)]
pub const TEST_COST: f64 = 0.0002;

#[cfg(test)]
impl Rules {
    pub fn test_liquid() -> Rules {
        Rules {
            qty_step: 0.0,
            min_qty: 0.0,
            min_notional: 5.0,
            max_market_qty: 1e12,
            taker_fee: 0.00055,
            mm_tiers: vec![MarginTier {
                limit: 1e12,
                rate: 0.01,
                deduction: 0.0,
                max_leverage: 100.0,
            }],
            book: BOOK_POINTS
                .iter()
                .map(|&n| (n, TEST_COST, TEST_COST))
                .collect(),
            book_ts: 0,
        }
    }
}

/// Cost of a market order of `notional` USDT against one side of the book
/// (`levels`: (price, size) best first), as a fraction of `mid`; None when the
/// levels cannot fill it.
pub fn walk_book(levels: &[(f64, f64)], mid: f64, notional: f64) -> Option<f64> {
    if !mid.is_finite()
        || mid <= 0.0
        || !notional.is_finite()
        || notional <= 0.0
        || levels
            .iter()
            .any(|&(p, q)| !p.is_finite() || !q.is_finite() || p <= 0.0 || q <= 0.0)
    {
        return None;
    }
    let (mut left, mut qty, mut spent) = (notional, 0.0, 0.0);
    for &(price, size) in levels {
        let take = (left / price).min(size);
        qty += take;
        spent += take * price;
        left -= take * price;
        if left <= 1e-9 {
            let vwap = spent / qty;
            return Some((vwap / mid - 1.0).abs());
        }
    }
    None
}

/// Book cost at every BOOK_POINTS size the book can fill.
pub fn measure_book(bids: &[(f64, f64)], asks: &[(f64, f64)]) -> Vec<(f64, f64, f64)> {
    let (Some(b), Some(a)) = (bids.first(), asks.first()) else {
        return vec![];
    };
    if b.0 > a.0
        || bids.windows(2).any(|w| w[0].0 < w[1].0)
        || asks.windows(2).any(|w| w[0].0 > w[1].0)
    {
        return vec![];
    }
    let mid = (a.0 + b.0) / 2.0;
    BOOK_POINTS
        .iter()
        .map_while(|&n| Some((n, walk_book(asks, mid, n)?, walk_book(bids, mid, n)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        Rules {
            qty_step: 0.001,
            min_qty: 0.001,
            min_notional: 5.0,
            max_market_qty: 1e12,
            taker_fee: 0.00055,
            mm_tiers: vec![
                MarginTier {
                    limit: 200_000.0,
                    rate: 0.01,
                    deduction: 0.0,
                    max_leverage: 100.0,
                },
                MarginTier {
                    limit: 400_000.0,
                    rate: 0.015,
                    deduction: 1000.0,
                    max_leverage: 66.0,
                },
            ],
            book: measure_book(&[(99.9, 1.0), (99.0, 10.0)], &[(100.1, 1.0), (101.0, 10.0)]),
            book_ts: 0,
        }
    }

    #[test]
    fn liquidation_equation_handles_fees_deductions_and_tier_boundaries() {
        let r = rules();
        for side in [super::super::Side::Long, super::super::Side::Short] {
            for (q, entry, margin) in [(10.0, 100.0, 500.0), (3000.0, 100.0, 15000.0)] {
                let price = r.liquidation_price(side, q, entry, margin, 1.0).unwrap();
                let tier = r.mm_tiers.iter().find(|t| q * price <= t.limit).unwrap();
                let equity = margin + side.sign() * q * (price - entry);
                let required = q * price * (tier.rate + r.taker_fee) - tier.deduction;
                assert!((equity - required).abs() < 1e-8);
            }
        }
        assert!(r
            .liquidation_price(super::super::Side::Long, 0.0, 100.0, 10.0, 1.0)
            .is_none());
        assert!(r
            .liquidation_price(super::super::Side::Long, 1.0, 100.0, -1.0, 1.0)
            .is_none());
    }

    #[test]
    fn walks_the_book_for_the_order_size() {
        // Mid 100. 50 USDT fits in the first ask level: cost 0.1%.
        assert!(
            (walk_book(&[(100.1, 1.0), (101.0, 10.0)], 100.0, 50.0).unwrap() - 0.001).abs() < 1e-9
        );
        // 200 USDT: 100.1 USDT at 100.1 then the rest at 101 -> worse VWAP.
        let c = walk_book(&[(100.1, 1.0), (101.0, 10.0)], 100.0, 200.0).unwrap();
        assert!(c > 0.001 && c < 0.01, "{c}");
        assert_eq!(
            walk_book(&[(100.1, 1.0)], 100.0, 200.0),
            None,
            "book too thin"
        );
    }

    #[test]
    fn interpolates_cost_and_refuses_beyond_depth() {
        let r = rules();
        let small = r.book_cost(10.0, true).unwrap();
        let mid = r.book_cost(150.0, true).unwrap();
        assert!(mid > small);
        // Depth: 100.1 + 1010 USDT on the ask side, so 1600 USDT cannot fill.
        assert_eq!(r.book_cost(1600.0, true), None);
    }

    #[test]
    fn order_qty_and_margin_tiers() {
        let r = rules();
        assert!((r.order_qty(200.0 / 110_000.0, 110_000.0).unwrap() - 0.001).abs() < 1e-12);
        assert_eq!(r.order_qty(0.0009, 110_000.0), None);
        assert!(r.leverage_allowed(1_000.0, 100.0));
        assert!(r.leverage_allowed(300_000.0, 66.0));
        assert!(!r.leverage_allowed(300_000.0, 67.0));
        assert!(!r.leverage_allowed(500_000.0, 1.0));
    }
}
