// kartouche — copy this file into your project; https://github.com/kamilomelo/kartouche
// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Kamilo Melo
// KARTOUCHE_TEMPLATE_VERSION: 2
#![allow(dead_code)]

// kartouche:config begin
const NAME: &str = "fleetty";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DESCRIPTION: &str = "your fleet, one terminal";
const ACCENT: &str = "38;5;244";
const APPEND_ONLY: bool = true;
const GLYPHS: &str = "auto";
// kartouche:config end

// kartouche:code begin v2
use std::io::{self, IsTerminal, Write};
use std::time::Instant;

#[derive(Clone, Copy)]
pub enum Stream { Stdout, Stderr }
#[derive(Clone, Copy)]
pub enum Style { Inline, Dots, Crumbs, Rail, Line, Log }

fn terminal(stream: Stream) -> bool {
    match stream { Stream::Stdout => io::stdout().is_terminal(), Stream::Stderr => io::stderr().is_terminal() }
}
fn glyph(c: char) -> char {
    if GLYPHS == "ascii" || (GLYPHS == "auto" && matches!(std::env::var("TERM").as_deref(), Ok("linux" | "dumb"))) {
        let unicode = "╭╮╰╯─│├❯●○◉›━✓✗╸·";
        let ascii = ",.`'-||>*o@>=+x=.";
        unicode.chars().position(|x| x == c).and_then(|i| ascii.chars().nth(i)).unwrap_or(c)
    } else { c }
}
fn g(text: &str) -> String { text.chars().map(glyph).collect() }
fn clean(text: &str) -> String { text.replace('\n', " ") }
fn paint(text: &str, role: &str, stream: Stream) -> String {
    if text.is_empty() || !terminal(stream) || std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty()) { return text.into(); }
    let code = match role { "dim" => "2", "bold" => "1", "accent" => ACCENT, "both" => return format!("\x1b[1;{ACCENT}m{text}\x1b[0m"), "green" => "32", _ => "31" };
    format!("\x1b[{code}m{text}\x1b[0m")
}
fn version(v: &str) -> String { let v = clean(v); if v.starts_with('v') { v } else { format!("v{v}") } }
fn prefix(c: char, stream: Stream) -> String { format!("{} ", paint(&format!("{}{}", glyph(c), glyph('─')), "dim", stream)) }
fn tail(stream: Stream) -> String { format!("{} ", paint(&g("╰─❯"), "dim", stream)) }

