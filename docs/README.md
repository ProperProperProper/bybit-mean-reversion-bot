# Code documentation

There is one page per source file. Each page documents every function, type and constant in that file: what it does, its inputs and outputs, how it works, and why it is built that way.

| Page | Source | What it covers |
|---|---|---|
| [engine.md](engine.md) | `src/engine/mod.rs`, `src/lib.rs` | Core types (`Bar`, `Market`, `Side`), fixed timing constants, runtime folder |
| [data.md](data.md) | `src/engine/data.rs` | Bybit REST client, signed read-only calls, SQLite cache, data sync |
| [rules.md](rules.md) | `src/engine/rules.rs` | Per-coin Bybit rules: lot sizes, account fee, margin tiers, order-book cost |
| [scores.md](scores.md) | `src/engine/scores.rs` | The screener: volatility, price action, volume, activity rank, trend score, Pulse |
| [xs.md](xs.md) | `src/engine/xs.rs` | The strategy engine: ranking, orders, margin, liquidation, drawdown rules, daily P&L |
| [walkforward.md](walkforward.md) | `src/engine/walkforward.rs` | The 14-day walk-forward gate and the strategy grids |
| [metrics.md](metrics.md) | `src/engine/metrics.rs` | Trade records and performance metrics |
| [research.md](research.md) | `src/engine/research.rs` | The three fixed 14-day research windows, research start equity |
| [keychain.md](keychain.md) | `src/engine/keychain.rs` | Read-only API credentials from the macOS Keychain |
| [governor.md](governor.md) | `src/engine/governor.rs` | CPU target at cooperative checkpoints, and deadlines |
| [supervisor.md](supervisor.md) | `src/engine/supervisor.rs` | Keeps background tasks alive (restart on error, panic or hang) |
| [bot.md](bot.md) | `src/bin/bot/main.rs`, `src/bin/bot/service.rs` | The `bot` binary: CLI, paper-trading service, web console |
| [examples.md](examples.md) | `examples/*.rs` | Data fetch, signal/risk research and cached current-grid validation |
| [audit.md](audit.md) | Audit status | Every finding, its fix and its regression test |
| [validation.md](validation.md) | Real-data results | Tests, walk-forwards, forward tests, signal families |

## Current verification

Audit findings, their fixes and the remaining limitations are in [audit.md](audit.md). The live strategy is long-only CalmDip; its validation results and their limits are in [validation.md](validation.md). Paper trades only while the walk-forward gate allows.

## How it fits together

```
Bybit REST ──► data::Client ──► data::Cache (SQLite) ──► Market (closed 15m bars, funding, rules)
                    │                                        │
          keychain (read-only: fees, balance)        scores::compute (screener, causal)
                                                             │
                                  walkforward::run_xs_with ──┤──► chooses XsParams on past data
                                                             │
                                    xs::XsPortfolio::step ◄──┘   decide at close, fill at next open
                                             │
                                  bot service (paper account, console :8787)
```

## Intended behavior and verification limits

- Features use bars through their decision time; decisions fill at the next open after they were ready. Tests check future-bar invariance.
- Inputs come from Bybit: traded and mark candles, funding, books, tiers, fees, lot rules, the account wallet. Interpolating the book curve and the isolated liquidation formula are the model parts; nothing missing is ever filled with an invented value.
- **5 USDT floor.** No entries or adds while the free balance (after margin committed anywhere on the account) is below 5 USDT.
- **14 days per test.** The walk-forward always uses exactly 1,344 closed 15-minute bars.
- **Read-only.** The bot never places orders. Credentials are used only to read fee rates and the wallet.
