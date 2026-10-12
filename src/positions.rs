//! Position ids, entries and closes straight from the Exchange's event log over JSON-RPC.
//! No ABI is published; the two topics and their data layouts were read off live logs and
//! verified against a known trade (see the plan's M4).

use alloy::primitives::{Address, B256, U256};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::str::FromStr;

pub const RPC_URL: &str = "https://rpc.hyperliquid.xyz/evm";
/// PAPERBOT_RPC (e.g. an Alchemy endpoint) or the public node.
pub fn rpc_url() -> String {
    std::env::var("PAPERBOT_RPC").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| RPC_URL.to_string())
}
/// PositionOpened(uint256 indexed positionId, address indexed user,
///   instrumentId, isLong, useDebt, entryPriceRaw, marginRaw, leverage)
pub const TOPIC_OPENED: &str = "0x88451ea1189f73b554f0bf39c09868560ed149b3d788fcc933ff66ba926827f3";
/// PositionClosed(uint256 indexed positionId, address indexed user, exitPriceRaw, …)
pub const TOPIC_CLOSED: &str = "0x87902ed2451849f465fc4d36a01d7f5c67b53af12f63ee85d2814a2d582f89bf";
const MAX_RANGE: u64 = 1000; // node limit per eth_getLogs

#[derive(Debug, Clone)]
pub struct Opened {
    pub block: u64,
    pub position_id: U256,
    pub instrument_id: u32,
    pub is_long: bool,
    pub entry_px_raw: u128,
    pub margin_raw: u128,
    pub leverage: u32,
}

#[derive(Debug, Clone)]
pub struct Closed {
    pub block: u64,
    pub position_id: U256,
    pub exit_px_raw: u128,
}

#[derive(Clone)]
pub struct Chain {
    http: reqwest::Client,
    rpc: String,
}

fn topic_addr(a: Address) -> String {
    format!("0x{:0>64}", alloy::hex::encode(a))
}
fn words(data: &str) -> Vec<U256> {
    let b = alloy::hex::decode(data.trim_start_matches("0x")).unwrap_or_default();
    b.chunks(32).map(U256::from_be_slice).collect()
}
fn hex_u64(v: &Value) -> u64 {
    v.as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0)
}

impl Chain {
    pub fn new(rpc: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build()?,
            rpc: rpc.to_string(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
        let mut delay = 1u64;
        for attempt in 1..=5 {
            let v: Value = self.http.post(&self.rpc).json(&body).send().await?.json().await?;
            match v.get("error") {
                None => return Ok(v["result"].clone()),
                Some(e) if e.to_string().to_lowercase().contains("rate limit") && attempt < 5 => {
                    tracing::debug!(attempt, "rpc rate limited; backing off {delay}s");
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                    delay *= 2;
                }
                Some(e) => return Err(anyhow!("rpc {method}: {e}")),
            }
        }
        unreachable!()
    }

    /// ERC-20 balanceOf(user), raw units.
    pub async fn erc20_balance(&self, token: &str, user: Address) -> Result<u128> {
        let data = format!("0x70a08231{}", topic_addr(user).trim_start_matches("0x"));
        let r = self.call("eth_call", json!([{"to": token, "data": data}, "latest"])).await?;
        Ok(words(r.as_str().unwrap_or("0x")).first().map(|w| w.to::<u128>()).unwrap_or(0))
    }

    pub async fn block_number(&self) -> Result<u64> {
        Ok(hex_u64(&self.call("eth_blockNumber", json!([])).await?))
    }

    async fn logs(&self, topics: Value, from: u64, to: u64) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut a = from;
        while a <= to {
            let b = (a + MAX_RANGE - 1).min(to);
            let r = self
                .call(
                    "eth_getLogs",
                    json!([{
                        "address": crate::EXCHANGE,
                        "fromBlock": format!("0x{a:x}"),
                        "toBlock": format!("0x{b:x}"),
                        "topics": topics,
                    }]),
                )
                .await?;
            out.extend(r.as_array().cloned().unwrap_or_default());
            a = b + 1;
        }
        Ok(out)
    }

    /// Every position `user` opened in [from, to].
    pub async fn opened(&self, user: Address, from: u64, to: u64) -> Result<Vec<Opened>> {
        let logs = self.logs(json!([TOPIC_OPENED, Value::Null, topic_addr(user)]), from, to).await?;
        logs.iter()
            .map(|l| {
                let w = words(l["data"].as_str().unwrap_or(""));
                if w.len() < 6 {
                    return Err(anyhow!("short PositionOpened data"));
                }
                Ok(Opened {
                    block: hex_u64(&l["blockNumber"]),
                    position_id: U256::from_str(l["topics"][1].as_str().unwrap_or("0x0"))?,
                    instrument_id: w[0].to::<u32>(),
                    is_long: w[1] == U256::from(1u8),
                    entry_px_raw: w[3].to::<u128>(),
                    margin_raw: w[4].to::<u128>(),
                    leverage: w[5].to::<u32>(),
                })
            })
            .collect()
    }

    /// Every close/bust of `user`'s positions in [from, to].
    pub async fn closed(&self, user: Address, from: u64, to: u64) -> Result<Vec<Closed>> {
        let logs = self.logs(json!([TOPIC_CLOSED, Value::Null, topic_addr(user)]), from, to).await?;
        logs.iter()
            .map(|l| {
                let w = words(l["data"].as_str().unwrap_or(""));
                Ok(Closed {
                    block: hex_u64(&l["blockNumber"]),
                    position_id: U256::from_str(l["topics"][1].as_str().unwrap_or("0x0"))?,
                    exit_px_raw: w.first().map(|x| x.to::<u128>()).unwrap_or(0),
                })
            })
            .collect()
    }
}

#[allow(dead_code)]
pub fn topic(s: &str) -> B256 {
    B256::from_str(s).unwrap()
}
