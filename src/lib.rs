pub mod api;
pub mod engine;
pub mod feed;
pub mod notify;
pub mod positions;
pub mod session;
pub mod sign;
pub mod state;

pub use engine::RunArgs;

pub const CHAIN_ID: u64 = 999;
pub const EXCHANGE: &str = "0x6cd5661646289fb6e65ea5c032310fded797d0a2";
/// USDC on HyperEVM (6 decimals) — papertrade's collateral token.
pub const USDC: &str = "0x6b9e773128f453f5c2c60935ee2de2cbc5390a24";
