//! EIP-712 intents exactly as the papertrade client signs them.
//!
//! The `signed` object posted to `/intents` is the message's fields serialised the way the
//! client does it (uints as decimal strings, addresses lowercase, bools as bools) with the
//! signature split flat onto the same object as `v` ("27"/"28"), `r`, `s`.

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::signers::{local::PrivateKeySigner, SignerSync};
use alloy::sol_types::{eip712_domain, sol, Eip712Domain, SolStruct};
use anyhow::Result;
use serde_json::{json, Map, Value};

sol! {
    struct RegisterSessionKey {
        address user;
        address key;
        uint64 expiry;
        address feeAddress;
        uint256 nonce;
        uint64 deadline;
    }
    struct OpenPosition {
        address user;
        uint32 instrumentId;
        bool isLong;
        uint128 size;
        uint32 leverage;
        bool useDebt;
        uint256 nonce;
        uint64 deadline;
    }
    struct Close {
        address user;
        uint256[] positionIds;
        uint256 nonce;
        uint64 deadline;
    }
}

const INTENT_SALT: &str = "papertrade:intent-id:v2";

/// The Exchange domain. The name really is the literal "String" (placeholder left in the
/// contract's EIP712 constructor); it is what the shipped client signs.
pub fn exchange_domain(chain_id: u64, exchange: Address) -> Eip712Domain {
    eip712_domain! {
        name: "String",
        version: "1",
        chain_id: chain_id,
        verifying_contract: exchange,
    }
}

fn addr(a: &Address) -> Value {
    Value::String(format!("{a:#x}"))
}
fn uint<T: ToString>(v: T) -> Value {
    Value::String(v.to_string())
}

pub trait Intent: SolStruct {
    const ACTION: &'static str;
    fn fields(&self) -> Map<String, Value>;
}

impl Intent for RegisterSessionKey {
    const ACTION: &'static str = "registerSessionKey";
    fn fields(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("user".into(), addr(&self.user));
        m.insert("key".into(), addr(&self.key));
        m.insert("expiry".into(), uint(self.expiry));
        m.insert("feeAddress".into(), addr(&self.feeAddress));
        m.insert("nonce".into(), uint(self.nonce));
        m.insert("deadline".into(), uint(self.deadline));
        m
    }
}

impl Intent for OpenPosition {
    const ACTION: &'static str = "openPosition";
    fn fields(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("user".into(), addr(&self.user));
        m.insert("instrumentId".into(), uint(self.instrumentId));
        m.insert("isLong".into(), json!(self.isLong));
        m.insert("size".into(), uint(self.size));
        m.insert("leverage".into(), uint(self.leverage));
        m.insert("useDebt".into(), json!(self.useDebt));
        m.insert("nonce".into(), uint(self.nonce));
        m.insert("deadline".into(), uint(self.deadline));
        m
    }
}

impl Intent for Close {
    const ACTION: &'static str = "closePositions";
    fn fields(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("user".into(), addr(&self.user));
        m.insert(
            "positionIds".into(),
            Value::Array(self.positionIds.iter().map(uint).collect()),
        );
        m.insert("nonce".into(), uint(self.nonce));
        m.insert("deadline".into(), uint(self.deadline));
        m
    }
}

#[derive(Debug, Clone)]
pub struct Signed {
    pub action: &'static str,
    pub body: Map<String, Value>,
    pub digest: B256,
    pub signer: Address,
    pub intent_id: B256,
}

pub fn sign<T: Intent>(signer: &PrivateKeySigner, domain: &Eip712Domain, msg: &T) -> Result<Signed> {
    let digest = msg.eip712_signing_hash(domain);
    let sig = signer.sign_hash_sync(&digest)?;
    let recovered = sig.recover_address_from_prehash(&digest)?;
    let mut body = msg.fields();
    body.insert("v".into(), uint(27u8 + sig.v() as u8));
    body.insert("r".into(), Value::String(format!("0x{:064x}", sig.r())));
    body.insert("s".into(), Value::String(format!("0x{:064x}", sig.s())));
    Ok(Signed {
        action: T::ACTION,
        body,
        digest,
        signer: recovered,
        intent_id: intent_id(&digest, &recovered),
    })
}

/// keccak256( keccak256("papertrade:intent-id:v2") ‖ digest ‖ signer )
pub fn intent_id(digest: &B256, signer: &Address) -> B256 {
    let mut buf = Vec::with_capacity(84);
    buf.extend_from_slice(keccak256(INTENT_SALT.as_bytes()).as_slice());
    buf.extend_from_slice(digest.as_slice());
    buf.extend_from_slice(signer.as_slice());
    keccak256(&buf)
}

/// The client seeds each nonce counter from 3 random bytes × 256 and then increments.
pub fn seed_nonce() -> U256 {
    let b: [u8; 3] = rand::random();
    let n = ((b[0] as u64) << 16 | (b[1] as u64) << 8 | b[2] as u64).max(1) * 256;
    U256::from(n)
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Client rule: deadline = now + 1h.
pub fn deadline() -> u64 {
    now_secs() + 3600
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const EXCHANGE: &str = "0x6cd5661646289fb6e65ea5c032310fded797d0a2";
    // private key 0x…01 → well-known address
    const KEY1: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";
    const ADDR1: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";

    fn fixed_open() -> OpenPosition {
        OpenPosition {
            user: Address::from_str(ADDR1).unwrap(),
            instrumentId: 0,
            isLong: true,
            size: 8_000_000_000_000_000_000_000u128,
            leverage: 160,
            useDebt: false,
            nonce: U256::from(12345u64),
            deadline: 1_800_000_000,
        }
    }

    #[test]
    fn signed_body_shape_and_recovery() {
        let signer = PrivateKeySigner::from_str(KEY1).unwrap();
        let domain = exchange_domain(999, Address::from_str(EXCHANGE).unwrap());
        let s = sign(&signer, &domain, &fixed_open()).unwrap();
        assert_eq!(s.signer, Address::from_str(ADDR1).unwrap());
        assert_eq!(s.action, "openPosition");
        assert_eq!(s.body["user"], "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf");
        assert_eq!(s.body["size"], "8000000000000000000000");
        assert_eq!(s.body["leverage"], "160");
        assert_eq!(s.body["isLong"], true);
        assert_eq!(s.body["nonce"], "12345");
        assert!(matches!(s.body["v"].as_str(), Some("27") | Some("28")));
        assert_eq!(s.body["r"].as_str().unwrap().len(), 66);
        assert_eq!(s.body["s"].as_str().unwrap().len(), 66);
        assert_eq!(s.intent_id, intent_id(&s.digest, &s.signer));
    }

    #[test]
    fn close_serialises_ids_as_decimal_strings() {
        let c = Close {
            user: Address::from_str(ADDR1).unwrap(),
            positionIds: vec![U256::from(221572u64), U256::from(221573u64)],
            nonce: U256::from(1u64),
            deadline: 1,
        };
        assert_eq!(c.fields()["positionIds"], json!(["221572", "221573"]));
    }

    // Golden vector from viem.hashTypedData / signTypedData with KEY1 — filled in once computed.
    include!("sign_golden.rs");
}
