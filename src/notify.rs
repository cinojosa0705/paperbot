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

// ---------- read-only command bot ----------

use crate::api::Api;
use crate::engine::keep_frac;
use crate::feed::Feed;
use crate::state::{ledger_rows, Params, State};
use std::path::PathBuf;

/// Answers stats/info commands in the configured chat. No trading commands exist on purpose.
pub struct Commander {
    n: Notifier,
    long: reqwest::Client,
    api: Api,
    feed: Option<Feed>,
    state_path: PathBuf,
    ledger_path: PathBuf,
    /// Telegram user ids allowed to issue commands (PAPERBOT_TG_USERS, comma-separated). Empty = anyone in the chat.
    allow: Vec<String>,
}

const COMMANDS: &[(&str, &str)] = &[
    ("status", "balance, open pair, session, mint rate"),
    ("stats", "ledger totals: PAPER minted, spent, PAPER per $"),
    ("pair", "the open pair, live off the Hyperliquid mid"),
    ("rate", "mint curve state"),
    ("cycles", "last cycles (/cycles 10)"),
    ("params", "live farm parameters"),
    ("set", "change one: /set move 0.4 | buffer 40 | market eth | capital 0.5 | ddstop 50 | ratefloor 8 | maxcycles 20|off"),
    ("pause", "no new pairs; the open pair finishes"),
    ("resume", "continue opening pairs"),
    ("help", "this list"),
];