pub fn render_title(name: &str, v: &str, stream: Stream) -> String {
    let suffix = if v.is_empty() { String::new() } else { format!("  {}", paint(&version(v), "dim", stream)) };
    format!("{} {} {}{suffix}", paint(&g("╭─"), "dim", stream), paint(&g("●"), "accent", stream), paint(&clean(name), "bold", stream))
}
pub fn render_banner(name: &str, v: &str, description: &str, status: Option<&str>, stream: Stream) -> String {
    let mut lines = vec![render_title(name, v, stream)];
    if let Some(status) = status.filter(|s| !s.is_empty()) {
        let head = if description.is_empty() { "╰─❯" } else { "├─" };
        lines.push(format!("{} {}", paint(&g(head), "dim", stream), clean(status)));
    }
    if !description.is_empty() { lines.push(format!("{} {}", paint(&g("╰─❯"), "dim", stream), clean(description))); }
    lines.join("\n")
}
pub fn render_card(name: &str, v: &str, description: &str, stream: Stream) -> String {
    let (name, desc, v) = (clean(name), clean(description), version(v));
    let left = name.chars().count() + 3;
    let right = v.chars().count() + 4;
    let w = (desc.chars().count() + 4).max(left + right + 2);
    let top = format!("{}{}{}{}{}", paint(&g("╭─ "), "dim", stream), paint(&name, "bold", stream),
        paint(&format!(" {}  ", g(&"─".repeat(w-left-right))), "dim", stream), paint(&v, "dim", stream), paint(&g(" ─╮"), "dim", stream));
    let body = format!("{}  {}{}  {}", paint(&g("│"), "dim", stream), desc, " ".repeat(w-4-desc.chars().count()), paint(&g("│"), "dim", stream));
    format!("{top}\n{body}\n{}", paint(&g(&format!("╰{}╯", "─".repeat(w))), "dim", stream))
}
pub fn render_end(ok: bool, text: &str, stream: Stream) -> String {
    let (symbol, role) = if ok { ("✓", "green") } else { ("✗", "red") };
    format!("{} {} {}", paint(&g("╰─"), "dim", stream), paint(&g(symbol), role, stream), clean(text))
}
pub fn render_frame(style: Style, n: usize, total: usize, label: &str, labels: &[&str], stream: Stream) -> String {
    let style = if matches!(style, Style::Dots) && total > 12 { Style::Line } else { style };
    let count = format!("{}/{}", n.min(total), total);
    let done = n > total;
    let mut lines = Vec::new();
    match style {
        Style::Inline => (),
        Style::Dots => lines.push(format!("{}{}{}  {}", prefix('├', stream), paint(&g(&"●".repeat(n.min(total))), "accent", stream),
            paint(&g(&"○".repeat(total.saturating_sub(n))), "dim", stream), paint(&count, "dim", stream))),
        Style::Crumbs => {
            let parts: Vec<_> = labels.iter().enumerate().map(|(i, x)| paint(&clean(x), if i == n-1 && !done { "both" } else { "dim" }, stream)).collect();
            lines.push(format!("{}{}", prefix('├', stream), parts.join(&format!(" {} ", paint(&g("›"), "dim", stream)))));
        }
        Style::Rail => for (i, x) in labels.iter().enumerate() {
            let (symbol, role) = if i < n-1 || done { ("✓", "green") } else if i == n-1 { ("◉", "accent") } else { ("○", "dim") };
            let label = if i == n-1 && !done { paint(&clean(x), "bold", stream) } else { clean(x) };
            lines.push(format!("{}{} {label}", prefix('├', stream), paint(&g(symbol), role, stream)));
        },
        Style::Line => {
            let filled = (40*n.min(total)+total)/(2*total);
            let bar = if filled == 20 { "━".repeat(20) } else if filled == 0 { String::new() } else { format!("{}╸", "━".repeat(filled-1)) };
            lines.push(format!("{}{}{}  {}", prefix('├', stream), paint(&g(&bar), "accent", stream),
                paint(&g(&"─".repeat(20-filled)), "dim", stream), paint(&count, "dim", stream)));
        }
        Style::Log => {
            let longest = labels.iter().map(|x| clean(x).chars().count()).max().unwrap_or(0);
            for (i, x) in labels.iter().take((n-1).min(total)).enumerate() {
                let label = clean(x);
                lines.push(format!("{}{}/{total} {label} {} {}", prefix('├', stream), i+1,
                    g(&"·".repeat(longest-label.chars().count()+7)), paint("ok", "accent", stream)));
            }
        }
    }
    let last = if done || matches!(style, Style::Rail) { count } else if matches!(style, Style::Dots | Style::Line) { clean(label) } else { format!("{count} {}", clean(label)) };
    lines.push(format!("{}{last}", tail(stream)));
    lines.join("\n")
}
pub fn banner(status: Option<&str>) { let _ = writeln!(io::stderr(), "{}", render_banner(NAME, VERSION, DESCRIPTION, status, Stream::Stderr)); }
pub fn card() { let _ = writeln!(io::stderr(), "{}", render_card(NAME, VERSION, DESCRIPTION, Stream::Stderr)); }

