# Audit status — 2 October 2026

This page tracks every finding from the 1 October audit and the follow-up audit of 2 October, with its current state in the code. "Fixed" means the code changed and a regression test covers it; the test is named. Everything listed is in the committed tree, which was deployed with a fresh-data reset on 2026-10-02.

The earlier findings have fixes, with the additional qualifications below. With them fixed, the old market-neutral Pulse strategy failed its validation on real data. The live strategy is now long-only CalmDip, whose entry signal met pre-registered out-of-sample thresholds as a **relative** edge; the full strategy passes its walk-forward on 1 of 4 data sets ([validation.md](validation.md)). Since 2026-10-03 paper trades the best settings that pass the two safety rules; the walk-forward check is information, not a gate.

## Findings from the 1 October audit

| # | Finding | State | Where / test |
|---|---|---|---|
| 1 | Queued exits bypassed liquidation at a gap open | Fixed: carried positions are checked for mark-price liquidation at the open before any queued stop, take-profit, rebalance or add | `xs::step`; `mark_gap_liquidates_before_queued_stop` |
| 2 | Funding did not reduce depleted isolated margin | Fixed: funding takes free cash first, then the position's margin, and the liquidation price is recomputed | `charge_funding`; `funding_uses_mark_and_debits_isolated_margin` |
| 3 | Late funding did not reconcile paper | Fixed as "fail visibly": paper records every settlement it applied; a settlement that appears later for a bar already processed stops the paper account with an explicit recovery error rather than silently diverging | `service::advance_paper` |
| 4 | A failed report could execute targets queued by an old schedule | Fixed: a settings or eligibility change cancels queued entries; entries check eligibility at execution | `install_entry_gate`; `disabled_gate_and_report_delay_prevent_stale_entries` |
| 5 | Newly observed cash flow changed historical replay | Fixed: missed bars replay first; the balance change applies at the observation boundary | `advance_paper` |
| 6 | Paper replay silently skipped history outside the loaded window | Fixed: replay must continue exactly from the checkpoint or it errors | `advance_paper` |
| 7 | An unreadable paper file silently started a new account | Fixed: missing vs unreadable/invalid are distinguished; invalid state stops the service; writes are fsynced and renamed | `load_paper`, `persist_paper` |
| 8 | Funding and liquidation used traded prices | Fixed: mark-price candles are fetched, cached and required; funding is valued at the mark open; liquidation triggers on mark extremes; tier deductions are used | `data::mark_range`, `Market::marks` |
| 9 | Portfolio could become one-sided | Fixed: a rebalance commits all legs or none, within a 2% long/short notional tolerance | `incomplete_pair_does_not_leave_one_sided_orders_or_fees` |
| 10 | PriceAction ranked unsigned strength | Fixed: it ranks the signed price-action percentile | `scores::compute` |
| 11 | Research fetch could not recover partial symbols | Fixed: candles, mark candles and funding are checked independently; incomplete data fails the run | `examples/fetch_research_data.rs` |
| 12 | Thin-book extrapolation invented negative exit prices | Fixed: a close outside measured depth invalidates the simulation instead of inventing a fill | `exit_price` |
| 13 | Missing candles consumed queued entries | Fixed: the rebalance waits for the bar with data, using the parameters captured at the decision | `paired_entries_wait_for_missing_candle_and_use_captured_parameters` |
| 14 | Leading gaps were indistinguishable from pre-listing time | Fixed: Bybit `launchTime` is stored; see also new finding D below | `incremental_sync_retries_internal_gaps_and_missing_tail` |
| 15 | The forward gate admitted losing final settings | Fixed then; superseded 2026-10-03: `forward_ok` was removed when the walk-forward stopped gating paper (user design: champion/challenger with two safety rules) | `walkforward::choose_live` |
| ops | `deploy.sh` reported success after failed bootstrap | Fixed: it exits non-zero | `deploy.sh` |
| ops | Funding pagination stopped after a malformed record | Fixed: a malformed record fails the request | `funding_range` |
| ops | Dashboard "last update" masked a stalled loop | Fixed: it shows the last processed bar | console |

## Findings from the 2 October audit (all fixed)

