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
