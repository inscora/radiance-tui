// radiance-tui — live radiance engine stats in the Linux console,
// in the spirit of `strata monitor`.
//
// Polls the engine's /health (unauthenticated) and /stats (API key) endpoints
// and renders a single-screen ratatui dashboard: speed, KV cache, prefix/linear
// cache hits, draft acceptance, GPU cards, and the live request table.

use std::collections::VecDeque;
use std::io;
use std::sync::mpsc::{self};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Sparkline, Table};
use ratatui::Frame;
use serde::Deserialize;

// ---------------------------------------------------------------- CLI ----

#[derive(Parser, Debug, Clone)]
#[command(name = "radiance-tui", about = "Live radiance engine stats in the console")]
struct Cli {
    /// Engine base URL
    #[arg(short, long, default_value = "http://127.0.0.1:8080")]
    url: String,

    /// API key (falls back to $RADIANCE_API_KEY)
    #[arg(short, long, env = "RADIANCE_API_KEY", default_value = "")]
    key: String,

    /// Poll interval in milliseconds
    #[arg(short, long, default_value_t = 250)]
    interval: u64,

    /// Print one snapshot as text and exit (no TUI)
    #[arg(long)]
    once: bool,
}

// ------------------------------------------------------------ schema ----

#[derive(Deserialize, Clone, Default)]
struct Health {
    #[serde(default)]
    status: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    in_flight: u64,
    #[serde(default)]
    queue_cap: u64,
}

#[derive(Deserialize, Clone)]
struct Card {
    index: u32,
    name: String,
    vram_used: f64,
    vram_total: f64,
}

#[derive(Deserialize, Clone)]
struct ReqRow {
    id: u64,
    state: String,
    prompt: u64,
    computed: u64,
    cached: u64,
    output: u64,
    max: u64,
    ctx: u64,
    #[serde(default)]
    kv_tokens: u64,
    #[serde(default)]
    slot: u64,
    age_s: f64,
    #[serde(default)]
    ttft_s: Option<f64>,
    #[serde(default)]
    prefill_s: Option<f64>,
    #[serde(default)]
    tps: Option<f64>,
}

#[derive(Deserialize, Clone, Default)]
struct Experts {
    #[serde(default)]
    n_layers: u32,
    #[serde(default)]
    n_experts: u32,
    #[serde(default)]
    promotions: u64,
    #[serde(default)]
    demotions: u64,
    #[serde(default)]
    starved_slab: u64,
}

#[derive(Deserialize, Clone)]
struct Stats {
    model: String,
    version: String,
    uptime_s: u64,
    running: u64,
    waiting: u64,
    preempted: u64,
    kv_util: f64,
    kv_tokens_used: u64,
    kv_tokens_total: u64,
    kv_bytes_used: f64,
    kv_bytes_carved: f64,
    prefix_hit_rate: f64,
    linear_hit_rate: f64,
    prefill_tps: f64,
    decode_tps: f64,
    draft_accept: f64,
    preemptions: u64,
    total_requests: u64,
    cards: Vec<Card>,
    requests: Vec<ReqRow>,
    #[serde(default)]
    experts: Experts,
}

#[derive(Default)]
struct Snapshot {
    health: Option<Health>,
    stats: Option<Stats>,
    error: Option<String>,
    need_key: bool,
}

// ------------------------------------------------------------- poller ----

