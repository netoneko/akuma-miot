//! The look, shared by the real REPL (`client::repl`) and `repl-mock`
//! (which now just drives this module against a canned or a live node).
//! Themes, colour, the banner, the per-effect renderer, the composer's
//! shape. Every renderer here returns a `String` (possibly several lines
//! joined by `\n`) rather than printing — `repl-mock` just `println!`s the
//! result; `client::repl` feeds it to a ratatui inline viewport
//! (`Terminal::insert_before`), which is what actually redraws the
//! terminal.
//!
//! Rules every skin obeys, all from `docs/CLI.md`:
//!  §0 ordinary stdout lines, no alternate screen — the terminal owns
//!     scrollback, select/copy and tmux copy-mode keep working. Ratatui's
//!     `Viewport::Inline` honours this: it reserves a fixed-height region
//!     at the bottom for the composer and scrolls everything else through
//!     normally, unlike a full-screen TUI;
//!  §1 the log grows upward; the composer is pinned last and is redrawn in
//!     place — output already printed never moves. Long lines are wrapped
//!     to the terminal width with a hanging indent, never left to the
//!     terminal's ragged hard wrap;
//!  §2 the prompt shows the resolved target (`root → tama ▸`);
//!  §6 banner once (`akuma_40`), one colour per sender, the `akuma_20`
//!     avatar beside a cat's `said`, no box-drawing around the log.
//!
//! Real line editing (history, kill/yank, `@name`/`/cmd` completion) lives
//! in `client::Input`, driven by raw-mode key events — this module only
//! renders the draft it's given.

use crate::common::Roster;
use std::sync::OnceLock;

const BANNER: &str = include_str!("../../../assets/akuma_40.txt");
const AVATAR: &str = include_str!("../../../assets/akuma_20.txt");

#[derive(Clone, Copy)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub const OFF: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";

fn colour() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

impl Rgb {
    fn fg(self) -> String {
        if colour() { format!("\x1b[38;2;{};{};{}m", self.0, self.1, self.2) } else { String::new() }
    }
    fn lerp(self, o: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb(m(self.0, o.0), m(self.1, o.1), m(self.2, o.2))
    }
}

// ── skins ───────────────────────────────────────────────────────────────

pub struct Theme {
    pub name: &'static str,
    /// Three stops; every gradient (banner, rules, sparkline) rides this.
    ramp: [Rgb; 3],
    /// Banner gradient direction: 0 = rows only, 1 = columns only.
    diagonal: f32,
    accent: Rgb, // labels, prompt glyph, the operator's chop
    done: Rgb,   // closed
    progress: Rgb, // claimed / submitted
    alarm: Rgb,  // directed
    warm: Rgb,   // nudged, stale
    paper: Rgb,
    smoke: Rgb,
    ink: Rgb,
    cats: [(&'static str, Rgb, &'static str); 6], // name, colour, chop
    title: &'static str,
    subtitle: &'static str,
    tagline: &'static str,
    /// What sits under the title, right of the cat.
    vista: Vista,
    rule: &'static str,
    prompt: &'static str,
    cursor: &'static str,
    /// Block numbers: `#1176` or `0x0498`.
    hex_blocks: bool,
    /// Section labels and verbs shouted.
    upper: bool,
    bracket_labels: bool,
}

enum Vista {
    Skyline,
    Glitch,
    Proverb,
}

/// Picks a skin by name (`bund`/`neon`/`ink`, default `bund`). Callers that
/// want an argv override (the mock) set `KOT_THEME` from argv before the
/// first call to [`theme`]; everything else just honours the env var.
pub fn theme() -> &'static Theme {
    static T: OnceLock<Theme> = OnceLock::new();
    T.get_or_init(|| pick(&std::env::var("KOT_THEME").unwrap_or_default()))
}

fn pick(name: &str) -> Theme {
    match name {
        "neon" => NEON,
        "ink" => INK_WASH,
        _ => BUND,
    }
}

