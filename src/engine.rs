//! The farm loop: open a long/short pair, wait for the move, close both in one intent, reconcile.

use crate::api::{Api, TradingState};
use crate::feed::Feed;
use crate::positions::Chain;
use crate::sign::{self, Close, Intent, OpenPosition};
use crate::state::{ledger_append, InFlight, State};
use crate::RunArgs;
use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::Eip712Domain;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::watch;

const E18: f64 = 1e18;
const BUST_BUFFER: f64 = 0.0005; // the contract trips liquidation ~5 bps before zero equity
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Engine {
    pub api: Api,
    pub chain: Chain,
    pub feed: Feed,
    pub signer: PrivateKeySigner,
    pub user: Address,
    pub domain: Eip712Domain,
    pub args: RunArgs,
    pub state_path: PathBuf,
    pub ledger_path: PathBuf,
    pub st: State,
    pub shutdown: watch::Receiver<bool>,
}

#[derive(Debug, Clone, Copy)]
struct Coin {
    name: &'static str,
    id: u32,
    scale: f64,
    pos_mult: f64,
}
const BTC: Coin = Coin { name: "BTC", id: 0, scale: 10.0, pos_mult: 814.598 };
const ETH: Coin = Coin { name: "ETH", id: 1, scale: 100.0, pos_mult: 483.979 };

/// Fraction of a raw gain the winner nets: deadband, impact scale, 2% win fee (docs).
pub fn keep_frac(d: f64, pos_mult: f64) -> f64 {
    let db = 2e-5;
    if d <= db {
        return 0.0;
    }
    let m = d - db;
    let scale = 0.9 / (1.0 + 1.0 / (m * 15000.0) + 1e5 / (1e6 * m * pos_mult));
    0.98 * scale * (m / d)
}

#[derive(Debug, Clone)]
struct Pair {
    coin: Coin,
    ids: Vec<U256>,
    entries: Vec<f64>,
    margin_raw: u128,
    leverage: u32,
    opened_at: Instant,
    mid_open: f64,
}

impl Engine {
    fn stopping(&self) -> bool {
        *self.shutdown.borrow()
    }

    pub async fn run(mut self) -> Result<()> {
        let d = self.args.move_pct / 100.0;
        let buf = self.args.buffer_pct / 100.0;
        let leverage = ((1.0 / (d * (1.0 + buf))).round() as u32).clamp(1, 1000);
        let start = Instant::now();
        let start_balance = self.balance().await?;
        let floor = (start_balance as f64 * (1.0 - self.args.drawdown_stop_pct / 100.0)) as u128;
        tracing::info!(
            balance = start_balance as f64 / E18, leverage, move = d, floor = floor as f64 / E18,
            dry_run = self.args.dry_run, "paperbot starting"
        );

        // crash recovery: a pair we opened and never closed
        if let Some(f) = self.st.in_flight.clone() {
            tracing::warn!(ids = ?f.position_ids, "resuming an in-flight pair");
            let pair = self.recover(&f).await?;
            self.watch_and_close(pair, d, buf).await?;
        }

        loop {
            if self.stopping() {
                break;
            }
            if let Some(max) = self.args.max_cycles {
                if self.st.cycles_done >= max {
                    tracing::info!("max cycles reached");
                    break;
                }
            }
            let elapsed_days = start.elapsed().as_secs_f64() / 86_400.0;
            if elapsed_days >= self.args.horizon_days {
                tracing::info!("horizon reached");
                break;
            }
            let balance = self.balance().await?;
            if balance <= floor {
                tracing::info!(balance = balance as f64 / E18, "drawdown stop");
                break;
            }
            let summary = self.api.summary().await?;
            let rate = summary.mint_rate();
            if rate < self.args.rate_floor {
                tracing::info!(rate, "mint rate below floor");
                break;
            }
            let ts = self.api.trading_state().await?;
            if ts.trading_paused || ts.action_paused("openPosition") || ts.action_paused("closePositions") {
                tracing::warn!("trading paused or intake gated; retrying in 10s");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }

            let coin = self.pick_coin(d, balance, floor, self.args.horizon_days - elapsed_days);
            let Some(mid) = self.feed.mid(coin.name).filter(|m| m.age() < Duration::from_secs(2)) else {
                tracing::warn!(coin = coin.name, "feed stale; waiting");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            };
            let margin_raw = self.size(&ts, balance, leverage)?;
            tracing::info!(
                cycle = self.st.cycles_done + 1, coin = coin.name, rate,
                margin = margin_raw as f64 / E18, leverage, mid = mid.px, "opening pair"
            );

            let pair = self.open_pair(coin, margin_raw, leverage, mid.px).await?;
            self.watch_and_close(pair, d, buf).await?;
        }
        Ok(())
    }

