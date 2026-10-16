# paperbot

Hedged-pair PAPER farm on papertrade (HyperEVM). One account, two isolated positions on the same
market in opposite directions; when price moves the target distance both close in one intent. The
loser mints PAPER on 98% of its loss, the winner returns ~85% of its gain; the haircut is the whole
cost of a cycle.

The bot holds **only a session key**. Session keys can sign OpenPosition / Close / CancelIntent and
nothing else — funding, withdrawals, staking and claims stay in the frontend with your wallet.

## Quickstart

Needs a Rust toolchain (`curl https://sh.rustup.rs -sSf | sh`). macOS and Linux; the launchd
supervision below is macOS only.

```sh
git clone <this repo> && cd paperbot
cargo build --release
mkdir -p ~/.paperbot && cp env.example ~/.paperbot/env   # fill it in, then:
source ~/.paperbot/env
```

1. Deposit USDC in the papertrade app as usual.
2. Register a session key — the master key is read from the environment once and never written:
   ```sh
   PAPERBOT_MASTER_KEY=0x… ./target/release/paperbot register          # add --dry-run to just print the body
   ```
   Registration posts one `registerSessionKey` intent signed by the master key and stores the fresh
   session key in `state.json` (mode 0600). Keys expire after 30 days (`--days`).
3. Check the account and the protocol, then dry-run before risking anything:
   ```sh
   ./target/release/paperbot status
   RUST_LOG=info ./target/release/paperbot run --dry-run
   ```

This bot deploys real USDC at high leverage. Start small, read `AUDIT.md` for what went wrong in
the first days, and keep the Telegram responder running so you hear about pauses and liquidations.

## Run

```sh
RUST_LOG=info ./target/release/paperbot run --dry-run      # whole loop, signs everything, posts nothing
RUST_LOG=info ./target/release/paperbot run                # defaults: auto market, 0.5% move, 25% buffer
```

| flag | default | meaning |
|---|---|---|
| `--market` | `auto` | `btc`, `eth`, `btc+`/`eth+` (that market first, the other one for the pair the relayer refuses — `openInterest`, `funding`), or `auto`: per-cycle switch, cheaper of (capital cost ÷ capital left) + (wait ÷ time left) |
| `--move` | `0.5` | % price move harvested per cycle; leverage = `1 / (move·(1+buffer))`. When the balance can't reach the $10,000 notional floor at the capped leverage, the engine shrinks the move for that pair (never under 0.10%) instead of idling, and says so |
| `--buffer-pct` | `25` | margin headroom past the move: a floor. The engine caps every pair's leverage so the trigger-to-bust gap covers the worst adverse move of the last 7 days (1-minute candles) over a window as long as the slowest relayer execution seen, never under 5 min; the pair-open message shows the leverage it ran with |
| `--capital-fraction` | `1.0` | share of balance deployed per cycle (split across the two legs) |
| `--drawdown-stop-pct` | `60` | pause when equity is down this much from its peak (the peak is remembered in `state.json` across restarts) |
| `--min-balance-usd` | `0` | stop when equity is at or below this many dollars (0 = off); the engine stops at whichever floor is higher |
| `--rate-floor` | `10` | stop when the mint rate (PAPER per $1 loss) is below this |
| `--max-cycles`, `--horizon-days` | — / `14` | hard stops |
| `--max-entry-gap-bps` | `5` | abort a pair whose two entries differ by more than this |

Ctrl-C closes any open pair, flushes the ledger and exits. If the process dies mid-cycle, the next
`run` finds the pair in `state.json`, re-reads its ids from the chain log and resumes watching it.
Every stop condition (drawdown, min balance, rate floor, max cycles, horizon) **pauses** the farm
with a reason (`/status` shows it) instead of exiting: under launchd an exit would be a restart with
no memory of why. `/resume` continues.

```sh
./target/release/paperbot close-all     # emergency: one Close intent for every open position
```

## GUI

```sh
./target/release/paperbot-gui --move 0.5 --buffer-pct 25     # same PAPERBOT_STATE / PAPERBOT_LEDGER env
```