pub struct Steps { style: Style, labels: Vec<String>, n: usize, previous: usize, redraw: bool, finished: bool, started: Instant }
impl Steps {
    pub fn start(style: Style, labels: &[&str]) -> Self {
        let mut s = Self { style, labels: labels.iter().map(|x| clean(x)).collect(), n: usize::from(!labels.is_empty()), previous: 0,
            redraw: terminal(Stream::Stderr) && !APPEND_ONLY, finished: false, started: Instant::now() };
        let _ = writeln!(io::stderr(), "{}", render_title(NAME, VERSION, Stream::Stderr));
        if s.n > 0 { s.show(None, false); }
        s
    }
    fn show(&mut self, end: Option<String>, done: bool) {
        if !self.redraw || self.labels.is_empty() {
            if let Some(text) = end { let _ = writeln!(io::stderr(), "{text}"); }
            else if self.n > 0 { let _ = writeln!(io::stderr(), "{}{}/{} {}", prefix('├', Stream::Stderr), self.n, self.labels.len(), self.labels[self.n-1]); }
            return;
        }
        let labels: Vec<_> = self.labels.iter().map(String::as_str).collect();
        let n = if done { labels.len()+1 } else { self.n };
        let mut lines: Vec<_> = render_frame(self.style, n, labels.len(), &labels[self.n-1], &labels, Stream::Stderr).split('\n').map(str::to_owned).collect();
        if let Some(end) = end { *lines.last_mut().unwrap() = end; }
        let mut stderr = io::stderr();
        if self.previous > 0 { let _ = write!(stderr, "\x1b[{}A", self.previous); }
        for line in &lines { let _ = writeln!(stderr, "{}{line}", if self.previous > 0 { "\r\x1b[2K" } else { "" }); }
        self.previous = lines.len();
    }
    pub fn next(&mut self) { if !self.finished && self.n < self.labels.len() { self.n += 1; self.show(None, false); } }
    pub fn done(&mut self, elapsed: Option<&str>) {
        if self.finished { return; } self.finished = true;
        let text = format!("done in {}", elapsed.map(clean).unwrap_or_else(|| format!("{:.1}s", self.started.elapsed().as_secs_f64())));
        self.show(Some(render_end(true, &text, Stream::Stderr)), true);
    }
    pub fn fail(&mut self, reason: Option<&str>) {
        if self.finished { return; } self.finished = true;
        let mut text = if self.labels.is_empty() { "failed".into() } else { format!("failed at {}/{} {}", self.n, self.labels.len(), self.labels[self.n-1]) };
        if let Some(r) = reason.filter(|x| !x.is_empty()) { text.push_str(&format!(" · {}", clean(r))); }
        self.show(Some(render_end(false, &text, Stream::Stderr)), false);
    }
}
impl Drop for Steps {
    fn drop(&mut self) {
        if !self.finished {
            self.finished = true;
            let text = if self.labels.is_empty() { "interrupted".into() } else { format!("interrupted at {}/{} {}", self.n, self.labels.len(), self.labels[self.n-1]) };
            self.show(Some(render_end(false, &text, Stream::Stderr)), false);
        }
    }
}
// kartouche:code end

// Fleetty-only rail outcome renderer. Keep this outside the upstream code
// markers so kartouche's template can be refreshed without losing it.
#[derive(Clone, Copy)]
pub enum RailState { Success, Incomplete, Failed }

pub fn render_results_rail(rows: &[(&str, String, RailState)], stream: Stream) -> String {
    let mut lines = Vec::with_capacity(rows.len() + 1);
    for (name, status, state) in rows {
        let (symbol, role) = match state {
            RailState::Success => ("✓", "green"),
            RailState::Incomplete => ("○", "accent"),
            RailState::Failed => ("✗", "red"),
        };
        lines.push(format!("{}{} {}: {}", prefix('├', stream), paint(&g(symbol), role, stream), clean(name), clean(status)));
    }
    lines.push(format!("{}{}/{} reported", tail(stream), rows.len(), rows.len()));
    lines.join("\n")
}
