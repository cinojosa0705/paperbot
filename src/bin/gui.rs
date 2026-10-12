//! paperbot GUI — the farm's control surface in papertrade's uwu dress.
//! Reads the same sources the bot does; can register a session key, start/stop the farm and
//! close everything. Deposits and withdrawals stay in the papertrade app (wallet signature).

use alloy::primitives::Address;
use clap::Parser;
use iced::widget::{button, canvas, column, container, image, progress_bar, row, scrollable, stack, svg, text, text_input, Space};
use iced::{mouse, Alignment, Background, Border, Color, Element, Font, Length, Rectangle, Renderer, Subscription, Task, Theme};
use paperbot::api::{AccountRow, Api, Summary};
use paperbot::engine::{keep_frac, Engine};
use paperbot::feed::Feed;
use paperbot::notify::Notifier;
use paperbot::positions::Chain;
use paperbot::state::{Params, State};
use paperbot::{session, sign, RunArgs, CHAIN_ID, EXCHANGE};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

// ---------- brand ----------
const fn c(r: u8, g: u8, b: u8) -> Color {
    Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
}
const BG: Color = c(0x11, 0x0e, 0x1c);
const PANEL: Color = c(0x1a, 0x16, 0x2b);
const PANEL2: Color = c(0x22, 0x1d, 0x38);
const HEAD: Color = c(0x1f, 0x1a, 0x33);
const LINE: Color = c(0x2f, 0x28, 0x50);
const FG: Color = c(0xec, 0xe9, 0xf7);
const FG2: Color = c(0xa6, 0x9e, 0xc6);
const MUTED: Color = c(0x7a, 0x72, 0x99);
const PINK: Color = c(0xf2, 0xa0, 0xcf);
const PINK_D: Color = c(0xd4, 0x58, 0x8f);
const LILAC: Color = c(0xb4, 0xa4, 0xf7);
const MINT: Color = c(0x7f, 0xe3, 0xb2);
const RED: Color = c(0xf2, 0x71, 0x8f);
const ETH_BLUE: Color = c(0x39, 0x87, 0xe5);

const SANS: Font = Font::with_name("Archivo");
const SANS_BOLD: Font = Font { weight: iced::font::Weight::Semibold, ..Font::with_name("Archivo") };
const MONO: Font = Font::with_name("IBM Plex Mono");
const MONO_MED: Font = Font { weight: iced::font::Weight::Medium, ..Font::with_name("IBM Plex Mono") };

const GIRL: &[u8] = include_bytes!("../../assets/uwu-resting-girl.png");
const STARS: &[u8] = include_bytes!("../../assets/uwu-stars-v1-D3Ov5TkS.jpg");
const ROOM: &[u8] = include_bytes!("../../assets/uwu-chart-room.png");
const BTC_SVG: &[u8] = include_bytes!("../../assets/btc-holo.svg");
const ETH_SVG: &[u8] = include_bytes!("../../assets/eth-holo.svg");
const E18: f64 = 1e18;

#[derive(Parser)]
#[command(name = "paperbot-gui")]
struct Cli {
    #[arg(long, env = "PAPERBOT_API", default_value = "https://exchange.papertrade.xyz")]
    api: String,
    #[arg(long, env = "PAPERBOT_STATE", default_value = "state.json")]
    state: PathBuf,
    #[arg(long, env = "PAPERBOT_LEDGER", default_value = "ledger.jsonl")]
    ledger: PathBuf,
    #[arg(long = "move", default_value_t = 0.5)]
    move_pct: f64,
    #[arg(long, default_value_t = 25.0)]
    buffer_pct: f64,
}

