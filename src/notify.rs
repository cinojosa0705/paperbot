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

    pub async fn send_keyboard(&self, text: &str, buttons: &[(String, String)]) -> Result<()> {
        let kb: Vec<Vec<serde_json::Value>> = buttons.iter().map(|(label, data)| vec![json!({"text": label, "callback_data": data})]).collect();
        let r = self
            .http
            .post(format!("https://api.telegram.org/bot{}/sendMessage", self.token))
            .json(&json!({ "chat_id": self.chat, "text": text, "parse_mode": "HTML", "disable_web_page_preview": true,
                           "reply_markup": { "inline_keyboard": kb } }))
            .send()
            .await?;
        if !r.status().is_success() {
            anyhow::bail!("telegram {}: {}", r.status(), r.text().await.unwrap_or_default());
        }
        Ok(())
    }

    pub async fn answer_callback(&self, id: &str, text: &str) {
        let _ = self.http.post(format!("https://api.telegram.org/bot{}/answerCallbackQuery", self.token))
            .json(&json!({ "callback_query_id": id, "text": text })).send().await;
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
use std::sync::{Arc, Mutex};

/// Answers the operator's commands in the configured chat. The only action that touches positions
/// is `/liquidate`, which needs a fresh confirmation code and leaves the farm paused.
pub struct Commander {
    n: Notifier,
    long: reqwest::Client,
    api: Api,
    feed: Option<Feed>,
    state_path: PathBuf,
    ledger_path: PathBuf,
    allow: Vec<String>,
    pending: Mutex<Option<(String, u64, String)>>,
    started: std::time::Instant,
    last_rec: Mutex<Option<String>>,
}

const COMMANDS: &[(&str, &str)] = &[
    ("status", "overview: farm · pair · account · 24h · protocol"),
    ("pair", "the open pair live: pnl, net if closed, progress, ETA"),
    ("positions", "every open position, flags untracked ones"),
    ("strategy", "BTC vs ETH: keep, cost, PAPER per $, vol, cycle time"),
    ("calculate", "optimal move per market from live stats, with apply buttons"),
    ("stats", "performance: all-time, 24h, per market, pace, supply share"),
    ("rate", "mint curve, measured decay, half-life"),
    ("cycles", "last cycles table (/cycles 10)"),
    ("health", "engine heartbeat, feed, exchange gates, session key"),
    ("params", "live parameters"),
    ("set", "/set move 0.4 · buffer 40 · market eth · capital 0.5 · ddstop 50 · ratefloor 8 · maxcycles 20|off"),
    ("pause", "no new pairs; the open pair finishes"),
    ("resume", "continue opening pairs"),
    ("liquidate", "close everything and pause (asks for a code)"),
    ("help", "this list"),
];

// ---------- formatting ----------
pub fn money(v: f64) -> String {
    let neg = v < 0.0;
    let a = v.abs();
    let whole = a.trunc() as u64;
    let cents = ((a - a.trunc()) * 100.0).round() as u64;
    let (whole, cents) = if cents == 100 { (whole + 1, 0) } else { (whole, cents) };
    format!("{}${}.{:02}", if neg { "−" } else { "" }, thousands(whole), cents)
}
pub fn paper(v: f64) -> String {
    thousands(v.max(0.0).round() as u64)
}
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 { out.push(','); }
        out.push(c);
    }
    out
}
pub fn dur(secs: u64) -> String {
    if secs >= 86_400 { format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600) }
    else if secs >= 3600 { format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60) }
    else { format!("{}m {:02}s", secs / 60, secs % 60) }
}
fn bar(frac: f64, width: usize) -> String {
    let f = frac.clamp(0.0, 1.0);
    let filled = (f * width as f64).round() as usize;
    format!("{}{} {:>3.0}%", "▰".repeat(filled), "▱".repeat(width - filled), f * 100.0)
}
fn h(title: &str) -> String {
    format!("<b>{title}</b>\n")
}
fn row(k: &str, v: impl AsRef<str>) -> String {
    format!("<code>{:<13}</code>{}\n", k, v.as_ref())
}
fn signed(v: f64) -> String {
    if v >= 0.0 { format!("+{}", money(v)) } else { money(v) }
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
            started: std::time::Instant::now(),
            last_rec: Mutex::new(None),
        })
    }

    pub fn spawn(self) {
        let me = Arc::new(self);
        let wd = me.clone();
        tokio::spawn(async move { wd.watchdog().await });
        tokio::spawn(async move {
            let this = me;
            let _ = this.set_commands().await;
            this.poll_loop().await;
        });
    }

    /// If a pair is in flight and the engine's heartbeat goes quiet, alert at 45s and close at 120s.
    async fn watchdog(&self) {
        let mut alerted = false;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let st = self.state();
            let Some(f) = st.in_flight.clone() else { alerted = false; continue };
            let age = heartbeat_age(&self.state_path).unwrap_or(u64::MAX);
            if age <= 45 {
                alerted = false;
                continue;
            }
            if !alerted {
                alerted = true;
                let _ = self.n.send_now(&format!("🚨 <b>Engine not responding</b> for {} with a pair open ({} {}×, ids {}) — closing it myself at 2 minutes",
                    dur(age.min(99_999)), f.coin, f.leverage, f.position_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", "))).await;
            }
            if age > 120 {
                let (_tx, rx) = tokio::sync::watch::channel(false);
                let note = match (Chain::new(&crate::positions::rpc_url()), self.feed.clone()) {
                    (Ok(chain), Some(feed)) => match Engine::new(self.api.clone(), chain, feed, self.state(), RunArgs::parse_from(["run"]), self.state_path.clone(), self.ledger_path.clone(), rx) {
                        Ok(mut eng) => match eng.close_all().await { Ok(()) => "closed".to_string(), Err(e) => format!("close failed: {e}") },
                        Err(e) => format!("engine: {e}"),
                    },
                    _ => "no feed/rpc".into(),
                };
                let pp = Params::path(&self.state_path);
                if let Some(mut p) = Params::load(&pp) { p.paused = true; let _ = p.save(&pp); }
                let _ = self.n.send_now(&format!("🛑 <b>Watchdog</b>: engine silent {} — pair {} · farm paused. Restart the engine and /resume.", dur(age.min(99_999)), note)).await;
                alerted = false;
            }
        }
    }

    async fn poll_loop(&self) {
        {
            let mut offset: i64 = 0;
            let mut conflicts = 0u32;
            let mut last_expiry_warning = 0u64;
            let mut last_calc = 0u64;
            loop {
                // every 15 minutes: recompute the optimum and post it if it moved materially
                let now = crate::sign::now_secs();
                if now.saturating_sub(last_calc) >= 900 {
                    last_calc = now;
                    if let Some((text, buttons, sig)) = self.calculate(true).await {
                        let changed = self.last_rec.lock().unwrap().as_deref() != Some(sig.as_str());
                        if changed {
                            *self.last_rec.lock().unwrap() = Some(sig);
                            let _ = self.n.send_keyboard(&text, &buttons).await;
                        }
                    }
                }
                if let Ok(st) = State::load(&self.state_path) {
                    if let Some(exp) = st.session_expiry {
                        let now = crate::sign::now_secs();
                        let left = exp.saturating_sub(now);
                        if left < 3 * 86_400 && now.saturating_sub(last_expiry_warning) > 86_400 {
                            last_expiry_warning = now;
                            let _ = self.n.send_now(&format!("⚠️ <b>Session key expires in {}</b>\nRe-register from the GUI before it lapses.", dur(left))).await;
                        }
                    }
                }
                match self.poll(offset).await {
                    Ok(updates) => {
                        conflicts = 0;
                        for u in updates {
                            offset = u["update_id"].as_i64().unwrap_or(offset - 1) + 1;
                            if let Some(cq) = u.get("callback_query") {
                                let from = cq["from"]["id"].as_i64().map(|i| i.to_string()).unwrap_or_default();
                                let name = cq["from"]["first_name"].as_str().unwrap_or("someone").to_string();
                                let id = cq["id"].as_str().unwrap_or("").to_string();
                                if !self.allow.is_empty() && !self.allow.contains(&from) {
                                    self.n.answer_callback(&id, "not allowed").await;
                                    continue;
                                }
                                let data = cq["data"].as_str().unwrap_or("").to_string();
                                let reply = self.apply_button(&data, &name).await;
                                self.n.answer_callback(&id, "applied").await;
                                let _ = self.n.send_now(&reply).await;
                                continue;
                            }
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
                            if reply.is_empty() { continue; }
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
        }
    }

    async fn set_commands(&self) -> Result<()> {
        let cmds: Vec<_> = COMMANDS.iter().map(|(c, d)| json!({"command": c, "description": d})).collect();
        self.n.http.post(format!("https://api.telegram.org/bot{}/setMyCommands", self.n.token)).json(&json!({ "commands": cmds })).send().await?;
        Ok(())
    }

    async fn poll(&self, offset: i64) -> Result<Vec<serde_json::Value>> {
        let r = self
            .long
            .post(format!("https://api.telegram.org/bot{}/getUpdates", self.n.token))
            .json(&json!({ "offset": offset, "timeout": 25, "allowed_updates": ["message", "callback_query"] }))
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
            "pair" => self.pair().await,
            "positions" | "position" => self.positions().await,
            "strategy" => self.strategy().await,
            "calculate" | "calc" | "optimize" => match self.calculate(false).await {
                Some((text, buttons, _)) => {
                    if let Err(e) = self.n.send_keyboard(&text, &buttons).await {
                        tracing::warn!(error = %e, "keyboard send failed");
                        return text;
                    }
                    String::new()
                }
                None => "not enough data yet (feed warming up or no account)".into(),
            },
            "stats" => self.stats().await,
            "rate" => self.rate().await,
            "cycles" => self.cycles(arg.and_then(|a| a.parse().ok()).unwrap_or(6)),
            "health" => self.health().await,
            "params" => format!("{}{}\n<i>changes apply from the next pair</i>", h("🔧 Parameters"), self.params_block(&self.params())),
            "set" => match (arg, arg2) {
                (Some(k), Some(v)) => self.set(k, v, name).await,
                _ => format!("{}<code>/set key value</code>\nkeys: move · buffer · market · capital · ddstop · ratefloor · maxcycles", h("🔧 Usage")),
            },
            "pause" | "resume" => {
                let mut p = self.params();
                p.paused = cmd == "pause";
                match p.save(&Params::path(&self.state_path)) {
                    Ok(()) => if p.paused { format!("⏸ <b>Paused</b> by {name}\nThe open pair (if any) finishes; no new pairs until /resume.") } else { format!("▶️ <b>Resumed</b> by {name}") },
                    Err(e) => format!("could not save params: {e}"),
                }
            }
            "liquidate" => self.liquidate(arg, from, name).await,
            _ => self.help(),
        }
    }

    fn help(&self) -> String {
        let mut s = h("🧻 UwU bot");
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
    fn params_block(&self, p: &Params) -> String {
        let d = p.move_pct / 100.0;
        let lev = ((1.0 / (d * (1.0 + p.buffer_pct / 100.0))).round() as u32).clamp(1, 1000);
        let mut s = String::new();
        if p.paused { s.push_str("⏸ paused\n"); }
        s.push_str(&row("market", &p.market));
        s.push_str(&row("move", format!("{:.2}%  →  {lev}×, bust guard {:.3}%", p.move_pct, (d * (1.0 + p.buffer_pct / 100.0) - 0.0005) * 100.0)));
        s.push_str(&row("buffer", format!("{:.0}%", p.buffer_pct)));
        s.push_str(&row("capital", format!("×{:.2} of balance", p.capital_fraction)));
        s.push_str(&row("dd stop", format!("−{:.0}% from peak", p.drawdown_stop_pct)));
        s.push_str(&row("rate floor", format!("{:.1} PAPER/$", p.rate_floor)));
        s.push_str(&row("max cycles", p.max_cycles.map(|m| m.to_string()).unwrap_or("off".into())));
        s
    }

    async fn set(&self, k: &str, v: &str, name: &str) -> String {
        let key = k.to_lowercase();
        let mut p = self.params();
        // refuse a move that cannot size a pair on the current balance
        if let (Ok(st), Ok(ts)) = (State::load(&self.state_path), self.api.trading_state().await) {
            if let Some(u) = st.user {
                if let Ok(a) = self.api.account_row(u).await {
                    let mut q = p.clone();
                    if q.set(&key, v).is_ok() {
                        let per_leg = ((a.balance_raw as f64 / 1e18 + st.in_flight.as_ref().map(|f| 2.0 * f.margin_raw as f64 / 1e18).unwrap_or(0.0)) * q.capital_fraction / 2.0).floor();
                        let lev = ((1.0 / (q.move_pct / 100.0 * (1.0 + q.buffer_pct / 100.0))).round()).clamp(1.0, 1000.0);
                        let min_notional = ts.min_open_notional_raw as f64 / 1e18;
                        if per_leg * lev < min_notional {
                            let max_move = 1.0 / ((min_notional / per_leg).ceil() * (1.0 + q.buffer_pct / 100.0)) * 100.0;
                            return format!("{}{} × {lev:.0}× = {} notional per leg, the minimum is {}.\nWith this balance the move must be ≤ {max_move:.2}% at a {:.0}% buffer — or add funds.",
                                h("❌ Can't open a pair with that"), money(per_leg), money(per_leg * lev), money(min_notional), q.buffer_pct);
                        }
                    }
                }
            }
        }
        match p.set(&key, v) {
            Ok(_) => match p.save(&Params::path(&self.state_path)) {
                Ok(()) => format!("{}{}\n<i>set by {name} · applies from the next pair</i>", h(&format!("🔧 {key} → {v}")), self.params_block(&p)),
                Err(e) => format!("could not save params: {e}"),
            },
            Err(e) => format!("❌ {e}"),
        }
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
        let engine = match heartbeat_age(&self.state_path) {
            Some(a) if a <= 5 => "engine alive".to_string(),
            Some(a) => format!("engine silent for {}", dur(a)),
            None => "no engine running".to_string(),
        };
        let mode = if p.liquidate { "🛑 liquidating" } else if p.paused { "⏸ paused" } else if st.in_flight.is_some() { "▶️ in a cycle" } else { "▶️ running" };
        format!("{mode} · {engine}")
    }

    fn totals(&self, rows: &[serde_json::Value]) -> (usize, f64, f64, f64) {
        let paper: f64 = rows.iter().map(|r| r["paper_minted"].as_f64().unwrap_or(0.0)).sum::<f64>() + 0.0;
        let spent: f64 = -rows.iter().map(|r| r["pnl_usd"].as_f64().unwrap_or(0.0)).sum::<f64>() + 0.0;
        let secs: f64 = rows.iter().map(|r| r["elapsed_s"].as_f64().unwrap_or(0.0)).sum();
        (rows.len(), paper, spent, secs)
    }

    async fn status(&self) -> String {
        let st = self.state();
        let p = self.params();
        let mut s = h("🧻 UwU bot");
        s.push_str(&row("farm", self.farm_line(&st, &p)));
        match st.in_flight.as_ref() {
            Some(f) => {
                let target = if f.target_move > 0.0 { f.target_move } else { p.move_pct / 100.0 };
                let age = dur(crate::sign::now_secs().saturating_sub(f.opened_at));
                match self.pair_value(f) {
                    Some((_, _, _, net, _, mv)) => {
                        s.push_str(&row("pair", format!("{} {}× · {} · {age}", f.coin, f.leverage, money(f.margin_raw as f64 / 1e18 * 2.0))));
                        s.push_str(&row("progress", format!("{}  {:.3}% of {:.2}%", bar(mv / target, 12), mv * 100.0, target * 100.0)));
                        s.push_str(&row("net now", signed(net)));
                    }
                    None => s.push_str(&row("pair", format!("{} {}× · {age}", f.coin, f.leverage))),
                }
            }
            None => s.push_str(&row("pair", "none")),
        }
        if let Some(u) = st.user {
            match self.api.account_row(u).await {
                Ok(a) => {
                    let avail = a.balance_raw as f64 / 1e18;
                    let in_use = if a.open_positions > 0 { st.in_flight.as_ref().map(|f| 2.0 * f.margin_raw as f64 / 1e18).unwrap_or(0.0) } else { 0.0 };
                    s.push_str(&row("equity", format!("{} (free {} · margin {})", money(avail + in_use + a.queued_raw as f64 / 1e18), money(avail), money(in_use))));
                    if a.queued_raw > 0 { s.push_str(&row("queued", money(a.queued_raw as f64 / 1e18))); }
                    s.push_str(&row("PAPER held", paper(a.paper_raw as f64 / 1e18)));
                }
                Err(e) => s.push_str(&row("account", format!("{e}"))),
            }
        } else {
            s.push_str(&row("account", "no session registered"));
        }
        let rows = ledger_rows(&self.ledger_path);
        let now = crate::sign::now_secs() as f64;
        let day: Vec<_> = rows.iter().filter(|r| r["ts"].as_f64().unwrap_or(0.0) >= now - 86_400.0).cloned().collect();
        let (dn, dp, ds, _) = self.totals(&day);
        let (an, ap, asp, _) = self.totals(&rows);
        s.push_str(&row("24h", format!("{dn} cycles · {} PAPER · {} spent", paper(dp), money(ds))));
        s.push_str(&row("all-time", format!("{an} cycles · {} PAPER · {} → <b>{:.0}/$</b>", paper(ap), money(asp), if asp > 0.0 { ap / asp } else { 0.0 })));
        if let Ok(sum) = self.api.summary().await {
            let decay = self.api.decay_24h(&sum).await.map(|d| format!(" ({:+.1}%/day)", -d * 100.0)).unwrap_or_default();
            s.push_str(&row("mint rate", format!("{:.2} PAPER/${decay}", sum.mint_rate())));
            s.push_str(&row("protocol", format!("LP {} · queue {}", money(sum.tracked_lp_usd), money(sum.queue_usd))));
        }
        if let Some(exp) = st.session_expiry {
            s.push_str(&row("session key", format!("{} left", dur(exp.saturating_sub(crate::sign::now_secs())))));
        }
        s
    }

    async fn pair(&self) -> String {
        let st = self.state();
        let p = self.params();
        let Some(f) = st.in_flight else { return format!("{}none", h("🟢 Pair")) };
        let Some((mid, lp, sp, net, kw, mv)) = self.pair_value(&f) else {
            return format!("{}{} {}× open, but no live feed in this process", h("🟢 Pair"), f.coin, f.leverage);
        };
        let d = if f.target_move > 0.0 { f.target_move } else { p.move_pct / 100.0 };
        let bust_guard = d * (1.0 + p.buffer_pct / 100.0) - 0.0005;
        let rate = self.api.summary().await.map(|s| s.mint_rate()).unwrap_or(0.0);
        let n = f.margin_raw as f64 / 1e18 * f.leverage as f64;
        let elapsed = crate::sign::now_secs().saturating_sub(f.opened_at);
        let (l, _w) = (lp.min(sp), lp.max(sp));
        let eta = self.feed.as_ref().and_then(|fd| fd.daily_vol(&f.coin)).map(|v| {
            let remaining = (d - mv).max(0.0);
            dur(((remaining / v).powi(2) * 86_400.0) as u64)
        });
        let keep_at_target = keep_frac(d, if f.coin == "ETH" { 483.979 } else { 814.598 });
        let mut s = h(&format!("🟢 {} pair · {}× · {} per leg", f.coin, f.leverage, money(f.margin_raw as f64 / 1e18)));
        s.push_str(&row("notional", format!("{} per side", money(n))));
        s.push_str(&row("entry → mid", format!("{:.1} → {:.1}", f.mid_at_open, mid)));
        s.push_str(&row("progress", format!("{}  {:.3}% of {:.2}%", bar(mv / d, 12), mv * 100.0, d * 100.0)));
        s.push_str(&row("bust guard", format!("{:.3}% ({:.3}% away)", bust_guard * 100.0, (bust_guard - mv).max(0.0) * 100.0)));
        if let Some(e) = eta { s.push_str(&row("ETA", format!("~{e} (realised vol)"))); }
        s.push_str(&row("open for", dur(elapsed)));
        s.push_str("\n");
        s.push_str(&row("long", signed(lp)));
        s.push_str(&row("short", signed(sp)));
        s.push_str(&row("net now", format!("<b>{}</b> (winner keeps {:.1}%)", signed(net), kw * 100.0)));
        s.push_str(&row("PAPER now", format!("<b>{}</b> @ {:.1}/$", paper(rate * 0.98 * (-l).max(0.0)), rate)));
        s.push_str(&row("at target", format!("{} PAPER for {} (keeps {:.1}%)", paper(rate * 0.98 * d * n), money((1.0 - keep_at_target) * d * n), keep_at_target * 100.0)));
        s.push_str(&row("ids", f.position_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")));
        s
    }

    async fn positions(&self) -> String {
        let st = self.state();
        let Some(user) = st.user else { return "no session registered".into() };
        let count = match self.api.account_row(user).await { Ok(a) => a.open_positions, Err(e) => return format!("account: {e}") };
        let mut s = h(&format!("📂 Open positions: {count}"));
        if count == 0 {
            return s + "none";
        }
        let tracked = st.in_flight.as_ref().map(|f| f.position_ids.len() as u32).unwrap_or(0);
        if let Some(f) = st.in_flight.as_ref() {
            let mid = self.feed.as_ref().and_then(|fd| fd.mid(&f.coin)).map(|m| m.px);
            let n = f.margin_raw as f64 / 1e18 * f.leverage as f64;
            s.push_str("<code>id      side  lev   margin    notional   entry      pnl</code>\n");
            for (i, id) in f.position_ids.iter().enumerate() {
                let long = f.longs.get(i).copied().unwrap_or(i == 0);
                let entry = f.entries.get(i).copied().unwrap_or(f.mid_at_open);
                let pnl = mid.map(|m| (m / entry - 1.0) * n * if long { 1.0 } else { -1.0 });
                s.push_str(&format!(
                    "<code>{:<7} {:<5} {:>4}× {:>8} {:>10} {:>9.1} {:>8}</code>\n",
                    id, if long { "LONG" } else { "SHORT" }, f.leverage, money(f.margin_raw as f64 / 1e18), money(n), entry,
                    pnl.map(signed).unwrap_or("—".into())
                ));
            }
            s.push_str(&format!("{} · tracked by the engine · open {}\n", f.coin, dur(crate::sign::now_secs().saturating_sub(f.opened_at))));
        }
        if count > tracked {
            s.push_str(&format!("\n⚠️ <b>{} position(s) not tracked by the engine.</b> A running engine adopts them on its next loop; /liquidate closes everything.", count - tracked));
        }
        s
    }

    async fn strategy(&self) -> String {
        let p = self.params();
        let d = p.move_pct / 100.0;
        let lev = ((1.0 / (d * (1.0 + p.buffer_pct / 100.0))).round() as u32).clamp(1, 1000);
        let rate = self.api.summary().await.map(|s| s.mint_rate()).unwrap_or(0.0);
        let mut s = h(&format!("🎯 Strategy · {} · {:.2}% move · {}×", p.market, p.move_pct, lev));
        s.push_str(&row("bust guard", format!("{:.3}% · close at {:.2}%", (d * (1.0 + p.buffer_pct / 100.0) - 0.0005) * 100.0, p.move_pct)));
        s.push_str(&row("mint rate", format!("{rate:.2} PAPER/$")));
        s.push_str("\n<code>              BTC        ETH</code>\n");
        let (kb, ke) = (keep_frac(d, 814.598), keep_frac(d, 483.979));
        let vol = |c: &str| self.feed.as_ref().and_then(|f| f.daily_vol(c));
        let (vb, ve) = (vol("BTC"), vol("ETH"));
        let cyc = |v: Option<f64>| v.map(|v| (v / d).powi(2)).map(|c| format!("{c:>8.1}/d")).unwrap_or("   —     ".into());
        let eta = |v: Option<f64>| v.map(|v| dur(((d / v).powi(2) * 86_400.0) as u64)).map(|e| format!("{e:>9}")).unwrap_or("   —     ".into());
        s.push_str(&format!("<code>{:<12}{:>8.1}%  {:>8.1}%</code>\n", "keeps", kb * 100.0, ke * 100.0));
        s.push_str(&format!("<code>{:<12}{:>8.1}%  {:>8.1}%</code>\n", "cost/cycle", (1.0 - kb) * 100.0, (1.0 - ke) * 100.0));
        s.push_str(&format!("<code>{:<12}{:>9.0}  {:>9.0}</code>\n", "PAPER / $", if rate > 0.0 { 0.98 * rate / (1.0 - kb) } else { 0.0 }, if rate > 0.0 { 0.98 * rate / (1.0 - ke) } else { 0.0 }));
        s.push_str(&format!("<code>{:<12}{}  {}</code>\n", "vol (real.)", vb.map(|v| format!("{:>8.2}%", v * 100.0)).unwrap_or("   —     ".into()), ve.map(|v| format!("{:>8.2}%", v * 100.0)).unwrap_or("   —     ".into())));
        s.push_str(&format!("<code>{:<12}{}  {}</code>\n", "cycles", cyc(vb), cyc(ve)));
        s.push_str(&format!("<code>{:<12}{}  {}</code>\n", "cycle time", eta(vb), eta(ve)));
        s.push_str("\n<i>BTC is the cheaper cycle, ETH the faster one; auto prices both against the remaining capital and time every cycle.</i>");
        s
    }

    /// (text, buttons, signature). `quiet` = scheduled run.
    pub async fn calculate(&self, quiet: bool) -> Option<(String, Vec<(String, String)>, String)> {
        use crate::optimizer::{optimize, Inputs};
        let st = self.state();
        let p = self.params();
        let user = st.user?;
        let (a, sum, ts) = (self.api.account_row(user).await.ok()?, self.api.summary().await.ok()?, self.api.trading_state().await.ok()?);
        let equity = a.balance_raw as f64 / 1e18 + if a.open_positions > 0 { st.in_flight.as_ref().map(|f| 2.0 * f.margin_raw as f64 / 1e18).unwrap_or(0.0) } else { 0.0 } + a.queued_raw as f64 / 1e18;
        let decay = self.api.decay_24h(&sum).await.unwrap_or(0.15).clamp(0.0, 0.8);
        let feed = self.feed.as_ref()?;
        let vols: Vec<(&'static str, f64)> = [("BTC", 0.024), ("ETH", 0.031)].iter().map(|(c, fb)| (*c, feed.daily_vol(c).unwrap_or(*fb))).collect();
        let sampled = ["BTC", "ETH"].iter().all(|c| feed.daily_vol(c).is_some());
        let inp = Inputs {
            equity_usd: equity, capital_fraction: p.capital_fraction, dd_stop: p.drawdown_stop_pct / 100.0,
            rate: sum.mint_rate(), decay_per_day: decay,
            min_notional: ts.min_open_notional_raw as f64 / 1e18,
            max_notional: ts.instruments.iter().map(|i| i.max_notional_raw as f64 / 1e18).fold(10e6, f64::min),
            horizon_days: 7.0, max_cycles_per_day: 300.0,
        };
        let recs = optimize(&vols, &inp);
        if recs.is_empty() {
            return Some((format!("{}no valid configuration: equity {} can't reach the {} minimum notional", h("🧮 Optimizer"), money(equity), money(inp.min_notional)), vec![], "none".into()));
        }
        let mut s = h(&format!("🧮 Optimizer · 7-day horizon{}", if quiet { " · 15-min check" } else { "" }));
        s.push_str(&row("inputs", format!("equity {} · rate {:.2}/$ · decay {:.0}%/day · vol BTC {:.2}% ETH {:.2}%{}",
            money(equity), sum.mint_rate(), decay * 100.0, vols[0].1 * 100.0, vols[1].1 * 100.0, if sampled { "" } else { " (defaults)" })));
        s.push_str("<code>       move   buf   lev  cyc/d  cost  PAPER/$ PAPER/day</code>\n");
        let mut buttons = Vec::new();
        for r in &recs {
            s.push_str(&format!("<code>{:<4} {:>5.2}% {:>4.0}% {:>4}× {:>5.1} {:>5.1}% {:>7.0} {:>9}</code>\n",
                r.coin, r.move_pct, r.buffer_pct, r.leverage, r.cycles_per_day, r.cost_pct, r.paper_per_usd, paper(r.paper_per_day)));
            buttons.push((format!("Apply {} · {:.2}% / {:.0}% ({}×)", r.coin, r.move_pct, r.buffer_pct, r.leverage),
                          format!("apply:{}:{:.2}:{:.0}", r.coin.to_lowercase(), r.move_pct, r.buffer_pct)));
        }
        let best = &recs[0];
        s.push_str(&format!("\n<b>{}</b> wins over 7 days: ~{} PAPER for {} in {:.1} days of farming\n", best.coin, paper(best.paper_total), money(best.paper_total / best.paper_per_usd.max(1e-9)), best.days_used));
        s.push_str(&format!("<i>current: {} · {:.2}% / {:.0}%</i>", p.market, p.move_pct, p.buffer_pct));
        buttons.push((format!("Apply AUTO · {:.2}% / {:.0}%", best.move_pct, best.buffer_pct), format!("apply:auto:{:.2}:{:.0}", best.move_pct, best.buffer_pct)));
        let sig = recs.iter().map(|r| format!("{}:{:.1}", r.coin, (r.move_pct * 5.0).round() / 5.0)).collect::<Vec<_>>().join("|");
        Some((s, buttons, sig))
    }

    async fn apply_button(&self, data: &str, name: &str) -> String {
        let parts: Vec<&str> = data.split(':').collect();
        if parts.len() != 4 || parts[0] != "apply" {
            return "unknown button".into();
        }
        let mut p = self.params();
        for (k, v) in [("market", parts[1]), ("move", parts[2]), ("buffer", parts[3])] {
            if let Err(e) = p.set(k, v) {
                return format!("❌ {e}");
            }
        }
        match p.save(&Params::path(&self.state_path)) {
            Ok(()) => format!("{}{}\n<i>applied by {name} · from the next pair</i>", h("🔧 Optimizer params applied"), self.params_block(&p)),
            Err(e) => format!("could not save params: {e}"),
        }
    }

    async fn stats(&self) -> String {
        let rows = ledger_rows(&self.ledger_path);
        if rows.is_empty() {
            return format!("{}no cycles in the ledger yet", h("📊 Performance"));
        }
        let (n, pap, spent, secs) = self.totals(&rows);
        let now = crate::sign::now_secs() as f64;
        let day: Vec<_> = rows.iter().filter(|r| r["ts"].as_f64().unwrap_or(0.0) >= now - 86_400.0).cloned().collect();
        let (dn, dp, ds, _) = self.totals(&day);
        let first = rows.first().and_then(|r| r["ts"].as_f64()).unwrap_or(now);
        let hours = ((now - first) / 3600.0).max(1.0 / 60.0);
        let eff = if spent > 0.0 { pap / spent } else { 0.0 };
        let mut s = h("📊 Performance");
        s.push_str(&row("all-time", format!("{n} cycles · <b>{}</b> PAPER · {} spent", paper(pap), money(spent))));
        s.push_str(&row("efficiency", format!("<b>{eff:.0} PAPER/$</b> · {} per 1k PAPER", money(if pap > 0.0 { spent / pap * 1000.0 } else { 0.0 }))));
        s.push_str(&row("24h", format!("{dn} cycles · {} PAPER · {} → {:.0}/$", paper(dp), money(ds), if ds > 0.0 { dp / ds } else { 0.0 })));
        s.push_str(&row("pace", format!("{} PAPER/h · ~{}/day · avg cycle {}", paper(pap / hours), paper(pap / hours * 24.0), dur((secs / n as f64) as u64))));
        for coin in ["BTC", "ETH"] {
            let rs: Vec<_> = rows.iter().filter(|r| r["coin"] == coin).cloned().collect();
            if rs.is_empty() { continue; }
            let (cn, cp, cs, csec) = self.totals(&rs);
            s.push_str(&row(coin, format!("{cn} cycles · {} PAPER · {} → {:.0}/$ · avg {}", paper(cp), money(cs), if cs > 0.0 { cp / cs } else { 0.0 }, dur((csec / cn as f64) as u64))));
        }
        let worst = rows.iter().map(|r| r["pnl_usd"].as_f64().unwrap_or(0.0)).fold(f64::MAX, f64::min);
        let best = rows.iter().map(|r| r["pnl_usd"].as_f64().unwrap_or(0.0)).fold(f64::MIN, f64::max);
        s.push_str(&row("best / worst", format!("{} / {}", signed(best), signed(worst))));
        if let Ok(sum) = self.api.summary().await {
            let st = self.state();
            let held = if let Some(u) = st.user { self.api.account_row(u).await.map(|a| a.paper_raw as f64 / 1e18).unwrap_or(pap) } else { pap };
            let share = held / sum.supply.max(1.0);
            s.push_str("\n");
            s.push_str(&row("PAPER held", format!("{} · {:.4}% of {:.2}B supply", paper(held), share * 100.0, sum.supply / 1e9)));
            let theo = 0.98 * sum.mint_rate() / (1.0 - keep_frac(self.params().move_pct / 100.0, 814.598));
            s.push_str(&row("vs model", format!("{:.0} PAPER/$ at today's rate and move (BTC)", theo)));
        }
        s
    }

    async fn rate(&self) -> String {
        match self.api.summary().await {
            Ok(s) => {
                let decay = self.api.decay_24h(&s).await.ok();
                let mut o = h("📈 Mint curve");
                o.push_str(&row("rate", format!("<b>{:.2}</b> PAPER per $1 of loss", s.mint_rate())));
                if let Some(d) = decay {
                    o.push_str(&row("decay", format!("{:+.1}%/day measured", -d * 100.0)));
                    if d > 0.0 { o.push_str(&row("half-life", format!("{:.1} days at this pace", (0.5f64).ln() / (1.0 - d).ln()))); }
                }
                o.push_str(&row("tail HWM", money(s.tail_progress_usd)));
                o.push_str(&row("tracked LP", format!("{} · queue {}", money(s.tracked_lp_usd), money(s.queue_usd))));
                o.push_str(&row("supply", format!("{:.3}B of 12B cap ({:.1}%)", s.supply / 1e9, s.supply / 12e9 * 100.0)));
                o.push_str(&row("sensitivity", format!("−{:.2}% rate per +$1M of LP gain", 2e6 / (120e6 + s.tail_progress_usd) * 100.0)));
                o
            }
            Err(e) => format!("summary failed: {e}"),
        }
    }

    fn cycles(&self, n: usize) -> String {
        let rows = ledger_rows(&self.ledger_path);
        if rows.is_empty() {
            return format!("{}none yet", h("🧾 Cycles"));
        }
        let mut s = h(&format!("🧾 Last {} cycles", n.min(rows.len())));
        s.push_str("<code>  #  coin  lev   move    time      pnl    PAPER   /$</code>\n");
        for r in rows.iter().rev().take(n.clamp(1, 30)) {
            let pnl = r["pnl_usd"].as_f64().unwrap_or(0.0);
            let pm = r["paper_minted"].as_f64().unwrap_or(0.0);
            s.push_str(&format!(
                "<code>{:>3}  {:<4} {:>4}× {:>6.3}% {:>7} {:>8} {:>8} {:>4.0}</code>\n",
                r["cycle"].as_u64().unwrap_or(0), r["coin"].as_str().unwrap_or("?"), r["leverage"].as_u64().unwrap_or(0),
                r["move"].as_f64().unwrap_or(0.0) * 100.0, dur(r["elapsed_s"].as_u64().unwrap_or(0)),
                signed(pnl), paper(pm), if pnl < 0.0 { pm / -pnl } else { 0.0 }
            ));
        }
        s
    }

    async fn health(&self) -> String {
        let st = self.state();
        let p = self.params();
        let mut s = h("🩺 Health");
        s.push_str(&row("farm", self.farm_line(&st, &p)));
        for coin in ["BTC", "ETH"] {
            match self.feed.as_ref().and_then(|f| f.mid(coin)) {
                Some(m) => s.push_str(&row(&format!("feed {coin}"), format!("{:.1} · {:.1}s ago", m.px, m.age().as_secs_f64()))),
                None => s.push_str(&row(&format!("feed {coin}"), "none")),
            }
        }
        match self.api.trading_state().await {
            Ok(ts) => s.push_str(&row("exchange", format!("trading {} · opens {} · closes {}",
                if ts.trading_paused { "PAUSED" } else { "live" }, if ts.action_paused("openPosition") { "gated" } else { "ok" }, if ts.action_paused("closePositions") { "gated" } else { "ok" }))),
            Err(e) => s.push_str(&row("exchange", format!("{e}"))),
        }
        if let Some(exp) = st.session_expiry {
            s.push_str(&row("session key", format!("{} left", dur(exp.saturating_sub(crate::sign::now_secs())))));
        }
        s.push_str(&row("responder", format!("up {}", dur(self.started.elapsed().as_secs()))));
        s.push_str(&row("state", format!("{} · {} cycles done", self.state_path.display(), st.cycles_done)));
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
                let mut s = h(&format!("🛑 Liquidate? · requested by {name}"));
                s.push_str(&row("open", format!("{open} position(s)")));
                if let Some(f) = st.in_flight.as_ref() {
                    if let Some((mid, lp, sp, net, _, _)) = self.pair_value(f) {
                        s.push_str(&row("pair", format!("{} {}× · mid {:.1}", f.coin, f.leverage, mid)));
                        s.push_str(&row("if closed", format!("long {} · short {} · net <b>{}</b>", signed(lp), signed(sp), signed(net))));
                    }
                }
                s.push_str("\nThis closes everything and pauses the farm.\n");
                s.push_str(&format!("Confirm within 60s: <code>/liquidate {code}</code>"));
                s
            }
            Some(c) => {
                let ok = {
                    let mut g = self.pending.lock().unwrap();
                    match g.take() {
                        Some((code, exp, by)) if code == c && crate::sign::now_secs() <= exp && by == from => true,
                        _ => false,
                    }
                };
                if !ok {
                    return "No valid confirmation — send /liquidate first, then the code within 60s from the same account.".into();
                }
                let pp = Params::path(&self.state_path);
                let mut p = self.params();
                p.paused = true;
                p.liquidate = true;
                if let Err(e) = p.save(&pp) {
                    return format!("could not write params: {e}");
                }
                let _ = self.n.send_now(&format!("🛑 <b>Liquidate confirmed</b> by {name} — pausing and closing…")).await;
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
                let note: String;
                if still_open > 0 {
                    let (_tx, rx) = tokio::sync::watch::channel(false);
                    match (Chain::new(&crate::positions::rpc_url()), self.feed.clone()) {
                        (Ok(chain), Some(feed)) => match Engine::new(self.api.clone(), chain, feed, self.state(), RunArgs::parse_from(["run"]), self.state_path.clone(), self.ledger_path.clone(), rx) {
                            Ok(mut eng) => { note = match eng.close_all().await { Ok(()) => "closed from the responder".into(), Err(e) => format!("close-all failed: {e}") } }
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
                let mut s = h(&format!("🛑 Liquidated · {note}"));
                s.push_str(&row("open now", after.as_ref().map(|a| a.open_positions.to_string()).unwrap_or("?".into())));
                s.push_str(&row("balance", after.map(|a| money(a.balance_raw as f64 / 1e18)).unwrap_or("?".into())));
                s.push_str("Farm is paused — /resume to continue.");
                s
            }
        }
    }
}