/// 外滩 — the Bund after dark. Pearl Tower pink through violet into
/// Lujiazui cyan, gold for the operator.
const BUND: Theme = Theme {
    name: "bund",
    ramp: [Rgb(255, 45, 149), Rgb(177, 124, 255), Rgb(0, 229, 255)],
    diagonal: 0.45,
    accent: Rgb(255, 45, 149),
    done: Rgb(255, 194, 51),
    progress: Rgb(46, 230, 166),
    alarm: Rgb(255, 75, 62),
    warm: Rgb(255, 138, 61),
    paper: Rgb(232, 232, 236),
    smoke: Rgb(140, 146, 160),
    ink: Rgb(78, 82, 96),
    cats: [
        ("root", Rgb(255, 194, 51), "根"),
        ("meow", Rgb(255, 45, 149), "喵"),
        ("tama", Rgb(0, 229, 255), "玉"),
        ("kuro", Rgb(177, 124, 255), "黑"),
        ("sora", Rgb(46, 230, 166), "空"),
        ("mimi", Rgb(255, 138, 61), "咪"),
    ],
    title: "恶魔猫窝",
    subtitle: "· akuma miot",
    tagline: "distributed cat system",
    vista: Vista::Skyline,
    rule: "─",
    prompt: "▸",
    cursor: "▍",
    hex_blocks: false,
    upper: false,
    bracket_labels: false,
};

/// 霓虹 — Lujiazui in the rain. Acid magenta, electric blue, toxic green;
/// hex block ids, shouted verbs, a glitch bar where the skyline was.
const NEON: Theme = Theme {
    name: "neon",
    ramp: [Rgb(255, 0, 200), Rgb(64, 120, 255), Rgb(57, 255, 20)],
    diagonal: 0.85,
    accent: Rgb(255, 0, 200),
    done: Rgb(57, 255, 20),
    progress: Rgb(0, 240, 255),
    alarm: Rgb(255, 60, 90),
    warm: Rgb(255, 210, 0),
    paper: Rgb(220, 240, 255),
    smoke: Rgb(132, 148, 184),
    ink: Rgb(70, 80, 110),
    cats: [
        ("root", Rgb(255, 210, 0), "根"),
        ("meow", Rgb(255, 0, 200), "喵"),
        ("tama", Rgb(0, 240, 255), "玉"),
        ("kuro", Rgb(150, 90, 255), "黑"),
        ("sora", Rgb(57, 255, 20), "空"),
        ("mimi", Rgb(255, 120, 40), "咪"),
    ],
    title: "恶魔猫窝",
    subtitle: "// A K U M A · M I O T",
    tagline: "[ distributed cat system ]",
    vista: Vista::Glitch,
    rule: "═",
    prompt: "❯",
    cursor: "█",
    hex_blocks: true,
    upper: true,
    bracket_labels: true,
};

/// 水墨 — ink wash. Greys, muted earth tones for the cats, one vermilion
/// seal for the operator and the prompt. Shanghai at six in the morning.
const INK_WASH: Theme = Theme {
    name: "ink",
    ramp: [Rgb(230, 228, 222), Rgb(150, 150, 148), Rgb(84, 86, 90)],
    diagonal: 0.3,
    accent: Rgb(204, 60, 48),
    done: Rgb(204, 60, 48),
    progress: Rgb(120, 160, 110),
    alarm: Rgb(204, 60, 48),
    warm: Rgb(196, 160, 90),
    paper: Rgb(226, 224, 218),
    smoke: Rgb(150, 150, 146),
    ink: Rgb(96, 96, 98),
    cats: [
        ("root", Rgb(204, 60, 48), "根"),
        ("meow", Rgb(160, 110, 150), "喵"),
        ("tama", Rgb(92, 107, 192), "玉"),
        ("kuro", Rgb(130, 150, 170), "黑"),
        ("sora", Rgb(120, 160, 110), "空"),
        ("mimi", Rgb(196, 160, 90), "咪"),
    ],
    title: "恶魔猫窝",
    subtitle: "  akuma miot",
    tagline: "distributed cat system",
    vista: Vista::Proverb,
    rule: "╌",
    prompt: "›",
    cursor: "▏",
    hex_blocks: false,
    upper: false,
    bracket_labels: false,
};