// ---------- log tail (tracing → ring buffer) ----------
#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<VecDeque<String>>>);
struct LogWriter(LogBuf, Vec<u8>);
impl std::io::Write for LogWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.1.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let s = String::from_utf8_lossy(&self.1).to_string();
        let mut q = self.0 .0.lock().unwrap();
        for l in s.lines().filter(|l| !l.trim().is_empty()) {
            q.push_back(l.to_string());
            while q.len() > 200 {
                q.pop_front();
            }
        }
        self.1.clear();
        Ok(())
    }
}
impl Drop for LogWriter {
    fn drop(&mut self) {
        let _ = std::io::Write::flush(self);
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogWriter;
    fn make_writer(&'a self) -> LogWriter {
        LogWriter(self.clone(), Vec::new())
    }
}

fn main() -> iced::Result {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let logs = LogBuf::default();
    tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_target(false)
        .without_time()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    iced::application("paperbot", App::update, App::view)
        .subscription(App::subscription)
        .theme(App::theme)
        .font(include_bytes!("../../assets/fonts/Archivo.ttf").as_slice())
        .font(include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf").as_slice())
        .font(include_bytes!("../../assets/fonts/IBMPlexMono-Medium.ttf").as_slice())
        .default_font(SANS)
        .window_size((940.0, 720.0))
        .run_with(move || App::new(cli, logs))
}

// ---------- data ----------
#[derive(Debug, Clone, Default)]
struct Cycle {
    cycle: u64,
    coin: String,
    leverage: u64,
    margin: f64,
    mv: f64,
    elapsed_s: u64,
    pnl: f64,
    paper: f64,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    FeedReady(Feed),
    Protocol(Result<Summary, String>),
    Account(Result<AccountRow, String>),
    Balances(Result<(f64, f64), String>),
    MasterKey(String),
    ToggleRegister,
    Register,
    Registered(Result<String, String>),
    Market(&'static str),
    MoveS(String),
    BufS(String),
    CapS(String),
    DdS(String),
    ApplyParams,
    StartBot,
    StopBot,
    CloseAll,
    BotDone(Result<(), String>),
}

enum Bot {
    Idle,
    Running(watch::Sender<bool>),
    Stopping,
}

struct App {
    api: Api,
    chain: Chain,
    state_path: PathBuf,
    ledger_path: PathBuf,
    feed: Option<Feed>,
    user: Option<Address>,
    st: State,
    cycles: Vec<Cycle>,
    summary: Option<Summary>,
    account: Option<AccountRow>,
    evm_usdc: Option<f64>,
    core_usdc: Option<f64>,
    mids: VecDeque<(f64, f64)>,
    chart_coin: &'static str,
    started: Instant,
    ticks: u64,
    err: Option<String>,
    // register
    master_key: String,
    show_register: bool,
    registering: bool,
    register_note: Option<(String, bool)>,
    // controls
    market: &'static str,
    move_s: String,
    buf_s: String,
    cap_s: String,
    dd_s: String,
    bot: Bot,
    bot_note: Option<(String, bool)>,
    live_params: Option<Params>,
    logs: LogBuf,
    girl: image::Handle,
    stars: image::Handle,
    room: image::Handle,
    btc: svg::Handle,
    eth: svg::Handle,
}

impl App {
    fn new(cli: Cli, logs: LogBuf) -> (Self, Task<Message>) {
        let app = App {
            api: Api::new(&cli.api).expect("api client"),
            chain: Chain::new(&paperbot::positions::rpc_url()).expect("rpc client"),
            state_path: cli.state,
            ledger_path: cli.ledger,
            feed: None,
            user: None,
            st: State::default(),
            cycles: vec![],
            summary: None,
            account: None,
            evm_usdc: None,
            core_usdc: None,
            mids: VecDeque::with_capacity(400),
            chart_coin: "BTC",
            started: Instant::now(),
            ticks: 0,
            err: None,
            master_key: String::new(),
            show_register: false,
            registering: false,
            register_note: None,
            market: "auto",
            move_s: format!("{}", cli.move_pct),
            buf_s: format!("{}", cli.buffer_pct),
            cap_s: "1.0".into(),
            dd_s: "60".into(),
            bot: Bot::Idle,
            bot_note: None,
            live_params: None,
            logs,
            girl: image::Handle::from_bytes(GIRL),
            stars: image::Handle::from_bytes(STARS),
            room: image::Handle::from_bytes(ROOM),
            btc: svg::Handle::from_memory(BTC_SVG),
            eth: svg::Handle::from_memory(ETH_SVG),
        };
        let (api, sp, lp) = (app.api.clone(), app.state_path.clone(), app.ledger_path.clone());
        (app, Task::perform(async move {
            let f = Feed::spawn();
            if let Some(c) = paperbot::notify::Commander::from_env(api, Some(f.clone()), sp, lp) {
                c.spawn();
            }
            f
        }, Message::FeedReady))
    }

    fn theme(&self) -> Theme {
        Theme::custom("uwu".into(), iced::theme::Palette { background: BG, text: FG, primary: PINK_D, success: MINT, danger: RED })
    }

    fn subscription(&self) -> Subscription<Message> {
        iced::time::every(Duration::from_millis(1000)).map(|_| Message::Tick)
    }

    fn running(&self) -> bool {
        !matches!(self.bot, Bot::Idle)
    }

    fn reload_local(&mut self) {
        if let Ok(st) = State::load(&self.state_path) {
            self.user = st.user;
            self.st = st;
        }
        self.live_params = Params::load(&Params::path(&self.state_path));
        if let Ok(txt) = std::fs::read_to_string(&self.ledger_path) {
            self.cycles = txt
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .map(|v| Cycle {
                    cycle: v["cycle"].as_u64().unwrap_or(0),
                    coin: v["coin"].as_str().unwrap_or("?").to_string(),
                    leverage: v["leverage"].as_u64().unwrap_or(0),
                    margin: v["margin_usd"].as_f64().unwrap_or(0.0),
                    mv: v["move"].as_f64().unwrap_or(0.0),
                    elapsed_s: v["elapsed_s"].as_u64().unwrap_or(0),
                    pnl: v["pnl_usd"].as_f64().unwrap_or(0.0),
                    paper: v["paper_minted"].as_f64().unwrap_or(0.0),
                })
                .collect();
        }
    }

    fn params_from_inputs(&self) -> Result<Params, String> {
        let a = self.run_args()?;
        let mut p = Engine::params_from_args(&a);
        p.paused = self.live_params.as_ref().map(|l| l.paused).unwrap_or(false);
        p.validate()?;
        Ok(p)
    }

    fn run_args(&self) -> Result<RunArgs, String> {
        let f = |s: &str, what: &str| s.trim().parse::<f64>().map_err(|_| format!("{what}: not a number"));
        let mut a = RunArgs::parse_from(["run"]);
        a.market = self.market.into();
        a.move_pct = f(&self.move_s, "move")?;
        a.buffer_pct = f(&self.buf_s, "buffer")?;
        a.capital_fraction = f(&self.cap_s, "capital fraction")?;
        a.drawdown_stop_pct = f(&self.dd_s, "drawdown stop")?;
        Ok(a)
    }

    fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::FeedReady(f) => self.feed = Some(f),
            Message::Protocol(r) => match r {
                Ok(s) => { self.summary = Some(s); self.err = None; }
                Err(e) => self.err = Some(e),
            },
            Message::Account(r) => if let Ok(a) = r { self.account = Some(a) },
            Message::Balances(r) => if let Ok((e, c)) = r { self.evm_usdc = Some(e); self.core_usdc = Some(c); },
            Message::MasterKey(s) => self.master_key = s,
            Message::ToggleRegister => { self.show_register = !self.show_register; self.register_note = None; }
            Message::Register => {
                if self.running() || self.registering || self.master_key.trim().is_empty() {
                    return Task::none();
                }
                let key = std::mem::take(&mut self.master_key);
                self.registering = true;
                let api = self.api.clone();
                let path = self.state_path.clone();
                return Task::perform(
                    async move {
                        let domain = sign::exchange_domain(CHAIN_ID, Address::from_str(EXCHANGE).unwrap());
                        session::register(&api, &key, 30, &path, &domain, false)
                            .await
                            .map(|r| format!("session key {:#x} registered for {:#x}", r.key, r.user))
                            .map_err(|e| e.to_string())
                    },
                    Message::Registered,
                );
            }
            Message::Registered(r) => {
                self.registering = false;
                match r {
                    Ok(m) => { self.register_note = Some((m, true)); self.show_register = false; }
                    Err(e) => self.register_note = Some((e, false)),
                }
                self.reload_local();
            }
            Message::Market(m) => self.market = m,
            Message::MoveS(s) => self.move_s = s,
            Message::BufS(s) => self.buf_s = s,
            Message::CapS(s) => self.cap_s = s,
            Message::DdS(s) => self.dd_s = s,
            Message::ApplyParams => {
                match self.params_from_inputs().and_then(|p| p.save(&Params::path(&self.state_path)).map(|_| p).map_err(|e| e.to_string())) {
                    Ok(p) => self.bot_note = Some((format!("params applied — next pair: {}", p.summary()), true)),
                    Err(e) => self.bot_note = Some((e, false)),
                }
            }
            Message::StartBot => {
                if self.running() { return Task::none(); }
                if let Err(e) = self.params_from_inputs().and_then(|p| p.save(&Params::path(&self.state_path)).map_err(|e| e.to_string())) {
                    self.bot_note = Some((e, false));
                    return Task::none();
                }
                let Some(feed) = self.feed.clone() else { self.bot_note = Some(("feed not ready".into(), false)); return Task::none(); };
                let args = match self.run_args() { Ok(a) => a, Err(e) => { self.bot_note = Some((e, false)); return Task::none(); } };
                let st = match State::load(&self.state_path) { Ok(s) => s, Err(e) => { self.bot_note = Some((e.to_string(), false)); return Task::none(); } };
                let (tx, rx) = watch::channel(false);
                match Engine::new(self.api.clone(), self.chain.clone(), feed, st, args, self.state_path.clone(), self.ledger_path.clone(), rx) {
                    Ok(eng) => {
                        self.bot = Bot::Running(tx);
                        self.bot_note = Some(("farm running".into(), true));
                        return Task::perform(async move { eng.run().await.map_err(|e| e.to_string()) }, Message::BotDone);
                    }
                    Err(e) => self.bot_note = Some((e.to_string(), false)),
                }
            }
            Message::StopBot => {
                if let Bot::Running(tx) = std::mem::replace(&mut self.bot, Bot::Stopping) {
                    let _ = tx.send(true);
                    self.bot_note = Some(("stopping after the open cycle…".into(), true));
                } else {
                    self.bot = Bot::Idle;
                }
            }
            Message::CloseAll => {
                if self.running() { return Task::none(); }
                let Some(feed) = self.feed.clone() else { return Task::none() };
                let st = match State::load(&self.state_path) { Ok(s) => s, Err(e) => { self.bot_note = Some((e.to_string(), false)); return Task::none(); } };
                let (_tx, rx) = watch::channel(false);
                match Engine::new(self.api.clone(), self.chain.clone(), feed, st, RunArgs::parse_from(["run"]), self.state_path.clone(), self.ledger_path.clone(), rx) {
                    Ok(mut eng) => {
                        self.bot = Bot::Stopping;
                        self.bot_note = Some(("closing all positions…".into(), true));
                        return Task::perform(async move { eng.close_all().await.map_err(|e| e.to_string()) }, Message::BotDone);
                    }
                    Err(e) => self.bot_note = Some((e.to_string(), false)),
                }
            }
            Message::BotDone(r) => {
                self.bot = Bot::Idle;
                self.bot_note = Some(match r { Ok(()) => ("farm stopped".into(), true), Err(e) => (e, false) });
                self.reload_local();
            }
            Message::Tick => {
                self.ticks += 1;
                self.reload_local();
                let coin: &'static str = match self.st.in_flight.as_ref().map(|f| f.coin.as_str()) {
                    Some("ETH") => "ETH",
                    _ => "BTC",
                };
                if coin != self.chart_coin { self.chart_coin = coin; self.mids.clear(); }
                if let Some(m) = self.feed.as_ref().and_then(|f| f.mid(coin)) {
                    self.mids.push_back((self.started.elapsed().as_secs_f64(), m.px));
                    while self.mids.len() > 360 { self.mids.pop_front(); }
                }
                if self.ticks % 3 != 1 { return Task::none(); }
                let api = self.api.clone();
                let mut tasks = vec![Task::perform(async move { api.summary().await.map_err(|e| e.to_string()) }, Message::Protocol)];
                if let Some(u) = self.user {
                    let api = self.api.clone();
                    tasks.push(Task::perform(async move { api.account_row(u).await.map_err(|e| e.to_string()) }, Message::Account));
                    if self.ticks % 9 == 1 {
                        let api = self.api.clone();
                        tasks.push(Task::perform(
                            async move {
                                let spot = api.hypercore_spot_usdc(u).await.map_err(|e| e.to_string())?;
                                let (perp, _) = api.hypercore_perp_usdc(u).await.unwrap_or((0.0, 0.0));
                                Ok((spot, perp))
                            },
                            Message::Balances,
                        ));
                    }
                }
                return Task::batch(tasks);
            }
        }
        Task::none()
    }

