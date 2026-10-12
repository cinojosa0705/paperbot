//! state.json (session key, nonce, in-flight cycle) and the append-only cycle ledger.

use alloy::primitives::{Address, B256, U256};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

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
