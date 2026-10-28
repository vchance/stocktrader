//! Self-contained HTML reports for plans: inline SVG charts and a few lines of script for hover readouts,
//! no external files, so a report opens from disk and can be archived or mailed as one file.

use std::collections::BTreeMap;
use std::fmt::Write;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use st_core::{Side, Timestamp};
use st_engine::{Metrics, exchange_date};

use crate::Run;
use crate::plans::Plan;
use crate::research::Window;

/// Categorical series slots, light and dark (the validated reference order; never cycled).
const SLOTS: usize = 7;

const STYLE: &str = r#"
:root {
  color-scheme: light;
  --page: #f9f9f7; --surface: #fcfcfb; --ink: #0b0b0b; --ink-2: #52514e; --muted: #898781;
  --grid: #e1e0d9; --axis: #c3c2b7; --border: rgba(11,11,11,0.10);
  --s1: #2a78d6; --s2: #eb6834; --s3: #1baf7a; --s4: #eda100; --s5: #e87ba4; --s6: #008300; --s7: #6250d6;
  --bench: #898781; --pos: rgba(42,120,214,0.22); --neg: rgba(227,73,72,0.22);
}
@media (prefers-color-scheme: dark) {
  :root:where(:not([data-theme="light"])) {
    color-scheme: dark;
    --page: #0d0d0d; --surface: #1a1a19; --ink: #f0efec; --ink-2: #c3c2b7; --muted: #898781;
    --grid: #2c2c2a; --axis: #383835; --border: rgba(255,255,255,0.10);
    --s1: #3987e5; --s2: #d95926; --s3: #199e70; --s4: #c98500; --s5: #d55181; --s6: #008300; --s7: #9085e9;
    --bench: #898781; --pos: rgba(57,135,229,0.30); --neg: rgba(230,103,103,0.30);
  }
}
:root[data-theme="dark"] {
  color-scheme: dark;
  --page: #0d0d0d; --surface: #1a1a19; --ink: #f0efec; --ink-2: #c3c2b7; --muted: #898781;
  --grid: #2c2c2a; --axis: #383835; --border: rgba(255,255,255,0.10);
  --s1: #3987e5; --s2: #d95926; --s3: #199e70; --s4: #c98500; --s5: #d55181; --s6: #008300; --s7: #9085e9;
  --bench: #898781; --pos: rgba(57,135,229,0.30); --neg: rgba(230,103,103,0.30);
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--page); color: var(--ink); font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 920px; margin: 0 auto; padding: 24px 16px 48px; }
h1 { font-size: 24px; margin: 0 0 4px; }
h2 { font-size: 17px; margin: 32px 0 8px; }
.sub { color: var(--ink-2); margin: 0 0 16px; }
.notes { white-space: pre-wrap; margin: 8px 0 0; }
.banner { border: 1px solid var(--border); border-left: 4px solid var(--s4); background: var(--surface); padding: 8px 12px; margin: 16px 0; }
section.card { background: var(--surface); border: 1px solid var(--border); border-radius: 8px; padding: 16px; margin: 12px 0; }
.scroll { overflow-x: auto; }
table { border-collapse: collapse; width: 100%; font-variant-numeric: tabular-nums; font-size: 14px; }
th, td { text-align: right; padding: 4px 8px; border-bottom: 1px solid var(--grid); white-space: nowrap; }
th:first-child, td:first-child { text-align: left; }
th { color: var(--ink-2); font-weight: 600; }
td.pos { background: var(--pos); } td.neg { background: var(--neg); }
pre { background: var(--surface); border: 1px solid var(--border); border-radius: 8px; padding: 12px; overflow-x: auto; font-size: 13px; }
figure { margin: 0; position: relative; }
figure svg { width: 100%; height: auto; display: block; touch-action: pan-y; }
.legend { display: flex; flex-wrap: wrap; gap: 4px 16px; margin: 0 0 8px; color: var(--ink-2); font-size: 13px; }
.legend span::before { content: ""; display: inline-block; width: 14px; height: 0; border-top: 2px solid var(--c); margin-right: 6px; vertical-align: middle; }
.legend span.dash::before { border-top-style: dashed; }
svg .grid { stroke: var(--grid); stroke-width: 1; }
svg .axis { stroke: var(--axis); stroke-width: 1; }
svg text { fill: var(--muted); font-size: 11px; font-variant-numeric: tabular-nums; }
svg .line { fill: none; stroke-width: 2; stroke-linejoin: round; stroke-linecap: round; }
svg .dash { stroke-dasharray: 5 4; }
svg .cross { stroke: var(--ink-2); stroke-width: 1; visibility: hidden; }
.tip { position: absolute; pointer-events: none; visibility: hidden; background: var(--surface); border: 1px solid var(--border);
  border-radius: 6px; padding: 6px 8px; font-size: 12px; box-shadow: 0 2px 8px rgba(0,0,0,0.12); white-space: nowrap; }