    // ---------- view ----------
    fn view(&self) -> Element<'_, Message> {
        let rate = self.summary.as_ref().map(|s| s.mint_rate());
        let total_paper: f64 = self.cycles.iter().map(|c| c.paper).sum();
        let total_spent: f64 = -self.cycles.iter().map(|c| c.pnl).sum::<f64>();
        let per_dollar = if total_spent > 0.0 { total_paper / total_spent } else { 0.0 };

        let topbar = container(
            row![
                text("papertrade").font(MONO_MED).size(15).color(FG),
                text("_").font(MONO_MED).size(15).color(PINK),
                text("  FARM").font(SANS_BOLD).size(10).color(MUTED),
                Space::with_width(Length::Fill),
                chip("mint", rate.map(|r| format!("{r:.2} /$")).unwrap_or("…".into()), FG),
                chip("LP", self.summary.as_ref().map(|s| format!("${:.2}M", s.tracked_lp_usd / 1e6)).unwrap_or("…".into()), FG),
                chip("queue", self.summary.as_ref().map(|s| format!("${:.0}", s.queue_usd)).unwrap_or("…".into()), MINT),
                chip("supply", self.summary.as_ref().map(|s| format!("{:.2}B", s.supply / 1e9)).unwrap_or("…".into()), FG),
                chip("tg", if Notifier::configured() { "on".into() } else { "off".into() }, if Notifier::configured() { MINT } else { MUTED }),
                chip("bot", match self.bot { Bot::Idle => "idle".into(), Bot::Running(_) => "running".into(), Bot::Stopping => "stopping".into() },
                     match self.bot { Bot::Idle => FG2, Bot::Running(_) => MINT, Bot::Stopping => PINK }),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .width(Length::Fill);

        let hero = stack![
            image(self.stars.clone()).width(Length::Fill).height(Length::Fixed(112.0)).content_fit(iced::ContentFit::Cover).opacity(0.55_f32),
            row![
                column![
                    text("Mint $PAPER by losing on purpose").font(SANS_BOLD).size(20).color(FG),
                    text("one account · long + short · both closed in one intent · the haircut is the only cost").size(11).color(FG2),
                    Space::with_height(6),
                    row![
                        hero_stat("PAPER minted", fmt_n(total_paper)),
                        hero_stat("PAPER / $1", if per_dollar > 0.0 { format!("{per_dollar:.0}") } else { "—".into() }),
                        hero_stat("spent", format!("${total_spent:.2}")),
                        hero_stat("cycles", self.cycles.len().to_string()),
                    ]
                    .spacing(22),
                ]
                .padding([12, 16])
                .width(Length::Fill),
                image(self.girl.clone()).height(Length::Fixed(112.0)).content_fit(iced::ContentFit::Contain).opacity(0.95_f32),
            ]
            .align_y(Alignment::End),
        ];

        // ---- wallet & session ----
        let short = |a: Address| { let s = format!("{a:#x}"); format!("{}…{}", &s[..6], &s[s.len() - 4..]) };
        let in_use = self.st.in_flight.as_ref().map(|f| 2.0 * f.margin_raw as f64 / E18).unwrap_or(0.0);
        let session_line: Element<'_, Message> = match (self.st.session_key.as_deref(), self.st.session_expiry) {
            (Some(k), exp) => {
                let addr = alloy::signers::local::PrivateKeySigner::from_str(k).map(|s| short(s.address())).unwrap_or("?".into());
                let left = exp.map(|e| e.saturating_sub(sign::now_secs())).unwrap_or(0);
                let (txt, col) = if left == 0 { ("expired".to_string(), RED) } else if left < 86_400 * 3 { (format!("{}d {}h left", left / 86_400, (left % 86_400) / 3600), PINK) } else { (format!("{}d {}h left", left / 86_400, (left % 86_400) / 3600), MINT) };
                row![text("session key").size(11).color(FG2), Space::with_width(Length::Fill), text(format!("{addr} · ")).font(MONO).size(11).color(FG2), text(txt).font(MONO).size(11).color(col)].into()
            }
            _ => kv("session key", "none".into(), RED),
        };
        let mut wallet_col = column![
            kv("HyperCore spot USDC", self.evm_usdc.map(|v| format!("${v:.2}")).unwrap_or("…".into()), FG),
            kv("HyperCore perp USDC", self.core_usdc.map(|v| format!("${v:.2}")).unwrap_or("…".into()), FG),
            kv("papertrade balance", self.account.as_ref().map(|a| format!("${:.2}", a.balance_raw as f64 / E18)).unwrap_or("…".into()), FG),
            kv("queued", self.account.as_ref().map(|a| format!("${:.2}", a.queued_raw as f64 / E18)).unwrap_or("…".into()), FG2),
            kv("margin in use", format!("${in_use:.2}"), if in_use > 0.0 { PINK } else { FG2 }),
            kv("open positions", self.account.as_ref().map(|a| a.open_positions.to_string()).unwrap_or("…".into()),
               match self.account.as_ref().map(|a| a.open_positions) { Some(2) => MINT, Some(1) => RED, _ => FG2 }),
            kv("PAPER held", self.account.as_ref().map(|a| fmt_n(a.paper_raw as f64 / E18)).unwrap_or("…".into()), PINK),
            session_line,
        ]
        .spacing(4);
        let reg_btn = small_btn(if self.show_register { "cancel" } else if self.st.session_key.is_some() { "re-register" } else { "register session key" }, Some(Message::ToggleRegister), PANEL2, FG2);
        wallet_col = wallet_col.push(row![Space::with_width(Length::Fill), reg_btn]);
        if self.show_register {
            let can = !self.registering && !self.running() && !self.master_key.trim().is_empty();
            wallet_col = wallet_col.push(
                column![
                    text("master key signs one RegisterSessionKey intent, then is discarded").size(10).color(MUTED),
                    row![
                        text_input("0x… master private key", &self.master_key).secure(true).font(MONO).size(11).padding(5).on_input(Message::MasterKey).style(input_style),
                        small_btn(if self.registering { "signing…" } else { "register" }, if can { Some(Message::Register) } else { None }, PINK_D, FG),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                ]
                .spacing(4),
            );
        }
        if let Some((m, ok)) = &self.register_note {
            wallet_col = wallet_col.push(text(m).size(10).color(if *ok { MINT } else { RED }));
        }
        wallet_col = wallet_col.push(text("deposit / withdraw in the papertrade app (wallet signature)").size(9.5).color(MUTED));
        let wallet = panel("Wallet & session", self.user.map(short).unwrap_or("no session".into()), wallet_col.into());

        // ---- in flight ----
        let d = self.move_s.trim().parse::<f64>().unwrap_or(0.5) / 100.0;
        let buf = self.buf_s.trim().parse::<f64>().unwrap_or(25.0) / 100.0;
        let in_flight: Element<'_, Message> = match &self.st.in_flight {
            Some(f) => {
                let mid = self.mids.back().map(|m| m.1).unwrap_or(f.mid_at_open);
                let mv = (mid / f.mid_at_open - 1.0).abs();
                let d = if f.target_move > 0.0 { f.target_move } else { d };
                let prog = (mv / d).clamp(0.0, 1.0) as f32;
                let elapsed = sign::now_secs().saturating_sub(f.opened_at);
                let icon = if f.coin == "ETH" { self.eth.clone() } else { self.btc.clone() };
                column![
                    row![
                        svg(icon).width(18).height(18),
                        text(format!(" {}  ", f.coin)).font(SANS_BOLD).size(13).color(FG),
                        side("LONG", MINT), text(" + ").size(11).color(MUTED), side("SHORT", RED),
                        Space::with_width(Length::Fill),
                        text(format!("{}×", f.leverage)).font(MONO).size(12).color(FG2),
                    ]
                    .align_y(Alignment::Center),
                    kv("margin per leg", format!("${:.2}", f.margin_raw as f64 / E18), FG),
                    kv("entry mid", format!("{:.1}", f.mid_at_open), FG),
                    kv("mid now", format!("{mid:.1}"), FG),
                    {
                        // live value of each leg off the Hyperliquid mid (the oracle the Exchange reads)
                        let n = f.margin_raw as f64 / E18 * f.leverage as f64;
                        let legs: Vec<(bool, f64)> = if f.entries.len() == 2 && f.longs.len() == 2 {
                            f.longs.iter().copied().zip(f.entries.iter().copied()).collect()
                        } else {
                            vec![(true, f.mid_at_open), (false, f.mid_at_open)]
                        };
                        let pnl = |(long, e): (bool, f64)| (mid / e - 1.0) * n * if long { 1.0 } else { -1.0 };
                        let lp = legs.iter().find(|l| l.0).map(|l| pnl(*l)).unwrap_or(0.0);
                        let sp = legs.iter().find(|l| !l.0).map(|l| pnl(*l)).unwrap_or(0.0);
                        let (w, l) = (lp.max(sp), lp.min(sp));
                        let kw = keep_frac((w / n).abs(), if f.coin == "ETH" { 483.979 } else { 814.598 });
                        let net = l + kw * w.max(0.0);
                        let paper_now = rate.unwrap_or(0.0) * 0.98 * (-l).max(0.0);
                        column![
                            kv("long pnl", format!("{lp:+.2}"), if lp >= 0.0 { MINT } else { RED }),
                            kv("short pnl", format!("{sp:+.2}"), if sp >= 0.0 { MINT } else { RED }),
                            kv("net if closed now", format!("{net:+.2}  (winner keeps {:.1}%)", kw * 100.0), if net >= 0.0 { MINT } else { PINK }),
                            kv("PAPER if closed now", fmt_n(paper_now), PINK),
                        ]
                        .spacing(5)
                    },
                    kv("move", format!("{:.3}% of {:.2}%", mv * 100.0, d * 100.0), if prog >= 1.0 { MINT } else { PINK }),
                    progress_bar(0.0..=1.0, prog).height(6).style(|_| progress_bar::Style {
                        background: Background::Color(PANEL2), bar: Background::Color(PINK_D),
                        border: Border { radius: 3.0.into(), ..Default::default() },
                    }),
                    kv("elapsed", format!("{}m {:02}s", elapsed / 60, elapsed % 60), FG2),
                    kv("ids", if f.position_ids.is_empty() { "pending".into() } else { f.position_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" · ") }, MUTED),
                ]
                .spacing(5)
                .into()
            }
            None => column![
                text("idle").font(SANS_BOLD).size(13).color(FG2),
                text(if self.running() { "waiting for the next cycle" } else { "no pair open — start the farm below" }).size(11).color(MUTED),
            ]
            .spacing(4)
            .into(),
        };
        let in_flight_panel = panel_with_bg("In flight", self.st.in_flight.as_ref().map(|_| "pair open".to_string()).unwrap_or_default(), in_flight, self.room.clone());

