# paperbot audit — 2026-10-14

Bottom-up review of the engine, responder, API/feed/chain layers, launchd supervision and the
2.4 days of production logs (10-12 03:32 → 10-14 12:50 UTC, 17 cycles). GUI out of scope.

## Numbers that frame everything

| | |
|---|---|
| wall time | 56.7 h |
| time with a pair open | 21.1 h (**37 %**) |
| spent / minted | $600 → 73,909 PAPER = 123/$ |
| 3 liquidated cycles (3, 4, 8) | $370 → 35,895 PAPER = 97/$ — **62 % of all capital burned** |
| 14 clean cycles | $230 → 38,014 PAPER = 165/$ |
| since the tail-cap fix (cycles 9–17) | 0 liquidations, 166/$ |

The mint rate decays ~13 %/day. Every idle hour costs ~0.5 % of the remaining yield, so **uptime
is a bigger lever than any strategy knob**. Two thirds of the wall time was idle:

| idle window | hours | cause |
|---|---|---|
| 10-12 05:58 → 21:34 | 15.6 | user pause after the liquidations (deliberate) |
| 10-13 00:13 → 09:15 | 9.0 | **watchdog false positive** paused the farm overnight |
| 10-14 08:41 → now | 4.2+ | **watchdog false positive** again; the "paused" Telegram message failed to send, nobody knows |
| 10-12 21:46 → 21:52 | 0.1 | 33 engine restarts in a close-refusal loop (old code, fixed) |
| open refusals (`openInterest`, `funding`) | 0.2 | BTC crowded; `market=btc` just retries every 20 s |
| close waits at the relayer | 0.4 | 413 s, 296 s, 288 s, 175 s, 70 s, 62 s |

## Root causes, in order of damage

### 1. The watchdog closes/pauses on every slow close (two incidents, 13 h lost)
`close_ids` polls the account for up to 10 min and `reconcile` for 15 s **without touching the
heartbeat** (`engine.rs` only beats at the loop top and in the watch poll tick). Any close that
takes > 120 s (the relayer does this routinely) looks like a dead engine to `tg-serve`, which
then posts a redundant close-all, writes `paused=true`, and overwrites `state.json` with its own
stale copy (`cycles_done` went backwards today at 08:11:50). Both incidents match to the second:
trigger at 00:06:23 → watchdog 00:08:27; trigger 08:06:53 → watchdog 08:08:55.

The same gap covers `wait_opened` (90 s), `recover`/`adopt` log scans (up to 90 RPC calls on the
Alchemy 10-block tier), and the tail-gap candle fetch.

### 2. Stops exit the process; launchd restarts it with amnesia
`drawdown stop`, `min balance`, `rate floor`, `max cycles`, `horizon` all `break` → `run_inner`
returns Ok → process exits → launchd restarts it in 10 s with a fresh `peak`, `start`, and
`horizon`. So the drawdown stop is relative to "equity when the process last restarted", which is
every few hours. On 10-13 23:03 a bogus leg-vanish row produced "drawdown stop at $9.54", the process
exited, restarted, and would have resumed farming at a 90 % lower peak if the balance had been sizeable.
`max_cycles` would restart-and-stop every 10 s forever, posting a Telegram message each time.

### 3. Critical Telegram messages are fire-and-forget
`Notifier::send` spawns one 8 s attempt, no retry. At 08:41:47 today both "cycle 17 done" and
"paused" failed (network blip). `/status` is the only way to find out, and it says "engine silent"
during any close wait because its liveness threshold is 5 s.

### 4. No price fallback while a pair is open
`watch_and_close` trusts the websocket. Reconnect is fast (≤3 s backoff) but a half-open socket
(pings sent, nothing received) is never detected; `Mid::age` is only checked before *opening*.
Under a 78× pair the bust guard is 0.83 % away; a silent feed for a few minutes is the one
scenario the engine cannot see.

### 5. Liquidation detection depends on an indexer field that has already vanished once
The leg-vanish check reads `currentOpenPositionCount` from the leaderboard row (fallback to
notional added 10-13). The chain's `PositionClosed` log is the truth and costs one `eth_getLogs`
call per 10 s on the 10-block tier.

### 6. Reconcile gives up after 15 s and writes whatever it has
A lagging `/trades` route produces a ledger row with `kinds: []`, pnl 0, paper 0 (the 10-13 23:03
row). Stats, `/cycles`, and the efficiency numbers silently drift.