    async fn balance(&self) -> Result<u128> {
        Ok(self.api.account_row(self.user).await?.balance_raw)
    }

    fn size(&self, ts: &TradingState, balance: u128, leverage: u32) -> Result<u128> {
        let per_leg = (balance as f64 * self.args.capital_fraction / 2.0) as u128;
        let per_leg = per_leg / 10u128.pow(18) * 10u128.pow(18); // whole dollars
        if per_leg < ts.min_trade_size_raw {
            bail!("margin per leg ${} below minimum ${}", per_leg as f64 / E18, ts.min_trade_size_raw as f64 / E18);
        }
        let notional = per_leg * leverage as u128;
        if notional < ts.min_open_notional_raw {
            bail!(
                "notional ${} below the ${} minimum: raise capital or leverage",
                notional as f64 / E18, ts.min_open_notional_raw as f64 / E18
            );
        }
        let max_notional = ts.instruments.iter().map(|i| i.max_notional_raw).min().unwrap_or(u128::MAX);
        Ok(per_leg.min(max_notional / leverage as u128))
    }

    /// Price one cycle on each coin against both remaining budgets; cheaper market wins.
    fn pick_coin(&self, d: f64, balance: u128, floor: u128, days_left: f64) -> Coin {
        match self.args.market.as_str() {
            "btc" => return BTC,
            "eth" => return ETH,
            _ => {}
        }
        let rem_cap = (balance.saturating_sub(floor)) as f64 / E18;
        let days_left = days_left.max(1e-3);
        let price = |c: Coin, fallback_vol: f64| {
            let vol = self.feed.daily_vol(c.name).unwrap_or(fallback_vol);
            let cost = (1.0 - keep_frac(d, c.pos_mult)) * (balance as f64 / E18) / 2.0;
            cost / rem_cap.max(1.0) + (d / vol).powi(2) / days_left
        };
        if price(ETH, 0.031) < price(BTC, 0.024) { ETH } else { BTC }
    }

    async fn submit<T: Intent>(&mut self, msg: &T) -> Result<sign::Signed> {
        let signed = sign::sign(&self.signer, &self.domain, msg)?;
        if self.args.dry_run {
            tracing::info!(action = signed.action, body = %serde_json::Value::Object(signed.body.clone()), "DRY RUN: would POST /intents");
            return Ok(signed);
        }
        let ack = self.api.post_intent(signed.action, &signed.body).await?;
        if ack.intent_id.to_lowercase() != format!("{:#x}", signed.intent_id) {
            bail!("relayer returned a different intentId {} for {}", ack.intent_id, signed.action);
        }
        tracing::debug!(action = signed.action, intent = %signed.intent_id, queue = %ack.queue_revision, "accepted");
        Ok(signed)
    }

