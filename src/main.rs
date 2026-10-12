use paperbot::{api, engine, feed, positions, sign, state, RunArgs, CHAIN_ID, EXCHANGE};

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::str::FromStr;


#[derive(Parser)]
#[command(name = "paperbot", about = "Hedged-pair PAPER farm on papertrade")]
struct Cli {
    #[arg(long, env = "PAPERBOT_API", default_value = "https://exchange.papertrade.xyz")]
    api: String,
    #[arg(long, env = "PAPERBOT_STATE", default_value = "state.json")]
    state: PathBuf,
    #[arg(long, env = "PAPERBOT_LEDGER", default_value = "ledger.jsonl")]
    ledger: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a session key and register it with a one-time master-key signature.
    Register {
        #[arg(long, env = "PAPERBOT_MASTER_KEY", hide_env_values = true)]
        master_key: String,
        #[arg(long, default_value_t = 30)]
        days: u64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Balance, open cycle, live mint rate, ledger totals.
    Status,
    /// Run the farm loop.
    Run(RunArgs),
    /// Emergency: close every open position in one intent.
    CloseAll,
}


#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let api = api::Api::new(&cli.api)?;
    let mut st = state::State::load(&cli.state)?;
    let exchange = Address::from_str(EXCHANGE)?;
    let domain = sign::exchange_domain(CHAIN_ID, exchange);

    match cli.cmd {
        Cmd::Register { master_key, days, dry_run } => {
            let master = PrivateKeySigner::from_str(master_key.trim()).context("master key")?;
            let session = PrivateKeySigner::random();
            let msg = sign::RegisterSessionKey {
                user: master.address(),
                key: session.address(),
                expiry: sign::now_secs() + days * 86_400,
                feeAddress: Address::ZERO,
                nonce: st.take_nonce(),
                deadline: sign::deadline(),
            };
            let signed = sign::sign(&master, &domain, &msg)?;
            tracing::info!(user = %master.address(), key = %session.address(), intent = %signed.intent_id, "registerSessionKey");
            if dry_run {
                println!("{}", serde_json::to_string_pretty(&signed.body)?);
                return Ok(());
            }
            let ack = api.post_intent(signed.action, &signed.body).await?;
            if ack.intent_id.to_lowercase() != format!("{:#x}", signed.intent_id) {
                bail!("relayer returned a different intent id: {}", ack.intent_id);
            }
            st.user = Some(master.address());
            st.session_key = Some(format!("0x{}", alloy::hex::encode(session.to_bytes())));
            st.session_expiry = Some(msg.expiry);
            st.save(&cli.state)?;
            println!("registered session key {} for {} (expires in {days}d); queue {}",
                session.address(), master.address(), ack.queue_revision);
        }
        Cmd::Status => {
            let s = api.summary().await?;
            println!("mint rate {:.2} PAPER/$ · tail HWM ${:.2}M · tracked LP ${:.2}M · queue ${:.0} · supply {:.3}B",
                s.mint_rate(), s.tail_progress_usd / 1e6, s.tracked_lp_usd / 1e6, s.queue_usd, s.supply / 1e9);
            let ts = api.trading_state().await?;
            println!("tradingPaused={} minMargin=${} minNotional=${} intake={}",
                ts.trading_paused, ts.min_trade_size_raw / 10u128.pow(18), ts.min_open_notional_raw / 10u128.pow(18), ts.intake_raw);
            if let Some(user) = st.user {
                println!("user {user:#x} · session expiry {:?} · cycles done {} · in flight {:?}",
                    st.session_expiry, st.cycles_done, st.in_flight.as_ref().map(|f| &f.position_ids));
                for t in api.account_trades(user, 5).await? {
                    println!("  {} {} {} x{} margin {} pnl {} minted {}", t.timestamp_ms, t.symbol,
                        if t.is_long { "LONG" } else { "SHORT" }, t.leverage, t.margin_raw, t.adjusted_pnl_raw, t.paper_minted_raw);
                }
            } else {
                println!("no session registered — run `paperbot register`");
            }
        }
        Cmd::Run(ref args) => {
            let args = args.clone();
            let mut eng = build_engine(api, st, &cli, domain, args)?;
            let row = eng.api.account_row(eng.user).await?;
            println!("user {:#x} · balance ${:.2} · queued ${:.2} · open positions {} · PAPER {:.0}",
                eng.user, row.balance_raw as f64 / 1e18, row.queued_raw as f64 / 1e18, row.open_positions, row.paper_raw as f64 / 1e18);
            eng.run().await?;
        }
        Cmd::CloseAll => {
            let mut eng = build_engine(api, st, &cli, domain, RunArgs::parse_from(["run"]))?;
            eng.close_all().await?;
        }
    }
    Ok(())
}

fn build_engine(api: api::Api, st: state::State, cli: &Cli, domain: alloy::sol_types::Eip712Domain, args: RunArgs) -> Result<engine::Engine> {
    let user = st.user.context("no session registered — run `paperbot register`")?;
    let key = st.session_key.clone().context("no session key in state")?;
    if let Some(exp) = st.session_expiry {
        if exp <= sign::now_secs() {
            bail!("session key expired — run `paperbot register` again");
        }
    }
    let signer = PrivateKeySigner::from_str(&key).context("session key")?;
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::warn!("ctrl-c: finishing the open cycle, then exiting");
        let _ = tx.send(true);
    });
    Ok(engine::Engine {
        api,
        chain: positions::Chain::new(positions::RPC_URL)?,
        feed: feed::Feed::spawn(),
        signer,
        user,
        domain,
        args,
        state_path: cli.state.clone(),
        ledger_path: cli.ledger.clone(),
        st,
        shutdown: rx,
    })
}
