//! Telegram notifications, fire-and-forget. Configured from PAPERBOT_TG_TOKEN + PAPERBOT_TG_CHAT.

use anyhow::Result;
use serde_json::json;

#[derive(Clone)]
pub struct Notifier {
    http: reqwest::Client,
    token: String,
    chat: String,
}

impl Notifier {
    pub fn from_env() -> Option<Self> {
        let token = std::env::var("PAPERBOT_TG_TOKEN").ok().filter(|s| !s.trim().is_empty())?;
        let chat = std::env::var("PAPERBOT_TG_CHAT").ok().filter(|s| !s.trim().is_empty())?;
        Some(Self {
            http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(8)).build().ok()?,
            token: token.trim().to_string(),
            chat: chat.trim().to_string(),
        })
    }

    pub fn configured() -> bool {
        Self::from_env().is_some()
    }

    pub async fn send_now(&self, text: &str) -> Result<()> {
        let r = self
            .http
            .post(format!("https://api.telegram.org/bot{}/sendMessage", self.token))
            .json(&json!({ "chat_id": self.chat, "text": text, "parse_mode": "HTML", "disable_web_page_preview": true }))
            .send()
            .await?;
        if !r.status().is_success() {
            anyhow::bail!("telegram {}: {}", r.status(), r.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Never blocks the caller; failures are logged, not raised.
    pub fn send(&self, text: String) {
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.send_now(&text).await {
                tracing::warn!(error = %e, "telegram send failed");
            }
        });
    }
}

// ---------- command bot ----------

use crate::api::Api;
use crate::engine::{keep_frac, Engine};
use crate::feed::Feed;
use crate::positions::Chain;
use crate::state::{heartbeat_age, ledger_rows, Params, State};
use crate::RunArgs;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Mutex;

/// Answers the operator's commands in the configured chat. The only action that touches positions
/// is `/liquidate`, which needs a fresh confirmation code and leaves the farm paused.
pub struct Commander {
    n: Notifier,
    long: reqwest::Client,
    api: Api,
    feed: Option<Feed>,
    state_path: PathBuf,
    ledger_path: PathBuf,
    /// Telegram user ids allowed to issue commands (PAPERBOT_TG_USERS, comma-separated). Empty = anyone in the chat.
    allow: Vec<String>,
    /// pending /liquidate confirmation: (code, expires_at, requested_by)
    pending: Mutex<Option<(String, u64, String)>>,
}

const COMMANDS: &[(&str, &str)] = &[
    ("status", "one-screen overview: farm, pair, account, 24h, protocol"),
    ("pair", "the open pair live: pnl per leg, net if closed, ETA to trigger"),
    ("stats", "all-time and 24h performance, per market, pace"),
    ("rate", "mint curve and measured decay"),
    ("cycles", "last cycles (/cycles 10)"),
    ("health", "engine heartbeat, feed, session key, responder"),
    ("params", "live farm parameters"),
    ("set", "/set move 0.4 | buffer 40 | market eth | capital 0.5 | ddstop 50 | ratefloor 8 | maxcycles 20|off"),
    ("pause", "no new pairs; the open pair finishes"),
    ("resume", "continue opening pairs"),
    ("liquidate", "close every open position and pause (asks for a code)"),
    ("help", "command list"),
];

fn money(v: f64) -> String {
    if v < 0.0 { format!("-${:.2}", -v) } else { format!("${v:.2}") }
}
fn dur(secs: u64) -> String {
    if secs >= 86_400 { format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600) }
    else if secs >= 3600 { format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60) }
    else { format!("{}m {:02}s", secs / 60, secs % 60) }
}

impl Commander {
    pub fn from_env(api: Api, feed: Option<Feed>, state_path: PathBuf, ledger_path: PathBuf) -> Option<Self> {
        // only one process may long-poll Telegram: `paperbot tg-serve` by default; the engine and the
        // GUI join in only with PAPERBOT_TG_COMMANDS=1
        if std::env::var("PAPERBOT_TG_COMMANDS").map(|v| v != "1").unwrap_or(true) {
            return None;
        }
        Some(Self {
            n: Notifier::from_env()?,
            long: reqwest::Client::builder().timeout(std::time::Duration::from_secs(40)).build().ok()?,
            api,
            feed,
            state_path,
            ledger_path,
            allow: std::env::var("PAPERBOT_TG_USERS").unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            pending: Mutex::new(None),
        })
    }