    async fn open_pair(&mut self, coin: Coin, margin_raw: u128, leverage: u32, mid: f64) -> Result<Pair> {
        let from_block = self.chain.block_number().await?.saturating_sub(2);
        let mk = |is_long: bool, nonce: U256| OpenPosition {
            user: self.user,
            instrumentId: coin.id,
            isLong: is_long,
            size: margin_raw,
            leverage,
            useDebt: false,
            nonce,
            deadline: sign::deadline(),
        };
        let (n1, n2) = (self.st.take_nonce(), self.st.take_nonce());
        let (long, short) = (mk(true, n1), mk(false, n2));
        // sign both first, then fire both posts concurrently
        let s1 = sign::sign(&self.signer, &self.domain, &long)?;
        let s2 = sign::sign(&self.signer, &self.domain, &short)?;
        self.st.in_flight = Some(InFlight {
            coin: coin.name.into(), instrument_id: coin.id,
            intent_ids: vec![s1.intent_id, s2.intent_id], position_ids: vec![],
            mid_at_open: mid, opened_at: sign::now_secs(), margin_raw, leverage,
        });
        self.st.save(&self.state_path)?;

        if self.args.dry_run {
            tracing::info!(long = %serde_json::Value::Object(s1.body.clone()), short = %serde_json::Value::Object(s2.body.clone()), "DRY RUN: would POST both opens");
            return Ok(Pair { coin, ids: vec![U256::ZERO, U256::ZERO], entries: vec![mid, mid], margin_raw, leverage, opened_at: Instant::now(), mid_open: mid });
        }
        let (r1, r2) = tokio::join!(
            self.api.post_intent(s1.action, &s1.body),
            self.api.post_intent(s2.action, &s2.body)
        );
        let (a1, a2) = match (r1, r2) {
            (Ok(a), Ok(b)) => (a, b),
            (Ok(_), Err(e)) | (Err(e), Ok(_)) => {
                tracing::error!(error = %e, "one open was refused; closing whatever landed");
                let opened = self.wait_opened(from_block, 1, OPEN_TIMEOUT).await.unwrap_or_default();
                if !opened.is_empty() {
                    self.close_ids(opened.iter().map(|o| o.position_id).collect()).await?;
                }
                self.clear_in_flight()?;
                bail!("pair aborted: {e}");
            }
            (Err(a), Err(b)) => {
                self.clear_in_flight()?;
                bail!("both opens refused: {a} / {b}");
            }
        };
        tracing::debug!(long = a1.intent_id, short = a2.intent_id, "opens accepted");

        let opened = self.wait_opened(from_block, 2, OPEN_TIMEOUT).await?;
        if opened.len() < 2 {
            tracing::error!(n = opened.len(), "naked leg: only one position landed; closing it");
            self.close_ids(opened.iter().map(|o| o.position_id).collect()).await?;
            self.clear_in_flight()?;
            bail!("pair aborted: one leg only");
        }
        let entries: Vec<f64> = opened.iter().map(|o| o.entry_px_raw as f64 / coin.scale).collect();
        let ids: Vec<U256> = opened.iter().map(|o| o.position_id).collect();
        let gap_bps = (entries[0] - entries[1]).abs() / mid * 1e4;
        if let Some(f) = self.st.in_flight.as_mut() {
            f.position_ids = ids.clone();
        }
        self.st.save(&self.state_path)?;
        tracing::info!(?ids, ?entries, gap_bps, "pair open");
        if gap_bps > self.args.max_entry_gap_bps {
            tracing::warn!(gap_bps, "entry gap too wide; closing pair");
            self.close_ids(ids.clone()).await?;
            self.clear_in_flight()?;
            bail!("pair aborted: entry gap {gap_bps:.1} bps");
        }
        Ok(Pair { coin, ids, entries, margin_raw, leverage, opened_at: Instant::now(), mid_open: mid })
    }

