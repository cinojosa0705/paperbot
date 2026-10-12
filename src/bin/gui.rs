//! paperbot GUI — the farm at a glance, in papertrade's uwu dress.
//! Reads the same sources the bot does: protocol summary, the account row, state.json,
//! ledger.jsonl and the Hyperliquid bbo feed. It never signs or posts anything.

use alloy::primitives::Address;
use clap::Parser;
use iced::widget::{canvas, column, container, image, progress_bar, row, scrollable, stack, svg, text, Space};
use iced::{mouse, Alignment, Background, Border, Color, Element, Font, Length, Rectangle, Renderer, Subscription, Task, Theme};
use paperbot::api::{AccountRow, Api, Summary};
use paperbot::engine::keep_frac;
use paperbot::feed::Feed;
use paperbot::state::State;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
    /// Mirror the running bot's --move
    #[arg(long = "move", default_value_t = 0.5)]
    move_pct: f64,
    /// Mirror the running bot's --buffer-pct
    #[arg(long, default_value_t = 25.0)]
    buffer_pct: f64,
}

fn main() -> iced::Result {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    iced::application("paperbot", App::update, App::view)
        .subscription(App::subscription)
        .theme(App::theme)
        .font(include_bytes!("../../assets/fonts/Archivo.ttf").as_slice())
        .font(include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf").as_slice())
        .font(include_bytes!("../../assets/fonts/IBMPlexMono-Medium.ttf").as_slice())
        .default_font(SANS)
        .window_size((900.0, 640.0))
        .run_with(move || App::new(cli))
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
}

struct App {
    api: Api,
    state_path: PathBuf,
    ledger_path: PathBuf,
    move_frac: f64,
    buf: f64,
    feed: Option<Feed>,
    user: Option<Address>,
    st: State,
    cycles: Vec<Cycle>,
    summary: Option<Summary>,
    account: Option<AccountRow>,
    mids: VecDeque<(f64, f64)>, // (t secs, px) for the chart coin
    chart_coin: &'static str,
    started: Instant,
    ticks: u64,
    err: Option<String>,
    girl: image::Handle,
    stars: image::Handle,
    room: image::Handle,
    btc: svg::Handle,
    eth: svg::Handle,
}

impl App {
    fn new(cli: Cli) -> (Self, Task<Message>) {
        let api = Api::new(&cli.api).expect("api client");
        let app = App {
            api,
            state_path: cli.state,
            ledger_path: cli.ledger,
            move_frac: cli.move_pct / 100.0,
            buf: cli.buffer_pct / 100.0,
            feed: None,
            user: None,
            st: State::default(),
            cycles: vec![],
            summary: None,
            account: None,
            mids: VecDeque::with_capacity(400),
            chart_coin: "BTC",
            started: Instant::now(),
            ticks: 0,
            err: None,
            girl: image::Handle::from_bytes(GIRL),
            stars: image::Handle::from_bytes(STARS),
            room: image::Handle::from_bytes(ROOM),
            btc: svg::Handle::from_memory(BTC_SVG),
            eth: svg::Handle::from_memory(ETH_SVG),
        };
        // the feed spawns tokio tasks, so create it inside the runtime
        (app, Task::perform(async { Feed::spawn() }, Message::FeedReady))
    }

    fn theme(&self) -> Theme {
        Theme::custom(
            "uwu".into(),
            iced::theme::Palette { background: BG, text: FG, primary: PINK_D, success: MINT, danger: RED },
        )
    }

    fn subscription(&self) -> Subscription<Message> {
        iced::time::every(Duration::from_millis(1000)).map(|_| Message::Tick)
    }

    fn reload_local(&mut self) {
        if let Ok(st) = State::load(&self.state_path) {
            self.user = st.user;
            self.st = st;
        }
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

    fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::FeedReady(f) => {
                self.feed = Some(f);
                Task::none()
            }
            Message::Protocol(r) => {
                match r {
                    Ok(s) => { self.summary = Some(s); self.err = None; }
                    Err(e) => self.err = Some(e),
                }
                Task::none()
            }
            Message::Account(r) => {
                if let Ok(a) = r { self.account = Some(a); }
                Task::none()
            }
            Message::Tick => {
                self.ticks += 1;
                self.reload_local();
                let coin: &'static str = match self.st.in_flight.as_ref().map(|f| f.coin.as_str()) {
                    Some("ETH") => "ETH",
                    _ => "BTC",
                };
                if coin != self.chart_coin {
                    self.chart_coin = coin;
                    self.mids.clear();
                }
                if let Some(m) = self.feed.as_ref().and_then(|f| f.mid(coin)) {
                    self.mids.push_back((self.started.elapsed().as_secs_f64(), m.px));
                    while self.mids.len() > 360 { self.mids.pop_front(); }
                }
                if self.ticks % 3 != 1 {
                    return Task::none();
                }
                let api = self.api.clone();
                let mut tasks = vec![Task::perform(
                    async move { api.summary().await.map_err(|e| e.to_string()) },
                    Message::Protocol,
                )];
                if let Some(u) = self.user {
                    let api = self.api.clone();
                    tasks.push(Task::perform(
                        async move { api.account_row(u).await.map_err(|e| e.to_string()) },
                        Message::Account,
                    ));
                }
                Task::batch(tasks)
            }
        }
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
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(BG)),
            border: Border { color: LINE, width: 0.0, radius: 0.0.into() },
            ..Default::default()
        });

        let hero = stack![
            image(self.stars.clone()).width(Length::Fill).height(Length::Fixed(118.0)).content_fit(iced::ContentFit::Cover).opacity(0.55_f32),
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
                .padding([14, 16])
                .width(Length::Fill),
                image(self.girl.clone()).height(Length::Fixed(118.0)).content_fit(iced::ContentFit::Contain).opacity(0.95_f32),
            ]
            .align_y(Alignment::End),
        ];

        let account = panel(
            "Account",
            self.user.map(|u| format!("{:#x}", u)).map(|s| format!("{}…{}", &s[..6], &s[s.len() - 4..])).unwrap_or("no session".into()),
            match &self.account {
                Some(a) => column![
                    kv("balance", format!("${:.2}", a.balance_raw as f64 / E18), FG),
                    kv("queued", format!("${:.2}", a.queued_raw as f64 / E18), FG2),
                    kv("open positions", a.open_positions.to_string(), if a.open_positions == 2 { MINT } else if a.open_positions == 1 { RED } else { FG2 }),
                    kv("PAPER held", fmt_n(a.paper_raw as f64 / E18), PINK),
                    kv("session", self.st.session_expiry.map(|e| {
                        let left = e.saturating_sub(paperbot::sign::now_secs());
                        format!("{}d {}h left", left / 86_400, (left % 86_400) / 3600)
                    }).unwrap_or("—".into()), FG2),
                ]
                .spacing(4)
                .into(),
                None => text(if self.user.is_some() { "loading…" } else { "run `paperbot register` first" }).size(11).color(MUTED).into(),
            },
        );

        let d = self.move_frac;
        let lev = ((1.0 / (d * (1.0 + self.buf))).round() as u32).clamp(1, 1000);
        let in_flight: Element<'_, Message> = match &self.st.in_flight {
            Some(f) => {
                let mid = self.mids.back().map(|m| m.1).unwrap_or(f.mid_at_open);
                let mv = (mid / f.mid_at_open - 1.0).abs();
                let prog = (mv / d).clamp(0.0, 1.0) as f32;
                let elapsed = paperbot::sign::now_secs().saturating_sub(f.opened_at);
                let icon = if f.coin == "ETH" { self.eth.clone() } else { self.btc.clone() };
                column![
                    row![
                        svg(icon).width(18).height(18),
                        text(format!(" {}  ", f.coin)).font(SANS_BOLD).size(13).color(FG),
                        side("LONG", MINT),
                        text(" + ").size(11).color(MUTED),
                        side("SHORT", RED),
                        Space::with_width(Length::Fill),
                        text(format!("{}×", f.leverage)).font(MONO).size(12).color(FG2),
                    ]
                    .align_y(Alignment::Center),
                    kv("margin per leg", format!("${:.2}", f.margin_raw as f64 / E18), FG),
                    kv("entry mid", format!("{:.1}", f.mid_at_open), FG),
                    kv("mid now", format!("{mid:.1}"), FG),
                    kv("move", format!("{:.3}% of {:.2}%", mv * 100.0, d * 100.0), if prog >= 1.0 { MINT } else { PINK }),
                    progress_bar(0.0..=1.0, prog).height(6).style(|_| progress_bar::Style {
                        background: Background::Color(PANEL2),
                        bar: Background::Color(PINK_D),
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
                text("no pair open — waiting for the next cycle").size(11).color(MUTED),
            ]
            .spacing(4)
            .into(),
        };
        let in_flight_panel = panel_with_bg(
            "In flight",
            self.st.in_flight.as_ref().map(|_| "pair open".to_string()).unwrap_or_default(),
            in_flight,
            self.room.clone(),
        );

        let r = rate.unwrap_or(0.0);
        let strat = |pm: f64| {
            let k = keep_frac(d, pm);
            (k, 1.0 - k, if r > 0.0 { 0.98 * r / (1.0 - k) } else { 0.0 })
        };
        let (kb, cb, eb) = strat(814.598);
        let (ke, ce, ee) = strat(483.979);
        let vol = |c: &str| self.feed.as_ref().and_then(|f| f.daily_vol(c));
        let strategy = panel(
            "Strategy",
            format!("{:.2}% move · {:.0}% buffer", d * 100.0, self.buf * 100.0),
            column![
                kv("leverage", format!("{lev}×"), FG),
                kv("bust guard at", format!("{:.3}%", (d * (1.0 + self.buf) - 0.0005) * 100.0), FG2),
                Space::with_height(4),
                row![
                    coin_col(self.btc.clone(), "BTC", kb, cb, eb, vol("BTC"), PINK_D),
                    coin_col(self.eth.clone(), "ETH", ke, ce, ee, vol("ETH"), ETH_BLUE),
                ]
                .spacing(14),
                Space::with_height(4),
                text("cheaper cycle: BTC · faster cycle: ETH — the bot prices both each cycle").size(10).color(LILAC),
            ]
            .spacing(4)
            .into(),
        );

        let entry = self.st.in_flight.as_ref().map(|f| f.mid_at_open);
        let chart = panel(
            &format!("{} live", self.chart_coin),
            self.mids.back().map(|m| format!("{:.1}", m.1)).unwrap_or("connecting…".into()),
            canvas(Spark { pts: self.mids.make_contiguous_clone(), entry, d, color: if self.chart_coin == "ETH" { ETH_BLUE } else { PINK } })
                .width(Length::Fill)
                .height(Length::Fixed(132.0))
                .into(),
        );

        let header = row![
            th("#", 28), th("coin", 44), th("lev", 44), th("margin", 70), th("move", 60), th("time", 56), th("pnl", 72), th("PAPER", 80),
        ]
        .spacing(6);
        let rows = self.cycles.iter().rev().take(8).map(|c| {
            row![
                td(c.cycle.to_string(), 28, MUTED), td(c.coin.clone(), 44, FG), td(format!("{}×", c.leverage), 44, FG2),
                td(format!("${:.0}", c.margin), 70, FG), td(format!("{:.3}%", c.mv * 100.0), 60, FG2),
                td(format!("{}m{:02}", c.elapsed_s / 60, c.elapsed_s % 60), 56, FG2),
                td(format!("{:+.2}", c.pnl), 72, if c.pnl < 0.0 { RED } else { MINT }), td(fmt_n(c.paper), 80, PINK),
            ]
            .spacing(6)
            .into()
        });
        let table: Element<'_, Message> = if self.cycles.is_empty() {
            text("no cycles yet").size(11).color(MUTED).into()
        } else {
            column(std::iter::once(header.into()).chain(rows)).spacing(3).into()
        };
        let cycles = panel("Cycles", format!("{} total · ledger", self.cycles.len()), scrollable(table).height(Length::Fixed(118.0)).into());

        let err: Element<'_, Message> = match &self.err {
            Some(e) => text(format!("api: {e}")).size(10).color(RED).into(),
            None => Space::with_height(0).into(),
        };

        let body = column![
            row![account.width(Length::FillPortion(2)), in_flight_panel.width(Length::FillPortion(3))].spacing(8),
            row![strategy.width(Length::FillPortion(3)), chart.width(Length::FillPortion(3))].spacing(8),
            cycles,
            err,
        ]
        .spacing(8)
        .padding([8, 10]);

        container(column![topbar, hero, scrollable(body)].spacing(0))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style { background: Some(Background::Color(BG)), ..Default::default() })
            .into()
    }
}

