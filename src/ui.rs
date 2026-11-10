//! Rendering: jcode-style cards (rounded dim borders, labels on the border,
//! ▰▱ meters, jcode's green/yellow/red). ratatui diffs frames, so only the
//! cells that changed reach the terminal.

use crate::sample::Sampler;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

const BORDER: Color = Color::Rgb(70, 70, 80);
const LABEL: Color = Color::Rgb(175, 175, 190);
const DIM: Color = Color::Rgb(125, 125, 140);
const MUTED: Color = Color::Rgb(140, 140, 150);
const TEXT: Color = Color::Rgb(200, 200, 210);
const EMPTY: Color = Color::Rgb(50, 50, 60);
const GREEN: Color = Color::Rgb(100, 200, 100);
const YELLOW: Color = Color::Rgb(255, 200, 100);
const RED: Color = Color::Rgb(255, 100, 100);
const BLUE: Color = Color::Rgb(140, 180, 255);
const PINK: Color = Color::Rgb(255, 135, 200);

fn s(text: impl Into<String>, fg: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(fg))
}
fn b(text: impl Into<String>, fg: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(fg).add_modifier(Modifier::BOLD))
}
fn key(k: &str) -> Span<'static> {
    s(format!("{k:<9}"), MUTED)
}

fn good_high(p: f64) -> Color {
    if p > 50.0 { GREEN } else if p > 20.0 { YELLOW } else { RED }
}
pub fn draw_color(w: f64) -> Color {
    if w < 6.0 { GREEN } else if w < 12.0 { YELLOW } else { RED }
}

fn meter(pct: f64, cells: usize, color: Color) -> Vec<Span<'static>> {
    let n = ((pct.clamp(0.0, 100.0) / 100.0) * cells as f64).round() as usize;
    vec![s("▰".repeat(n), color), s("▱".repeat(cells - n), EMPTY)]
}

fn hm(h: f64) -> String {
    if !h.is_finite() || h <= 0.0 || h > 99.0 {
        return "--".into();
    }
    format!("{}h {:02}m", h as u64, (h.fract() * 60.0) as u64)
}

/// Fit a line into `cells` display cells, ending with an ellipsis if cut.
fn clip(line: Line<'static>, cells: usize) -> Line<'static> {
    if line.width() <= cells {
        return line;
    }
    let mut out = Vec::new();
    let mut used = 0;
    let budget = cells.saturating_sub(1);
    for span in line.spans {
        let w = span.content.width();
        if used + w <= budget {
            used += w;
            out.push(span);
            continue;
        }
        let mut t = String::new();
        for ch in span.content.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw > budget {
                break;
            }
            used += cw;
            t.push(ch);
        }
        out.push(Span::styled(t, span.style));
        break;
    }
    out.push(s("…", DIM));
    Line::from(out)
}

struct Card {
    title: &'static str,
    right: Vec<Span<'static>>,
    footer: Vec<Span<'static>>,
    rows: Vec<Line<'static>>,
}

