# `src/bin/bot/`: the `bot` binary

Paper trading and signals only. It never places orders.

## `main.rs`

- `bot serve`: the paper-trading service, using the runtime folder.
- `bot backtest`: one historical simulation using the paper engine (real free balance, current top 20, rules measured now, 14 days synced), printing the full-window search report as JSON.

## `service.rs`

### Constants

| Name | Value | Meaning |
|---|---|---|
| `PORT` | 8787 | Console, bound to 127.0.0.1 |
| `RULES_REFRESH` | 1 h | Re-measure fees, margin tiers and order books |
| `SEARCH_DEADLINE` | 50 min | Abandon a parameter search that runs longer (the previous result stays in use) |
| `SEARCH_PAUSE` | 60 min | Wait after a search **finishes** before the next one starts |
| `SEARCH_RETRY` | 5 min | Wait after a failed search |
| `PAPER_FILE` | `paper_xs.json` | The persisted paper account |

### Paper state

`PaperState` holds the portfolio, the last processed bar, the settings last used, the last mirrored wallet balance, the settlements already applied, and a schema version.

- **`load_paper`:** a missing file is a first run; an unreadable, invalid or unsupported file stops the service (no silent reset). A legacy file migrates only if it holds no positions or trades.
- **`persist_paper`:** validates, writes a temp file, fsyncs, renames, fsyncs the directory.
- **`advance_paper(ps, market, scores, next, allowed, ready_ts, observed)`:**
  1. Replays every bar after the checkpoint with the **previous** settings. Replay must continue exactly from the checkpoint, and a settlement that turns up late for an already processed bar is an error.
  2. Then applies the observed account: a wallet change is mirrored as a deposit or withdrawal (`cash_flow`, never profit), and the account's committed margin becomes `reserved`.
  3. Installs the newest settings and entry eligibility (cancelling stale queued entries), decides at the latest close, and defers any fill to the first open after `ready_ts`.

### `serve(dir)`

Loads the paper account, the Keychain credentials and the real balance (no credentials: the service stops and launchd retries). It computes the forward-test chart in the background, starts `bar_task` and `console` under the supervisor, and starts a watchdog that exits the process if `bar_task` is silent for 40 minutes.

### `forward_test_daily(dir, start)`

The console chart: settings chosen by the live full-window search on research window 2 trade window 3 (never seen), joined so the screener keeps its look-back. Totals plus one row per UTC day. A simulation error or unclosed final position rejects the chart instead of publishing partial results.

### `bar_task`: once per closed 15-minute bar

1. Lists the top 20 token USDT perpetuals by 24h turnover and stores their launch times; hourly, re-measures their rules. The universe is the top 20 of them with complete rules (a warning is logged if fewer qualify).
2. Reads the real account: wallet and committed margin. Purges every stored row of ineligible symbols (delisted, delisting, missing/invalid leverage, 1×-only). A held ineligible symbol fails the bar for explicit recovery: it is never fetched and never given an invented exit.
3. Syncs traded and mark candles and funding for the universe plus any held (eligible) symbol.
4. Builds the 14-day market. **A symbol with incomplete real data sits out this bar** (logged); a held symbol must be complete or the bar fails.
5. Computes the screener scores and hands the market and the free balance (paper equity, or the wallet, minus committed margin) to `search_task`.
6. Advances the paper account (above) with the settings from the latest search. Entries are allowed while those settings pass the safety rules; before the first search finishes, or when nothing passes them, no new positions open. Saves the account and logs each account change, open, add and close.
7. Publishes the signal table and status.

Any error ends the run and the supervisor restarts it.

### `search_task`: the parameter search

Backtests every live-grid combination (`walkforward::LIVE_COMBOS`, 2,949,120, about 20 minutes on 20 coins) over the newest 14-day market with `walkforward::search_full`, then `walkforward::choose_live` against the settings in use, and stores the result for `bar_task`. After its first search it builds the dashboard's forward-test chart (choose on research window 2 with the same full search, trade window 3), so the two searches don't share the CPU budget. The next search starts `SEARCH_PAUSE` (60 min) after this one **finishes**. A failed or timed-out search keeps the previous result and retries after `SEARCH_RETRY`. Its stall limit is `SEARCH_DEADLINE` + 10 min because the task cannot send heartbeats while the search runs.

### `console`

A minimal HTTP server: `/` (the page), `/api/status`, `/api/signals`, `/api/research`, all `no-store`. The page shows the real balance and the margin committed elsewhere, paper equity and P&L, the dynamic allocation (pairs funded and why, including the 5 USDT floor), positions, next targets, the event log, recent trades, the parameter search (combinations, usable count, best settings and their in-sample 14-day result), and the 14-day forward-test chart. It reloads itself when the service restarts.

## Deployment (`deploy.sh`)

Builds, runs every test target and strict clippy, stops and verifies the old service/process, deletes stale runtime data, and refetches real research windows before it installs to `~/Library/Application Support/BybitMeanReversionBot/bin/bot`, and (re)starts the launchd job `com.bybitmeanreversion.bot`, failing if launchd does not accept it. The installed binary is re-signed ad hoc with a fixed identifier (`SIGN_ID`), which the LuLu firewall allow rule for that path expects. Without that, every rebuild gets a new linker identifier, and the bot's connections wait on a firewall prompt.
