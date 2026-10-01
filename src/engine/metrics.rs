//! Closed-trade records and performance metrics shared by every engine (costs
//! come from each symbol's Bybit rules, `rules::Rules`).

use super::Side;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub symbol: String,
    pub side: Side,
    pub entry_ts: i64,
    pub exit_ts: i64,
    pub entry: f64,
    pub exit: f64,
    pub qty: f64,
    pub leverage: f64,
    pub fees: f64,
    pub funding: f64,
    pub pnl: f64,
    /// pnl / margin (or deal funds) committed.
    pub r: f64,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub start_equity: f64,
    pub end_equity: f64,
    pub open_unrealized: f64,
    pub trades: usize,
    pub wins: usize,
    pub gross_profit: f64,
    pub gross_loss: f64,
    pub liquidations: usize,
    pub max_drawdown_pct: f64,
    pub avg_r: f64,
}

impl Metrics {
    pub fn net(&self) -> f64 {
        self.end_equity + self.open_unrealized - self.start_equity
    }
    pub fn return_pct(&self) -> f64 {
        if self.start_equity > 0.0 {
            self.net() / self.start_equity * 100.0
        } else {
            0.0
        }
    }
    pub fn win_rate(&self) -> f64 {
        if self.trades == 0 {
            0.0
        } else {
            self.wins as f64 / self.trades as f64 * 100.0
        }
    }
    pub fn profit_factor(&self) -> f64 {
        if self.gross_loss > 0.0 {
            self.gross_profit / self.gross_loss
        } else if self.gross_profit > 0.0 {
            f64::INFINITY
        } else {
            0.0
        }
    }
}