fn card(f: &mut Frame, area: Rect, c: Card) {
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title_top(Line::from(vec![s("─", BORDER), b(format!(" {} ", c.title), LABEL)]).left_aligned());
    if !c.right.is_empty() {
        let mut r = vec![s(" ", BORDER)];
        r.extend(c.right);
        r.push(s(" ─", BORDER));
        block = block.title_top(Line::from(r).right_aligned());
    }
    if !c.footer.is_empty() {
        let mut ft = vec![s("─ ", BORDER)];
        ft.extend(c.footer);
        ft.push(s(" ", BORDER));
        block = block.title_bottom(Line::from(ft).left_aligned());
    }
    let inner = block.inner(area);
    let width = inner.width.saturating_sub(2) as usize;
    let lines: Vec<Line> = c
        .rows
        .into_iter()
        .take(inner.height as usize)
        .map(|l| {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(clip(l, width).spans);
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Multi-row bar chart, newest sample on the right.
fn chart(hist: &[f64], cells: usize, rows: usize) -> Vec<Line<'static>> {
    let vals = &hist[hist.len().saturating_sub(cells)..];
    let top = vals.iter().copied().fold(1.0, f64::max);
    const EIGHTHS: [char; 8] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];
    (0..rows)
        .rev()
        .map(|r| {
            let mut spans = vec![Span::raw(" ".repeat(cells - vals.len()))];
            for &v in vals {
                let e = v / top * rows as f64 * 8.0 - r as f64 * 8.0;
                let ch = if e >= 8.0 { '█' } else { EIGHTHS[e.max(0.0) as usize] };
                spans.push(s(ch.to_string(), draw_color(v)));
            }
            Line::from(spans)
        })
        .collect()
}

pub fn render(f: &mut Frame, sm: &mut Sampler) {
    let area = f.area();
    let two = area.width >= 90 && area.height >= 14;
    let (cols, rows) = (area.width, area.height);
    let cw = if two { cols / 2 } else { cols };
    let mc = (cw as usize).saturating_sub(18).max(6);

    let (power_a, why_a, bat_a, prof_a) = if two {
        let top = rows.saturating_sub(6).max(7).min(rows);
        let [t, bt] = Layout::vertical([Constraint::Length(top), Constraint::Fill(1)]).areas(area);
        let [p, w] = Layout::horizontal([Constraint::Length(cw), Constraint::Fill(1)]).areas(t);
        let [ba, pr] = Layout::horizontal([Constraint::Length(cw), Constraint::Fill(1)]).areas(bt);
        (p, w, ba, pr)
    } else {
        let [p, w, ba, pr] = Layout::vertical([
            Constraint::Min(5),
            Constraint::Length(12),
            Constraint::Length(5),
            Constraint::Length(5),
        ])
        .areas(area);
        (p, w, ba, pr)
    };

    let bat = sm.bat_state.clone();
    let avg = sm.avg();
    let deep = sm.deep_idle();
    let top = sm.top();
    let total_cpu: f64 = top.iter().map(|(p, _)| p).sum();
    let bright = sm.brightness();

    // ---- power ----
    let hist: Vec<f64> = sm.hist.iter().copied().collect();
    let inner_w = (power_a.width as usize).saturating_sub(4);
    let chart_rows = (power_a.height as usize).saturating_sub(4);
    let mut rows_p = vec![
        Line::from(vec![
            key("now"),
            b(format!("{:5.1} W", bat.watts), draw_color(bat.watts)),
            s("   peak ", MUTED),
            s(format!("{:.1} W", sm.peak()), TEXT),
        ]),
        Line::from(vec![key("history"), s(format!("last {}s", hist.len().min(inner_w)), DIM)]),
    ];
    rows_p.extend(chart(&hist, inner_w, chart_rows));
    card(f, power_a, Card {
        title: "⚡ power",
        right: vec![s(format!("{avg:.1} W"), draw_color(avg)), s(" 1 min avg", DIM)],
        footer: vec![s("green <6 W   yellow <12 W   red", DIM)],
        rows: rows_p,
    });

    // ---- why ----
    let mut why = Vec::new();
    if let Some(r) = sm.rails.filter(|r| r.psys > 0.0) {
        let soc = (r.package - r.core - r.uncore).max(0.0);
        let sep = || s(" · ", DIM);
        why.push(Line::from(vec![
            key("split"),
            s(format!("cpu {:.1}", r.core), TEXT), sep(),
            s(format!("gpu/uncore {:.1}", r.uncore), TEXT), sep(),
            s(format!("ram {:.1}", r.dram), TEXT), sep(),
            s(format!("soc {soc:.1}"), TEXT), s(" W", DIM),
        ]));
        why.push(Line::from(vec![
            key(""),
            s(format!("platform {:.1}", (r.psys - r.package).max(0.0)), TEXT), sep(),
            s(format!("screen+rest ~{:.1}", (avg - r.psys).max(0.0)), TEXT), s(" W", DIM),
        ]));
    }
    why.push(Line::from(vec![
        key("cpu"),
        s(format!("{total_cpu:.0}%"), TEXT),
        s(format!(" of one core, {:.0}% of all {}", total_cpu / sm.ncpu as f64, sm.ncpu), DIM),
    ]));
    let mut hints: Vec<(Color, String)> = Vec::new();
    const COMPILERS: [&str; 6] = ["rustc", "cargo", "jr-cargo", "cc1plus", "ld", "mold"];
    if top.iter().take(40).any(|(p, k)| *p >= 1.0 && COMPILERS.contains(&k.as_str())) {
        hints.push((RED, "local compile running".into()));
    }
    if deep.is_some_and(|d| d < 5.0) {
        hints.push((YELLOW, "chip never deep-sleeps: constant wakeups".into()));
    }
    if let Some(br) = bright.filter(|b| *b > 40) {
        hints.push((YELLOW, format!("screen at {br}%, the OLED is a big share")));
    }
    if total_cpu > 100.0 {
        hints.push((YELLOW, format!("busy CPU: {total_cpu:.0}% of a core")));
    }
    if sm.jcode > 10 {
        hints.push((DIM, format!("{} jcode processes redrawing", sm.jcode)));
    }
    hints.truncate(3);
    let room = (why_a.height as usize).saturating_sub(2).saturating_sub(why.len());
    let shown_hints = room.saturating_sub(4).min(hints.len());
    for (c, h) in hints.into_iter().take(shown_hints) {
        why.push(Line::from(s(format!("● {h}"), c)));
    }
    why.push(Line::from(s("top by cpu", MUTED)));
    let pw = (why_a.width as usize).saturating_sub(4 + 2 + 14 + 1 + 6 + 1).max(4);
    let nprocs = room.saturating_sub(1 + shown_hints).max(3);
    for (p, k) in top.iter().take(nprocs) {
        let col = if *p >= 50.0 { RED } else if *p >= 10.0 { YELLOW } else { MUTED };
        let name: String = k.chars().take(14).collect();
        let mut l = vec![s(format!("  {name:<14}"), TEXT), s(format!(" {p:5.1}% "), col)];
        l.extend(meter(p.min(100.0), pw, col));
        why.push(Line::from(l));
    }
    card(f, why_a, Card {
        title: "🔍 why",
        right: vec![s(format!("{avg:.1} W"), draw_color(avg))],
        footer: vec![s("cpu % of one core, last 5 s", DIM)],
        rows: why,
    });

    // ---- battery ----
    let (time_line, state) = match bat.status.as_str() {
        "Discharging" => (
            vec![b(hm(bat.wh_now / avg), TEXT), s(" left   ", MUTED), s(format!("{} from full", hm(bat.wh_full / avg)), DIM)],
            s("▼ discharging", RED),
        ),
        "Charging" => (
            vec![b(hm((bat.wh_full - bat.wh_now) / avg), TEXT), s(" to full", MUTED)],
            s("▲ charging", GREEN),
        ),
        other => (vec![s(other.to_string(), MUTED)], s(other.to_lowercase(), BLUE)),
    };
    let mut charge = vec![key("charge")];
    charge.extend(meter(bat.capacity as f64, mc, good_high(bat.capacity as f64)));
    charge.push(s(format!(" {:3}%", bat.capacity), TEXT));
    let mut tl = vec![key("time")];
    tl.extend(time_line);
    card(f, bat_a, Card {
        title: "🔋 battery",
        right: vec![state],
        footer: vec![],
        rows: vec![
            Line::from(charge),
            Line::from(tl),
            Line::from(vec![
                key("energy"),
                s(format!("{:.1}", bat.wh_now), TEXT),
                s(format!(" / {:.1} Wh   health {}%", bat.wh_full, bat.health), DIM),
            ]),
        ],
    });

    // ---- profile ----
    let prof = sm.profile.text().unwrap_or_else(|| "?".into());
    let pcol = match prof.as_str() {
        "low-power" | "quiet" | "cool" => GREEN,
        "balanced" => YELLOW,
        _ => RED,
    };
    let mut idle = vec![key("idle")];
    match deep {
        Some(d) => {
            idle.extend(meter(d, mc, good_high(d)));
            idle.push(s(format!(" {d:3.0}%"), TEXT));
        }
        None => idle.push(s("counter unavailable", DIM)),
    }
    let mut prow = vec![
        Line::from(idle),
        Line::from(vec![
            key("platform"),
            s(prof, pcol),
            s("   epp ", MUTED),
            s(sm.epp.text().unwrap_or_else(|| "?".into()), TEXT),
            s("   gov ", MUTED),
            s(sm.gov.text().unwrap_or_else(|| "?".into()), TEXT),
        ]),
    ];
    if let Some(br) = bright {
        let mut l = vec![key("screen")];
        l.extend(meter(br as f64, mc, PINK));
        l.push(s(format!(" {br:3}%"), TEXT));
        prow.push(Line::from(l));
    }
    card(f, prof_a, Card {
        title: "⚙ profile",
        right: vec![s("deep idle goal >70%", DIM)],
        footer: vec![s("q quit", DIM)],
        rows: prow,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_counts_cells_and_adds_ellipsis() {
        let l = clip(Line::from(vec![s("⚡ab", TEXT), s("cdef", TEXT)]), 5);
        assert_eq!(l.width(), 5);
        assert!(l.to_string().ends_with('…'));
        assert_eq!(clip(Line::from("abc"), 5).to_string(), "abc");
    }

    #[test]
    fn hm_formats() {
        assert_eq!(hm(1.5), "1h 30m");
        assert_eq!(hm(f64::INFINITY), "--");
        assert_eq!(hm(0.0), "--");
    }

    #[test]
    fn chart_fills_requested_cells() {
        let c = chart(&[1.0, 2.0, 4.0], 10, 3);
        assert_eq!(c.len(), 3);
        assert!(c.iter().all(|l| l.width() == 10));
    }
}
