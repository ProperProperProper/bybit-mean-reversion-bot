# `src/engine/metrics.rs`: trades and metrics

Status (2 October 2026): drawdown is sampled from bar-close marked equity and the final realised equity, not continuously intrabar.

## `struct Trade`

One closed position:

| Field | Meaning |
|---|---|
| `symbol`, `side` | The coin, and long or short |
| `entry_ts`, `exit_ts` | Entry and exit times |
| `entry`, `exit` | Average entry and fill prices |
| `qty`, `leverage` | Quantity and leverage |
| `fees`, `funding` | Total fees paid, and net funding (positive = paid) |
| `pnl` | Net P&L after fees and funding |
| `r` | `pnl / margin posted` |
| `reason` | `Rebalance`, `Stop`, `Stop (gap)`, `Stop (close)`, `Take profit (close)`, `Breaker`, `Liquidated`, `Delisted` or `End of test` |

## `struct Metrics`

| Field | Meaning |
|---|---|
| `start_equity`, `end_equity` | Cash at the start and the end |
| `open_unrealized` | Mark-to-market of still-open positions |
| `trades`, `wins` | Closed trades, and how many had `pnl > 0` |
| `gross_profit`, `gross_loss` | Sum of winning pnl, and sum of \|losing pnl\| |
| `liquidations` | Positions liquidated |
| `max_drawdown_pct` | Largest fall of marked equity from a peak, % |

| Method | Returns |
|---|---|
| `net()` | `end_equity + open_unrealized − start_equity` |
| `return_pct()` | `net / start_equity × 100` (0 if the start is 0) |
| `profit_factor()` | `gross_profit / gross_loss`; ∞ if there are no losses but some profit; 0 if no trades |
