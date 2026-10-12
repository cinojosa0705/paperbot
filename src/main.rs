use alloy::primitives::Address;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use paperbot::notify::Commander;
use paperbot::{api, engine, feed, positions, session, sign, state, RunArgs, CHAIN_ID, EXCHANGE};
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
    /// Serve the Telegram commands (the one process that should poll); no trading.
    TgServe,
    /// Print what the Telegram command bot would answer (no Telegram needed).
    #[command(hide = true)]
    TgReply { cmd: String },
}

fn ctrl_c() -> tokio::sync::watch::Receiver<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::warn!("ctrl-c: finishing the open cycle, then exiting");
        let _ = tx.send(true);
    });
    rx
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let api = api::Api::new(&cli.api)?;
    let st = state::State::load(&cli.state)?;
    let domain = sign::exchange_domain(CHAIN_ID, Address::from_str(EXCHANGE)?);

    match cli.cmd {
        Cmd::Register { master_key, days, dry_run } => {
            let r = session::register(&api, &master_key, days, &cli.state, &domain, dry_run).await?;
            if dry_run {
                println!("{}", serde_json::to_string_pretty(&r.body)?);
            } else {
                println!("registered session key {} for {} (expires in {days}d)", r.key, r.user);
            }
        }
        Cmd::Status => {
            let s = api.summary().await?;
            println!(
                "mint rate {:.2} PAPER/$ · tail HWM ${:.2}M · tracked LP ${:.2}M · queue ${:.0} · supply {:.3}B",
                s.mint_rate(), s.tail_progress_usd / 1e6, s.tracked_lp_usd / 1e6, s.queue_usd, s.supply / 1e9
            );
            let ts = api.trading_state().await?;
            println!(
                "tradingPaused={} minMargin=${} minNotional=${} intake={}",
                ts.trading_paused, ts.min_trade_size_raw / 10u128.pow(18), ts.min_open_notional_raw / 10u128.pow(18), ts.intake_raw
            );
            if let Some(user) = st.user {
                println!(
                    "user {user:#x} · session expiry {:?} · cycles done {} · in flight {:?}",
                    st.session_expiry, st.cycles_done, st.in_flight.as_ref().map(|f| &f.position_ids)
                );
                for t in api.account_trades(user).await? {
                    println!(
                        "  {} {} {} x{} margin {} pnl {} minted {}",
                        t.timestamp_ms, t.symbol, if t.is_long { "LONG" } else { "SHORT" }, t.leverage, t.margin_raw, t.adjusted_pnl_raw, t.paper_minted_raw
                    );
                }
            } else {
                println!("no session registered — run `paperbot register`");
            }
        }
        Cmd::Run(args) => {
            let eng = engine::Engine::new(
                api, positions::Chain::new(&positions::rpc_url())?, feed::Feed::spawn(), st, args, cli.state.clone(), cli.ledger.clone(), ctrl_c(),
            )?;
            let row = eng.api.account_row(eng.user).await?;
            println!(
                "user {:#x} · balance ${:.2} · queued ${:.2} · open positions {} · PAPER {:.0}",
                eng.user, row.balance_raw as f64 / 1e18, row.queued_raw as f64 / 1e18, row.open_positions, row.paper_raw as f64 / 1e18
            );
            if let Some(c) = Commander::from_env(eng.api.clone(), Some(eng.feed.clone()), cli.state.clone(), cli.ledger.clone()) {
                c.spawn();
            }
            eng.run().await?;
        }
        Cmd::CloseAll => {
            let mut eng = engine::Engine::new(
                api, positions::Chain::new(&positions::rpc_url())?, feed::Feed::spawn(), st, RunArgs::parse_from(["run"]), cli.state.clone(), cli.ledger.clone(), ctrl_c(),
            )
            .context("engine")?;
            eng.close_all().await?;
        }
        Cmd::TgServe => {
            std::env::set_var("PAPERBOT_TG_COMMANDS", "1");
            let c = Commander::from_env(api, Some(feed::Feed::spawn()), cli.state.clone(), cli.ledger.clone())
                .context("PAPERBOT_TG_TOKEN / PAPERBOT_TG_CHAT not set")?;
            println!("serving Telegram commands for chat {}", std::env::var("PAPERBOT_TG_CHAT").unwrap_or_default());
            c.spawn();
            tokio::signal::ctrl_c().await?;
        }
        Cmd::TgReply { cmd } => {
            std::env::set_var("PAPERBOT_TG_COMMANDS", "1");
            std::env::set_var("PAPERBOT_TG_TOKEN", std::env::var("PAPERBOT_TG_TOKEN").unwrap_or("x".into()));
            std::env::set_var("PAPERBOT_TG_CHAT", std::env::var("PAPERBOT_TG_CHAT").unwrap_or("0".into()));
            let c = Commander::from_env(api, Some(feed::Feed::spawn()), cli.state.clone(), cli.ledger.clone()).context("commander")?;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await; // let the feed connect for /pair
            if cmd.trim_start_matches('/') == "calculate" {
                if let Some((t, b, _)) = c.calculate(false).await { println!("{t}\nbuttons: {:?}", b.iter().map(|x| &x.0).collect::<Vec<_>>()); }
            } else {
                println!("{}", c.handle(&format!("/{}", cmd.trim_start_matches('/')), "0", "cli").await);
            }
        }
    }
    Ok(())
}
