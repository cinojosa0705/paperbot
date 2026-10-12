//! exchange.papertrade.xyz client: public reads under /query and /state, writes to /intents.

use alloy::primitives::Address;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::time::Duration;

pub const TAIL_SCALE_USD: f64 = 120e6;
pub const FLAT_RATE: f64 = 100.0;
const E18: f64 = 1e18;

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.message)
    }
}
impl std::error::Error for ApiError {}

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    base: String,
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub tail_progress_usd: f64,
    pub tracked_lp_usd: f64,
    pub queue_usd: f64,
    pub supply: f64,
    pub block: u64,
}
impl Summary {
    pub fn mint_rate(&self) -> f64 {
        FLAT_RATE * (TAIL_SCALE_USD / (TAIL_SCALE_USD + self.tail_progress_usd.max(0.0))).powi(2)
    }
}

#[derive(Debug, Clone)]
pub struct Instrument {
    pub id: u32,
    pub name: String,
    pub enabled: bool,
    pub position_multiplier: f64,
    pub max_leverage: u32,
    pub max_notional_raw: u128,
}

#[derive(Debug, Clone)]
pub struct TradingState {
    pub trading_paused: bool,
    pub min_trade_size_raw: u128,
    pub min_open_notional_raw: u128,
    pub instruments: Vec<Instrument>,
    pub intake_raw: Value,
}
impl TradingState {
    /// The intake tuple's last element lists paused actions (empty today). Treat a mention of
    /// the action as paused.
    pub fn action_paused(&self, action: &str) -> bool {
        self.intake_raw
            .as_array()
            .and_then(|a| a.last())
            .map(|v| v.to_string().contains(action))
            .unwrap_or(false)
    }
    pub fn instrument(&self, name: &str) -> Option<&Instrument> {
        self.instruments.iter().find(|i| i.name == name)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trade {
    pub timestamp_ms: u64,
    pub position_id: String,
    pub instrument_id: u32,
    pub symbol: String,
    pub is_long: bool,
    pub entry_price_raw: String,
    pub exit_price_raw: String,
    pub price_scale_raw: String,
    pub margin_raw: String,
    pub leverage: u32,
    pub kind: String,
    pub adjusted_pnl_raw: String,
    pub user_paid_fee_raw: String,
    pub paper_minted_raw: String,
    pub paper_mint_basis_raw: String,
}

/// `/state/leaderboard/accounts/{addr}` → selfRow: the indexer's live view of one account.
#[derive(Debug, Clone)]
pub struct AccountRow {
    pub balance_raw: u128,
    pub queued_raw: u128,
    pub open_positions: u32,
    pub paper_raw: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentAck {
    pub action: String,
    pub intent_id: String,
    #[serde(default)]
    pub queue_revision: Value,
}

fn raw_u128(v: &Value) -> Result<u128> {
    v.as_str()
        .ok_or_else(|| anyhow!("expected string, got {v}"))?
        .parse()
        .context("raw u128")
}
fn raw_usd(v: &Value) -> Result<f64> {
    Ok(raw_u128(v)? as f64 / E18)
}

impl Api {
    pub fn new(base: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("paperbot/0.1")
            .build()?;
        Ok(Self { http, base: base.trim_end_matches('/').to_string() })
    }

    pub async fn get_value(&self, path: &str) -> Result<Value> {
        let r = self
            .http
            .get(format!("{}{}", self.base, path))
            .header("accept", "application/json")
            .send()
            .await?;
        let status = r.status().as_u16();
        let body: Value = r.json().await.unwrap_or(Value::Null);
        if status >= 400 {
            return Err(ApiError { status, code: "http".into(), message: body.to_string() }.into());
        }
        Ok(body)
    }

    pub async fn summary(&self) -> Result<Summary> {
        let v = self.get_value("/query/protocol/summary").await?;
        Ok(Summary {
            tail_progress_usd: raw_usd(&v["paper"]["tailProgress"])?,
            tracked_lp_usd: raw_usd(&v["paper"]["trackedLp"])?,
            queue_usd: raw_usd(&v["balances"]["queue"])?,
            supply: raw_usd(&v["paper"]["supply"])?,
            block: v["source"]["block"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
        })
    }

    /// Measured mint-rate decay over the last 24h (fraction, e.g. 0.18), from the hourly supply series.
    pub async fn decay_24h(&self, now: &Summary) -> Result<f64> {
        let v = self.get_value("/query/protocol/history?interval=1h").await?;
        let sup = v["columns"]["paperSupply"].as_array().ok_or_else(|| anyhow!("no paperSupply"))?;
        let n = sup.len();
        if n < 25 {
            anyhow::bail!("not enough history");
        }
        let tail_now = FLAT_RATE * TAIL_SCALE_USD * now.tail_progress_usd / (TAIL_SCALE_USD + now.tail_progress_usd);
        let flat = (now.supply - tail_now).max(0.0);
        let s24 = sup[n - 25].as_f64().unwrap_or(0.0) - flat;
        if s24 <= 0.0 {
            anyhow::bail!("supply 24h ago below the flat region");
        }
        let h24 = TAIL_SCALE_USD * s24 / (FLAT_RATE * TAIL_SCALE_USD - s24);
        let r24 = FLAT_RATE * (TAIL_SCALE_USD / (TAIL_SCALE_USD + h24)).powi(2);
        Ok(1.0 - now.mint_rate() / r24)
    }

    pub async fn trading_state(&self) -> Result<TradingState> {
        let v = self.get_value("/state/trading").await?;
        let instruments = v["instruments"]
            .as_array()
            .ok_or_else(|| anyhow!("instruments missing"))?
            .iter()
            .filter_map(|t| {
                Some(Instrument {
                    id: t[0].as_u64()? as u32,
                    name: t[1].as_str()?.to_string(),
                    enabled: t[4].as_bool().unwrap_or(true),
                    position_multiplier: raw_usd(&t[13]).ok()?,
                    max_leverage: t[14].as_u64().unwrap_or(1000) as u32,
                    max_notional_raw: raw_u128(&t[15]).ok()?,
                })
            })
            .collect();
        Ok(TradingState {
            trading_paused: v["tradingPaused"].as_bool().unwrap_or(true),
            min_trade_size_raw: raw_u128(&v["minimumTradeSizeRaw"])?,
            min_open_notional_raw: raw_u128(&v["minimumOpenNotionalRaw"])?,
            instruments,
            intake_raw: v["intake"].clone(),
        })
    }

    /// HyperCore spot USDC for `user` (the balance a papertrade deposit is sent from / withdrawn to).
    pub async fn hypercore_spot_usdc(&self, user: Address) -> Result<f64> {
        let v: Value = self
            .http
            .post("https://api.hyperliquid.xyz/info")
            .json(&json!({"type": "spotClearinghouseState", "user": format!("{user:#x}")}))
            .send()
            .await?
            .json()
            .await?;
        Ok(v["balances"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|b| b["coin"] == "USDC")
            .and_then(|b| b["total"].as_str()?.parse().ok())
            .unwrap_or(0.0))
    }

    /// HyperCore perp account USDC (account value, withdrawable).
    pub async fn hypercore_perp_usdc(&self, user: Address) -> Result<(f64, f64)> {
        let v: Value = self
            .http
            .post("https://api.hyperliquid.xyz/info")
            .json(&json!({"type": "clearinghouseState", "user": format!("{user:#x}")}))
            .send()
            .await?
            .json()
            .await?;
        let f = |x: &Value| x.as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        Ok((f(&v["marginSummary"]["accountValue"]), f(&v["withdrawable"])))
    }

    pub async fn account_row(&self, user: Address) -> Result<AccountRow> {
        let v = self.get_value(&format!("/state/leaderboard/accounts/{user:#x}")).await?;
        let r = &v["selfRow"];
        if r.is_null() {
            return Err(anyhow!("account {user:#x} not indexed yet (no deposit?)"));
        }
        Ok(AccountRow {
            balance_raw: raw_u128(&r["currentBalance"])?,
            queued_raw: raw_u128(&r["currentQueued"]).unwrap_or(0),
            open_positions: r["currentOpenPositionCount"].as_u64().unwrap_or(0) as u32,
            paper_raw: raw_u128(&r["paperTotal"]).unwrap_or(0),
        })
    }

    /// Latest closed positions, newest first. The route only accepts its default page (or `limit=75`).
    pub async fn account_trades(&self, user: Address) -> Result<Vec<Trade>> {
        let v = self.get_value(&format!("/query/accounts/{user:#x}/trades")).await?;
        Ok(serde_json::from_value(v["items"].clone()).context("trades items")?)
    }

    /// POST /intents {action, signed}. The relayer echoes the intentId it computed; callers
    /// compare it to the locally derived one.
    pub async fn post_intent(&self, action: &str, signed: &Map<String, Value>) -> Result<IntentAck> {
        let r = self
            .http
            .post(format!("{}/intents", self.base))
            .header("accept", "application/json")
            .json(&json!({ "action": action, "signed": signed }))
            .send()
            .await?;
        let status = r.status().as_u16();
        let body: Value = r.json().await.unwrap_or(Value::Null);
        let failed = status >= 400
            || body.get("error").is_some()
            || body.get("ok").and_then(Value::as_bool) == Some(false);
        if failed {
            let code = body["code"]
                .as_str()
                .or(body["error"].as_str())
                .unwrap_or("unknown")
                .to_string();
            let message = body["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| body.to_string());
            return Err(ApiError { status, code, message }.into());
        }
        Ok(serde_json::from_value(body).context("intent ack")?)
    }
}