        // ---- strategy + controls ----
        let r = rate.unwrap_or(0.0);
        let strat = |pm: f64| { let k = keep_frac(d, pm); (k, 1.0 - k, if r > 0.0 { 0.98 * r / (1.0 - k) } else { 0.0 }) };
        let (kb, cb, eb) = strat(814.598);
        let (ke, ce, ee) = strat(483.979);
        let vol = |cn: &str| self.feed.as_ref().and_then(|f| f.daily_vol(cn));
        let lev = ((1.0 / (d * (1.0 + buf))).round() as u32).clamp(1, 1000);
        let seg = |name: &'static str, label: &'static str| {
            let on = self.market == name;
            small_btn(label, Some(Message::Market(name)), if on { PINK_D } else { PANEL2 }, if on { FG } else { FG2 })
        };
        let num = |label: &'static str, v: &str, on: fn(String) -> Message| -> Element<'_, Message> {
            row![text(label).size(10).color(FG2), text_input("", v).font(MONO).size(11).padding(4).width(Length::Fixed(52.0)).on_input(on).style(input_style)]
                .spacing(4).align_y(Alignment::Center).into()
        };
        let (start_label, start_msg) = match self.bot {
            Bot::Idle => ("start farm", Some(Message::StartBot)),
            Bot::Running(_) => ("stop after cycle", Some(Message::StopBot)),
            Bot::Stopping => ("stopping…", None),
        };
        let strategy = panel(
            "Strategy",
            self.live_params.as_ref().map(|p| format!("live: {}", p.summary())).unwrap_or_else(|| format!("{lev}× · bust guard {:.3}%", (d * (1.0 + buf) - 0.0005) * 100.0)),
            column![
                row![seg("auto", "AUTO"), seg("btc", "BTC"), seg("eth", "ETH"), Space::with_width(8),
                     num("move %", &self.move_s, Message::MoveS), num("buffer %", &self.buf_s, Message::BufS)].spacing(5).align_y(Alignment::Center),
                row![num("capital ×", &self.cap_s, Message::CapS), num("dd stop %", &self.dd_s, Message::DdS)].spacing(10).align_y(Alignment::Center),
                row![
                    coin_col(self.btc.clone(), "BTC", kb, cb, eb, vol("BTC"), PINK_D),
                    coin_col(self.eth.clone(), "ETH", ke, ce, ee, vol("ETH"), ETH_BLUE),
                ]
                .spacing(14),
                row![
                    small_btn(start_label, if self.st.session_key.is_some() { start_msg } else { None }, if matches!(self.bot, Bot::Running(_)) { PANEL2 } else { PINK_D }, FG),
                    small_btn("apply", Some(Message::ApplyParams), PANEL2, LILAC),
                    small_btn("close all", if self.running() || self.st.session_key.is_none() { None } else { Some(Message::CloseAll) }, PANEL2, RED),
                    Space::with_width(Length::Fill),
                    text(self.bot_note.as_ref().map(|(m, _)| m.clone()).unwrap_or_default()).size(10)
                        .color(self.bot_note.as_ref().map(|(_, ok)| if *ok { MINT } else { RED }).unwrap_or(MUTED)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
                text("cheaper cycle: BTC · faster cycle: ETH — auto prices both every cycle").size(9.5).color(LILAC),
            ]
            .spacing(6)
            .into(),
        );

        // ---- chart ----
        let entry = self.st.in_flight.as_ref().map(|f| f.mid_at_open);
        let chart = panel(
            &format!("{} live", self.chart_coin),
            self.mids.back().map(|m| format!("{:.1}", m.1)).unwrap_or("connecting…".into()),
            canvas(Spark { pts: self.mids.iter().copied().collect(), entry, d, color: if self.chart_coin == "ETH" { ETH_BLUE } else { PINK } })
                .width(Length::Fill).height(Length::Fixed(150.0)).into(),
        );

        // ---- cycles ----
        let header = row![th("#", 26), th("coin", 40), th("lev", 40), th("margin", 64), th("move", 56), th("time", 50), th("pnl", 64), th("PAPER", 70)].spacing(5);
        let rows = self.cycles.iter().rev().take(6).map(|cy| {
            row![
                td(cy.cycle.to_string(), 26, MUTED), td(cy.coin.clone(), 40, FG), td(format!("{}×", cy.leverage), 40, FG2),
                td(format!("${:.0}", cy.margin), 64, FG), td(format!("{:.3}%", cy.mv * 100.0), 56, FG2),
                td(format!("{}m{:02}", cy.elapsed_s / 60, cy.elapsed_s % 60), 50, FG2),
                td(format!("{:+.2}", cy.pnl), 64, if cy.pnl < 0.0 { RED } else { MINT }), td(fmt_n(cy.paper), 70, PINK),
            ]
            .spacing(5)
            .into()
        });
        let table: Element<'_, Message> = if self.cycles.is_empty() {
            text("no cycles yet").size(11).color(MUTED).into()
        } else {
            column(std::iter::once(header.into()).chain(rows)).spacing(3).into()
        };
        let cycles = panel("Cycles", format!("{} total", self.cycles.len()), scrollable(table).height(Length::Fixed(104.0)).into());

        // ---- log ----
        let lines: Vec<String> = { let q = self.logs.0.lock().unwrap(); q.iter().rev().take(7).map(|l| l.chars().take(96).collect()).collect::<Vec<_>>().into_iter().rev().collect() };
        let log_col = column(lines.into_iter().map(|l| {
            let col = if l.contains("ERROR") { RED } else if l.contains("WARN") { PINK } else { FG2 };
            text(l).font(MONO).size(9.5).color(col).into()
        })).spacing(2);
        let log = panel("Log", self.err.as_ref().map(|e| format!("api: {e}")).unwrap_or_default(), scrollable(log_col).height(Length::Fixed(104.0)).into());

        let body = column![
            row![wallet.width(Length::FillPortion(2)), in_flight_panel.width(Length::FillPortion(3))].spacing(8),
            row![strategy.width(Length::FillPortion(3)), chart.width(Length::FillPortion(3))].spacing(8),
            row![cycles.width(Length::FillPortion(3)), log.width(Length::FillPortion(3))].spacing(8),
        ]
        .spacing(8)
        .padding([8, 10]);

        container(column![topbar, hero, scrollable(body)])
            .width(Length::Fill).height(Length::Fill)
            .style(|_| container::Style { background: Some(Background::Color(BG)), ..Default::default() })
            .into()
    }
}