    async fn wait_opened(&self, from_block: u64, want: usize, timeout: Duration) -> Result<Vec<crate::positions::Opened>> {
        let t0 = Instant::now();
        let known: Vec<U256> = self.st.in_flight.as_ref().map(|f| f.position_ids.clone()).unwrap_or_default();
        loop {
            let to = self.chain.block_number().await?;
            let mut got = self.chain.opened(self.user, from_block, to).await?;
            got.retain(|o| !known.contains(&o.position_id) || known.len() < want);
            if got.len() >= want {
                return Ok(got);
            }
            if t0.elapsed() > timeout {
                return Ok(got);
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
    }

    async fn recover(&mut self, f: &InFlight) -> Result<Pair> {
        let coin = if f.instrument_id == 0 { BTC } else { ETH };
        let latest = self.chain.block_number().await?;
        let from = latest.saturating_sub(((sign::now_secs() - f.opened_at) as u64 + 60) * 2);
        let opened = self.chain.opened(self.user, from, latest).await?;
        let closed = self.chain.closed(self.user, from, latest).await?;
        let live: Vec<_> = opened
            .into_iter()
            .filter(|o| o.instrument_id == f.instrument_id && !closed.iter().any(|c| c.position_id == o.position_id))
            .collect();
        if live.is_empty() {
            tracing::info!("in-flight pair already closed");
            self.clear_in_flight()?;
            bail!("nothing to recover");
        }
        Ok(Pair {
            coin,
            ids: live.iter().map(|o| o.position_id).collect(),
            entries: live.iter().map(|o| o.entry_px_raw as f64 / coin.scale).collect(),
            margin_raw: f.margin_raw, leverage: f.leverage, opened_at: Instant::now(), mid_open: f.mid_at_open,
        })
    }

    async fn watch_and_close(&mut self, pair: Pair, d: f64, buf: f64) -> Result<()> {
        let entry = pair.entries.iter().sum::<f64>() / pair.entries.len() as f64;
        let bust_move = d * (1.0 + buf) - BUST_BUFFER;
        let mut rx = self.feed.subscribe(pair.coin.name);
        let mut poll = tokio::time::interval(Duration::from_secs(1));
        let mut shutdown = self.shutdown.clone();
        let mut last_move = 0.0;
        loop {
            tokio::select! {
                _ = rx.changed() => {
                    let Some(m) = *rx.borrow() else { continue };
                    last_move = (m.px / entry - 1.0).abs();
                    if last_move >= d {
                        tracing::info!(mid = m.px, mv = last_move, "trigger");
                        break;
                    }
                    if last_move >= bust_move - 0.0003 {
                        tracing::warn!(mv = last_move, "approaching bust; closing now");
                        break;
                    }
                }
                _ = poll.tick() => {
                    if self.args.dry_run { continue; }
                    let row = self.api.account_row(self.user).await?;
                    if row.open_positions < pair.ids.len() as u32 {
                        tracing::error!(open = row.open_positions, "a leg vanished (liquidation?); closing the rest");
                        break;
                    }
                }
                _ = shutdown.changed() => {
                    tracing::warn!("shutdown: closing open pair");
                    break;
                }
            }
        }
        let mid_close = self.feed.mid(pair.coin.name).map(|m| m.px).unwrap_or(entry);
        if self.args.dry_run {
            tracing::info!(mid_close, elapsed_s = pair.opened_at.elapsed().as_secs(), "DRY RUN: would close pair");
            self.clear_in_flight()?;
            self.st.cycles_done += 1;
            self.st.save(&self.state_path)?;
            return Ok(());
        }
        self.close_ids(pair.ids.clone()).await?;
        self.reconcile(&pair, mid_close, last_move).await?;
        self.clear_in_flight()?;
        self.st.cycles_done += 1;
        self.st.save(&self.state_path)?;
        Ok(())
    }

    /// One Close intent for all ids; confirm they are gone, resubmit with a fresh nonce if not.
    async fn close_ids(&mut self, ids: Vec<U256>) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        for attempt in 1..=3 {
            let msg = Close { user: self.user, positionIds: ids.clone(), nonce: self.st.take_nonce(), deadline: sign::deadline() };
            self.st.save(&self.state_path)?;
            if let Err(e) = self.submit(&msg).await {
                tracing::error!(attempt, error = %e, "close refused");
                continue;
            }
            if self.args.dry_run {
                return Ok(());
            }
            let t0 = Instant::now();
            while t0.elapsed() < CLOSE_TIMEOUT {
                tokio::time::sleep(Duration::from_millis(800)).await;
                let row = self.api.account_row(self.user).await?;
                if row.open_positions == 0 {
                    tracing::info!(?ids, ms = t0.elapsed().as_millis(), "pair closed");
                    return Ok(());
                }
            }
            tracing::warn!(attempt, "close not confirmed; resubmitting");
        }
        Err(anyhow!("could not confirm close of {ids:?}"))
    }

    async fn reconcile(&mut self, pair: &Pair, mid_close: f64, mv: f64) -> Result<()> {
        let want: Vec<String> = pair.ids.iter().map(|i| i.to_string()).collect();
        let mut trades = Vec::new();
        for _ in 0..15 {
            trades = self.api.account_trades(self.user, 20).await?;
            trades.retain(|t| want.contains(&t.position_id));
            if trades.len() == pair.ids.len() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let pnl: i128 = trades.iter().filter_map(|t| t.adjusted_pnl_raw.parse::<i128>().ok()).sum();
        let minted: u128 = trades.iter().filter_map(|t| t.paper_minted_raw.parse::<u128>().ok()).sum();
        let fees: u128 = trades.iter().filter_map(|t| t.user_paid_fee_raw.parse::<u128>().ok()).sum();
        let row = json!({
            "ts": sign::now_secs(), "cycle": self.st.cycles_done + 1, "coin": pair.coin.name,
            "leverage": pair.leverage, "margin_usd": pair.margin_raw as f64 / E18,
            "ids": want, "entries": pair.entries, "mid_open": pair.mid_open, "mid_close": mid_close,
            "move": mv, "elapsed_s": pair.opened_at.elapsed().as_secs(),
            "pnl_usd": pnl as f64 / E18, "fees_usd": fees as f64 / E18, "paper_minted": minted as f64 / E18,
            "kinds": trades.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        });
        tracing::info!(%row, "cycle done");
        ledger_append(&self.ledger_path, &row).context("ledger")
    }

    fn clear_in_flight(&mut self) -> Result<()> {
        self.st.in_flight = None;
        self.st.save(&self.state_path)
    }

    /// Emergency: close everything this user has open (from the last ~2h of logs).
    pub async fn close_all(&mut self) -> Result<()> {
        let latest = self.chain.block_number().await?;
        let from = latest.saturating_sub(8000);
        let opened = self.chain.opened(self.user, from, latest).await?;
        let closed = self.chain.closed(self.user, from, latest).await?;
        let ids: Vec<U256> = opened
            .iter()
            .filter(|o| !closed.iter().any(|c| c.position_id == o.position_id))
            .map(|o| o.position_id)
            .collect();
        tracing::info!(?ids, "close-all");
        self.close_ids(ids).await?;
        self.clear_in_flight()
    }
}