**A. P1 — The code did not compile.** A test called `advance_paper` with the old argument list after a signature change, so the bot binary's tests failed to build. Fixed.

**B. P1 — Risk tiers were rejected for every symbol.** Bybit returns an empty `mmDeduction` for each symbol's lowest risk tier, and for every tier of some symbols (QNT, LIT, STX, DOT on 1 October). The parser treated `""` as invalid and dropped the symbol, so `fetch_rules` produced rules for **0 of 50** symbols. Deployed, the bot would have had no tradeable symbols, and the refresh would have wiped the stored rules. Fixed: tiered maintenance margin is continuous at each boundary, which fixes every deduction (`d[i] = d[i-1] + limit[i-1] × (rate[i] − rate[i-1])`). Checked against Bybit's published values for BTC, ETH, SOL and DOGE: every tier matches to better than 1e-8. Deductions are derived that way, and a published value that disagrees rejects the symbol. Tests: `deductions_follow_bybit_tiers` (real BTCUSDT tiers). Now 50 of 50 symbols have rules.

**C. P1 — A failed refresh could wipe all rules.** `put_rules` replaces the snapshot, and `fetch_rules` silently skipped symbols, so an API change or outage stored an empty snapshot. Fixed: a refresh that measures fewer than half the symbols is an error, and the previous snapshot stays.

**D. P2 — New listings stalled the sync.** CTUSDT launched at 1790844328000, but Bybit's first 15-minute candle opens one bar after the launch boundary (the pre-trading period). The symbol was re-downloaded and counted as a sync failure every bar. Fixed: when Bybit's complete history of a symbol listed inside the window starts after its launch boundary, that first candle is stored (`first_trades`) as where its data begins. A leading gap for a symbol listed before the window is still a real gap. Test: `new_listing_starts_at_bybits_first_candle` (real CTUSDT timestamps).

**E. P1 — One incomplete symbol stopped every bar.** `Market::validate` rejected the whole market if any symbol lacked a candle, and sync tolerates up to 20% failed symbols, so a single gap (a sync failure or an exchange gap) made every walk-forward fail. Fixed: `Market::validate_symbol` checks each symbol; incomplete symbols sit out that bar with a logged warning; a **held** symbol must be complete or the bar fails.

**F. P1 — Funding at the newest close killed the paper account.** The market loads funding stamped at the close of its last bar, but that settlement's mark price is the next bar's open, which doesn't exist yet. The engine set `execution_error`, permanently stopping paper at the first funding settlement while a position was held. Fixed: the settlement is charged when the next bar is processed. Test: `settlement_at_the_newest_close_waits_for_the_next_bar`.

**G. P1 — Lot rounding rejected most rebalances.** Target feasibility checked minimum orders but not rounding. At a balance of about 110 USDT, coarse quantity steps left the long and short sides more than 2% apart, and the rebalance was rejected. Across the current grid on real data, **919 rebalances were rejected in 192 runs**; the account sat flat instead, which flattered earlier results. Fixed in two steps:
- A contract whose quantity step loses more than 1% of the slot's notional is not eligible at that slot size; smaller balances use fewer pairs, larger ones admit coarser contracts.
- The slot budget is fixed at the decision close, with 1% headroom for closing the outgoing positions, so the fill uses exactly the sizes whose rounding was checked.

Result on the same real data: **0 rejected rebalances**. Test: `coarse_lot_steps_are_left_out_instead_of_unbalancing_the_basket` (fails without the fix).

**H. P2 — Volatility-scaled sizing broke neutrality.** Inverse-volatility weights were normalised across both sides together, so one side could outweigh the other and the rebalance was rejected. Fixed: weights are normalised within each side.