.tip b { font-weight: 600; }
.tip .row::before { content: ""; display: inline-block; width: 10px; height: 0; border-top: 2px solid var(--c); margin-right: 6px; vertical-align: middle; }
"#;

/// Crosshair and tooltip for every line chart: snaps to the nearest point of each series. Values are written
/// with textContent only.
const SCRIPT: &str = r#"
for (const fig of document.querySelectorAll('figure[data-chart]')) {
  const d = JSON.parse(fig.dataset.chart), svg = fig.querySelector('svg'), cross = svg.querySelector('.cross');
  const tip = fig.querySelector('.tip'), vb = svg.viewBox.baseVal;
  const fmt = v => d.fmt === 'money' ? '$' + Math.round(v).toLocaleString()
    : d.fmt === 'pct' ? (v * 100).toFixed(1) + '%' : v.toFixed(1);
  const nearest = (xs, t) => { let lo = 0, hi = xs.length - 1;
    while (hi - lo > 1) { const m = (lo + hi) >> 1; if (xs[m] < t) lo = m; else hi = m; }
    return Math.abs(xs[lo] - t) <= Math.abs(xs[hi] - t) ? lo : hi; };
  const hide = () => { cross.style.visibility = 'hidden'; tip.style.visibility = 'hidden'; };
  svg.addEventListener('pointerleave', hide);
  svg.addEventListener('pointermove', e => {
    const r = svg.getBoundingClientRect(), px = (e.clientX - r.left) / r.width * vb.width;
    if (px < d.l || px > d.r) return hide();
    const t = d.x0 + (px - d.l) / (d.r - d.l) * (d.x1 - d.x0);
    const first = d.series.find(s => s.x.length);
    if (!first) return hide();
    const i0 = nearest(first.x, t), sx = d.l + (first.x[i0] - d.x0) / (d.x1 - d.x0) * (d.r - d.l);
    cross.setAttribute('x1', sx); cross.setAttribute('x2', sx); cross.style.visibility = 'visible';
    tip.replaceChildren();
    const date = document.createElement('div');
    date.textContent = new Date(first.x[i0] * 1000).toISOString().slice(0, 10); tip.append(date);
    for (const s of d.series) {
      if (!s.x.length) continue;
      const i = nearest(s.x, first.x[i0]), row = document.createElement('div'), b = document.createElement('b');
      row.className = 'row'; row.style.setProperty('--c', s.color);
      b.textContent = fmt(s.y[i]); row.append(b, ' ' + s.label); tip.append(row);
    }
    tip.style.visibility = 'visible';
    const left = (sx / vb.width) * r.width;
    tip.style.left = Math.min(left + 12, r.width - tip.offsetWidth) + 'px'; tip.style.top = '8px';
  });
}
"#;

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn pct(x: f64) -> String {
    if x.is_finite() { format!("{:.2}%", x * 100.0) } else { "-".into() }
}

fn money(d: Decimal) -> String {
    let v = d.round().to_i64().unwrap_or(0);
    let digits = v.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    format!("{}${out}", if v < 0 { "-" } else { "" })
}

#[derive(Clone, Copy, PartialEq)]
enum Fmt {
    Money,
    Pct,
    Index,
}

