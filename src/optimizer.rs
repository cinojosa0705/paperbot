//! The dashboard's farm model, fed with live inputs: sweep the move per market and rank by PAPER
//! over a horizon, with capital bleed and rate decay.

use crate::engine::{keep_frac, min_buffer};

#[derive(Debug, Clone)]
pub struct Rec {
    pub coin: &'static str,
    pub move_pct: f64,
    pub buffer_pct: f64,
    pub leverage: u32,
    pub paper_total: f64,
    pub paper_per_day: f64,
    pub paper_per_usd: f64,
    pub cycles_per_day: f64,
    pub cost_pct: f64,
    pub days_used: f64,
    pub notional: f64,
}

pub struct Inputs {
    pub equity_usd: f64,
    pub capital_fraction: f64,
    pub dd_stop: f64,
    pub rate: f64,
    pub decay_per_day: f64,
    pub min_notional: f64,
    pub max_notional: f64,
    pub horizon_days: f64,
    pub max_cycles_per_day: f64,
}

pub const POS_MULT: [(&str, f64); 2] = [("BTC", 814.598), ("ETH", 483.979)];

fn sim(d: f64, buf: f64, pos_mult: f64, vol: f64, i: &Inputs) -> Option<Rec> {
    let lev = ((1.0 / (d * (1.0 + buf))).round() as u32).clamp(1, 1000);
    let keep = keep_frac(d, pos_mult);
    let cpd = (vol / d).powi(2).min(i.max_cycles_per_day);
    let per_leg0 = (i.equity_usd * i.capital_fraction / 2.0).floor();
    if per_leg0 * (lev as f64) < i.min_notional {
        return None;
    }
    let floor = i.equity_usd * (1.0 - i.dd_stop);
    let (mut c, mut day, mut mint, mut n, mut spent) = (i.equity_usd, 0.0f64, 0.0f64, 0u32, 0.0f64);
    let mut notional = 0.0;
    while day < i.horizon_days && c > floor && n < 100_000 {
        let per_leg = ((c * i.capital_fraction / 2.0).floor()).min(i.max_notional / lev as f64);
        notional = per_leg * lev as f64;
        if notional < i.min_notional {
            break;
        }
        let rate = i.rate * (1.0 - i.decay_per_day).max(0.0).powf(day);
        let basis = d * notional * 0.98;
        let cost = (1.0 - keep) * d * notional;
        mint += rate * basis;
        c -= cost;
        spent += cost;
        day += 1.0 / cpd;
        n += 1;
    }
    if n == 0 {
        return None;
    }
    Some(Rec {
        coin: "",
        move_pct: d * 100.0,
        buffer_pct: buf * 100.0,
        leverage: lev,
        paper_total: mint,
        paper_per_day: mint / day.max(1e-6),
        paper_per_usd: if spent > 0.0 { mint / spent } else { 0.0 },
        cycles_per_day: cpd,
        cost_pct: (1.0 - keep) * 100.0,
        days_used: day,
        notional,
    })
}

/// Best move per market (buffer = the smallest valid one with 10% headroom, ≥ 25%).
pub fn optimize(vols: &[(&'static str, f64)], i: &Inputs) -> Vec<Rec> {
    let mut out = Vec::new();
    for (coin, pm) in POS_MULT {
        let Some(&(_, vol)) = vols.iter().find(|(c, _)| *c == coin) else { continue };
        let mut best: Option<Rec> = None;
        let mut k = 0;
        while k <= 60 {
            let d = 0.0005 * 200f64.powf(k as f64 / 60.0); // 0.05% … 10%
            // the same headroom rule the engine enforces, rounded up to a whole percent
            let buf = ((min_buffer(d) * 100.0).ceil() / 100.0).max(0.25);
            if let Some(r) = sim(d, buf, pm, vol, i) {
                if best.as_ref().map(|b| r.paper_total > b.paper_total).unwrap_or(true) {
                    best = Some(r);
                }
            }
            k += 1;
        }
        if let Some(mut b) = best {
            b.coin = coin;
            // round to what /set accepts and the bot can reason about
            b.move_pct = (b.move_pct * 100.0).round() / 100.0;
            b.buffer_pct = b.buffer_pct.ceil();
            out.push(b);
        }
    }
    out.sort_by(|a, b| b.paper_total.partial_cmp(&a.paper_total).unwrap());
    out
}