// ── paint ───────────────────────────────────────────────────────────────

/// The skin's ramp, `t` in 0..1.
fn ramp(t: f32) -> Rgb {
    let [a, b, c] = theme().ramp;
    if t < 0.5 { a.lerp(b, t * 2.0) } else { b.lerp(c, (t - 0.5) * 2.0) }
}

pub fn paint(c: Rgb, s: &str) -> String {
    format!("{}{s}{OFF}", c.fg())
}
pub fn bold(c: Rgb, s: &str) -> String {
    format!("{BOLD}{}{s}{OFF}", c.fg())
}
pub fn dim(s: &str) -> String {
    paint(theme().smoke, s)
}
fn faint(s: &str) -> String {
    paint(theme().ink, s)
}
pub fn plain(s: &str) -> String {
    paint(theme().paper, s)
}
/// A peer gone stale, a retry, anything that wants attention without being
/// an alarm.
pub fn warn(s: &str) -> String {
    paint(theme().warm, s)
}
/// A hard failure — no quorum, a peer that never answered.
pub fn alert(s: &str) -> String {
    paint(theme().alarm, s)
}
/// A confirmation — submitted, cleared, quorum restored.
pub fn ok(s: &str) -> String {
    paint(theme().progress, s)
}
fn shout(s: &str) -> String {
    if theme().upper { s.to_uppercase() } else { s.to_string() }
}

/// Columns of the terminal we are attached to. Honours `COLUMNS`, then asks
/// `stty` on the inherited stdin, then falls back to 80.
pub fn term_width() -> usize {
    if let Some(c) = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()) {
        return c;
    }
    std::process::Command::new("stty")
        .arg("size")
        .stdin(std::process::Stdio::inherit())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().nth(1).and_then(|c| c.parse().ok()))
        .filter(|&c| c > 20)
        .unwrap_or(80)
}

/// Cells a plain (uncoloured) string occupies: CJK is two wide.
pub fn cells(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if matches!(c as u32, 0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6) {
                2
            } else {
                1
            }
        })
        .sum()
}

/// `s` with its `\x1b[...m` sequences removed — what the terminal will
/// actually place in cells.
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            for d in it.by_ref() {
                if d == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn vcells(s: &str) -> usize {
    cells(&strip_ansi(s))
}

/// Right-pad an already-colored string to `width` display cells — for
/// padding *after* coloring a name through [`who`] (padding a name before
/// `who` would break its cat-color lookup, which matches on the exact name).
pub fn pad(s: &str, width: usize) -> String {
    let w = vcells(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// Word-wrap by display cells, colour codes counting for nothing. A colour
/// that spans a break simply stays on into the next line.
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut w_cells = 0usize;
    for w in s.split_whitespace() {
        let wc = vcells(w);
        let cur = out.last_mut().unwrap();
        if !cur.is_empty() && w_cells + 1 + wc > width {
            w_cells = wc;
            out.push(w.to_string());
        } else {
            if !cur.is_empty() {
                cur.push(' ');
                w_cells += 1;
            }
            w_cells += wc;
            cur.push_str(w);
        }
    }
    out
}

// ── the litter ──────────────────────────────────────────────────────────

/// One colour per sender, forever — that is what makes the scroll readable.
fn cat(name: &str) -> Rgb {
    let t = theme();
    t.cats.iter().find(|(n, _, _)| *n == name).map(|c| c.1).unwrap_or_else(|| {
        let h = name.bytes().fold(7usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize));
        t.cats[1 + h % (t.cats.len() - 1)].1
    })
}
/// Each cat's chop — one hanzi, stamped beside the name where there is room.
fn chop(name: &str) -> &'static str {
    theme().cats.iter().find(|(n, _, _)| *n == name).map(|c| c.2).unwrap_or("猫")
}
pub fn who(name: &str) -> String {
    bold(cat(name), name)
}
/// Name with its chop: `玉 tama`.
pub fn sealed(name: &str) -> String {
    format!("{} {}", paint(cat(name), chop(name)), who(name))
}
pub fn task(id: &str) -> String {
    paint(theme().done, id)
}

