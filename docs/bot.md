# `src/bin/bot/`: the `bot` binary

Paper trading and signals only. It never places orders.

## `main.rs`

- `bot serve`: the paper-trading service, using the runtime folder.
- `bot backtest`: one live-equivalent run (real balance, current top 50, rules measured now, 14 days synced), printing the walk-forward report as JSON.

## `service.rs`

### Constants

| Name | Value | Meaning |
|---|---|---|
| `PORT` | 8787 | Console, bound to 127.0.0.1 |
| `RULES_REFRESH` | 1 h | Re-measure fees, margin tiers and order books |
| `WF_DEADLINE` | 15 min | Abort a walk-forward that runs longer |
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

The console chart: settings chosen by the live walk-forward on research window 2 trade window 3, joined so the screener keeps its look-back. Totals plus one row per UTC day.

### `bar_task`: once per closed 15-minute bar

1. Lists the top 75 token USDT perpetuals by 24h turnover and stores their launch times; hourly, re-measures their rules. The universe is the top 50 of them with complete rules (a warning is logged if fewer qualify).
2. Reads the real account: wallet and committed margin.
3. Syncs traded and mark candles and funding for the universe plus any held symbol.
4. Builds the 14-day market. **A symbol with incomplete real data sits out this bar** (logged); a held symbol must be complete or the bar fails.
5. Runs the live walk-forward from the free balance: paper equity (or the wallet) minus committed margin.
6. Advances the paper account (above) with entries allowed only when the report `PASSED` or is `FORWARD TEST`, saves it, and logs each account change, open, add and close.
7. Publishes the signal table and status.

Any error ends the run and the supervisor restarts it.

### `console`

A minimal HTTP server: `/` (the page), `/api/status`, `/api/signals`, `/api/research`, all `no-store`. The page shows the real balance and the margin committed elsewhere, paper equity and P&L, the dynamic allocation (pairs funded and why, including the 5 USDT floor), positions, next targets, the event log, recent trades, the walk-forward verdict, and the 14-day forward-test chart. It reloads itself when the service restarts.

## Deployment (`deploy.sh`)

Builds, runs every test target, installs to `~/Library/Application Support/BybitMeanReversionBot/bin/bot`, and (re)starts the launchd job `com.bybitmeanreversion.bot`, failing if launchd does not accept it. The installed binary is re-signed ad hoc with a fixed identifier (`SIGN_ID`), which the LuLu firewall allow rule for that path expects. Without that, every rebuild gets a new linker identifier, and the bot's connections wait on a firewall prompt.
