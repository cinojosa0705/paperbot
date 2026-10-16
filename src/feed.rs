//! Hyperliquid public BBO feed — the same best bid/ask the Exchange's precompile reads.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

pub const WS_URL: &str = "wss://api.hyperliquid.xyz/ws";
pub const COINS: [&str; 2] = ["BTC", "ETH"];

#[derive(Debug, Clone, Copy)]
pub struct Mid {
    pub px: f64,
    pub ts_ms: u64,
    pub recv: Instant,
}
impl Mid {
    pub fn age(&self) -> Duration {
        self.recv.elapsed()
    }
}

#[derive(Deserialize)]
struct Level {
    px: String,
}
#[derive(Deserialize)]
struct BboData {
    coin: String,
    time: u64,
    bbo: [Option<Level>; 2],
}
#[derive(Deserialize)]
struct Frame {
    channel: String,
    #[serde(default)]
    data: serde_json::Value,
}

/// One-minute price samples per coin for realised vol (last 3 h).
#[derive(Default)]
struct Samples(HashMap<String, VecDeque<(u64, f64)>>);

#[derive(Clone)]
pub struct Feed {
    mids: Arc<HashMap<String, watch::Sender<Option<Mid>>>>,
    samples: Arc<Mutex<Samples>>,
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Feed")
    }
}

impl Feed {
    pub fn spawn() -> Self {
        let mids: HashMap<String, watch::Sender<Option<Mid>>> = COINS
            .iter()
            .map(|c| (c.to_string(), watch::channel(None).0))
            .collect();
        let feed = Feed { mids: Arc::new(mids), samples: Arc::default() };
        let f = feed.clone();
        tokio::spawn(async move {
            let mut backoff = 1u64;
            loop {
                match f.run_once().await {
                    Ok(()) => backoff = 1,
                    Err(e) => tracing::warn!(error = %e, "feed disconnected; reconnecting in {backoff}s"),
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(3); // a price blackout under a 159× pair is the one thing we cannot afford
            }
        });
        feed
    }

    pub fn subscribe(&self, coin: &str) -> watch::Receiver<Option<Mid>> {
        self.mids[coin].subscribe()
    }

    pub fn mid(&self, coin: &str) -> Option<Mid> {
        *self.mids[coin].borrow()
    }

    /// Daily realised vol from 1-minute log returns, or None until there are 10 samples.
    pub fn daily_vol(&self, coin: &str) -> Option<f64> {
        let s = self.samples.lock().unwrap();
        let q = s.0.get(coin)?;
        if q.len() < 10 {
            return None;
        }
        let rets: Vec<f64> = q.iter().zip(q.iter().skip(1)).map(|(a, b)| (b.1 / a.1).ln()).collect();
        let mean = rets.iter().sum::<f64>() / rets.len() as f64;
        let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
        Some(var.sqrt() * (1440f64).sqrt())
    }

    async fn run_once(&self) -> Result<()> {
        let (ws, _) = tokio_tungstenite::connect_async(WS_URL).await?;
        let (mut tx, mut rx) = ws.split();
        for c in COINS {
            tx.send(Message::Text(
                serde_json::json!({"method":"subscribe","subscription":{"type":"bbo","coin":c}})
                    .to_string()
                    .into(),
            ))
            .await?;
        }
        let mut ping = tokio::time::interval(Duration::from_secs(50));
        ping.tick().await;
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    tx.send(Message::Text(r#"{"method":"ping"}"#.into())).await?;
                }
                msg = rx.next() => {
                    let Some(msg) = msg else { anyhow::bail!("stream ended") };
                    if let Message::Text(t) = msg? {
                        self.handle(&t);
                    }
                }
            }
        }
    }

    fn handle(&self, text: &str) {
        let Ok(f) = serde_json::from_str::<Frame>(text) else { return };
        if f.channel != "bbo" {
            return;
        }
        let Ok(d) = serde_json::from_value::<BboData>(f.data) else { return };
        let (Some(b), Some(a)) = (&d.bbo[0], &d.bbo[1]) else { return };
        let (Ok(b), Ok(a)) = (b.px.parse::<f64>(), a.px.parse::<f64>()) else { return };
        let mid = Mid { px: (a + b) / 2.0, ts_ms: d.time, recv: Instant::now() };
        if let Some(tx) = self.mids.get(&d.coin) {
            tx.send_replace(Some(mid));
        }
        let mut s = self.samples.lock().unwrap();
        let q = s.0.entry(d.coin).or_default();
        if q.back().map_or(true, |(t, _)| d.time >= t + 60_000) {
            q.push_back((d.time, mid.px));
            while q.len() > 181 {
                q.pop_front();
            }
        }
    }
}