    pub fn spawn(self) {
        tokio::spawn(async move {
            let _ = self.set_commands().await;
            let mut offset: i64 = 0;
            let mut conflicts = 0u32;
            let mut last_expiry_warning = 0u64;
            loop {
                // periodic watch: session key expiry (once a day while under 3 days)
                if let Ok(st) = State::load(&self.state_path) {
                    if let Some(exp) = st.session_expiry {
                        let now = crate::sign::now_secs();
                        let left = exp.saturating_sub(now);
                        if left < 3 * 86_400 && now.saturating_sub(last_expiry_warning) > 86_400 {
                            last_expiry_warning = now;
                            let _ = self.n.send_now(&format!("⚠️ session key expires in {} — re-register from the GUI", dur(left))).await;
                        }
                    }
                }
                match self.poll(offset).await {
                    Ok(updates) => {
                        conflicts = 0;
                        for u in updates {
                            offset = u["update_id"].as_i64().unwrap_or(offset - 1) + 1;
                            let m = &u["message"];
                            let chat = m["chat"]["id"].as_i64().map(|i| i.to_string()).unwrap_or_default();
                            let Some(text) = m["text"].as_str() else { continue };
                            if chat != self.n.chat || !text.starts_with('/') {
                                continue;
                            }
                            let from = m["from"]["id"].as_i64().map(|i| i.to_string()).unwrap_or_default();
                            if !self.allow.is_empty() && !self.allow.contains(&from) {
                                tracing::info!(from, "ignored command from a non-allowlisted user");
                                continue;
                            }
                            let name = m["from"]["first_name"].as_str().unwrap_or("someone").to_string();
                            let reply = self.handle(text, &from, &name).await;
                            if let Err(e) = self.n.send_now(&reply).await {
                                tracing::warn!(error = %e, "telegram reply failed");
                            }
                        }
                    }
                    Err(e) => {
                        let s = e.to_string();
                        if s.contains("409") {
                            conflicts += 1;
                            if conflicts == 1 {
                                tracing::warn!("another process is polling telegram commands; this one backs off");
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                        } else {
                            tracing::warn!(error = %s, "telegram poll failed");
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            }
        });
    }

    async fn set_commands(&self) -> Result<()> {
        let cmds: Vec<_> = COMMANDS.iter().map(|(c, d)| json!({"command": c, "description": d})).collect();
        self.n
            .http
            .post(format!("https://api.telegram.org/bot{}/setMyCommands", self.n.token))
            .json(&json!({ "commands": cmds }))
            .send()
            .await?;
        Ok(())
    }

    async fn poll(&self, offset: i64) -> Result<Vec<serde_json::Value>> {
        let r = self
            .long
            .post(format!("https://api.telegram.org/bot{}/getUpdates", self.n.token))
            .json(&json!({ "offset": offset, "timeout": 25, "allowed_updates": ["message"] }))
            .send()
            .await?;
        if r.status().as_u16() == 409 {
            anyhow::bail!("409 conflict");
        }
        let v: serde_json::Value = r.json().await?;
        if v["ok"] != true {
            anyhow::bail!("getUpdates: {v}");
        }
        Ok(v["result"].as_array().cloned().unwrap_or_default())
    }

    /// Public so the CLI can print a reply without Telegram (`paperbot tg-reply status`).
    pub async fn handle(&self, text: &str, from: &str, name: &str) -> String {
        let mut it = text.split_whitespace();
        let cmd = it.next().unwrap_or("").trim_start_matches('/').split('@').next().unwrap_or("").to_lowercase();
        let arg = it.next();
        let arg2 = it.next();
        match cmd.as_str() {
            "status" => self.status().await,
            "pair" | "position" => self.pair().await,
            "stats" => self.stats(),
            "rate" => self.rate().await,
            "cycles" => self.cycles(arg.and_then(|a| a.parse().ok()).unwrap_or(5)),
            "health" => self.health().await,
            "params" => format!("🔧 <b>params</b>\n{}\nchanges apply from the next pair", self.params().summary()),
            "set" => match (arg, arg2) {
                (Some(k), Some(v)) => {
                    let mut p = self.params();
                    // a move that can't size a $10k-notional pair on the current balance is refused up front
                    if let (Ok(st), Ok(ts)) = (State::load(&self.state_path), self.api.trading_state().await) {
                        if let Some(u) = st.user {
                            if let Ok(a) = self.api.account_row(u).await {
                                let mut q = p.clone();
                                if q.set(&k.to_lowercase(), v).is_ok() {
                                    let per_leg = (a.balance_raw as f64 / 1e18 + st.in_flight.as_ref().map(|f| 2.0 * f.margin_raw as f64 / 1e18).unwrap_or(0.0)) * q.capital_fraction / 2.0;
                                    let lev = ((1.0 / (q.move_pct / 100.0 * (1.0 + q.buffer_pct / 100.0))).round()).clamp(1.0, 1000.0);
                                    let min_notional = ts.min_open_notional_raw as f64 / 1e18;
                                    if per_leg.floor() * lev < min_notional {
                                        let max_move = 1.0 / ((min_notional / per_leg.floor()).ceil() * (1.0 + q.buffer_pct / 100.0)) * 100.0;
                                        return format!("❌ {k} = {v} can't open a pair: ${:.0}/leg × {lev:.0}× = ${:.0} notional, minimum is ${min_notional:.0}\nwith this balance the move must be ≤ {max_move:.2}% at a {:.0}% buffer (or add funds)", per_leg.floor(), per_leg.floor() * lev, q.buffer_pct);
                                    }
                                }
                            }
                        }
                    }
                    match p.set(&k.to_lowercase(), v) {
                        Ok(sum) => match p.save(&Params::path(&self.state_path)) {
                            Ok(()) => format!("🔧 {name} set {k} = {v}\n{sum}\napplies from the next pair"),
                            Err(e) => format!("could not save params: {e}"),
                        },
                        Err(e) => format!("❌ {e}"),
                    }
                }
                _ => "usage: /set &lt;key&gt; &lt;value&gt; — keys: move buffer market capital ddstop ratefloor maxcycles".into(),
            },
            "pause" | "resume" => {
                let mut p = self.params();
                p.paused = cmd == "pause";
                match p.save(&Params::path(&self.state_path)) {
                    Ok(()) => if p.paused { format!("⏸ paused by {name} — the open pair (if any) finishes, no new pairs") } else { format!("▶️ resumed by {name}") },
                    Err(e) => format!("could not save params: {e}"),
                }
            }
            "liquidate" => self.liquidate(arg, from, name).await,
            _ => self.help(),
        }
    }

    fn help(&self) -> String {
        let mut s = String::from("🧻 <b>UwU bot</b>\n");
        for (c, d) in COMMANDS {
            s.push_str(&format!("/{c} — {d}\n"));
        }
        s
    }

    fn params(&self) -> Params {
        Params::load(&Params::path(&self.state_path)).unwrap_or_default()
    }
    fn state(&self) -> State {
        State::load(&self.state_path).unwrap_or_default()
    }

    /// Live valuation of the open pair off the feed: (mid, long pnl, short pnl, net if closed, keep%, move)
    fn pair_value(&self, f: &crate::state::InFlight) -> Option<(f64, f64, f64, f64, f64, f64)> {
        let mid = self.feed.as_ref().and_then(|fd| fd.mid(&f.coin)).map(|m| m.px)?;
        let n = f.margin_raw as f64 / 1e18 * f.leverage as f64;
        let legs: Vec<(bool, f64)> = if f.entries.len() == 2 && f.longs.len() == 2 {
            f.longs.iter().copied().zip(f.entries.iter().copied()).collect()
        } else {
            vec![(true, f.mid_at_open), (false, f.mid_at_open)]
        };
        let pnl = |(long, e): (bool, f64)| (mid / e - 1.0) * n * if long { 1.0 } else { -1.0 };
        let lp = legs.iter().find(|l| l.0).map(|l| pnl(*l)).unwrap_or(0.0);
        let sp = legs.iter().find(|l| !l.0).map(|l| pnl(*l)).unwrap_or(0.0);
        let (w, l) = (lp.max(sp), lp.min(sp));
        let kw = keep_frac((w / n).abs(), if f.coin == "ETH" { 483.979 } else { 814.598 });
        Some((mid, lp, sp, l + kw * w.max(0.0), kw, (mid / f.mid_at_open - 1.0).abs()))
    }

    fn farm_line(&self, st: &State, p: &Params) -> String {
        let hb = heartbeat_age(&self.state_path);
        let engine = match hb {
            Some(a) if a <= 5 => "engine alive".to_string(),
            Some(a) => format!("engine silent {}", dur(a)),
            None => "no engine running".to_string(),
        };
        let mode = if p.liquidate { "🛑 liquidating" } else if p.paused { "⏸ paused" } else if st.in_flight.is_some() { "▶️ in a cycle" } else { "▶️ running" };
        format!("{mode} · {engine}")
    }

    async fn status(&self) -> String {
        let st = self.state();
        let p = self.params();
        let mut s = String::from("🧻 <b>UwU bot</b>\n");
        s.push_str(&format!("farm: {}\n", self.farm_line(&st, &p)));
        match st.in_flight.as_ref() {
            Some(f) => {
                let age = dur(crate::sign::now_secs().saturating_sub(f.opened_at));
                let target = if f.target_move > 0.0 { f.target_move * 100.0 } else { p.move_pct };
                match self.pair_value(f) {
                    Some((_, _, _, net, _, mv)) => s.push_str(&format!("pair: {} {}× · {age} · move {:.3}% of {:.2}% · net now {}\n", f.coin, f.leverage, mv * 100.0, target, money(net))),
                    None => s.push_str(&format!("pair: {} {}× · {age}\n", f.coin, f.leverage)),
                }
            }
            None => s.push_str("pair: none\n"),
        }
        if let Some(u) = st.user {
            match self.api.account_row(u).await {
                Ok(a) => {
                    let avail = a.balance_raw as f64 / 1e18;
                    let in_use = if a.open_positions > 0 { st.in_flight.as_ref().map(|f| 2.0 * f.margin_raw as f64 / 1e18).unwrap_or(0.0) } else { 0.0 };
                    s.push_str(&format!(
                        "account: equity {} (available {} · in use {}) · queued {} · PAPER held {:.0}\n",
                        money(avail + in_use), money(avail), money(in_use), money(a.queued_raw as f64 / 1e18), a.paper_raw as f64 / 1e18
                    ))
                }
                Err(e) => s.push_str(&format!("account: {e}\n")),
            }
            let spot = self.api.hypercore_spot_usdc(u).await.unwrap_or(0.0);
            let (perp, _) = self.api.hypercore_perp_usdc(u).await.unwrap_or((0.0, 0.0));
            s.push_str(&format!("wallet: HyperCore spot {} · perp {}\n", money(spot), money(perp)));
        } else {
            s.push_str("account: no session registered\n");
        }
        let rows = ledger_rows(&self.ledger_path);
        let now = crate::sign::now_secs() as f64;
        let day: Vec<_> = rows.iter().filter(|r| r["ts"].as_f64().unwrap_or(0.0) >= now - 86_400.0).collect();
        let sum = |rs: &[&serde_json::Value]| {
            let paper: f64 = rs.iter().map(|r| r["paper_minted"].as_f64().unwrap_or(0.0)).sum();
            let spent: f64 = -rs.iter().map(|r| r["pnl_usd"].as_f64().unwrap_or(0.0)).sum::<f64>() + 0.0;
            (paper + 0.0, spent)
        };
        let (dp, ds) = sum(&day);
        let all: Vec<_> = rows.iter().collect();
        let (ap, asp) = sum(&all);
        s.push_str(&format!("24h: {} cycles · {dp:.0} PAPER · {} spent\n", day.len(), money(ds)));
        s.push_str(&format!("all-time: {} cycles · {ap:.0} PAPER · {} → {:.0} PAPER/$\n", rows.len(), money(asp), if asp > 0.0 { ap / asp } else { 0.0 }));
        if let Ok(sum) = self.api.summary().await {
            let decay = self.api.decay_24h(&sum).await.map(|d| format!(" ({:+.1}%/day)", -d * 100.0)).unwrap_or_default();
            s.push_str(&format!("protocol: rate {:.2}/${decay} · LP ${:.2}M · queue ${:.0}\n", sum.mint_rate(), sum.tracked_lp_usd / 1e6, sum.queue_usd));
        }
        if let Some(exp) = st.session_expiry {
            s.push_str(&format!("session key: {} left", dur(exp.saturating_sub(crate::sign::now_secs()))));
        }
        s
    }

    async fn pair(&self) -> String {
        let st = self.state();
        let p = self.params();
        let Some(f) = st.in_flight else { return "no pair open".into() };
        let Some((mid, lp, sp, net, kw, mv)) = self.pair_value(&f) else {
            return format!("pair open on {} {}× but no live feed in this process", f.coin, f.leverage);
        };
        let d = if f.target_move > 0.0 { f.target_move } else { p.move_pct / 100.0 };
        let bust_guard = d * (1.0 + p.buffer_pct / 100.0) - 0.0005;
        let rate = self.api.summary().await.map(|s| s.mint_rate()).unwrap_or(0.0);
        let elapsed = crate::sign::now_secs().saturating_sub(f.opened_at);
        let eta = self.feed.as_ref().and_then(|fd| fd.daily_vol(&f.coin)).map(|v| {
            let remaining = (d - mv).max(0.0);
            let days = (remaining / v).powi(2);
            format!(" · ETA ~{}", dur((days * 86_400.0) as u64))
        }).unwrap_or_default();
        let (l, _w) = (lp.min(sp), lp.max(sp));
        format!(
            "🟢 <b>{} pair</b> {}× · ${:.0}/leg · open {}\nentries {:.1} / {:.1} → mid {:.1}\nmove {:.3}% of {:.2}% (bust guard {:.3}%){eta}\nlong {} · short {}\nnet if closed now <b>{}</b> (winner keeps {:.1}%)\nPAPER if closed now <b>{:.0}</b> @ {:.1}/$\nids {}",
            f.coin, f.leverage, f.margin_raw as f64 / 1e18, dur(elapsed),
            f.entries.first().copied().unwrap_or(f.mid_at_open), f.entries.get(1).copied().unwrap_or(f.mid_at_open), mid,
            mv * 100.0, d * 100.0, bust_guard * 100.0,
            money(lp), money(sp), money(net), kw * 100.0, rate * 0.98 * (-l).max(0.0), rate,
            f.position_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
        )
    }

    fn stats(&self) -> String {
        let rows = ledger_rows(&self.ledger_path);
        if rows.is_empty() {
            return "no cycles in the ledger yet".into();
        }
        let agg = |rs: &[&serde_json::Value]| -> (usize, f64, f64, f64) {
            let paper: f64 = rs.iter().map(|r| r["paper_minted"].as_f64().unwrap_or(0.0)).sum();
            let spent: f64 = -rs.iter().map(|r| r["pnl_usd"].as_f64().unwrap_or(0.0)).sum::<f64>() + 0.0;
            let secs: f64 = rs.iter().map(|r| r["elapsed_s"].as_f64().unwrap_or(0.0)).sum();
            (rs.len(), paper, spent, secs)
        };
        let all: Vec<_> = rows.iter().collect();
        let (n, paper, spent, secs) = agg(&all);
        let now = crate::sign::now_secs() as f64;
        let day: Vec<_> = rows.iter().filter(|r| r["ts"].as_f64().unwrap_or(0.0) >= now - 86_400.0).collect();
        let (dn, dpaper, dspent, _) = agg(&day);
        let first = rows.first().and_then(|r| r["ts"].as_f64()).unwrap_or(now);
        let hours = ((now - first) / 3600.0).max(1.0 / 60.0);
        let mut s = format!(
            "📊 <b>farm stats</b>\nall-time: {n} cycles · {paper:.0} PAPER · {} spent → <b>{:.0} PAPER/$</b>\n24h: {dn} cycles · {dpaper:.0} PAPER · {} → {:.0} PAPER/$\npace: {:.0} PAPER/h over {:.1}h · ~{:.0}/day · avg cycle {}\n",
            money(spent), if spent > 0.0 { paper / spent } else { 0.0 },
            money(dspent), if dspent > 0.0 { dpaper / dspent } else { 0.0 },
            paper / hours, hours, paper / hours * 24.0, dur((secs / n as f64) as u64)
        );
        for coin in ["BTC", "ETH"] {
            let rs: Vec<_> = rows.iter().filter(|r| r["coin"] == coin).collect();
            if rs.is_empty() { continue; }
            let (cn, cp, cs, csec) = agg(&rs);
            s.push_str(&format!("{coin}: {cn} cycles · {cp:.0} PAPER · {} → {:.0}/$ · avg {}\n", money(cs), if cs > 0.0 { cp / cs } else { 0.0 }, dur((csec / cn as f64) as u64)));
        }
        let worst = rows.iter().map(|r| r["pnl_usd"].as_f64().unwrap_or(0.0)).fold(f64::MAX, f64::min);
        let best = rows.iter().map(|r| r["pnl_usd"].as_f64().unwrap_or(0.0)).fold(f64::MIN, f64::max);
        s.push_str(&format!("best / worst cycle {} / {}", money(best), money(worst)));
        s
    }

    async fn rate(&self) -> String {
        match self.api.summary().await {
            Ok(s) => {
                let decay = self.api.decay_24h(&s).await.ok();
                let half = decay.filter(|d| *d > 0.0).map(|d| format!("\nat that pace the rate halves in {:.1} days", (0.5f64).ln() / (1.0 - d).ln()));
                format!(
                    "📈 <b>mint curve</b>\nrate <b>{:.2}</b> PAPER per $1 loss{}{}\ntail HWM ${:.2}M · tracked LP ${:.2}M · queue ${:.0}\nsupply {:.3}B of 12B\neach +$1M of LP gain → −{:.2}% rate",
                    s.mint_rate(),
                    decay.map(|d| format!(" · measured {:+.1}%/day", -d * 100.0)).unwrap_or_default(),
                    half.unwrap_or_default(),
                    s.tail_progress_usd / 1e6, s.tracked_lp_usd / 1e6, s.queue_usd, s.supply / 1e9,
                    2e6 / (120e6 + s.tail_progress_usd) * 100.0
                )
            }
            Err(e) => format!("summary failed: {e}"),
        }
    }

    fn cycles(&self, n: usize) -> String {
        let rows = ledger_rows(&self.ledger_path);
        if rows.is_empty() {
            return "no cycles yet".into();
        }
        let mut s = format!("🧾 <b>last {} cycles</b>\n<code>", n.min(rows.len()));
        for r in rows.iter().rev().take(n.clamp(1, 30)) {
            s.push_str(&format!(
                "#{:<3} {:<3} {:>4}x {:>6.3}% {:>5} {:>+8.2} {:>8.0}\n",
                r["cycle"].as_u64().unwrap_or(0), r["coin"].as_str().unwrap_or("?"), r["leverage"].as_u64().unwrap_or(0),
                r["move"].as_f64().unwrap_or(0.0) * 100.0, dur(r["elapsed_s"].as_u64().unwrap_or(0)),
                r["pnl_usd"].as_f64().unwrap_or(0.0), r["paper_minted"].as_f64().unwrap_or(0.0)
            ));
        }
        s.push_str("</code>#  coin lev move time pnl PAPER");
        s
    }

    async fn health(&self) -> String {
        let st = self.state();
        let p = self.params();
        let mut s = String::from("🩺 <b>health</b>\n");
        s.push_str(&format!("farm: {}\n", self.farm_line(&st, &p)));
        for coin in ["BTC", "ETH"] {
            match self.feed.as_ref().and_then(|f| f.mid(coin)) {
                Some(m) => s.push_str(&format!("feed {coin}: {:.1} ({:.1}s ago)\n", m.px, m.age().as_secs_f64())),
                None => s.push_str(&format!("feed {coin}: none\n")),
            }
        }
        match self.api.trading_state().await {
            Ok(ts) => s.push_str(&format!("exchange: tradingPaused={} · opens gated={} · closes gated={}\n", ts.trading_paused, ts.action_paused("openPosition"), ts.action_paused("closePositions"))),
            Err(e) => s.push_str(&format!("exchange: {e}\n")),
        }
        if let Some(exp) = st.session_expiry {
            s.push_str(&format!("session key: {} left\n", dur(exp.saturating_sub(crate::sign::now_secs()))));
        }
        s.push_str(&format!("state: {} · cycles done {}", self.state_path.display(), st.cycles_done));
        s
    }

    async fn liquidate(&self, code: Option<&str>, from: &str, name: &str) -> String {
        let st = self.state();
        let Some(user) = st.user else { return "no session registered".into() };
        let open = self.api.account_row(user).await.map(|a| a.open_positions).unwrap_or(0);
        match code {
            None => {
                let code = format!("{:04}", rand::random::<u16>() % 10_000);
                let exp = crate::sign::now_secs() + 60;
                *self.pending.lock().unwrap() = Some((code.clone(), exp, from.to_string()));
                let mut s = format!("🛑 <b>liquidate?</b> requested by {name}\n");
                s.push_str(&format!("open positions: {open}\n"));
                if let Some(f) = st.in_flight.as_ref() {
                    match self.pair_value(f) {
                        Some((mid, lp, sp, net, _, _)) => s.push_str(&format!("{} {}× · mid {:.1} · long {} · short {} · net if closed {}\n", f.coin, f.leverage, mid, money(lp), money(sp), money(net))),
                        None => s.push_str(&format!("{} {}×\n", f.coin, f.leverage)),
                    }
                }
                s.push_str("this closes everything and pauses the farm.\n");
                s.push_str(&format!("confirm within 60s: <code>/liquidate {code}</code>"));
                s
            }
            Some(c) => {
                let ok = {
                    let mut g = self.pending.lock().unwrap();
                    match g.take() {
                        Some((code, exp, by)) if code == c && crate::sign::now_secs() <= exp && by == from => true,
                        Some(_) | None => false,
                    }
                };
                if !ok {
                    return "no valid confirmation — send /liquidate first, then the code within 60s (same user)".into();
                }
                // 1. pause + flag: a running engine closes its own pair and clears the flag
                let pp = Params::path(&self.state_path);
                let mut p = self.params();
                p.paused = true;
                p.liquidate = true;
                if let Err(e) = p.save(&pp) {
                    return format!("could not write params: {e}");
                }
                let _ = self.n.send_now(&format!("🛑 liquidate confirmed by {name} — pausing and closing…")).await;
                let engine_alive = heartbeat_age(&self.state_path).map(|a| a <= 5).unwrap_or(false);
                if engine_alive && open > 0 {
                    for _ in 0..20 {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        if self.api.account_row(user).await.map(|a| a.open_positions == 0).unwrap_or(false) {
                            break;
                        }
                    }
                }
                let still_open = self.api.account_row(user).await.map(|a| a.open_positions).unwrap_or(open);
                // 2. nothing (or no engine) closed them: do it from here
                let note: String;
                if still_open > 0 {
                    let (_tx, rx) = tokio::sync::watch::channel(false);
                    match (Chain::new(&crate::positions::rpc_url()), self.feed.clone()) {
                        (Ok(chain), Some(feed)) => match Engine::new(self.api.clone(), chain, feed, self.state(), RunArgs::parse_from(["run"]), self.state_path.clone(), self.ledger_path.clone(), rx) {
                            Ok(mut eng) => { note = match eng.close_all().await {
                                Ok(()) => "closed from the responder".into(),
                                Err(e) => format!("close-all failed: {e}"),
                            } }
                            Err(e) => note = format!("engine: {e}"),
                        },
                        _ => note = "no feed/rpc in this process".into(),
                    }
                } else {
                    note = if engine_alive { "closed by the engine".into() } else { "nothing was open".into() };
                }
                if let Some(mut p) = Params::load(&pp) {
                    p.liquidate = false;
                    p.paused = true;
                    let _ = p.save(&pp);
                }
                let after = self.api.account_row(user).await.ok();
                format!(
                    "🛑 <b>liquidated</b> — {note}\nopen positions now: {} · balance {}\nfarm is paused; /resume to continue",
                    after.as_ref().map(|a| a.open_positions.to_string()).unwrap_or("?".into()),
                    after.map(|a| money(a.balance_raw as f64 / 1e18)).unwrap_or("?".into())
                )
            }
        }
    }
}