impl Commander {
    pub fn from_env(api: Api, feed: Option<Feed>, state_path: PathBuf, ledger_path: PathBuf) -> Option<Self> {
        if std::env::var("PAPERBOT_TG_COMMANDS").map(|v| v == "0").unwrap_or(false) {
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
        })
    }

    pub fn spawn(self) {
        tokio::spawn(async move {
            let _ = self.set_commands().await;
            let mut offset: i64 = 0;
            let mut conflicts = 0u32;
            loop {
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
                            let reply = self.handle(text).await;
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
    pub async fn handle(&self, text: &str) -> String {
        let mut it = text.split_whitespace();
        let cmd = it.next().unwrap_or("").trim_start_matches('/').split('@').next().unwrap_or("").to_lowercase();
        let arg = it.next();
        let arg2 = it.next();
        match cmd.as_str() {
            "params" => format!("🔧 <b>params</b>\n{}\nchanges apply from the next pair", self.params().summary()),
            "set" => match (arg, arg2) {
                (Some(k), Some(v)) => {
                    let mut p = self.params();
                    match p.set(&k.to_lowercase(), v) {
                        Ok(sum) => match p.save(&Params::path(&self.state_path)) {
                            Ok(()) => format!("🔧 set {k} = {v}\n{sum}\napplies from the next pair"),
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
                    Ok(()) => if p.paused { "⏸ paused — the open pair (if any) finishes, no new pairs".into() } else { "▶️ resumed".into() },
                    Err(e) => format!("could not save params: {e}"),
                }
            }
            "status" => self.status().await,
            "stats" => self.stats(),
            "pair" | "position" => self.pair().await,
            "rate" => self.rate().await,
            "cycles" => self.cycles(arg.and_then(|a| a.parse().ok()).unwrap_or(5)),
            _ => self.help(),
        }
    }

    fn help(&self) -> String {
        let mut s = String::from("🧻 <b>UwU bot</b> — stats and knobs, no trading commands\n");
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

    async fn status(&self) -> String {
        let st = self.state();
        let mut s = String::from("🧻 <b>UwU bot status</b>\n");
        match st.in_flight.as_ref() {
            Some(f) => s.push_str(&format!("pair open: {} {}× · ${:.0}/leg\n", f.coin, f.leverage, f.margin_raw as f64 / 1e18)),
            None => s.push_str("no pair open\n"),
        }
        if let Some(u) = st.user {
            match self.api.account_row(u).await {
                Ok(a) => s.push_str(&format!(
                    "balance ${:.2} · queued ${:.2} · open positions {} · PAPER held {:.0}\n",
                    a.balance_raw as f64 / 1e18, a.queued_raw as f64 / 1e18, a.open_positions, a.paper_raw as f64 / 1e18
                )),
                Err(e) => s.push_str(&format!("account: {e}\n")),
            }
            if let Some(exp) = st.session_expiry {
                let left = exp.saturating_sub(crate::sign::now_secs());
                s.push_str(&format!("session key: {}d {}h left\n", left / 86_400, (left % 86_400) / 3600));
            }
        } else {
            s.push_str("no session registered\n");
        }
        s.push_str(&format!("cycles done {}\n", st.cycles_done));
        if let Ok(sum) = self.api.summary().await {
            s.push_str(&format!("mint rate {:.2} /$ · LP ${:.2}M · queue ${:.0}", sum.mint_rate(), sum.tracked_lp_usd / 1e6, sum.queue_usd));
        }
        s
    }

    fn stats(&self) -> String {
        let rows = ledger_rows(&self.ledger_path);
        if rows.is_empty() {
            return "no cycles in the ledger yet".into();
        }
        let n = rows.len() as f64;
        let paper: f64 = rows.iter().map(|r| r["paper_minted"].as_f64().unwrap_or(0.0)).sum();
        let spent: f64 = -rows.iter().map(|r| r["pnl_usd"].as_f64().unwrap_or(0.0)).sum::<f64>();
        let secs: f64 = rows.iter().map(|r| r["elapsed_s"].as_f64().unwrap_or(0.0)).sum();
        let first = rows.first().and_then(|r| r["ts"].as_f64()).unwrap_or(0.0);
        let last = rows.last().and_then(|r| r["ts"].as_f64()).unwrap_or(first);
        let hours = ((last - first) / 3600.0).max(1.0 / 60.0);
        let worst = rows.iter().map(|r| r["pnl_usd"].as_f64().unwrap_or(0.0)).fold(f64::MAX, f64::min);
        let best = rows.iter().map(|r| r["pnl_usd"].as_f64().unwrap_or(0.0)).fold(f64::MIN, f64::max);
        format!(
            "📊 <b>farm stats</b>\ncycles {} · PAPER minted {:.0} · spent ${:.2}\n<b>{:.0} PAPER per $1</b>\navg cycle {:.0}s · {:.0} PAPER/h over {:.1}h\nbest/worst cycle ${:+.2} / ${:+.2}",
            rows.len(), paper, spent, if spent > 0.0 { paper / spent } else { 0.0 }, secs / n, paper / hours, hours, best, worst
        )
    }

    async fn pair(&self) -> String {
        let st = self.state();
        let Some(f) = st.in_flight else { return "no pair open".into() };
        let Some(mid) = self.feed.as_ref().and_then(|fd| fd.mid(&f.coin)).map(|m| m.px) else {
            return format!("pair open on {} {}× but no live feed in this process", f.coin, f.leverage);
        };
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
        let net = l + kw * w.max(0.0);
        let rate = self.api.summary().await.map(|s| s.mint_rate()).unwrap_or(0.0);
        let mv = (mid / f.mid_at_open - 1.0).abs();
        let elapsed = crate::sign::now_secs().saturating_sub(f.opened_at);
        format!(
            "🟢 <b>{} pair</b> {}× · ${:.0}/leg · {}m{:02}s\nentry {:.1} → mid {:.1} · move {:.3}%\nlong {:+.2} · short {:+.2}\nnet if closed now <b>{:+.2}</b> (winner keeps {:.1}%)\nPAPER if closed now <b>{:.0}</b> @ {:.1}/$\nids {}",
            f.coin, f.leverage, f.margin_raw as f64 / 1e18, elapsed / 60, elapsed % 60, f.mid_at_open, mid, mv * 100.0,
            lp, sp, net, kw * 100.0, rate * 0.98 * (-l).max(0.0), rate,
            f.position_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
        )
    }

    async fn rate(&self) -> String {
        match self.api.summary().await {
            Ok(s) => format!(
                "📈 <b>mint curve</b>\nrate {:.2} PAPER per $1 loss\ntail HWM ${:.2}M · tracked LP ${:.2}M · queue ${:.0}\nsupply {:.3}B of 12B\neach +$1M of LP gain → −{:.2}% rate",
                s.mint_rate(), s.tail_progress_usd / 1e6, s.tracked_lp_usd / 1e6, s.queue_usd, s.supply / 1e9,
                2e6 / (120e6 + s.tail_progress_usd) * 100.0
            ),
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
                "#{:<3} {:<3} {:>4}x {:>6.3}% {:>4}s {:>+8.2} {:>8.0}\n",
                r["cycle"].as_u64().unwrap_or(0), r["coin"].as_str().unwrap_or("?"), r["leverage"].as_u64().unwrap_or(0),
                r["move"].as_f64().unwrap_or(0.0) * 100.0, r["elapsed_s"].as_u64().unwrap_or(0),
                r["pnl_usd"].as_f64().unwrap_or(0.0), r["paper_minted"].as_f64().unwrap_or(0.0)
            ));
        }
        s.push_str("</code>#  coin lev move time pnl PAPER");
        s
    }
}