/// Message text with every `@name` lit in that cat's colour — in the log
/// and, live, in the composer as you type it.
pub fn tags(body: &str, roster: &Roster) -> String {
    let names: Vec<&str> = roster.names().collect();
    body.split(' ')
        .map(|w| match w.strip_prefix('@') {
            Some(t) => {
                let name = t.trim_end_matches(|c: char| !c.is_alphanumeric());
                if names.contains(&name) {
                    format!("{}{}", bold(cat(name), &format!("@{name}")), plain(&t[name.len()..]))
                } else if ["all", "litter", "cats"].contains(&name) {
                    format!("{}{}", bold(theme().paper, &format!("@{name}")), plain(&t[name.len()..]))
                } else {
                    plain(w)
                }
            }
            None => plain(w),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ── chrome ──────────────────────────────────────────────────────────────

/// A hairline that rides the ramp over `n` cells. The only rule drawn.
fn rule(n: usize) -> String {
    (0..n).map(|i| paint(ramp(i as f32 / n.max(1) as f32), theme().rule)).collect()
}

/// Shade each glyph of `r` by its column along the ramp; spaces stay bare.
fn shade_row(r: &str) -> String {
    let width = r.chars().count().max(1) as f32;
    r.chars().enumerate().map(|(i, ch)| if ch == ' ' { " ".into() } else { paint(ramp(i as f32 / width), &ch.to_string()) }).collect()
}

/// What sits under the title, right of the cat — four rows.
fn vista() -> [String; 4] {
    match theme().vista {
        // Lujiazui from the Bund side; the `◉` is the Pearl.
        Vista::Skyline => [
            shade_row("            ◉         ▲"),
            shade_row("   ▂▃▅ ▇█▇ ▐▌ ▃▆█▆ ▐█▌ ▂▅▇▅"),
            shade_row("▁▂▃▅██▇▅███▇▅██▇█████▅▃▇███▅▃▂▁"),
            faint("≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈") + &dim("  黄浦江"),
        ],
        // Signal bars and a scanline; the rain on the glass.
        Vista::Glitch => [
            shade_row("▚▞▚▚▞▚▞▞▚▞▚▚▞▚▞▚▞▞▚▞▚▚▞▚▞▚▞▞▚▞▚▚"),
            format!("{}  {}", shade_row("▓▒░ SYNC ░▒▓"), dim("live · synced")),
            shade_row("▁▂▃▄▅▆▇█▇▆▅▄▃▂▁▁▂▃▄▅▆▇█▇▆▅▄▃▂▁▁▂"),
            faint("╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌") + &dim("  雨夜"),
        ],
        // Shanghai's own motto, and nothing else.
        Vista::Proverb => [
            String::new(),
            format!("{}  {}", plain("海纳百川"), dim("the sea takes in every river")),
            String::new(),
            faint("〜〜〜〜〜〜〜〜〜〜〜〜〜〜〜〜") + &dim("  黄浦江"),
        ],
    }
}

/// The banner: every glyph lit by its position, so the cat reads as one
/// neon sign rather than stripes. Title and vista to its right.
pub fn banner() -> String {
    let t = theme();
    let lines: Vec<&str> = BANNER.lines().collect();
    let rows = lines.len().max(1) as f32;
    let cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(1) as f32;
    let v = vista();
    let side: Vec<String> = vec![
        format!("{}  {}", bold(t.accent, t.title), dim(t.subtitle)),
        dim(&shout(t.tagline)),
        dim("a litter of models, coordinating on chain"),
        String::new(),
        format!("{} {}", dim("kot"), dim(env!("CARGO_PKG_VERSION"))),
        v[0].clone(),
        v[1].clone(),
        v[2].clone(),
        v[3].clone(),
    ];
    let mut out = Vec::new();
    for (r, l) in lines.iter().enumerate() {
        let mut row = String::new();
        for (c, ch) in l.chars().enumerate() {
            if ch == ' ' {
                row.push(' ');
            } else {
                let pos = (r as f32 / rows) * (1.0 - t.diagonal) + (c as f32 / cols) * t.diagonal;
                row.push_str(&paint(ramp(pos), &ch.to_string()));
            }
        }
        let pad = " ".repeat((42usize).saturating_sub(l.chars().count()));
        let right = side.get(r.wrapping_sub(5)).cloned().unwrap_or_default();
        out.push(format!("{row}{pad}  {right}"));
    }
    out.push(String::new());
    out.join("\n")
}

pub fn kv(zh: &str, en: &str, val: String) -> String {
    let t = theme();
    let label = if t.bracket_labels { format!("[{zh}]") } else { zh.to_string() };
    format!("  {} {} {val}", paint(t.accent, &label), dim(&format!("{:<7}", shout(en))))
}

/// Recent gaps between blocks as bars — the chain's pulse at a glance.
pub fn sparkline(gaps: &[u64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = *gaps.iter().max().unwrap_or(&1) as f32;
    let n = gaps.len().max(1) as f32;
    gaps.iter()
        .enumerate()
        .map(|(i, &g)| {
            let idx = ((g as f32 / max) * 7.0).round() as usize;
            paint(ramp(i as f32 / n), &BARS[idx.min(7)].to_string())
        })
        .collect()
}

pub fn section(zh: &str, en: &str) -> String {
    let t = theme();
    let en = shout(en);
    let lead = if t.bracket_labels { format!("{} ", faint("//")) } else { format!("{} ", rule(3)) };
    let label_w = cells(zh) + 1 + cells(&en);
    let rest = term_width().saturating_sub(2 + vcells(&lead) + label_w + 1).max(3);
    format!("\n  {lead}{} {} {}", bold(t.paper, zh), dim(&en), rule(rest))
}

// ── the log ─────────────────────────────────────────────────────────────

fn block_id(block: u64) -> String {
    if theme().hex_blocks { format!("0x{block:04x}") } else { format!("#{block}") }
}

/// Wall-clock time, plus the gap since the previous event in parentheses:
/// `21:40`, then `21:42 (+3s)`. The first line of a run has no gap.
fn stamp(time: &str, block: u64) -> String {
    let (hm, gap) = time.split_once(' ').unwrap_or((time, ""));
    let mut t = dim(hm);
    if !gap.is_empty() {
        t.push(' ');
        t.push_str(&dim(gap));
    }
    let pad = " ".repeat(TIME_W.saturating_sub(cells(time)));
    format!("  {t}{pad}  {}  ", dim(&format!("{:<6}", block_id(block))))
}

/// The time column: `≈02:20 (+11m30s)` at its widest.
const TIME_W: usize = 16;
/// Cells the stamp occupies: two of margin, the time column, block column
/// and their gaps. Continuation lines hang under the text, not column 0.
const STAMP_W: usize = 2 + TIME_W + 2 + 6 + 2;

/// Free text under a hanging left `indent`, wrapped to the terminal width —
/// what `/tasks` and `/peers` use for a task's own text, matching `obs`'s
/// column without needing a time/block stamp.
pub fn hang(indent: usize, text: &str) -> String {
    let width = term_width().saturating_sub(indent).max(20);
    wrap(text, width)
        .into_iter()
        .enumerate()
        .map(|(i, l)| if i == 0 { l } else { format!("{}{l}", " ".repeat(indent)) })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A protocol observation — the effect in prose, the protocol's own verbs,
/// wrapped to the terminal under its own column.
pub fn obs(time: &str, block: u64, text: String) -> String {
    let width = term_width().saturating_sub(STAMP_W).max(20);
    wrap(&text, width)
        .into_iter()
        .enumerate()
        .map(|(i, l)| if i == 0 { format!("{}{l}", stamp(time, block)) } else { format!("{}{l}", " ".repeat(STAMP_W)) })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The verbs, each with a glyph and a colour so a scroll can be scanned by
/// shape alone: progress, done, a demand, a poke. The words are the
/// protocol's own; only the case is the skin's.
fn verb(glyph: &str, word: &str, c: Rgb) -> String {
    paint(c, &format!("{glyph} {}", shout(word)))
}
pub fn claimed() -> String {
    verb("◇", "claimed", theme().progress)
}
pub fn submitted() -> String {
    verb("◆", "submitted", theme().progress)
}
pub fn closed() -> String {
    verb("✓", "closed", theme().done)
}
pub fn directed(what: &str) -> String {
    bold(theme().alarm, &format!("⚑ {}", shout(what)))
}
pub fn nudged() -> String {
    verb("↻", "nudged", theme().warm)
}

pub fn mesh(text: String) -> String {
    let t = theme();
    let tag = if t.bracket_labels { "[网] MESH" } else { "网 mesh" };
    format!("  {} {}  {text}", " ".repeat(TIME_W), paint(t.cats[3].1, tag))
}

/// A cat speaking: the small Akuma shaded from the sender's colour down
/// into haze, header on its first row, the message flowing beside it.
pub fn said(time: &str, block: u64, from: &str, to: &str, body: &str, roster: &Roster) -> String {
    let art: Vec<&str> = AVATAR.lines().collect();
    let c = cat(from);
    let arrow = if to == "litter" { dim("· to the litter") } else { format!("{} {}", dim("→"), who(to)) };
    let head = format!("{} {arrow}   {}", sealed(from), stamp(time, block).trim_start());
    let mut rows: Vec<String> = vec![head];
    let width = term_width().saturating_sub(2 + 20 + 2 + 1).max(20);
    rows.extend(wrap(body, width).into_iter().map(|l| tags(&l, roster)));
    let n = rows.len().max(art.len());
    let mut out = vec![String::new()];
    for i in 0..n {
        let a = art.get(i).copied().unwrap_or("");
        let r = rows.get(i).cloned().unwrap_or_default();
        let shade = c.lerp(theme().ink, i as f32 / art.len() as f32 * 0.7);
        out.push(format!("  {}  {r}", paint(shade, &format!("{a:<20}"))));
    }
    out.push(String::new());
    out.join("\n")
}

/// The prompt as it stands for `target`: `根 root → 玉 tama ▸ `. Slash
/// commands have no target, so just `根 root ▸ `.
pub fn prompt(me: &str, target: Option<&str>) -> String {
    let t = theme();
    let to = match target {
        Some("litter") => format!("{} {} ", dim("→"), paint(t.paper, "猫群 litter")),
        Some(n) => format!("{} {} ", dim("→"), sealed(n)),
        None => String::new(),
    };
    format!("{} {to}{} ", sealed(me), paint(t.accent, t.prompt))
}

/// What you typed, left in scrollback exactly where the composer stood when
/// you hit ⏎ — at the margin, behind its prompt, like any shell.
pub fn typed(me: &str, target: Option<&str>, line: &str, roster: &Roster) -> String {
    let p = prompt(me, target);
    let indent = 2 + vcells(&p);
    let width = term_width().saturating_sub(indent).max(20);
    wrap(&tags(line, roster), width)
        .into_iter()
        .enumerate()
        .map(|(i, l)| if i == 0 { format!("  {p}{l}") } else { format!("{}{l}", " ".repeat(indent)) })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Your own line: the echo, then the chain's word that it was sealed.
pub fn me(time: &str, block: u64, me: &str, to: &str, body: &str, roster: &Roster) -> String {
    format!("{}\n{}", typed(me, Some(to), body, roster), obs(time, block, paint(theme().progress, "✓ sealed")))
}

// ── keys ────────────────────────────────────────────────────────────────

/// A key and what it does: the key legible, the label quiet.
fn key(k: &str, what: &str) -> String {
    format!("{} {}", plain(k), dim(what))
}

/// `/keys` — the whole control scheme, printed into the log like any other
/// output. Real emacs-style line editing (`ratatui_textarea::TextArea`,
/// driven by raw-mode key events in `client::Input`) plus history, search
/// and completion layered on top — not just what a tty's canonical mode
/// gives for free.
pub fn keys() -> String {
    let col = |a: &str, b: &str| format!("      {a}{}{b}", " ".repeat(34usize.saturating_sub(vcells(a))));
    let mut out = vec![String::new()];
    out.push(col(&key("⌃a  ⌃e", "start · end of line"), &key("⌃k", "kill to end")));
    out.push(col(&key("⌃b  ⌃f  ← →", "char back · forward"), &key("⌃w", "kill word back")));
    out.push(col(&key("⌃p  ⌃n  ↑ ↓", "history back · forward"), &key("⌃y", "yank")));
    out.push(col(&key("⌃u", "undo"), &key("⌃r", "search history")));
    out.push(col(&key("⇥  ⇧⇥", "complete @name / cmd, cycle"), &key("⌃c", "clear the draft")));
    out.push(col(&key("⌃d", "delete forward, or quit if empty"), &key("⏎", "send")));
    out.push(String::new());
    out.push(format!("      {}", dim("say something → the litter, or @name a cat. /keys shows this again.")));
    out.push(String::new());
    out.join("\n")
}

// ── the composer ────────────────────────────────────────────────────────

/// One hairline with the connection on its right — the node this client
/// talks to, the primary that will actually seal the block, the head it has
/// seen — then the prompt with the resolved target, the draft (if any) with
/// its `@name`s lit, and a hint row. The real REPL (`client::repl`) renders
/// this same shape as a ratatui widget in an inline viewport, so it's the
/// only thing ever redrawn (`docs/CLI.md` §1); this plain form is what
/// `repl-mock` prints for a one-shot preview.
/// The hairline: connection status, right-aligned — the node this client
/// talks to, the primary that will actually seal the block, the head it has
/// seen. Its own function since the real REPL renders it as one row of a
/// ratatui layout, separate from the (live, editable) prompt row.
pub fn composer_status(node: &str, primary: &str, head: u64) -> String {
    let t = theme();
    let status = format!(
        "{} {} {} {} {} {}",
        paint(t.progress, "●"),
        dim(node),
        dim("→"),
        who(primary),
        dim("·"),
        dim(&format!("head {}", block_id(head)))
    );
    format!("  {}  {status}", rule(term_width().saturating_sub(4 + vcells(&status)).max(3)))
}

#[allow(clippy::too_many_arguments)]
pub fn composer_lines(node: &str, primary: &str, head: u64, me: &str, target: &str, draft: &str, cursor: usize, roster: &Roster, hint: &str) -> Vec<String> {
    let t = theme();
    let mut out = Vec::new();
    out.push(String::new());
    out.push(composer_status(node, primary, head));
    let prompt = prompt(me, Some(target));
    let byte_at = draft.char_indices().nth(cursor).map(|(b, _)| b).unwrap_or(draft.len());
    let (before, after) = draft.split_at(byte_at);
    out.push(format!("  {prompt}{}{}{}", tags(before, roster), paint(t.accent, t.cursor), tags(after, roster)));
    if !hint.is_empty() {
        out.push(format!("  {}{hint}", " ".repeat(vcells(&prompt))));
    }
    out
}

/// The hint row: every key `client::Input` actually implements (`docs/CLI.md`
/// — see `/keys` for the full legend).
pub fn hint() -> String {
    [key("⇥", "complete"), key("⌃r", "search"), key("↑↓", "history"), key("⌃d", "quit"), key("/keys", "all bindings")].join("   ")
}

// ── time ────────────────────────────────────────────────────────────────

/// Local HH:MM for a unix time, via the shell's `date` — no tz tables of
/// our own.
pub fn hhmm(unix: i64) -> String {
    let try_args = |a: &[&str]| {
        std::process::Command::new("date")
            .args(a)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
    };
    try_args(&["-r", &unix.to_string(), "+%H:%M"]) // bsd / macos
        .or_else(|| try_args(&["-d", &format!("@{unix}"), "+%H:%M"])) // gnu
        .unwrap_or_else(|| "--:--".into())
}

pub fn human(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// The time column for an event at `block`, given the head is now: an
/// estimate marked `≈`, plus the exact gap in blocks since `prev`.
pub fn when(block: u64, prev: Option<u64>, head: u64, now: i64, block_ms: u64) -> String {
    let secs_per_block = block_ms / 1000;
    let at = now - (head.saturating_sub(block) * secs_per_block) as i64;
    match prev {
        Some(p) => format!("≈{} (+{})", hhmm(at), human(block.saturating_sub(p) * secs_per_block)),
        None => format!("≈{}", hhmm(at)),
    }
}

// ── effects ─────────────────────────────────────────────────────────────

/// An account field, resolved through the roster; `?` when null.
pub fn name_of(eff: &serde_json::Value, field: &str, roster: &Roster) -> String {
    eff[field]
        .as_str()
        .map(|s| miot_keys::from_hex(s).map(|a| roster.name_of(&a)).unwrap_or_else(|_| s.to_string()))
        .unwrap_or_else(|| "?".into())
}

/// One effect, in the protocol's own words and the skin's colours — used
/// for both replay (an estimated `time`) and live tailing (the exact wall
/// clock). `me_name` is whichever cat we're signed in as, so our own
/// `said` effects render through [`me`] instead of [`said`].
pub fn render(time: &str, block: u64, eff: &serde_json::Value, roster: &Roster, me_name: &str) -> String {
    let task_id = || task(eff["task"].as_str().unwrap_or("?"));
    let text = |f: &str| eff[f].as_str().unwrap_or("").to_string();
    match eff["t"].as_str().unwrap_or("") {
        "said" => {
            let from = name_of(eff, "from", roster);
            let to = if eff["to"].is_null() { "litter".to_string() } else { name_of(eff, "to", roster) };
            if from == me_name {
                me(time, block, me_name, &to, &text("body"), roster)
            } else {
                said(time, block, &from, &to, &text("body"), roster)
            }
        }
        "opened" => obs(time, block, format!("{} opened {}: {}", who(&name_of(eff, "who", roster)), task_id(), plain(&text("text")))),
        "planned" => obs(time, block, format!("{} planned {} into {} subtasks", who(&name_of(eff, "who", roster)), task_id(), plain(&eff["count"].to_string()))),
        "assigned" => obs(time, block, format!("{} assigned to {}: {}", task_id(), who(&name_of(eff, "to", roster)), dim(&text("what")))),
        "directed" => obs(time, block, format!("{} directed on {}: {}", who(&name_of(eff, "to", roster)), task_id(), directed(eff["directive"].as_str().unwrap_or("?")))),
        "nudge" => {
            let last = if eff["last"].as_bool().unwrap_or(false) { ", last" } else { "" };
            obs(time, block, format!("{} {} {} {}", who(&name_of(eff, "to", roster)), nudged(), task_id(), dim(&format!("({} left{last})", eff["remaining"]))))
        }
        "record" => {
            let act = eff["act"].as_str().unwrap_or("?");
            let v = match act {
                "claimed" => claimed(),
                "submitted" => submitted(),
                other => verb("·", other, theme().progress),
            };
            let t = text("text");
            let suffix = if t.is_empty() { String::new() } else { format!(": {}", dim(&t)) };
            obs(time, block, format!("{} {v} {}{suffix}", who(&name_of(eff, "who", roster)), task_id()))
        }
        "requeued" => obs(time, block, format!("{} requeued from {}: {}", task_id(), who(&name_of(eff, "from", roster)), dim(eff["why"].as_str().unwrap_or("?")))),
        "budget_spent" => obs(time, block, format!("{} spent its nudge budget on {}", who(&name_of(eff, "holder", roster)), task_id())),
        "closed" => obs(time, block, format!("{} {} by {}: {}", task_id(), closed(), who(&name_of(eff, "author", roster)), plain(&text("title")))),
        "failed" => obs(time, block, format!("{} {}", task_id(), bold(theme().alarm, "✗ failed"))),
        "rehomed" => obs(time, block, format!("{} rehomed from {} to {}", task_id(), who(&name_of(eff, "from", roster)), who(&name_of(eff, "to", roster)))),
        other => obs(time, block, format!("{} {}", dim(other), faint(&eff.to_string()))),
    }
}