A compact iced window in papertrade's uwu dress: protocol chips (mint rate, LP, queue, supply),
wallet (HyperCore spot and perp USDC, papertrade balance, queued, margin in use, PAPER held, session expiry), the in-flight pair with a live
move-to-trigger bar, the strategy card (leverage, bust guard, BTC vs ETH keep / cost / PAPER-per-$ and
realised vol), a live mid sparkline with entry and trigger bands, and the cycle ledger. It reads the
same files and endpoints the bot does and never signs or posts anything; pass the same `--move` /
`--buffer-pct` you run the bot with so the strategy card matches.

## Telegram

```sh
export PAPERBOT_TG_TOKEN=123456:ABC…     # @BotFather token
export PAPERBOT_TG_CHAT=-100…            # the group id
export PAPERBOT_TG_USERS=111,222         # user ids allowed to issue commands (unset = anyone in the group)
export PAPERBOT_RPC=https://…            # HyperEVM JSON-RPC; the public node rate-limits log scans
export PAPERBOT_RPC_LOG_RANGE=10         # blocks per eth_getLogs (Alchemy free tier = 10; public node = 1000)
```

Put them (and the state/ledger paths) in `~/.paperbot/env` and `source` it — never in the repo.

**Commands** (allowlisted users only): `/status` (farm · pair · account · 24h · all-time · protocol ·
session), `/portfolio` (HyperCore USDC, papertrade equity, PAPER minted/wallet/staked, supply share, lifetime pnl, rank, farm runway), `/pair` (live pnl per leg, net if closed, PAPER if closed, bust-guard distance, ETA to trigger),
`/stats` (all-time, 24h, per market, pace, projection), `/flex` (brag card: account-wide PAPER incl. manual trades, cycles, volume, multiple of the protocol rate, never the spend), `/rate` (curve + measured decay + half-life),
`/positions` (every open position with live pnl, flags untracked ones), `/cycles [n]`, `/health` (engine heartbeat, feed age, exchange gates, session key), `/params`,
`/set <key> <value>`, `/pause`, `/resume`, `/liquidate`, `/help`. Once a day (`PAPERBOT_DIGEST_UTC_HOUR`, default 13)
the responder posts `/stats` as a digest. Alerts that matter (paused, drawdown, watchdog, a leg gone) are retried for two minutes.

**Cards.** `/status`, `/stats`, `/flex`, the daily digest and every "cycle done" come as a 1200×630 PNG with the
text as caption: the big number (PAPER minted, or move progress for an open pair), four chips, the all-time
line, the uwu girl. `src/card.rs` renders an SVG template in-process with resvg — fonts and the girl are
embedded, no browser (headless Brave hangs under launchd). No emoji on the cards (no colour-emoji font); the
text caption keeps them. Preview without Telegram: `PAPERBOT_CARD_OUT=/tmp/c.png paperbot tg-reply stats`.
If a render or `sendPhoto` fails the text is sent alone.

`/set` keys: `move` (%), `buffer` (%), `market` (auto|btc|eth|btc+|eth+), `capital` (0–1), `ddstop` (%), `minbalance` ($, `off`; hard equity floor, stops the farm when reached),
`ratefloor`, `maxcycles` (n|off); validated, applies from the next pair. `/pause` stops new pairs while
the open one finishes and silences the hourly optimizer post (`/calculate` still answers on demand). `/liquidate` shows what would be closed and issues a 60-second code; `/liquidate
<code>` (same user) pauses the farm, lets a running engine close its own pair, and closes everything from
the responder if no engine answers within 20 s. The engine also pushes alerts on its own: drawdown
−25% / −50%, session key under 3 days, refusals, naked legs, stop reasons.

All three surfaces share one file, `params.json` next to `state.json`: Telegram `/set` and the GUI's
**apply** button write it, the engine re-reads it at the top of every cycle, and the GUI's strategy
header shows what's live. The CLI flags only seed the file the first time. Run the responder as its own
process — `paperbot tg-serve` — so it works whether or not the farm or the GUI is up; the engine and
the GUI only poll if `PAPERBOT_TG_COMMANDS=1` (Telegram allows one poller at a time). `paperbot tg-reply status` prints a reply offline.