// ---------- widgets ----------
fn chip<'a>(k: &'a str, v: String, c: Color) -> Element<'a, Message> {
    container(row![text(k).font(MONO).size(10).color(FG2), text(" "), text(v).font(MONO_MED).size(10).color(c)])
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
        row![
            text(title.to_uppercase()).font(SANS_BOLD).size(9.5).color(FG2),
            Space::with_width(Length::Fill),
            text(meta).font(MONO).size(9.5).color(MUTED),
        ]
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
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::alignment::Horizontal::Right);
    panel_frame(stack![art, column![panel_head(title, meta), container(body).padding(10).width(Length::Fill)]].into())
}

fn kv<'a>(k: &'a str, v: String, c: Color) -> Element<'a, Message> {
    row![text(k).size(11).color(FG2), Space::with_width(Length::Fill), text(v).font(MONO).size(11).color(c)].into()
}

fn side<'a>(s: &'a str, c: Color) -> Element<'a, Message> {
    container(text(s).font(MONO).size(10).color(c)).padding([1, 6]).style(move |_| container::Style {
        background: Some(Background::Color(Color { a: 0.18, ..c })),
        border: Border { radius: 4.0.into(), ..Default::default() },
        ..Default::default()
    })
    .into()
}

fn coin_col<'a>(icon: svg::Handle, name: &'a str, keep: f64, cost: f64, eff: f64, vol: Option<f64>, c: Color) -> Element<'a, Message> {
    column![
        row![svg(icon).width(14).height(14), text(format!(" {name}")).font(SANS_BOLD).size(11).color(c)].align_y(Alignment::Center),
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
fn td<'a>(s: String, w: u16, c: Color) -> Element<'a, Message> {
    text(s).font(MONO).size(10.5).color(c).width(Length::Fixed(w as f32)).into()
}

fn fmt_n(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 { format!("{:.2}B", v / 1e9) } else if a >= 1e6 { format!("{:.2}M", v / 1e6) } else if a >= 1e3 { format!("{:.1}k", v / 1e3) } else { format!("{v:.0}") }
}

trait ContiguousClone {
    fn make_contiguous_clone(&self) -> Vec<(f64, f64)>;
}
impl ContiguousClone for VecDeque<(f64, f64)> {
    fn make_contiguous_clone(&self) -> Vec<(f64, f64)> {
        self.iter().copied().collect()
    }
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

