pub mod api;
pub mod card;
pub mod engine;
pub mod feed;
pub mod notify;
pub mod optimizer;
pub mod positions;
pub mod session;
pub mod sign;
pub mod state;

pub use engine::RunArgs;

pub const CHAIN_ID: u64 = 999;
pub const EXCHANGE: &str = "0x6cd5661646289fb6e65ea5c032310fded797d0a2";
pub const PAPER: &str = "0xe40f17915daa230324030003a197cdaef2261c0e";