When both are set the engine pings on start, every pair open, every cycle result (pnl, PAPER minted,
running PAPER/$), aborts (naked leg, wide entry gap, a leg vanishing), the stop reason, and fatal
errors. Sends are fire-and-forget and never delay a close. The GUI's `tg` chip shows whether it's on.

## Running on a laptop

**Supervised (recommended):** `scripts/install-launchd.sh` installs three user launchd agents from
`~/.paperbot/env` — `com.paperbot.engine` (`run`), `com.paperbot.tg` (`tg-serve`) and
`com.paperbot.caffeinate` — with `KeepAlive`, so each is restarted within ~10 s if it dies and comes
back at login. On restart the engine adopts whatever pair is open. Logs: `~/.paperbot/run.log`,
`tg-serve.log`. Re-run the script after changing the env file; `launchctl bootout gui/$(id -u)/com.paperbot.engine`
stops one; after a rebuild `launchctl kickstart -k` **both** the engine and the responder (it embeds the engine's code).
The engine beats a heartbeat file every second from a background task for as long as the process lives. If a pair
is open and the heartbeat is silent for 2 minutes the responder alerts; at 10 minutes it posts one Close for the
recorded ids and pauses the farm. It never writes `state.json` — the engine owns that file and reconciles on its
own. `scripts/doctor.sh` prints launchd state, heartbeat age, params, the pair, reachability and the last log lines.


```sh
caffeinate -i ./target/release/paperbot-gui        # keeps the machine awake while the window is open
```

Starting the farm from the GUI runs the engine inside the GUI process: closing the window stops it
(open pairs are *not* closed on a hard kill — use "stop after cycle" or `close-all`). The CLI `run`
is equivalent and survives the window; don't run both against the same state file.

## What it watches

- **Price**: Hyperliquid's public `bbo` websocket — the same best bid/ask the Exchange's precompile
  reads. Trigger is `|mid / entry − 1| ≥ move`; an extra guard closes early if the move approaches
  the bust trigger (5 bps before zero equity). While a pair is open and the socket has been silent for
  5 s, the REST `allMids` price stands in every second (an alert after 30 s), so a bust is never unseen.
- **Relayer backlog**: an accepted intent is not an executed one (a close has waited 146 s, opens ~270 s).
  Acked opens with no position yet block any new open until they land or their 1 h deadline passes (the relayer has executed opens 39 min late; `/query/accounts/{addr}/queue` lists them, the chat gets a note every 10 min); a queued
  close is polled for up to 10 minutes (`noOpenPositions` on a resubmit means it is still queued).
- **Positions**: the Exchange's `PositionOpened` / `PositionClosed` logs over JSON-RPC, filtered by
  your address — this is where position ids and entry prices come from.
- **Balance / open count**: `/state/leaderboard/accounts/{addr}`. If the open-position count drops
  while a pair is on (liquidation), the other leg is closed at once. The chain's `PositionClosed` log
  is scanned every 5 s for the pair's ids as the primary liquidation signal; the indexer count is secondary.
- **Mint rate**: `/query/protocol/summary` → `100·(120M/(120M+tailProgress))²`.
- **Reconciliation**: `/query/accounts/{addr}/trades` after each close (up to 2 min); one JSON line per
  cycle in the ledger with pnl, fees, PAPER minted, entries, elapsed time. A row written before the indexer
  served the trades is marked `partial` and backfilled by the engine the next time it is idle.

## Notes

- Minimums from `/state/trading`: $10 margin, **$10,000 notional** per position, $10M max.
- The two opens are two intents and may land in different blocks; the close of both legs is one
  intent. The entry gap is logged per cycle.
- The server can pause intake per action (`intake_action_paused`); the bot waits and retries.
- `cargo test` covers the EIP-712 signing against a viem-computed golden vector (digest, r, s, v,
  intentId) for the exact domain the live client uses.