### 7. Two processes own `state.json`
`tg-serve` builds an `Engine` for `/liquidate` and the watchdog, and `close_ids`/`clear_in_flight`
save state from that copy: nonce, `cycles_done`, and `in_flight` get overwritten by whichever
process saves last (the 10-13 23:55 record wipe, today's `cycles_done` regression).

### 8. Smaller throughput leaks
- `market=btc` + `openInterest` refusal: retries BTC every 20 s (30 refusals, 10 min, today 02:49).
  In `auto` the engine alternates; in a fixed market it has no fallback.
- The close resubmit at 20 s always gets `noOpenPositions` and only adds a nonce; harmless but
  the log reads like a failure.
- `/status` liveness uses ≤5 s while the paused loop beats every 5 s: flaps.
- Restart storms spam Telegram ("Engine error #1/#10/#20").

## What is already right (keep)
Tail-sized leverage (0 liquidations in 9 cycles), the pending-opens wait, `noOpenPositions` = wait,
atomic heartbeat file, record-trusting recovery, single-intent close of both legs, session-key-only
custody, launchd KeepAlive, `/liquidate` with a code.

## Plan

### Phase 0 — stop the bleeding (today, ~1 h of code)
1. **Heartbeat as a background task.** `tokio::spawn` in `Engine::run` that touches the file every
   second for the life of the process. Delete the inline touches. The heartbeat then means
   "process alive", which is the only thing the file can honestly say.
2. **Watchdog: alert at 2 min, close at 10 min, never write `state.json`.** launchd already
   restarts a dead engine in 10 s and the engine re-adopts the pair; the watchdog is only for a
   hung process. Its close path posts the Close intent and returns; the engine reconciles.
3. **Stops become pauses.** `stop_note` writes `paused=true` and `pause_reason` into `params.json`
   and the loop continues (serving heartbeat). No more exits, no restart amnesia.
4. **Retry critical sends.** `Notifier::send_critical`: 4 attempts over ~2 min. Use it for
   paused/stopped/watchdog/liquidation/naked-leg messages. `/status` shows `pause_reason` and when.
5. **Persist `peak_equity`** in `state.json` so the drawdown stop survives restarts.
6. `/resume` the farm (operator).

### Phase 1 — survive with a pair open (this week)
1. **Price fallback.** In the watch poll tick, if the feed mid is older than 5 s, fetch
   `allMids` from the Hyperliquid info endpoint and feed it into the trigger check; alert once
   after 30 s of fallback.
2. **Chain-log liquidation detector.** Incremental `closed(user, last_block, latest)` every 5 s
   while a pair is open; a `PositionClosed` for one of our ids → close the other leg at once.
   Keep the indexer count as the secondary signal.
3. **Reconcile waits up to 2 min**, writes `partial: true` when the trades are missing, and the
   loop top backfills partial rows (`state.reconcile_pending`).
4. **One owner for `state.json`.** The responder never constructs an `Engine`; `/liquidate` and
   the watchdog go through a small `close_all_detached(api, signer, ids)` that only POSTs.

### Phase 2 — throughput
1. **Market fallback on refusal.** New param `market: btc|eth|auto|btc+ | eth+` ("preferred,
   fall back to the other on `openInterest`/`funding` for that pair only").
2. **Skip the 20 s resubmit**: on an acked close, wait on the account row for up to 10 min
   directly; resubmit only on an explicit refusal other than `noOpenPositions`.
3. **Uptime in `/stats`**: time-in-pair / wall, and idle time by reason (paused, refused, close
   wait, feed stale). This is the number to watch from now on.

### Phase 3 — operations
1. Daily digest at 09:00 local: cycles, PAPER, $/PAPER, uptime, idle reasons, rate and decay.
2. Rate-limit engine-error alerts to one per 10 min with a counter.
3. `scripts/doctor.sh`: prints launchd state, heartbeat age, params, last 5 log lines, Telegram
   reachability. The thing to run when the group goes quiet.
4. Commit the current tree (all of the above is uncommitted work from 10-13/14).

### Not doing
GUI changes. Log rotation (100 KB in 2 days). Unit tests around the engine loop (no mocks; the dry
run covers it). A smaller move for throughput: cost per PAPER is the objective, uptime is the lever.