struct Series {
    label: String,
    color: &'static str,
    dashed: bool,
    points: Vec<(i64, f64)>,
}

fn color(slot: usize) -> &'static str {
    ["var(--s1)", "var(--s2)", "var(--s3)", "var(--s4)", "var(--s5)", "var(--s6)", "var(--s7)"][slot % SLOTS]
}

fn to_points(curve: &[(Timestamp, Decimal)], scale: f64) -> Vec<(i64, f64)> {
    curve.iter().filter_map(|(t, v)| Some((t.timestamp(), v.to_f64()? * scale))).collect()
}

/// A "nice" step (1, 2 or 5 times a power of ten) giving about `target` intervals over `range`.
fn nice_step(range: f64, target: f64) -> f64 {
    let raw = (range / target).max(f64::MIN_POSITIVE);
    let mag = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 5.0, 10.0].into_iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag)
}

fn axis_label(v: f64, fmt: Fmt) -> String {
    match fmt {
        Fmt::Money if v.abs() >= 1e6 => format!("${:.1}M", v / 1e6),
        Fmt::Money if v.abs() >= 1e3 => format!("${:.0}k", v / 1e3),
        Fmt::Money => format!("${v:.0}"),
        // Adding 0.0 turns -0 into 0, so the top of a drawdown axis reads "0%".
        Fmt::Pct => format!("{:.0}%", v * 100.0 + 0.0),
        Fmt::Index => format!("{v:.0}"),
    }
}

/// X ticks at the start of each year, or of each quarter for spans under about three years.
fn x_ticks(x0: i64, x1: i64) -> Vec<(i64, String)> {
    let (d0, d1) = (DateTime::from_timestamp(x0, 0).unwrap_or_default(), DateTime::from_timestamp(x1, 0).unwrap_or_default());
    let quarterly = (d1 - d0).num_days() < 3 * 365;
    let mut out = Vec::new();
    let (mut y, mut m) = (d0.year(), 1);
    while y <= d1.year() {
        if let Some(t) = NaiveDate::from_ymd_opt(y, m, 1).and_then(|d| Utc.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).single()) {
            let ts = t.timestamp();
            if ts >= x0 && ts <= x1 {
                out.push((ts, if quarterly { format!("{y}-{m:02}") } else { y.to_string() }));
            }
        }
        if quarterly && m < 10 {
            m += 3;
        } else {
            (y, m) = (y + 1, 1);
        }
    }
    out
}