// ---------- widgets ----------
fn input_style(_: &Theme, _: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Background::Color(c(0x15, 0x12, 0x1f)),
        border: Border { color: LINE, width: 1.0, radius: 5.0.into() },
        icon: FG2, placeholder: MUTED, value: FG, selection: PINK_D,
    }
}

fn small_btn<'a>(label: &'a str, on: Option<Message>, bg: Color, fg: Color) -> Element<'a, Message> {
    let b = button(text(label).size(10.5).font(SANS_BOLD).color(if on.is_some() { fg } else { MUTED }))
        .padding([4, 9])
        .style(move |_, status| button::Style {
            background: Some(Background::Color(if matches!(status, button::Status::Hovered) { Color { a: 0.85, ..bg } } else { bg })),
            text_color: fg,
            border: Border { color: LINE, width: 1.0, radius: 5.0.into() },
            ..Default::default()
        });
    match on { Some(m) => b.on_press(m), None => b }.into()
}

fn chip<'a>(k: &'a str, v: String, col: Color) -> Element<'a, Message> {
    container(row![text(k).font(MONO).size(10).color(FG2), text(" "), text(v).font(MONO_MED).size(10).color(col)])
        .padding([3, 7])
        .style(|_| container::Style {
            background: Some(Background::Color(PANEL2)),
            border: Border { color: LINE, width: 1.0, radius: 5.0.into() },
            ..Default::default()
        })
        .into()
}

