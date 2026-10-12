//! state.json (session key, nonce, in-flight cycle) and the append-only cycle ledger.

use alloy::primitives::{Address, B256, U256};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlight {
    pub coin: String,
    pub instrument_id: u32,
    pub intent_ids: Vec<B256>,
    pub position_ids: Vec<U256>,
    /// entry price and side per position id, same order
    #[serde(default)]
    pub entries: Vec<f64>,
    #[serde(default)]
    pub longs: Vec<bool>,
    /// the move this pair closes at (fraction), fixed at open time
    #[serde(default)]
    pub target_move: f64,
    pub mid_at_open: f64,
    pub opened_at: u64,
    pub margin_raw: u128,
    pub leverage: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub user: Option<Address>,
    pub session_key: Option<String>,
    pub session_expiry: Option<u64>,
    pub nonce: Option<U256>,
    pub cycles_done: u64,
    pub in_flight: Option<InFlight>,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let s = std::fs::read_to_string(path).context("read state")?;
        Ok(serde_json::from_str(&s).context("parse state")?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
            f.sync_all()?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Client rule: one monotonic counter per user for the trading routes, random seed.
    pub fn take_nonce(&mut self) -> U256 {
        let n = self.nonce.unwrap_or_else(crate::sign::seed_nonce);
        self.nonce = Some(n + U256::from(1u64));
        n
    }
}

pub fn ledger_append(path: &Path, row: &serde_json::Value) -> Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{row}")?;
    Ok(())
}

pub fn ledger_rows(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default()
}

/// Live-tunable farm parameters — the one file Telegram `/set`, the GUI and the engine agree on.
/// The engine re-reads it at the top of every cycle; changes apply to the next pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Params {
    pub market: String,
    pub move_pct: f64,
    pub buffer_pct: f64,
    pub capital_fraction: f64,
    pub drawdown_stop_pct: f64,
    pub rate_floor: f64,
    pub max_cycles: Option<u64>,
    pub paused: bool,
    /// set by /liquidate: the engine closes its pair, clears the flag and stays paused
    pub liquidate: bool,
}

impl Default for Params {
    fn default() -> Self {
        Self { market: "auto".into(), move_pct: 0.5, buffer_pct: 25.0, capital_fraction: 1.0, drawdown_stop_pct: 60.0, rate_floor: 10.0, max_cycles: None, paused: false, liquidate: false }
    }
}

impl Params {
    pub fn path(state_path: &Path) -> PathBuf {
        state_path.with_file_name("params.json")
    }
    pub fn load(path: &Path) -> Option<Self> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?).context("write params")
    }
    /// Minimum buffer so the close lands ≥0.10% before the 5 bps bust trigger (the engine enforces it too).
    pub fn min_buffer_pct(move_pct: f64) -> f64 {
        (0.0015 / (move_pct / 100.0) * 100.0).ceil()
    }
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !matches!(self.market.as_str(), "auto" | "btc" | "eth") { return Err("market must be auto, btc or eth".into()); }
        if !(0.01..=20.0).contains(&self.move_pct) { return Err("move must be 0.01–20 (%)".into()); }
        let min_buf = Self::min_buffer_pct(self.move_pct);
        if self.buffer_pct < min_buf { return Err(format!("buffer must be ≥ {min_buf:.0}% for a {:.2}% move (bust guard)", self.move_pct)); }
        if self.buffer_pct > 1000.0 { return Err("buffer must be ≤ 1000%".into()); }
        if !(0.01..=1.0).contains(&self.capital_fraction) { return Err("capital must be 0.01–1".into()); }
        if !(1.0..=100.0).contains(&self.drawdown_stop_pct) { return Err("ddstop must be 1–100 (%)".into()); }
        if self.rate_floor < 0.0 { return Err("ratefloor must be ≥ 0".into()); }
        Ok(())
    }
    /// `/set key value` — returns the human message for the change.
    pub fn set(&mut self, key: &str, value: &str) -> std::result::Result<String, String> {
        let f = || value.parse::<f64>().map_err(|_| format!("{value:?} is not a number"));
        match key {
            "move" => self.move_pct = f()?,
            "buffer" | "buf" => self.buffer_pct = f()?,
            "market" => self.market = value.to_lowercase(),
            "capital" | "cap" => self.capital_fraction = f()?,
            "ddstop" | "dd" => self.drawdown_stop_pct = f()?,
            "ratefloor" | "floor" => self.rate_floor = f()?,
            "maxcycles" | "max" => self.max_cycles = if value == "off" { None } else { Some(value.parse().map_err(|_| "maxcycles: integer or off".to_string())?) },
            _ => return Err(format!("unknown key {key:?} — move · buffer · market · capital · ddstop · ratefloor · maxcycles")),
        }
        self.validate()?;
        Ok(self.summary())
    }
    pub fn summary(&self) -> String {
        let lev = ((1.0 / (self.move_pct / 100.0 * (1.0 + self.buffer_pct / 100.0))).round() as u32).clamp(1, 1000);
        format!(
            "{}{} · move {:.2}% · buffer {:.0}% ({lev}×) · capital ×{:.2} · dd stop {:.0}% · rate floor {:.1} · max cycles {}",
            if self.paused { "⏸ paused · " } else { "" }, self.market, self.move_pct, self.buffer_pct, self.capital_fraction,
            self.drawdown_stop_pct, self.rate_floor, self.max_cycles.map(|m| m.to_string()).unwrap_or("off".into())
        )
    }
}

/// Engine liveness: a file whose mtime the engine bumps every second while it runs.
pub fn heartbeat_path(state_path: &Path) -> PathBuf {
    state_path.with_file_name("heartbeat")
}
pub fn heartbeat_touch(state_path: &Path) {
    let _ = std::fs::write(heartbeat_path(state_path), crate::sign::now_secs().to_string());
}
/// Seconds since the engine last beat, if ever.
pub fn heartbeat_age(state_path: &Path) -> Option<u64> {
    let t: u64 = std::fs::read_to_string(heartbeat_path(state_path)).ok()?.trim().parse().ok()?;
    Some(crate::sign::now_secs().saturating_sub(t))
}