/// A line chart with gridlines, axes, a legend (for two or more series) and a hover readout.
fn line_chart(series: &[Series], fmt: Fmt, zero_top: bool) -> String {
    let (w, h, l, r, top, bottom) = (760.0, 260.0, 60.0, 744.0, 10.0, 232.0);
    let all = || series.iter().flat_map(|s| s.points.iter());
    let (Some(x0), Some(x1)) = (all().map(|p| p.0).min(), all().map(|p| p.0).max()) else {
        return "<p class=\"sub\">No data to chart.</p>".into();
    };
    let x1 = x1.max(x0 + 1);
    let mut lo = all().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let mut hi = all().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    if zero_top {
        // Drawdowns: 0 at the top, and at least a 1% range so a curve that never fell isn't centred on 0.
        hi = 0.0;
        lo = lo.min(-0.01);
    }
    if (hi - lo).abs() < 1e-9 {
        (lo, hi) = (lo - 1.0, hi + 1.0);
    }
    let step = nice_step(hi - lo, 4.0);
    let (lo, hi) = ((lo / step).floor() * step, (hi / step).ceil() * step);
    let sx = |x: i64| l + (x - x0) as f64 / (x1 - x0) as f64 * (r - l);
    let sy = |y: f64| bottom - (y - lo) / (hi - lo) * (bottom - top);

    let mut svg = format!("<svg viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"line chart\">");
    let mut v = lo;
    while v <= hi + step / 2.0 {
        let y = sy(v);
        let _ = write!(svg, "<line class=\"grid\" x1=\"{l}\" x2=\"{r}\" y1=\"{y:.1}\" y2=\"{y:.1}\"/>");
        let _ = write!(svg, "<text x=\"{}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>", l - 6.0, y + 4.0, esc(&axis_label(v, fmt)));
        v += step;
    }
    let _ = write!(svg, "<line class=\"axis\" x1=\"{l}\" x2=\"{r}\" y1=\"{bottom}\" y2=\"{bottom}\"/>");
    for (t, label) in x_ticks(x0, x1) {
        let x = sx(t);
        let _ = write!(svg, "<line class=\"axis\" x1=\"{x:.1}\" x2=\"{x:.1}\" y1=\"{bottom}\" y2=\"{}\"/>", bottom + 4.0);
        let _ = write!(svg, "<text x=\"{x:.1}\" y=\"{}\" text-anchor=\"middle\">{}</text>", bottom + 18.0, esc(&label));
    }
    for s in series {
        let pts: Vec<String> = s.points.iter().map(|(x, y)| format!("{:.1},{:.1}", sx(*x), sy(*y))).collect();
        let class = if s.dashed { "line dash" } else { "line" };
        let _ = write!(svg, "<polyline class=\"{class}\" style=\"stroke:{}\" points=\"{}\"/>", s.color, pts.join(" "));
    }
    let _ = write!(svg, "<line class=\"cross\" x1=\"0\" x2=\"0\" y1=\"{top}\" y2=\"{bottom}\"/></svg>");

    let data = serde_json::json!({
        "fmt": match fmt { Fmt::Money => "money", Fmt::Pct => "pct", Fmt::Index => "index" },
        "x0": x0, "x1": x1, "l": l, "r": r,
        "series": series.iter().map(|s| serde_json::json!({
            "label": s.label, "color": s.color,
            "x": s.points.iter().map(|p| p.0).collect::<Vec<_>>(),
            "y": s.points.iter().map(|p| (p.1 * 10_000.0).round() / 10_000.0).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    let legend = if series.len() >= 2 {
        let items: String = series
            .iter()
            .map(|s| format!("<span class=\"{}\" style=\"--c:{}\">{}</span>", if s.dashed { "dash" } else { "" }, s.color, esc(&s.label)))
            .collect();
        format!("<div class=\"legend\">{items}</div>")
    } else {
        String::new()
    };
    format!("{legend}<figure data-chart=\"{}\">{svg}<div class=\"tip\"></div></figure>", esc(&data.to_string()))
}

/// Fall from the running peak, as a fraction (0 at a new high, -0.2 when 20% below it).
fn drawdown(points: &[(i64, f64)]) -> Vec<(i64, f64)> {
    let mut peak = f64::NEG_INFINITY;
    points
        .iter()
        .map(|&(x, v)| {
            peak = peak.max(v);
            (x, if peak > 0.0 { v / peak - 1.0 } else { 0.0 })
        })
        .collect()
}

/// Return per (year, month), from month-end equity; the first month is measured from `start`.
fn monthly_returns(curve: &[(Timestamp, Decimal)], start: Decimal) -> BTreeMap<(i32, u32), f64> {
    let mut month_end: BTreeMap<(i32, u32), Decimal> = BTreeMap::new();
    for (t, v) in curve {
        let d = exchange_date(*t);
        month_end.insert((d.year(), d.month()), *v);
    }
    let mut prev = start;
    let mut out = BTreeMap::new();
    for (k, v) in month_end {
        if let (Some(p), Some(c)) = (prev.to_f64(), v.to_f64())
            && p > 0.0
        {
            out.insert(k, c / p - 1.0);
        }
        prev = v;
    }
    out
}

fn monthly_table(curve: &[(Timestamp, Decimal)], start: Decimal) -> String {
    let returns = monthly_returns(curve, start);
    if returns.is_empty() {
        return String::new();
    }
    let mut html = String::from("<div class=\"scroll\"><table><thead><tr><th>Year</th>");
    for m in ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec", "Year"] {
        let _ = write!(html, "<th>{m}</th>");
    }
    html.push_str("</tr></thead><tbody>");
    let years: Vec<i32> = returns.keys().map(|k| k.0).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let cell = |r: Option<f64>| match r {
        Some(r) => {
            // Shown to 0.1%, so anything that rounds to 0.0% is neither a gain nor a loss.
            let shown = (r * 1000.0).round() / 10.0 + 0.0;
            let class = if shown > 0.0 { "pos" } else if shown < 0.0 { "neg" } else { "" };
            format!("<td class=\"{class}\">{shown:.1}%</td>")
        }
        None => "<td></td>".into(),
    };
    for y in years {
        let _ = write!(html, "<tr><td>{y}</td>");
        let mut year = 1.0;
        for m in 1..=12 {
            let r = returns.get(&(y, m)).copied();
            if let Some(r) = r {
                year *= 1.0 + r;
            }
            html.push_str(&cell(r));
        }
        html.push_str(&cell(Some(year - 1.0)));
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table></div>");
    html
}

fn metrics_table(rows: &[(String, &Metrics, Option<f64>)]) -> String {
    let mut html = String::from(
        "<div class=\"scroll\"><table><thead><tr><th></th><th>Start</th><th>End</th><th>Total return</th><th>CAGR</th>\
         <th>vs benchmark</th><th>Max drawdown</th><th>Sharpe</th><th>Fills</th></tr></thead><tbody>",
    );
    for (label, m, excess) in rows {
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.2}</td><td>{}</td></tr>",
            esc(label),
            money(m.start_equity),
            money(m.end_equity),
            pct(m.total_return),
            pct(m.cagr),
            excess.map_or("-".into(), |e| format!("{:+.2}%", e * 100.0)),
            pct(-m.max_drawdown.abs() + 0.0),
            m.sharpe,
            m.num_fills
        );
    }
    html.push_str("</tbody></table></div>");
    html
}

/// "avg dollar volume 123 below minimum" and "... 456 ..." group together.
fn reason_kind(reason: &str) -> String {
    let mut out = String::new();
    let mut in_number = false;
    for c in reason.chars() {
        if c.is_ascii_digit() || (in_number && (c == '.' || c == ',')) {
            if !in_number {
                out.push('#');
            }
            in_number = true;
        } else {
            in_number = false;
            out.push(c);
        }
    }
    out
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title><style>{STYLE}</style></head><body><main>{body}</main><script>{SCRIPT}</script></body></html>",
        esc(title)
    )
}

fn span(curve: &[(Timestamp, Decimal)]) -> String {
    match (curve.first(), curve.last()) {
        (Some(a), Some(b)) => format!("{} to {}", exchange_date(a.0), exchange_date(b.0)),
        _ => "no sessions".into(),
    }
}

/// Says which part of history a report covers: the held-back period, or everything before it.
fn window_banner(window: &Window) -> String {
    match (window.holdout, window.trade_from, window.end) {
        (true, Some(from), _) => format!(
            "<div class=\"banner\"><b>Held-back period from {from}.</b> These results are from data the plan was not \
             developed on. Each look is logged; a result looked at more than once, or tuned after looking, is no \
             longer an unbiased test.</div>"
        ),
        (false, _, Some(end)) => format!("<p class=\"sub\">Development period, through {end}. Later data is held back.</p>"),
        _ => String::new(),
    }
}

const SYNTHETIC: &str = "<div class=\"banner\"><b>Synthetic prices.</b> These results come from generated data and only show \
    that the plan runs; they say nothing about how it would trade on the real market.</div>";

/// One plan's backtest: summary against the benchmark, equity and drawdown charts, monthly returns,
/// recent fills, rejected signals by reason, and the plan file itself.
pub fn plan_report(plan: &Plan, source: &str, run: &Run, synthetic: bool, window: &Window) -> String {
    let report = &run.report;
    let m = &report.metrics;
    let bench_name = plan.backtest.benchmark.to_ascii_uppercase();
    let mut body = format!("<h1>{}</h1><p class=\"sub\">{} · {} · {} bars</p>", esc(&plan.name), esc(&span(&report.equity_curve)), plan.mode, run.bars);
    if !plan.notes.trim().is_empty() {
        let _ = write!(body, "<p class=\"notes\">{}</p>", esc(plan.notes.trim()));
    }
    if synthetic {
        body.push_str(SYNTHETIC);
    }
    body.push_str(&window_banner(window));

    let mut rows = vec![(plan.name.clone(), m, run.benchmark.as_ref().map(|(_, b)| m.cagr - b.cagr))];
    if let Some((_, b)) = &run.benchmark {
        rows.push((format!("{bench_name} buy & hold"), b, None));
    }
    let _ = write!(body, "<h2>Summary</h2><section class=\"card\">{}</section>", metrics_table(&rows));

    let mut series = vec![Series { label: plan.name.clone(), color: color(0), dashed: false, points: to_points(&report.equity_curve, 1.0) }];
    if let Some((curve, _)) = &run.benchmark {
        series.push(Series { label: format!("{bench_name} buy & hold"), color: color(1), dashed: false, points: to_points(curve, 1.0) });
    }
    let _ = write!(body, "<h2>Equity</h2><section class=\"card\">{}</section>", line_chart(&series, Fmt::Money, false));
    let dd: Vec<Series> = series
        .iter()
        .map(|s| Series { label: s.label.clone(), color: s.color, dashed: s.dashed, points: drawdown(&s.points) })
        .collect();
    let _ = write!(body, "<h2>Drawdown</h2><section class=\"card\">{}</section>", line_chart(&dd, Fmt::Pct, true));
    let _ = write!(body, "<h2>Monthly returns</h2><section class=\"card\">{}</section>", monthly_table(&report.equity_curve, m.start_equity));

    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    for r in &report.rejections {
        *reasons.entry(reason_kind(&r.reason)).or_default() += 1;
    }
    if !reasons.is_empty() {
        let mut sorted: Vec<(String, usize)> = reasons.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let rows: String = sorted.iter().map(|(r, n)| format!("<tr><td>{}</td><td>{n}</td></tr>", esc(r))).collect();
        let _ = write!(
            body,
            "<h2>Rejected signals</h2><section class=\"card\"><div class=\"scroll\"><table><thead><tr><th>Reason</th><th>Count</th></tr></thead><tbody>{rows}</tbody></table></div></section>"
        );
    }

    let fills = &report.fills;
    if !fills.is_empty() {
        let shown = fills.len().min(100);
        let rows: String = fills[fills.len() - shown..]
            .iter()
            .rev()
            .map(|f| {
                let side = if f.side == Side::Buy { "Buy" } else { "Sell" };
                format!(
                    "<tr><td>{}</td><td>{}</td><td>{side}</td><td>{}</td><td>{}</td></tr>",
                    exchange_date(f.at),
                    esc(f.symbol.as_str()),
                    f.qty.normalize(),
                    f.price.round_dp(2)
                )
            })
            .collect();
        let _ = write!(
            body,
            "<h2>Fills</h2><p class=\"sub\">The latest {shown} of {}, newest first.</p><section class=\"card\"><div class=\"scroll\"><table><thead><tr><th>Date</th><th>Symbol</th><th>Side</th><th>Qty</th><th>Price</th></tr></thead><tbody>{rows}</tbody></table></div></section>",
            fills.len()
        );
    }
    let _ = write!(body, "<h2>Plan file</h2><pre>{}</pre>", esc(source));
    page(&format!("{} backtest", plan.name), &body)
}

/// Several plans over the same dates: growth of 100 for each and the benchmark, and a summary table.
pub fn compare_report(results: &[(&Plan, Run)], synthetic: bool, window: &Window) -> String {
    // The span the results actually cover, which the requested window may extend past.
    let range = results.first().map_or("no data".to_string(), |(_, r)| span(&r.report.equity_curve));
    let mut body = format!("<h1>Plan comparison</h1><p class=\"sub\">{} plans · {}</p>", results.len(), esc(&range));
    if synthetic {
        body.push_str(SYNTHETIC);
    }
    body.push_str(&window_banner(window));
    if results.len() > SLOTS {
        let _ = write!(body, "<p class=\"sub\">Charting the first {SLOTS} plans; all are in the table.</p>");
    }
    let mut rows = Vec::new();
    let mut series = Vec::new();
    for (i, (plan, run)) in results.iter().enumerate() {
        let m = &run.report.metrics;
        rows.push((plan.name.clone(), m, run.benchmark.as_ref().map(|(_, b)| m.cagr - b.cagr)));
        if i < SLOTS {
            let scale = m.start_equity.to_f64().filter(|s| *s > 0.0).map_or(1.0, |s| 100.0 / s);
            series.push(Series { label: plan.name.clone(), color: color(i), dashed: false, points: to_points(&run.report.equity_curve, scale) });
        }
    }
    // The first plan's benchmark stands for all of them when they share one.
    let shared = results.iter().all(|(p, _)| p.backtest.benchmark.eq_ignore_ascii_case(&results[0].0.backtest.benchmark));
    if let Some((plan, run)) = results.first()
        && shared
        && let Some((curve, b)) = &run.benchmark
    {
        let label = format!("{} buy & hold", plan.backtest.benchmark.to_ascii_uppercase());
        let scale = b.start_equity.to_f64().filter(|s| *s > 0.0).map_or(1.0, |s| 100.0 / s);
        series.push(Series { label: label.clone(), color: "var(--bench)", dashed: true, points: to_points(curve, scale) });
        rows.push((label, b, None));
    }
    let _ = write!(body, "<h2>Summary</h2><section class=\"card\">{}</section>", metrics_table(&rows));
    let _ = write!(body, "<h2>Growth of 100</h2><section class=\"card\">{}</section>", line_chart(&series, Fmt::Index, false));
    for (plan, _) in results {
        if !plan.notes.trim().is_empty() {
            let _ = write!(body, "<h2>{}</h2><p class=\"notes\">{}</p>", esc(&plan.name), esc(plan.notes.trim()));
        }
    }
    page("Plan comparison", &body)
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(esc("<a href=\"x\">&</a>"), "&lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;");
        assert_eq!(money(dec!(1234567.6)), "$1,234,568");
        assert_eq!(money(dec!(-950)), "-$950");
        assert_eq!(nice_step(87.0, 4.0), 50.0);
        assert_eq!(nice_step(0.37, 4.0), 0.1);
        assert_eq!(reason_kind("avg dollar volume 1234567 below minimum"), "avg dollar volume # below minimum");
        assert_eq!(reason_kind("traded price 4.25 below minimum 5"), "traded price # below minimum #");
        let dd = drawdown(&[(0, 100.0), (1, 120.0), (2, 90.0), (3, 130.0)]);
        assert_eq!(dd.iter().map(|p| (p.1 * 100.0).round()).collect::<Vec<_>>(), vec![0.0, 0.0, -25.0, 0.0]);
    }

    #[test]
    fn monthly_returns_chain_from_the_start() {
        let at = |m, d| Utc.with_ymd_and_hms(2025, m, d, 21, 0, 0).unwrap();
        let curve = vec![(at(1, 10), dec!(105)), (at(1, 31), dec!(110)), (at(2, 28), dec!(99))];
        let r = monthly_returns(&curve, dec!(100));
        assert!((r[&(2025, 1)] - 0.10).abs() < 1e-9);
        assert!((r[&(2025, 2)] + 0.10).abs() < 1e-9);
    }

    #[test]
    fn chart_has_axes_series_legend_and_escaped_data() {
        let s = |label: &str, slot| Series { label: label.into(), color: color(slot), dashed: false, points: vec![(0, 1.0), (86_400 * 400, 3.0)] };
        let html = line_chart(&[s("a<b", 0), s("bench", 1)], Fmt::Index, false);
        assert!(html.contains("<polyline") && html.contains("class=\"legend\""));
        assert!(html.contains("a&lt;b") && !html.contains("a<b"));
        assert!(line_chart(&[s("only", 0)], Fmt::Index, false).find("class=\"legend\"").is_none(), "one series, no legend");
    }
}