fn hero_stat<'a>(k: &'a str, v: String) -> Element<'a, Message> {
    column![text(v).font(MONO_MED).size(16).color(FG), text(k).size(9).color(MUTED)].spacing(1).into()
}

fn panel_head<'a>(title: &str, meta: String) -> Element<'a, Message> {
    container(
        row![text(title.to_uppercase()).font(SANS_BOLD).size(9.5).color(FG2), Space::with_width(Length::Fill), text(meta).font(MONO).size(9.5).color(MUTED)]
            .align_y(Alignment::Center),
    )
    .padding([6, 10])
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(HEAD)),
        border: Border { color: LINE, width: 0.0, radius: iced::border::Radius { top_left: 8.0, top_right: 8.0, bottom_right: 0.0, bottom_left: 0.0 } },
        ..Default::default()
    })
    .into()
}

fn panel_frame<'a>(inner: Element<'a, Message>) -> container::Container<'a, Message> {
    container(inner).width(Length::Fill).style(|_| container::Style {
        background: Some(Background::Color(PANEL)),
        border: Border { color: LINE, width: 1.0, radius: 8.0.into() },
        ..Default::default()
    })
}

fn panel<'a>(title: &str, meta: String, body: Element<'a, Message>) -> container::Container<'a, Message> {
    panel_frame(column![panel_head(title, meta), container(body).padding(10).width(Length::Fill)].into())
}