**I. P1 — No minimum balance; other positions ignored.** Implemented the 5 USDT rule:
- The real account's committed margin (`totalPositionIM + totalOrderIM + locked`, read-only from `/v5/account/wallet-balance`) is reserved and never counted as free. If Bybit doesn't report those fields (portfolio margin), the whole wallet is treated as committed.
- No rebalance entry and no add happens while the free balance (equity − this bot's margin − reserved) is below `MIN_ENTRY_BALANCE` = 5 USDT. Exits continue.
- Sizing and the walk-forward use the free balance only.

Tests: `no_entries_or_adds_below_the_free_balance_floor`, `account_balance_reserves_committed_margin`.

**J. P2 — Signed requests had no retry.** One timeout on the balance or fee-rate request failed the whole bar. Fixed: signed GETs are re-signed and retried like public ones.

**K. P2 — Research fetch never downloaded mark prices**, so freshly fetched windows failed validation and the dashboard's forward test broke. Fixed.

**L. P2 — One missing ticker failed the bar.** `top_margin_tokens` errored if any instrument lacked a ticker (a new listing). Fixed: it is left out of that bar's ranking.

**M. Dead code removed:** `Cache::last_bar_ts` (no callers), a `keep = false` branch and an unreachable duplicate-position check in the rebalance, a redundant `pending_params` write, and a loop clippy flagged as never looping.

**N. Deployment and the LuLu firewall.** The linker signs each build ad hoc with an identifier that embeds a build hash. The LuLu allow rule for the installed bot matches path **and** signing identifier, so every redeploy would wait on a firewall prompt. `deploy.sh` now re-signs the installed binary with the identifier the existing rule expects.

## Remaining limitations and review findings

- **Survivorship bias in the research windows.** `window_k.db` holds today's top-20 coins only, which favours coins that recently rallied. Long-only backtests on those windows are inflated; entry evidence comes from `signal_ic` on broad-universe windows (every eligible token ranked by turnover at the time).
- **No independent holdout.** The research windows were also used to choose the strategy family. Only data that arrives after a frozen procedure is an untouched test.
- **Historical execution uses today's measurements.** Order books, fees and risk tiers are current measurements applied to past bars; Bybit publishes no historical books.
- **Results are noise-sensitive at this balance.** With 11–22 USDT slots, small sizing differences change which coins pass the lot checks; doubling costs can change the selected coins and with them the result. Single-coin moves (squeezes of +50% to +100%) dominate the outcomes.
- **Late settlements stop paper rather than reconcile it.** If Bybit publishes a settlement after its bar was processed, the paper account stops with an explicit error; there is no automatic replay from an earlier checkpoint.

## Follow-up review — Codex, 2 October

- Fixed future-endpoint filtering in `signal_ic`; membership is frozen before future prices are read. Missing prices exclude the whole sample, with an explicit count. Regression: `missing_future_price_cannot_replace_a_decision_time_member`. The purged holdout rerun has zero excluded samples and unchanged 24h results; survivorship bias remains.
- Fixed persisted reserved-margin validation. Regression: `persisted_reserved_margin_must_be_finite_and_nonnegative`.
- Fixed silent omission of deferred terminal funding: a final close now fails explicitly if a closing-boundary settlement remains unpaid. It requires the real boundary mark, rather than estimating one. Regression: `terminal_close_cannot_omit_deferred_funding`.
- Historical candle repairs and removed funding rows are not detected by the current settlement ledger. The earlier broad recovery claim was too strong. Automatic reconciliation remains unimplemented.
- The strategy's relative evidence is conditional on stored-symbol coverage. Non-overlapping returns can still be dependent; the current t-statistic does not correct for that.
- Delayed volatility-scaled rebalances compute weights from the execution bar's preceding candle rather than storing decision-time weights. The current live grid disables this option; its delayed-execution semantics still need correction before enabling it.
- Removed the `Delisting` exit path: after the strict eligibility change, a held ineligible symbol stops the bar for explicit recovery before replay, so `exit_delisting` could no longer run (dead code).
- Bybit's transient `retCode 10016` ("svc error") failed a research fetch and the first fresh sync on 2026-10-02; public and signed requests now retry it with backoff like rate limits.
- **Fresh-start sync bug (fixed):** with only `start`, Bybit's kline endpoint returns the first 1000 candles after `start`, not the newest. Newest-first paging therefore stopped after one page, and every sync on an empty database missed the last 344 bars and failed until the retry filled the gap (seen on both 2026-10-02 resets, first attributed to Bybit). Requests now always send `end`.