fn poll(cli: &Cli, tx: &mpsc::Sender<Snapshot>) {
    let agent = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build();
    let agent = match agent {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Snapshot {
                error: Some(format!("http client: {e}")),
                ..Default::default()
            });
            return;
        }
    };

    let mut snap = Snapshot::default();

    // /health needs no key and tells us the engine is there even before auth.
    match agent.get(format!("{}/health", cli.url)).send() {
        Ok(r) => match r.json::<Health>() {
            Ok(h) => snap.health = Some(h),
            Err(e) => snap.error = Some(format!("/health: {e}")),
        },
        Err(e) => snap.error = Some(format!("{}/health: {e}", cli.url)),
    }

    let mut req = agent.get(format!("{}/stats", cli.url));
    if !cli.key.is_empty() {
        req = req.bearer_auth(&cli.key);
    }
    match req.send() {
        Ok(r) if r.status().as_u16() == 401 => snap.need_key = true,
        Ok(r) => match r.json::<Stats>() {
            Ok(s) => snap.stats = Some(s),
            Err(e) => snap.error = Some(format!("/stats: {e}")),
        },
        Err(e) => snap.error = Some(format!("{}/stats: {e}", cli.url)),
    }

    let _ = tx.send(snap);
}

// ------------------------------------------------------------ format ----

fn human_bytes(b: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    let u = UNITS[i];
    if i == 0 {
        format!("{v:.0} {u}")
    } else {
        format!("{v:.1} {u}")
    }
}

fn human_uptime(s: u64) -> String {
    let (d, rem) = (s / 86400, s % 86400);
    let (h, rem) = (rem / 3600, rem % 3600);
    let (m, sec) = (rem / 60, rem % 60);
    if d > 0 {
        format!("{d}d {h:02}h {m:02}m")
    } else if h > 0 {
        format!("{h}h {m:02}m {sec:02}s")
    } else {
        format!("{m}m {sec:02}s")
    }
}

fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

// ------------------------------------------------------------- render ----

// Sampled ring buffers for the speed sparklines. Two tricks make short queries
// visible: samples arrive at the poll rate (4 Hz by default, so sub-second
// bursts aren't missed), and each new sample PEAK-HOLDS the previous one with
// a slow decay — a one-off burst paints a visible band that fades out instead
// of being a single 1-px column lost in a 60 s window.
struct History {
    decode: VecDeque<u64>,
    prefill: VecDeque<u64>,
}

impl History {
    fn new() -> Self {
        let len = 240; // 60 s at the 250 ms default poll
        History {
            decode: vec![0; len].into(),
            prefill: vec![0; len].into(),
        }
    }
    fn push(&mut self, s: &Stats) {
        for (q, instant) in [
            (&mut self.decode, (s.decode_tps * 10.0) as u64),
            (&mut self.prefill, s.prefill_tps as u64),
        ] {
            let held = q
                .back()
                .map(|&prev| (prev * 93 / 100).max(instant))
                .unwrap_or(instant);
            q.pop_front();
            q.push_back(held);
        }
    }
}

fn centered_rect(frac_w: f64, frac_h: f64, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .margin(0)
        .constraints([
            Constraint::Percentage(((1.0 - frac_h) * 50.0) as u16),
            Constraint::Percentage((frac_h * 100.0) as u16),
            Constraint::Percentage(((1.0 - frac_h) * 50.0) as u16),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(((1.0 - frac_w) * 50.0) as u16),
            Constraint::Percentage((frac_w * 100.0) as u16),
            Constraint::Percentage(((1.0 - frac_w) * 50.0) as u16),
        ])
        .split(v[1])[1]
}