fn panel_with_bg<'a>(title: &str, meta: String, body: Element<'a, Message>, bg: image::Handle) -> container::Container<'a, Message> {
    let art = container(image(bg).height(Length::Fill).content_fit(iced::ContentFit::Cover).opacity(0.10_f32))
        .width(Length::Fill).height(Length::Fill).align_x(iced::alignment::Horizontal::Right);
    panel_frame(stack![art, column![panel_head(title, meta), container(body).padding(10).width(Length::Fill)]].into())
}

fn kv<'a>(k: &'a str, v: String, col: Color) -> Element<'a, Message> {
    row![text(k).size(11).color(FG2), Space::with_width(Length::Fill), text(v).font(MONO).size(11).color(col)].into()
}

fn side<'a>(s: &'a str, col: Color) -> Element<'a, Message> {
    container(text(s).font(MONO).size(10).color(col)).padding([1, 6]).style(move |_| container::Style {
        background: Some(Background::Color(Color { a: 0.18, ..col })),
        border: Border { radius: 4.0.into(), ..Default::default() },
        ..Default::default()
    })
    .into()
}

fn coin_col<'a>(icon: svg::Handle, name: &'a str, keep: f64, cost: f64, eff: f64, vol: Option<f64>, col: Color) -> Element<'a, Message> {
    column![
        row![svg(icon).width(14).height(14), text(format!(" {name}")).font(SANS_BOLD).size(11).color(col)].align_y(Alignment::Center),
        kv("winner keeps", format!("{:.1}%", keep * 100.0), FG),
        kv("cost / cycle", format!("{:.1}%", cost * 100.0), FG),
        kv("PAPER / $1", if eff > 0.0 { format!("{eff:.0}") } else { "…".into() }, PINK),
        kv("vol (realised)", vol.map(|v| format!("{:.2}%/d", v * 100.0)).unwrap_or("sampling…".into()), FG2),
    ]
    .spacing(3)
    .width(Length::Fill)
    .into()
}

