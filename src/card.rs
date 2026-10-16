//! PNG cards for Telegram: one SVG template rasterised in-process (resvg), fonts and the uwu
//! girl embedded so the binary needs nothing on disk. 1200×630, the size Telegram previews best.

use anyhow::{anyhow, Result};
use base64::Engine as _;
use resvg::usvg;
use std::sync::OnceLock;

const ARCHIVO: &[u8] = include_bytes!("../assets/fonts/Archivo.ttf");
const PLEX: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-Medium.ttf");
const GIRL: &[u8] = include_bytes!("../assets/uwu-resting-girl.png");

pub const BG: &str = "#110e1c";
pub const FG: &str = "#ece9f7";
pub const DIM: &str = "#9a93b3";
pub const PINK: &str = "#f2a0cf";
pub const PINK_D: &str = "#d4588f";
pub const MINT: &str = "#7fe3b2";
pub const RED: &str = "#f2718f";

/// What every card is made of. Text only — no emoji (resvg has no colour-emoji font).
#[derive(Debug, Clone, Default)]
pub struct Card {
    pub kicker: String,            // small pink line on top: "UwU bot · cycle 39"
    pub hero: String,              // the big number: "1,662"
    pub hero_unit: String,         // "PAPER"
    pub sub: String,               // line under the hero: "for $9.66 → 172 PAPER/$"
    pub chips: Vec<(String, String)>, // up to 4 (label, value)
    pub footer: String,            // bottom line
    pub progress: Option<f64>,     // 0..1 bar under the sub line
    pub accent: &'static str,      // MINT / RED / PINK
}

/// XML-escape and drop emoji (no colour-emoji font in the embedded set; they render as boxes).
fn esc(s: &str) -> String {
    let kept: String = s.chars().filter(|c| c.is_ascii() || "×→·−–—’…%".contains(*c) || (*c as u32) < 0x2000).collect();
    kept.trim().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn fontdb() -> &'static std::sync::Arc<usvg::fontdb::Database> {
    static DB: OnceLock<std::sync::Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_font_data(ARCHIVO.to_vec());
        db.load_font_data(PLEX.to_vec());
        std::sync::Arc::new(db)
    })
}

fn girl_data_uri() -> &'static str {
    static URI: OnceLock<String> = OnceLock::new();
    URI.get_or_init(|| format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(GIRL)))
}

pub fn svg(c: &Card) -> String {
    let accent = if c.accent.is_empty() { PINK } else { c.accent };
    let mut chips = String::new();
    for (i, (k, v)) in c.chips.iter().take(4).enumerate() {
        let x = 64 + i as i32 * 164;
        chips.push_str(&format!(
            r##"<rect x="{x}" y="430" width="152" height="76" rx="16" fill="#1c1730" stroke="#2c2545"/>
<text x="{tx}" y="459" font-family="IBM Plex Mono" font-size="17" fill="{DIM}">{k}</text>
<text x="{tx}" y="491" font-family="Archivo" font-weight="700" font-size="22" fill="{FG}">{v}</text>"##,
            tx = x + 16, k = esc(k), v = esc(v)
        ));
    }
    let progress = match c.progress {
        Some(p) => {
            let w = (p.clamp(0.0, 1.0) * 620.0).round();
            format!(
                r##"<rect x="64" y="384" width="620" height="14" rx="7" fill="#1c1730"/>
<rect x="64" y="384" width="{w}" height="14" rx="7" fill="{accent}"/>"##
            )
        }
        None => String::new(),
    };
    // hero font shrinks with length so "1,234,567" still fits left of the girl
    let hero_size = match c.hero.chars().count() { 0..=6 => 128, 7..=9 => 104, _ => 84 };
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="1200" height="630" viewBox="0 0 1200 630">
<defs>
  <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
    <stop offset="0" stop-color="#1a1230"/><stop offset="0.6" stop-color="{BG}"/><stop offset="1" stop-color="#2a0f2a"/>
  </linearGradient>
  <radialGradient id="glow" cx="0.82" cy="0.55" r="0.5">
    <stop offset="0" stop-color="{PINK_D}" stop-opacity="0.35"/><stop offset="1" stop-color="{PINK_D}" stop-opacity="0"/>
  </radialGradient>
  <clipPath id="card"><rect width="1200" height="630"/></clipPath>
</defs>
<g clip-path="url(#card)">
  <rect width="1200" height="630" fill="url(#bg)"/>
  <rect width="1200" height="630" fill="url(#glow)"/>
  <image xlink:href="{girl}" x="620" y="110" width="700" height="420" preserveAspectRatio="xMidYMid meet" opacity="0.98"/>
  <rect x="0" y="0" width="1200" height="630" fill="none" stroke="#2c2545" stroke-width="2"/>
  <text x="64" y="92" font-family="IBM Plex Mono" font-size="22" fill="{accent}" letter-spacing="2">{kicker}</text>
  <text x="64" y="{hero_y}" font-family="Archivo" font-weight="800" font-size="{hero_size}" fill="{FG}">{hero}<tspan font-size="40" fill="{accent}" dx="14">{unit}</tspan></text>
  <text x="64" y="350" font-family="Archivo" font-size="30" fill="{DIM}">{sub}</text>
  {progress}
  {chips}
  <text x="64" y="570" font-family="IBM Plex Mono" font-size="19" fill="{DIM}">{footer}</text>
  <text x="1136" y="570" text-anchor="end" font-family="Archivo" font-weight="700" font-size="20" fill="{PINK}">uwu bot</text>
</g>
</svg>"##,
        girl = girl_data_uri(),
        kicker = esc(&c.kicker.to_uppercase()),
        hero_y = 92 + 40 + hero_size * 7 / 8 + 60,
        hero = esc(&c.hero),
        unit = esc(&c.hero_unit),
        sub = esc(&c.sub),
        footer = esc(&c.footer),
    )
}

/// Rasterise to PNG bytes. ~40 ms; safe to call from a blocking context.
pub fn render(c: &Card) -> Result<Vec<u8>> {
    let opt = usvg::Options { fontdb: fontdb().clone(), ..Default::default() };
    let tree = usvg::Tree::from_str(&svg(c), &opt).map_err(|e| anyhow!("svg: {e}"))?;
    let size = tree.size().to_int_size();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height()).ok_or_else(|| anyhow!("pixmap"))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    pixmap.encode_png().map_err(|e| anyhow!("png: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_a_png() {
        let png = render(&Card {
            kicker: "UwU bot · cycle 39".into(), hero: "1,662".into(), hero_unit: "PAPER".into(),
            sub: "for $9.66 → 172 PAPER/$ (model 128/$)".into(),
            chips: vec![("market".into(), "BTC 42×".into()), ("move".into(), "0.40% in 28m".into())],
            footer: "all-time 93,750 PAPER · $630 → 149/$".into(), progress: Some(0.4), accent: MINT,
        }).unwrap();
        assert!(png.starts_with(b"\x89PNG") && png.len() > 10_000);
        if let Ok(p) = std::env::var("CARD_OUT") { std::fs::write(p, &png).unwrap(); }
    }
}