fn ui(
    f: &mut Frame,
    snap: &Snapshot,
    hist: &History,
    paused: bool,
    last_ok: Option<Instant>,
) {
    if snap.stats.is_none() {
        // Nothing parsed yet: health banner or a modal error box.
        let msg = if snap.need_key {
            vec![
                Line::from("No stats without an API key."),
                Line::from(""),
                Line::from("  radiance-tui --url <engine-url> --key <api-key>   (or set RADIANCE_API_KEY)"),
                Line::from(""),
                Line::from("Waiting for a valid key..."),
            ]
        } else if let Some(e) = &snap.error {
            vec![
                Line::from(Span::styled(
                    "Engine not answering:",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(e.clone()),
                Line::from(""),
                Line::from("Retrying..."),
            ]
        } else {
            vec![Line::from("Connecting...")]
        };
        let area = centered_rect(0.7, 0.4, f.area());
        f.render_widget(ratatui::widgets::Clear, area);
        f.render_widget(
            Paragraph::new(msg).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" radiance-tui ")
                    .border_style(Style::default().fg(if snap.need_key {
                        Color::Yellow
                    } else {
                        Color::Red
                    })),
            ),
            area,
        );
        return;
    }

    let s = snap.stats.as_ref().unwrap();
    let alive = snap
        .health
        .as_ref()
        .map(|h| h.status == "ok")
        .unwrap_or(false);

    let top = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Length(6),  // speed + drafts
            Constraint::Length(7),  // kv + caches
            Constraint::Min(7),     // cards / experts / requests
            Constraint::Length(1),  // footer
        ])
        .split(f.area());

    // ---- header ----
    let stale = last_ok.map(|t| t.elapsed() > Duration::from_secs(5)).unwrap_or(true);
    let engine = match (alive, stale) {
        (true, false) => Span::styled("● serving", Style::default().fg(Color::Green)),
        (true, true) => Span::styled("● stale", Style::default().fg(Color::Yellow)),
        _ => Span::styled("● down", Style::default().fg(Color::Red)),
    };
    let in_flight = snap
        .health
        .as_ref()
        .map(|h| format!("{}/{}", h.in_flight, h.queue_cap))
        .unwrap_or_else(|| format!("{}/?", s.running + s.waiting));
    let hdr = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" {} ", s.model),
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan),
        ),
        Span::styled(format!("v{}  ", s.version), Style::default().fg(Color::DarkGray)),
        engine,
        Span::raw(format!(
            "  up {}  reqs {}  running {}  waiting {}  queue {}",
            human_uptime(s.uptime_s),
            s.total_requests,
            s.running,
            s.waiting,
            in_flight
        )),
    ]));
    f.render_widget(hdr.block(Block::default().borders(Borders::ALL).title(" radiance ")), top[0]);

    // ---- speed row ----
    let speed = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ])
        .split(top[1]);

    // Sparkline only renders the first `width` samples it's given, so hand it
    // the LAST window_width samples; LeftToRight then puts the newest bar on
    // the right edge and fresh activity shows up immediately.
    let win = (speed[0].width.saturating_sub(2)) as usize;
    let dec: Vec<u64> = hist.decode.iter().rev().take(win).rev().cloned().collect();
    let pre: Vec<u64> = hist.prefill.iter().rev().take(win).rev().cloned().collect();
    f.render_widget(
        Sparkline::default()
            .direction(ratatui::widgets::RenderDirection::LeftToRight)
            .data(dec)
            .style(Style::default().fg(Color::Green))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" decode {:.0} tok/s ", s.decode_tps)),
            ),
        speed[0],
    );
    f.render_widget(
        Sparkline::default()
            .direction(ratatui::widgets::RenderDirection::LeftToRight)
            .data(pre)
            .style(Style::default().fg(Color::Blue))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" prefill {:.0} tok/s ", s.prefill_tps)),
            ),
        speed[1],
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!(" draft accept   {}", pct(s.draft_accept))),
            Line::from(format!(" preemptions    {}", s.preemptions)),
            Line::from(format!(" preempted now  {}", s.preempted)),
        ])
        .block(Block::default().borders(Borders::ALL).title(" engine ")),
        speed[2],
    );

    // ---- kv + cache row ----
    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ])
        .split(top[2]);

    let kv_ratio = s.kv_util.clamp(0.0, 1.0);
    let kv_color = match kv_ratio {
        x if x > 0.9 => Color::Red,
        x if x > 0.7 => Color::Yellow,
        _ => Color::Green,
    };
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(kv_color))
            .ratio(kv_ratio)
            .label(format!(
                "tokens {}/{} · {}",
                s.kv_tokens_used,
                s.kv_tokens_total,
                pct(s.kv_util)
            ))
            .block(Block::default().borders(Borders::ALL).title(" kv cache ")),
        mid[0],
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                " vram {}/{}",
                human_bytes(s.kv_bytes_used),
                human_bytes(s.kv_bytes_carved)
            )),
            Line::from(format!(" prefix hit    {}", pct(s.prefix_hit_rate))),
            Line::from(format!(" linear hit    {}", pct(s.linear_hit_rate))),
        ])
        .block(Block::default().borders(Borders::ALL).title(" cache ")),
        mid[1],
    );
    let resident = s
        .experts
        .n_experts
        .saturating_mul(s.experts.n_layers);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                " experts {}×{} ({} total)",
                s.experts.n_layers, s.experts.n_experts, resident
            )),
            Line::from(format!(
                " promote {} / demote {}",
                s.experts.promotions, s.experts.demotions
            )),
            Line::from(format!(" starved slab  {}", s.experts.starved_slab)),
        ])
        .block(Block::default().borders(Borders::ALL).title(" experts ")),
        mid[2],
    );

    // ---- cards + requests ----
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(top[3]);

    let mut card_lines = Vec::new();
    for c in &s.cards {
        let r = if c.vram_total > 0.0 {
            c.vram_used / c.vram_total
        } else {
            0.0
        };
        let filled = (r * 28.0).round() as usize;
        let bar: String = format!("{}{}", "█".repeat(filled), "░".repeat(28 - filled));
        card_lines.push(Line::from(format!(" #{} {}", c.index, c.name)));
        card_lines.push(Line::from(Span::styled(
            format!(" {bar} {}", pct(r)),
            Style::default().fg(if r > 0.95 {
                Color::Red
            } else if r > 0.85 {
                Color::Yellow
            } else {
                Color::Green
            }),
        )));
        card_lines.push(Line::from(format!(
             "   {}/{}",
            human_bytes(c.vram_used),
            human_bytes(c.vram_total)
        )));
    }
    f.render_widget(
        Paragraph::new(card_lines).block(Block::default().borders(Borders::ALL).title(" cards ")),
        bottom[0],
    );

    let header = Row::new(vec![
        "id", "state", "slot", "prompt", "cached", "out/max", "ctx|comp", "kv", "ttft", "pre",
        "tps", "age",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan));
    let rows: Vec<Row> = s
        .requests
        .iter()
        .map(|r| {
            let state_style = match r.state.as_str() {
                "running" | "decoding" => Style::default().fg(Color::Green),
                "prefill" => Style::default().fg(Color::Blue),
                "waiting" | "queued" => Style::default().fg(Color::Yellow),
                "done" => Style::default().fg(Color::DarkGray),
                _ => Style::default().fg(Color::Magenta),
            };
            Row::new(vec![
                r.id.to_string(),
                r.state.clone(),
                r.slot.to_string(),
                r.prompt.to_string(),
                r.cached.to_string(),
                format!("{}/{}", r.output, r.max),
                format!("{}|{}", r.ctx, r.computed),
                r.kv_tokens.to_string(),
                r.ttft_s.map(|x| format!("{x:.2}s")).unwrap_or_default(),
                r.prefill_s.map(|x| format!("{x:.2}s")).unwrap_or_default(),
                r.tps.map(|x| format!("{x:.0}")).unwrap_or_default(),
                format!("{:.0}s", r.age_s),
            ])
            .style(state_style)
        })
        .collect();
    let widths = [
        Constraint::Length(5),
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Length(11),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(5),
    ];
    let empty = s.requests.is_empty();
    let note = if empty {
        vec![Row::new(vec![Cell::from("no requests")]).style(Style::default().fg(Color::DarkGray))]
    } else {
        rows
    };
    f.render_widget(
        Table::new(note, widths)
            .header(header)
            .column_spacing(1)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" requests ({}) ", s.requests.len())),
            ),
        bottom[1],
    );

    // ---- footer ----
    let foot = Paragraph::new(Line::from(vec![
        Span::styled(" q ", Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::raw(" quit   "),
        Span::styled(" space ", Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::raw(if paused {
            " resume (paused) "
        } else {
            " freeze frame "
        }),
        Span::styled(" s ", Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::raw(" poll now "),
    ]));
    f.render_widget(foot, top[4]);
}

// --------------------------------------------------------------- main ----

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if cli.once {
        let (tx, rx) = mpsc::channel();
        poll(&cli, &tx);
        let snap = rx.recv_timeout(Duration::from_secs(8))?;
        return print_once(&snap);
    }

    let (tx0, rx) = mpsc::channel::<Snapshot>();
    let tx = tx0.clone();
    thread::spawn({
        let cli = cli.clone();
        move || loop {
            poll(&cli, &tx0);
            thread::sleep(Duration::from_millis(cli.interval));
        }
    });

    ratatui::run(|terminal| {
        let mut snap = Snapshot::default();
        let mut hist = History::new();
        let mut paused = false;
        let mut last_ok: Option<Instant> = None;
        loop {
            // Drain to the newest frame (old frames would only lag).
            while let Ok(s) = rx.try_recv() {
                let fresh = s.stats.is_some();
                if !paused {
                    if let Some(st) = &s.stats {
                        hist.push(st);
                    }
                    snap = s;
                    if fresh {
                        last_ok = Some(Instant::now());
                    }
                }
            }

            terminal.draw(|f| ui(f, &snap, &hist, paused, last_ok))?;

            let wait = if paused {
                Duration::from_millis(200)
            } else {
                Duration::from_millis(cli.interval.min(250))
            };
            if event::poll(wait)? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char(' ') => paused = !paused,
                        KeyCode::Char('s') => poll(&cli, &tx),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        Ok(())
    })
}

fn print_once(snap: &Snapshot) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let out = io::stdout();
    let mut w = out.lock();
    if let Some(h) = &snap.health {
        writeln!(
            w,
            "health: {} model={} in_flight={} queue_cap={}",
            h.status, h.model, h.in_flight, h.queue_cap
        )?;
    }
    if snap.need_key {
        writeln!(w, "stats: 401 — pass --key or set RADIANCE_API_KEY")?;
        return Ok(());
    }
    if let Some(e) = &snap.error {
        writeln!(w, "error: {e}")?;
    }
    if let Some(s) = &snap.stats {
        writeln!(w, "stats:  model={} v={} up={}", s.model, s.version, human_uptime(s.uptime_s))?;
        writeln!(
            w,
            "        running={} waiting={} total={} prefill={:.0} tok/s decode={:.0} tok/s draft_accept={}",
            s.running, s.waiting, s.total_requests, s.prefill_tps, s.decode_tps, pct(s.draft_accept)
        )?;
        writeln!(
            w,
            "        kv {} tokens {}/{} vram {}/{} prefix_hit={} linear_hit={}",
            pct(s.kv_util),
            s.kv_tokens_used,
            s.kv_tokens_total,
            human_bytes(s.kv_bytes_used),
            human_bytes(s.kv_bytes_carved),
            pct(s.prefix_hit_rate),
            pct(s.linear_hit_rate)
        )?;
        for c in &s.cards {
            writeln!(
                w,
                "card {}  {}  {}/{}",
                c.index,
                c.name,
                human_bytes(c.vram_used),
                human_bytes(c.vram_total)
            )?;
        }
        for r in &s.requests {
            writeln!(
                w,
                "req {}: {} prompt={} cached={} output={} tps={}",
                r.id,
                r.state,
                r.prompt,
                r.cached,
                r.output,
                r.tps.map(|x| format!("{x:.0}")).unwrap_or_else(|| "-".into())
            )?;
        }
    }
    Ok(())
}