fn th<'a>(s: &'a str, w: u16) -> Element<'a, Message> {
    text(s.to_uppercase()).size(9).color(MUTED).width(Length::Fixed(w as f32)).into()
}
fn td<'a>(s: String, w: u16, col: Color) -> Element<'a, Message> {
    text(s).font(MONO).size(10.5).color(col).width(Length::Fixed(w as f32)).into()
}

fn fmt_n(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 { format!("{:.2}B", v / 1e9) } else if a >= 1e6 { format!("{:.2}M", v / 1e6) } else if a >= 1e3 { format!("{:.1}k", v / 1e3) } else { format!("{v:.0}") }
}

// ---------- sparkline ----------
struct Spark {
    pts: Vec<(f64, f64)>,
    entry: Option<f64>,
    d: f64,
    color: Color,
}

impl<M> canvas::Program<M> for Spark {
    type State = ();
    fn draw(&self, _: &(), renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<canvas::Geometry> {
        let mut f = canvas::Frame::new(renderer, bounds.size());
        let (w, h) = (bounds.width, bounds.height);
        if self.pts.len() < 2 {
            return vec![f.into_geometry()];
        }
        let t0 = self.pts[0].0;
        let t1 = self.pts.last().unwrap().0.max(t0 + 1.0);
        let mut lo = self.pts.iter().map(|p| p.1).fold(f64::MAX, f64::min);
        let mut hi = self.pts.iter().map(|p| p.1).fold(f64::MIN, f64::max);
        if let Some(e) = self.entry {
            lo = lo.min(e * (1.0 - self.d));
            hi = hi.max(e * (1.0 + self.d));
        }
        let pad = ((hi - lo).max(hi * 2e-5)) * 0.12;
        lo -= pad;
        hi += pad;
        let x = |t: f64| (((t - t0) / (t1 - t0)) as f32 * (w - 2.0)) + 1.0;
        let y = |p: f64| h - (((p - lo) / (hi - lo)) as f32 * (h - 2.0)) - 1.0;
        for i in 1..4 {
            let yy = h * i as f32 / 4.0;
            f.stroke(&canvas::Path::line([0.0, yy].into(), [w, yy].into()), canvas::Stroke::default().with_color(Color { a: 0.5, ..LINE }).with_width(1.0));
        }
        if let Some(e) = self.entry {
            let dash = canvas::Stroke { line_dash: canvas::LineDash { segments: &[4.0, 4.0], offset: 0 }, ..canvas::Stroke::default().with_width(1.0) };
            f.stroke(&canvas::Path::line([0.0, y(e)].into(), [w, y(e)].into()), dash.clone().with_color(FG2));
            for lvl in [e * (1.0 + self.d), e * (1.0 - self.d)] {
                f.stroke(&canvas::Path::line([0.0, y(lvl)].into(), [w, y(lvl)].into()), dash.clone().with_color(Color { a: 0.8, ..MINT }));
            }
        }
        let path = canvas::Path::new(|b| {
            b.move_to([x(self.pts[0].0), y(self.pts[0].1)].into());
            for p in &self.pts[1..] {
                b.line_to([x(p.0), y(p.1)].into());
            }
        });
        f.stroke(&path, canvas::Stroke::default().with_color(self.color).with_width(2.0));
        let last = self.pts.last().unwrap();
        f.fill(&canvas::Path::circle([x(last.0), y(last.1)].into(), 3.0), self.color);
        f.fill_text(canvas::Text {
            content: format!("{:.1}", last.1),
            position: [x(last.0) - 6.0, y(last.1) - 8.0].into(),
            color: FG,
            size: 11.0.into(),
            font: MONO,
            horizontal_alignment: iced::alignment::Horizontal::Right,
            vertical_alignment: iced::alignment::Vertical::Bottom,
            ..canvas::Text::default()
        });
        vec![f.into_geometry()]
    }
}
