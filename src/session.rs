//! Session-key registration: one master-key signature, then the bot only ever holds the session key.

use crate::api::Api;
use crate::sign::{self, RegisterSessionKey};
use crate::state::State;
use alloy::primitives::{Address, B256};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::Eip712Domain;
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct Registered {
    pub user: Address,
    pub key: Address,
    pub expiry: u64,
    pub intent_id: B256,
    pub body: Map<String, Value>,
}

/// Signs `RegisterSessionKey` with the master key (read from `master_key`, never stored), posts it,
/// verifies the relayer's intentId and saves the fresh session key into `state_path`.
/// With `dry_run` nothing is posted or saved; the signed body is returned for inspection.
pub async fn register(
    api: &Api,
    master_key: &str,
    days: u64,
    state_path: &Path,
    domain: &Eip712Domain,
    dry_run: bool,
) -> Result<Registered> {
    let master = PrivateKeySigner::from_str(master_key.trim()).context("master key")?;
    let session = PrivateKeySigner::random();
    let mut st = State::load(state_path)?;
    let msg = RegisterSessionKey {
        user: master.address(),
        key: session.address(),
        expiry: sign::now_secs() + days * 86_400,
        feeAddress: Address::ZERO,
        nonce: st.take_nonce(),
        deadline: sign::deadline(),
    };
    let signed = sign::sign(&master, domain, &msg)?;
    let out = Registered { user: master.address(), key: session.address(), expiry: msg.expiry, intent_id: signed.intent_id, body: signed.body.clone() };
    if dry_run {
        return Ok(out);
    }
    let ack = api.post_intent(signed.action, &signed.body).await?;
    if ack.intent_id.to_lowercase() != format!("{:#x}", signed.intent_id) {
        bail!("relayer returned a different intent id: {}", ack.intent_id);
    }
    st.user = Some(master.address());
    st.session_key = Some(format!("0x{}", alloy::hex::encode(session.to_bytes())));
    st.session_expiry = Some(msg.expiry);
    st.save(state_path)?;
    tracing::info!(user = %out.user, key = %out.key, "session key registered");
    Ok(out)
}
