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

/// Off under `NO_COLOR`, and off when stdout isn't a terminal — a cat's
/// agent loop under systemd writes to the journal, where escape codes are
/// just noise. Asked once; neither changes mid-run.
fn colour() -> bool {
    use std::io::IsTerminal;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
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
    subtitle: "· akuma tea house",
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
    subtitle: "// A K U M A · T E A  H O U S E",
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
    subtitle: "  akuma tea house",
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
    if !colour() {
        return s.to_string();
    }
    format!("{}{s}{OFF}", c.fg())
}
pub fn bold(c: Rgb, s: &str) -> String {
    if !colour() {
        return s.to_string();
    }
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
/// the terminal itself (an ioctl), then falls back to 100.
///
/// Never spawns anything. This used to shell out to `stty size` on every
/// call, which was tolerable while only the REPL rendered — then a cat's
/// agent loop started drawing every tool call through [`tool`], under herd
/// on Akuma, where each process spawn is expensive and fragile (sshd's pipe
/// leak, the spawn-slot class) and a blocked child would stall a tokio
/// worker the node's own HTTP server shares.
pub fn term_width() -> usize {
    use std::io::IsTerminal;
    if let Some(c) = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()) {
        return c;
    }
    if std::io::stdout().is_terminal() {
        if let Ok((cols, _)) = crossterm::terminal::size() {
            if cols > 20 {
                return cols as usize;
            }
        }
    }
    100
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

/// The time column: `09-23 22:35:10Z +11m30s` at its widest.
const TIME_W: usize = 23;
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
/// Insert a space at each PascalCase word boundary. A `Directive`'s name
/// (`PlanNeeded`, `ClearanceNeeded`, `ReassignNeeded` — `miot_primitives::
/// Directive`) arrives as one run of letters; `shout`ed as-is, the case
/// distinction that marked the word boundary is exactly what gets erased,
/// collapsing it into an unreadable blob like "PLANNEEDED".
fn split_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && c.is_uppercase() {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

pub fn directed(what: &str) -> String {
    bold(theme().alarm, &format!("⚑ {}", shout(&split_words(what))))
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
/// `off_record`: this one was never written to the block log — see
/// `Effect::Said`'s doc comment.
pub fn said(time: &str, block: u64, from: &str, to: &str, body: &str, roster: &Roster, off_record: bool) -> String {
    said_ex(time, block, from, to, body, roster, off_record, "")
}

/// [`said`] with a thread/tag annotation on the header row — a reply's
/// `↩ #parent` and its topic tags, from `Effect::Message`.
pub fn said_ex(time: &str, block: u64, from: &str, to: &str, body: &str, roster: &Roster, off_record: bool, note: &str) -> String {
    let art: Vec<&str> = AVATAR.lines().collect();
    let c = cat(from);
    let arrow = if to == "litter" { format!("{} {}", dim("→"), dim("litter")) } else { format!("{} {}", dim("→"), who(to)) };
    let otr = if off_record { format!("  {}", dim("· off the record")) } else { String::new() };
    let extra = if note.is_empty() { String::new() } else { format!("  {}", dim(note)) };
    let head = format!("{} {arrow}{otr}{extra}   {}", sealed(from), stamp(time, block).trim_start());
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

/// Your own line: the echo, then the chain's word that it was sealed — or,
/// for `off_record`, the word that it deliberately wasn't.
pub fn me(time: &str, block: u64, me: &str, to: &str, body: &str, roster: &Roster, off_record: bool) -> String {
    format!("{}\n{}", typed(me, Some(to), body, roster), sealed_line(time, block, off_record))
}

/// Just the chain's word on a line already echoed: `✓ sealed` in its block,
/// or that an off-the-record one deliberately wasn't.
pub fn sealed_line(time: &str, block: u64, off_record: bool) -> String {
    let note = if off_record { "↝ off the record — not sealed" } else { "✓ sealed" };
    obs(time, block, paint(theme().progress, note))
}

// ── tools and turns ─────────────────────────────────────────────────────
//
// What a model *did*, as opposed to what the chain recorded: shared by
// `kot chat` (one model, no chain) and a cat's agent loop (`kot run`,
// several cats often tailing into one terminal or journal) so both read the
// same. Who called it leads every line — in a litter that's the whole point.

/// Most lines of a tool's output shown inline; the rest is counted, not
/// dropped silently. `kot chat` keeps the full text for `Inspect`.
const TOOL_BODY_LINES: usize = 12;

/// One tool call's outcome, as its caller wants it shown.
pub struct ToolOut {
    /// The call's gist: a command, a path, a recipient — one line.
    pub arg: String,
    pub ok: bool,
    /// Right-hand facts: `exit 0`, `4.1 KB`, `12ms`.
    pub meta: Vec<String>,
    /// What came back. Empty for a call with nothing to show.
    pub body: String,
}

impl ToolOut {
    pub fn new(arg: impl Into<String>, ok: bool) -> Self {
        ToolOut { arg: arg.into(), ok, meta: Vec::new(), body: String::new() }
    }
    pub fn meta(mut self, m: impl Into<String>) -> Self {
        self.meta.push(m.into());
        self
    }
    pub fn body(mut self, b: impl Into<String>) -> Self {
        self.body = b.into();
        self
    }
    /// The plain-text form a model reads back (`Inspect`), no colour.
    pub fn text(&self) -> String {
        let meta = if self.meta.is_empty() { String::new() } else { format!("  ({})", self.meta.join(", ")) };
        if self.body.is_empty() { format!("{}{meta}", self.arg) } else { format!("{}{meta}\n{}", self.arg, self.body) }
    }
}

/// `4.1 KB` — for a file read or written.
pub fn bytes(n: usize) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)),
    }
}

/// `1,204` — token counts get big enough to want separators.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `32k` — a context window, compactly.
fn kilo(n: u32) -> String {
    if n >= 1000 && n % 1000 == 0 { format!("{}k", n / 1000) } else if n >= 1000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() }
}

pub fn millis(ms: u64) -> String {
    if ms < 1000 { format!("{ms}ms") } else { format!("{:.1}s", ms as f64 / 1000.0) }
}

/// Cut `s` to `n` display cells with a trailing `…` if it didn't fit.
fn clip(s: &str, n: usize) -> String {
    if cells(s) <= n {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = cells(&ch.to_string());
        if w + cw + 1 > n {
            break;
        }
        w += cw;
        out.push(ch);
    }
    out.push('…');
    out
}

/// A tool call: a header row naming who called what with which argument,
/// facts right-aligned, then the output hung off a gutter and capped at
/// [`TOOL_BODY_LINES`]:
///
/// ```text
///   ⚙ 玉 tama ▸ Bash  ls -la /tmp                     ✓ exit 0 · 12ms
///     │ total 8
///     │ … 23 more lines
/// ```
pub fn tool(caller: &str, name: &str, out: &ToolOut) -> String {
    let t = theme();
    let width = term_width();
    let (mark, mc) = if out.ok { ("✓", t.progress) } else { ("✗", t.alarm) };
    let meta = if out.meta.is_empty() {
        paint(mc, mark)
    } else {
        format!("{} {}", paint(mc, mark), dim(&out.meta.join(" · ")))
    };
    let lead = format!("  {} {} {} {}", paint(t.accent, "⚙"), sealed(caller), dim(t.prompt), bold(t.paper, name));
    let room = width.saturating_sub(vcells(&lead) + 2 + vcells(&meta) + 2).max(8);
    let arg_line = out.arg.lines().next().unwrap_or("");
    let arg = if arg_line.is_empty() { String::new() } else { format!("  {}", plain(&clip(arg_line, room))) };
    let gap = width.saturating_sub(vcells(&lead) + vcells(&arg) + vcells(&meta) + 1).max(2);
    let mut rows = vec![format!("{lead}{arg}{}{meta}", " ".repeat(gap))];

    let body = out.body.trim_end_matches('\n');
    if !body.is_empty() {
        let gutter = paint(if out.ok { t.ink } else { t.alarm }, "│");
        let lines: Vec<&str> = body.lines().collect();
        let inner = width.saturating_sub(6).max(20);
        for l in lines.iter().take(TOOL_BODY_LINES) {
            rows.push(format!("    {gutter} {}", dim(&clip(&l.replace('\t', "    "), inner))));
        }
        if lines.len() > TOOL_BODY_LINES {
            rows.push(format!("    {gutter} {}", faint(&format!("… {} more lines", lines.len() - TOOL_BODY_LINES))));
        }
    }
    rows.join("\n")
}

/// A model starting a turn: `◌ 玉 tama thinking · #1176 said`.
pub fn thinking(caller: &str, what: &str) -> String {
    format!("\n  {} {} {} {}", paint(theme().warm, "◌"), sealed(caller), dim("thinking ·"), dim(what))
}

/// Most lines of a reasoning block shown: the first few (what it set out to
/// do) and the rest from the end (where it got to). The transcript keeps it
/// all.
const MUSING_HEAD: usize = 4;
const MUSING_LINES: usize = 16;

/// What a model thought, or wrote beside its tool calls — hung off a gutter
/// like a tool's output, head and tail if long:
///
/// ```text
///   ✎ 喵 meow reasoning
///     ┆ The build died at the link step, so …
///     ┆ … 31 lines …
///     ┆ I'll rerun it with a 600s timeout.
/// ```
pub fn musing(caller: &str, label: &str, text: &str) -> String {
    let t = theme();
    let mut rows = vec![format!("  {} {} {}", paint(t.warm, "✎"), sealed(caller), dim(label))];
    let gutter = faint("┆");
    let inner = term_width().saturating_sub(6).max(20);
    let lines: Vec<&str> = text.trim().lines().filter(|l| !l.trim().is_empty()).collect();
    let row = |l: &str| format!("    {gutter} {}", dim(&clip(&l.replace('\t', "    "), inner)));
    if lines.len() <= MUSING_LINES {
        rows.extend(lines.iter().map(|l| row(l)));
    } else {
        let tail = MUSING_LINES - MUSING_HEAD;
        rows.extend(lines[..MUSING_HEAD].iter().map(|l| row(l)));
        rows.push(format!("    {gutter} {}", faint(&format!("… {} lines …", lines.len() - MUSING_LINES))));
        rows.extend(lines[lines.len() - tail..].iter().map(|l| row(l)));
    }
    rows.join("\n")
}

/// A tool call leaving — its result row comes later, when it lands:
/// `◌ 喵 meow ▸ Bash  $ cargo build   started · 2 in flight`.
pub fn started(caller: &str, name: &str, arg: &str, in_flight: usize) -> String {
    let t = theme();
    let lead = format!("  {} {} {} {}", paint(t.warm, "◌"), sealed(caller), dim(t.prompt), plain(name));
    let tag = dim(&if in_flight > 1 { format!("started · {in_flight} in flight") } else { "started".to_string() });
    let room = term_width().saturating_sub(vcells(&lead) + vcells(&tag) + 6).max(8);
    let arg = if arg.is_empty() { String::new() } else { format!("  {}", dim(&clip(arg, room))) };
    format!("{lead}{arg}  {tag}")
}

/// A bar for the context window, `▕███▌░░░░░▏`, coloured by how full it is.
fn meter(pct: u32, n: usize) -> String {
    const PART: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let t = theme();
    let c = match pct {
        p if p >= 80 => t.alarm,
        p if p >= 50 => t.warm,
        _ => t.progress,
    };
    let eighths = (pct.min(100) as usize * n * 8) / 100;
    let mut bar = "█".repeat(eighths / 8);
    if eighths % 8 > 0 && eighths / 8 < n {
        bar.push(PART[eighths % 8 - 1]);
    }
    let rest = n.saturating_sub(cells(&bar));
    format!("{}{}{}{}", faint("▕"), paint(c, &bar), faint(&"░".repeat(rest)), faint("▏"))
}

/// What one model turn cost, after its tools have run:
///
/// ```text
///   ◆ 玉 tama  in 1,204 · out 88 · 1,292 tok  ▕█▌░░░░░░░░▏ 4% of 32k · 3.4s · 2 tools
/// ```
///
/// `window` is `None` when the provider can't say — no meter then, rather
/// than one that's made up.
pub struct TurnCost {
    pub prompt: u32,
    pub out: u32,
    pub total: u32,
    /// Of `prompt`, what the provider served from its prompt cache — 0 when
    /// it doesn't say ([`miot_llm::Turn::cached_tokens`]).
    pub cached: u32,
    pub window: Option<u32>,
    pub ms: u64,
    /// Tool calls this turn, `SendMessage` not included.
    pub tools: usize,
    /// `SendMessage` calls this turn — talking, counted apart from tools.
    pub messages: usize,
}

pub fn turn(caller: &str, c: &TurnCost) -> String {
    let t = theme();
    let sep = || dim(" · ");
    let mut s = format!(
        "  {} {}  {} {}{}{}{} {}{}{} {}",
        paint(t.done, "◆"),
        sealed(caller),
        dim("in"),
        plain(&thousands(c.prompt as u64)),
        if c.cached > 0 { dim(&format!(" ({} cached)", thousands(c.cached as u64))) } else { String::new() },
        sep(),
        dim("out"),
        plain(&thousands(c.out as u64)),
        sep(),
        bold(t.paper, &thousands(c.total as u64)),
        dim("tok"),
    );
    if let Some(w) = c.window.filter(|&w| w > 0) {
        let pct = ((c.total as u64 * 100) / w as u64) as u32;
        s.push_str(&format!("  {} {}", meter(pct, 10), dim(&format!("{pct}% of {}", kilo(w)))));
    }
    s.push_str(&sep());
    s.push_str(&dim(&millis(c.ms)));
    s.push_str(&sep());
    s.push_str(&dim(&match c.tools {
        0 => "no tools".to_string(),
        1 => "1 tool".to_string(),
        n => format!("{n} tools"),
    }));
    if c.messages > 0 {
        s.push_str(&sep());
        s.push_str(&dim(&if c.messages == 1 { "1 message".to_string() } else { format!("{} messages", c.messages) }));
    }
    s
}

/// What a model said, outside the chain (`kot chat`): its chop and name,
/// then the words at full brightness under a hanging indent.
pub fn reply(caller: &str, body: &str) -> String {
    let lead = format!("  {} ", sealed(caller));
    let indent = vcells(&lead);
    let width = term_width().saturating_sub(indent).max(20);
    let mut rows = Vec::new();
    for (p, para) in body.split('\n').enumerate() {
        for (i, l) in wrap(para, width).into_iter().enumerate() {
            let head = if p == 0 && i == 0 { lead.clone() } else { " ".repeat(indent) };
            rows.push(format!("{head}{}", plain(&l)));
        }
    }
    format!("\n{}", rows.join("\n"))
}

/// One cat's cumulative stats as a phrase — `12 turns · 9 tool calls · 4
/// messages · 59,635 tok · 22m42s thinking` — from a `stats_reported`
/// event or a `/stats` row, which share field names. `messages` is left
/// out when the reporter didn't count them apart (an older build), rather
/// than shown as a misleading zero.
pub fn stats_phrase(v: &serde_json::Value) -> String {
    let n = |f: &str| v[f].as_u64().unwrap_or(0);
    let plural = |k: u64, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
    let mut parts = vec![plural(n("turns"), "turn", "turns"), plural(n("tool_calls"), "tool call", "tool calls")];
    if let Some(m) = v["messages"].as_u64() {
        parts.push(plural(m, "message", "messages"));
    }
    parts.push(format!("{} tok", thousands(n("tokens"))));
    parts.push(format!("{} thinking", human(n("ms") / 1000)));
    parts.join(" · ")
}

/// A side note from the loop itself — compaction, a budget warning, a
/// retry — quieter than a tool, louder than nothing.
pub fn note(s: &str) -> String {
    format!("  {} {}", paint(theme().warm, "↯"), dim(s))
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
pub fn composer_status(node: &str, primary: &str, head: u64, sealing: usize) -> String {
    let t = theme();
    let pending = match sealing {
        0 => String::new(),
        1 => format!("{}  ", paint(t.warm, "◌ sealing…")),
        n => format!("{}  ", paint(t.warm, &format!("◌ sealing {n}…"))),
    };
    let status = format!(
        "{pending}{} {} {} {} {} {}",
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
    out.push(composer_status(node, primary, head, 0));
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

/// A unix-ms instant as UTC: `09-24 01:40:12Z`. Always UTC, always the
/// same shape — the fleet spans hosts and timezones, and a log is only
/// comparable across them in one zone.
pub fn clock(unix_ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(unix_ms as i64).map(|t| t.format("%m-%d %H:%M:%SZ").to_string()).unwrap_or_else(|| "--".into())
}

/// UTC HH:MM for a unix time (seconds).
pub fn hhmm(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0).map(|t| t.format("%H:%MZ").to_string()).unwrap_or_else(|| "--:--".into())
}

/// The time column for an event, from its block's real seal time (`at`,
/// unix ms, from `/events`) — plus the gap since the previous stamped event.
/// A block with no seal time (sealed before they existed, or by a primary
/// on an older build) gets no time at all: an honest blank beats a guess.
pub fn stamp_at(at: Option<u64>, prev: Option<u64>) -> String {
    match (at, prev) {
        (Some(t), Some(p)) if t >= p => format!("{} +{}", clock(t), human((t - p) / 1000)),
        (Some(t), _) => clock(t),
        (None, _) => String::new(),
    }
}

pub fn human(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// An *estimated* time column for an event at `block`, marked `≈`: head
/// distance times the nominal block time. Only for `repl-mock`'s canned
/// events, which have no seal times — real clients use [`stamp_at`].
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
            let off_record = eff["off_record"].as_bool().unwrap_or(false);
            if from == me_name {
                me(time, block, me_name, &to, &text("body"), roster, off_record)
            } else {
                said(time, block, &from, &to, &text("body"), roster, off_record)
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
        "standalone_artifact" => obs(
            time,
            block,
            format!("{} published {} {}: {}", who(&name_of(eff, "author", roster)), paint(theme().done, "▤ artifact"), task(&eff["id"].as_str().map(str::to_string).unwrap_or_else(|| eff["id"].to_string())), plain(&text("title"))),
        ),
        // Cumulative, one per turn — the running meter of what a cat has
        // cost so far, so it reads as a tally rather than an event.
        "stats_reported" => {
            obs(
                time,
                block,
                format!("{} {} {}", who(&name_of(eff, "who", roster)), dim("∑"), dim(&stats_phrase(eff))),
            )
        }
        // A reply and/or a tagged message — [`Effect::Message`]. Renders
        // like speech (it is), with the thread/tag note on the header.
        "message" => {
            let from = name_of(eff, "from", roster);
            let to = if eff["to"].is_null() { "litter".to_string() } else { name_of(eff, "to", roster) };
            let off_record = eff["off_record"].as_bool().unwrap_or(false);
            let mut note = String::new();
            if let Some(p) = eff["parent"].as_str() {
                note = format!("↩ #{p}");
            } else if let Some(id) = eff["id"].as_str() {
                note = format!("#{id}");
            }
            for t in eff["tags"].as_array().into_iter().flatten() {
                if let Some(t) = t.as_str() {
                    if !note.is_empty() {
                        note.push_str(" · ");
                    }
                    note.push('#');
                    note.push_str(t);
                }
            }
            if from == me_name {
                let base = me(time, block, me_name, &to, &text("body"), roster, off_record);
                if note.is_empty() {
                    base
                } else {
                    format!("{base} {}", dim(&format!("({note})")))
                }
            } else {
                said_ex(time, block, &from, &to, &text("body"), roster, off_record, &note)
            }
        }
        // A reaction — [`Effect::Reacted`]. One glyph of acknowledgment
        // where a whole speech block used to be typed.
        "reacted" => obs(
            time,
            block,
            format!("{} reacted {} on #{}", who(&name_of(eff, "who", roster)), plain(eff["emoji"].as_str().unwrap_or("·")), eff["target"].as_str().unwrap_or("?")),
        ),
        // An artifact vote — [`Effect::Voted`].
        "voted" => obs(
            time,
            block,
            format!(
                "{} voted {} {}",
                who(&name_of(eff, "who", roster)),
                if eff["up"].as_bool().unwrap_or(false) { paint(theme().done, "▲") } else { paint(theme().alarm, "▼") },
                format!("§{}", eff["artifact"].as_str().unwrap_or("?"))
            ),
        ),
        other => obs(time, block, format!("{} {}", dim(other), faint(&eff.to_string()))),
    }
}

#[cfg(test)]
mod time_tests {
    use super::*;

    #[test]
    fn clock_is_utc() {
        // 2026-09-24T01:40:12.345Z
        assert_eq!(clock(1_790_214_012_345), "09-24 01:40:12Z");
    }

    #[test]
    fn stamp_is_blank_without_a_seal_time() {
        assert_eq!(stamp_at(None, Some(1)), "");
    }

    #[test]
    fn stamp_shows_the_gap_since_the_previous() {
        assert_eq!(stamp_at(Some(1_790_214_012_345), Some(1_790_214_012_345 - 75_000)), "09-24 01:40:12Z +1m15s");
    }
}

// ── activity ────────────────────────────────────────────────────────────
//
// What each cat is doing right now (`crate::activity`), from a node's
// `GET /activity`. Two forms: one row in the composer, redrawn in place
// (`docs/CLI.md` §8 — the spinner on the composer line, not a rewrite of
// scrollback), and a full snapshot printed into the log on demand
// (`/activity`, `kot activity`).

/// A record not refreshed for this long is shown as unknown, not current:
/// that cat's node has gone quiet, and "thinking" may no longer be true.
pub const ACTIVITY_STALE_MS: u64 = 15_000;

fn seen_name(s: &crate::activity::Seen, roster: &Roster) -> String {
    miot_keys::from_hex(&s.account).map(|a| roster.name_of(&a)).unwrap_or_else(|_| s.activity.name.clone())
}

/// The composer's activity row — one short phrase per cat, clipped to the
/// terminal:
///
/// ```text
///   活 喵 meow ◌ 42s · 黑 kuro ⚙2 Bash 1m03s · 玉 tama · ✗Bash
/// ```
pub fn activity_row(seen: &[crate::activity::Seen], roster: &Roster) -> String {
    let t = theme();
    if seen.is_empty() {
        return format!("  {} {}", faint("活"), faint("no cat is reporting activity to this node"));
    }
    let mut parts = Vec::new();
    for s in seen {
        let a = &s.activity;
        let name = who(&seen_name(s, roster));
        if s.age_ms > ACTIVITY_STALE_MS {
            parts.push(format!("{name} {}", faint(&format!("? {} ago", human(s.age_ms / 1000)))));
            continue;
        }
        let secs = a.in_phase_ms(s.age_ms) / 1000;
        let what = match a.phase.as_str() {
            "thinking" => paint(t.warm, &format!("◌ {}", human(secs))),
            "compacting" => paint(t.warm, &format!("◌ compacting {}", human(secs))),
            _ if !a.running.is_empty() => {
                let oldest = a.running.iter().min_by_key(|f| f.since).expect("not empty");
                let ms = a.at.saturating_sub(oldest.since) + s.age_ms;
                paint(t.accent, &format!("⚙{} {} {}", a.running.len(), oldest.tool, human(ms / 1000)))
            }
            _ => dim("·"),
        };
        // The newest finished call, if it failed and it's recent — the
        // thing worth noticing without asking.
        let failed = a
            .recent
            .last()
            .filter(|f| !f.ok && a.at.saturating_sub(f.at) + s.age_ms < 60_000)
            .map(|f| format!(" {}", paint(t.alarm, &format!("✗{}", f.tool))))
            .unwrap_or_default();
        // Its own to-do list, as a fraction — how far through a job it is.
        let progress = if a.tasks_total > 0 { format!(" {}", dim(&format!("{}/{}", a.tasks_finished, a.tasks_total))) } else { String::new() };
        parts.push(format!("{name} {what}{progress}{failed}"));
    }
    let row = format!("  {} {}", faint("活"), parts.join(&dim(" · ")));
    clip_ansi(&row, term_width().saturating_sub(1))
}

/// [`clip`] for a string that already carries colour: count only visible
/// cells, keep every escape, reset at the cut.
fn clip_ansi(s: &str, n: usize) -> String {
    if vcells(s) <= n {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            out.push(ch);
            for c in chars.by_ref() {
                out.push(c);
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let cw = cells(&ch.to_string());
        if w + cw + 1 > n {
            break;
        }
        w += cw;
        out.push(ch);
    }
    out.push('…');
    if colour() {
        out.push_str("\x1b[0m");
    }
    out
}

/// One local task: a glyph for where it's at, its id, its text, and what
/// came of it if it's finished.
fn task_line(t: &crate::activity::TaskLine, width: usize) -> String {
    let th = theme();
    let (mark, c) = match t.status.as_str() {
        "doing" => ("◐", th.warm),
        "done" => ("✓", th.progress),
        "failed" => ("✗", th.alarm),
        "dropped" => ("–", th.ink),
        _ => ("○", th.ink),
    };
    let text = clip(&t.text, width.saturating_sub(8));
    let body = match t.status.as_str() {
        "doing" => bold(th.paper, &text),
        "todo" => plain(&text),
        _ => dim(&text),
    };
    let mut row = format!("{} {} {body}", paint(c, mark), dim(&t.id));
    if !t.note.is_empty() {
        row.push_str(&format!("\n  {}     {}", " ".repeat(9), faint(&format!("↳ {}", clip(&t.note, width.saturating_sub(12))))));
    }
    row
}

/// One cat's local task list (`/tasks <name>`, `kot task list --cat`):
///
/// ```text
///   喵 meow  local tasks · 3/4 finished · ◐ building  heard 1.2s ago
///       ✓ L1 Inspect /tmp/meow-greet source
///       ✓ L2 Clean old build artifacts
///            ↳ rm -f hello removed the stale binary
///       ◐ L3 Build hello from scratch with 600s Bash timeout
///       ○ L4 Verify binary runs and send root the output
/// ```
pub fn local_tasks_text(seen: &[crate::activity::Seen], roster: &Roster, name: &str) -> String {
    let name = name.trim_start_matches('@');
    let Some(s) = seen.iter().find(|s| seen_name(s, roster) == name) else {
        return format!("  {}", dim(&format!("no activity from {name} — not running an agent loop on a build that reports it, or out of this node's earshot")));
    };
    let a = &s.activity;
    let t = theme();
    let heard = if s.age_ms > ACTIVITY_STALE_MS {
        warn(&format!("last heard {} ago — may be out of date", human(s.age_ms / 1000)))
    } else {
        dim(&format!("heard {} ago", millis(s.age_ms)))
    };
    let mut out = vec![String::new()];
    if a.tasks_total == 0 {
        out.push(format!("  {}  {}  {heard}", sealed(name), dim("local tasks · none — it hasn't written any down this session")));
        return out.join("\n");
    }
    let doing = a.tasks.iter().find(|t| t.status == "doing").map(|d| format!(" · {}", paint(t.warm, &format!("◐ {}", d.id)))).unwrap_or_default();
    out.push(format!(
        "  {}  {}{doing}  {heard}",
        sealed(name),
        dim(&format!("local tasks · {}/{} finished", a.tasks_finished, a.tasks_total))
    ));
    let width = term_width().saturating_sub(10).max(20);
    let shown = a.tasks.len() as u32;
    if shown < a.tasks_total {
        out.push(format!("      {}", faint(&format!("… {} older finished ones not carried", a.tasks_total - shown))));
    }
    for task in &a.tasks {
        out.push(format!("      {}", task_line(task, width)));
    }
    out.join("\n")
}

/// The full snapshot, one block per cat (`/activity [name]`,
/// `kot activity`): where it is in the loop and for how long, what woke
/// it, every call in flight, how the finished ones went, its open local
/// tasks, the tail of its last reasoning.
pub fn activity_text(seen: &[crate::activity::Seen], roster: &Roster, only: Option<&str>) -> String {
    let t = theme();
    let mut out = Vec::new();
    let pick: Vec<&crate::activity::Seen> = seen.iter().filter(|s| only.is_none_or(|n| seen_name(s, roster) == n.trim_start_matches('@'))).collect();
    if pick.is_empty() {
        return format!(
            "  {}",
            dim(&match only {
                Some(n) => format!("no activity from {n} — not running an agent loop, or out of this node's earshot"),
                None => "no cat is reporting activity to this node (an older build, or no agent loops running)".to_string(),
            })
        );
    }
    let inner = term_width().saturating_sub(12).max(20);
    for s in pick {
        let a = &s.activity;
        let secs = a.in_phase_ms(s.age_ms) / 1000;
        let phase = match a.phase.as_str() {
            "thinking" | "compacting" => paint(t.warm, &format!("◌ {} {}", a.phase, human(secs))),
            "waiting" => paint(t.accent, &format!("⚙ waiting on tools {}", human(secs))),
            _ => dim(&format!("· idle {}", human(secs))),
        };
        let heard = if s.age_ms > ACTIVITY_STALE_MS {
            warn(&format!("last heard {} ago — may be out of date", human(s.age_ms / 1000)))
        } else {
            dim(&format!("heard {} ago", millis(s.age_ms)))
        };
        out.push(String::new());
        out.push(format!("  {}  {phase}  {}  {heard}", sealed(&seen_name(s, roster)), dim(&format!("turn {} · {}", a.turns, a.model))));
        let label = |l: &str| dim(&format!("{l:>9}"));
        if !a.why.is_empty() {
            out.push(format!("  {} {}", label("on"), plain(&clip(&a.why, inner))));
        }
        if a.running.is_empty() {
            out.push(format!("  {} {}", label("running"), dim("nothing")));
        } else {
            for (i, f) in a.running.iter().enumerate() {
                let ms = a.at.saturating_sub(f.since) + s.age_ms;
                let head = if i == 0 { label(&format!("running {}", a.running.len())) } else { " ".repeat(9) };
                out.push(format!("  {head} {} {}  {}  {}", paint(t.warm, "◌"), bold(t.paper, &f.tool), dim(&clip(&f.arg, inner.saturating_sub(20))), dim(&human(ms / 1000))));
            }
        }
        let mut done = format!("{} {}", paint(t.progress, &format!("✓ {}", a.ok)), paint(if a.failed > 0 { t.alarm } else { t.ink }, &format!("✗ {}", a.failed)));
        if a.held > 0 {
            done.push_str(&format!("  {}", warn(&format!("{} result(s) held until someone writes", a.held))));
        }
        if a.followups > 0 {
            done.push_str(&format!("  {}", dim(&format!("{} follow-up turn(s) in a row", a.followups))));
        }
        out.push(format!("  {} {done}", label("done")));
        for f in a.recent.iter().rev() {
            let (mark, c) = if f.ok { ("✓", t.progress) } else { ("✗", t.alarm) };
            let meta = if f.meta.is_empty() { String::new() } else { format!("  {}", dim(&f.meta)) };
            out.push(format!("  {} {} {}  {}{meta}", " ".repeat(9), paint(c, mark), plain(&f.tool), dim(&clip(&f.arg, inner.saturating_sub(30)))));
        }
        if a.tasks_total > 0 {
            out.push(format!("  {} {}", label("tasks"), dim(&format!("{}/{} finished — /tasks {} for the list", a.tasks_finished, a.tasks_total, seen_name(s, roster)))));
            for t in a.tasks.iter().filter(|t| matches!(t.status.as_str(), "todo" | "doing")) {
                out.push(format!("  {} {}", " ".repeat(9), task_line(t, inner)));
            }
        }
        if !a.thought.is_empty() {
            let width = inner.max(20);
            for (i, l) in wrap(&a.thought, width).into_iter().take(4).enumerate() {
                out.push(format!("  {} {}", if i == 0 { label("thought") } else { " ".repeat(9) }, dim(&l)));
            }
        }
        if a.tokens > 0 {
            let ctx = match a.window.filter(|&w| w > 0) {
                Some(w) => format!("{} of {} tok", thousands(a.tokens as u64), kilo(w)),
                None => format!("{} tok", thousands(a.tokens as u64)),
            };
            out.push(format!("  {} {}", label("context"), dim(&ctx)));
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    use crate::activity::{Activity, Finished, Flight, Seen, TaskLine};

    fn roster() -> Roster {
        Roster(vec![("meow".into(), miot_keys::Identity::from_seed(&[7; 32]).account()), ("kuro".into(), miot_keys::Identity::from_seed(&[8; 32]).account())])
    }

    fn seen(n: u8, age_ms: u64, a: Activity) -> Seen {
        Seen { account: miot_keys::to_hex(&miot_keys::Identity::from_seed(&[n; 32]).account()), age_ms, activity: a }
    }

    fn cats() -> Vec<Seen> {
        vec![
            seen(7, 200, Activity { name: "meow".into(), phase: "thinking".into(), since: 1_000, at: 43_000, ..Default::default() }),
            seen(
                8,
                0,
                Activity {
                    name: "kuro".into(),
                    phase: "waiting".into(),
                    since: 0,
                    at: 70_000,
                    running: vec![
                        Flight { id: 3, tool: "Bash".into(), arg: "$ cargo build".into(), since: 7_000, ..Default::default() },
                        Flight { id: 4, tool: "Peers".into(), since: 69_000, ..Default::default() },
                    ],
                    ok: 9,
                    failed: 2,
                    recent: vec![Finished { tool: "Bash".into(), arg: "$ make".into(), ok: false, meta: "exit 2".into(), at: 60_000, ..Default::default() }],
                    tasks: vec![
                        TaskLine { id: "L1".into(), status: "done".into(), text: "configure".into(), note: "used defconfig".into() },
                        TaskLine { id: "L2".into(), status: "doing".into(), text: "build it".into(), note: String::new() },
                        TaskLine { id: "L3".into(), status: "todo".into(), text: "send root the output".into(), note: String::new() },
                    ],
                    tasks_finished: 1,
                    tasks_total: 3,
                    thought: "the link step needs more time".into(),
                    ..Default::default()
                },
            ),
        ]
    }

    #[test]
    fn the_row_names_each_cats_phase_and_its_oldest_call() {
        let row = strip_ansi(&activity_row(&cats(), &roster()));
        assert!(row.contains("meow ◌ 42s"), "{row}");
        // Oldest of two in flight, timed from its own start; the recent
        // failure is flagged.
        assert!(row.contains("kuro ⚙2 Bash 1m03s 1/3 ✗Bash"), "with its task progress: {row}");
    }

    #[test]
    fn a_stale_record_is_not_shown_as_current() {
        let mut c = cats();
        c[0].age_ms = ACTIVITY_STALE_MS + 1_000;
        let row = strip_ansi(&activity_row(&c, &roster()));
        assert!(row.contains("meow ? 16s ago"), "{row}");
        assert!(!row.contains("◌"), "{row}");
    }

    #[test]
    fn the_snapshot_has_everything_and_filters_by_name() {
        let all = strip_ansi(&activity_text(&cats(), &roster(), None));
        for want in ["◌ thinking 42s", "⚙ waiting on tools 1m10s", "running 2", "$ cargo build", "✓ 9", "✗ 2", "exit 2", "1/3 finished — /tasks kuro", "◐ L2 build it", "○ L3 send root", "the link step needs more time"] {
            assert!(all.contains(want), "missing {want:?} in:\n{all}");
        }
        let one = strip_ansi(&activity_text(&cats(), &roster(), Some("@meow")));
        assert!(one.contains("meow") && !one.contains("kuro"), "{one}");
        let none = strip_ansi(&activity_text(&cats(), &roster(), Some("tama")));
        assert!(none.contains("no activity from tama"), "{none}");
        assert!(strip_ansi(&activity_text(&[], &roster(), None)).contains("no cat is reporting"));
        // The snapshot shows only what's open; the finished one is for /tasks.
        assert!(!all.contains("L1 configure"), "{all}");
    }

    #[test]
    fn a_cats_task_list_shows_progress_status_and_notes() {
        let t = strip_ansi(&local_tasks_text(&cats(), &roster(), "@kuro"));
        assert!(t.contains("kuro  local tasks · 1/3 finished · ◐ L2"), "{t}");
        assert!(t.contains("✓ L1 configure") && t.contains("↳ used defconfig"), "{t}");
        assert!(t.contains("◐ L2 build it") && t.contains("○ L3 send root the output"), "{t}");
        // In id order.
        let at = |row: &str| t.find(row).unwrap_or_else(|| panic!("no {row:?} in {t}"));
        assert!(at("✓ L1") < at("◐ L2 build") && at("◐ L2 build") < at("○ L3"), "{t}");

        let mut c = cats();
        c[1].activity.tasks_total = 30;
        assert!(strip_ansi(&local_tasks_text(&c, &roster(), "kuro")).contains("… 27 older finished ones not carried"));
        assert!(strip_ansi(&local_tasks_text(&cats(), &roster(), "meow")).contains("none — it hasn't written any down"));
        assert!(strip_ansi(&local_tasks_text(&cats(), &roster(), "tama")).contains("no activity from tama"));
    }
}

// ── markdown ────────────────────────────────────────────────────────────
//
// An artifact is markdown, written for a reader: in the REPL it's shown
// rendered, not as source. Small on purpose — the subset cats actually
// write (headings, emphasis, code, lists, quotes, links) — and it returns
// ANSI lines like everything else here; wrapping is the REPL's own
// (`client::insert_ansi`). `kot artifact` still prints the raw markdown, so
// it pipes.

/// `src` rendered for the terminal, indented two cells like the log.
pub fn markdown(src: &str) -> String {
    let t = theme();
    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    for raw in src.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if let Some(marker) = &fence {
            if trimmed.starts_with(marker.as_str()) {
                fence = None;
                out.push(format!("    {}", faint("└")));
            } else {
                out.push(format!("    {} {}", faint("│"), paint(t.ink, &line.replace('\t', "    "))));
            }
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker = trimmed[..3].to_string();
            let lang = trimmed[3..].trim();
            out.push(format!("    {}{}", faint("┌"), if lang.is_empty() { String::new() } else { format!(" {}", dim(lang)) }));
            fence = Some(marker);
            continue;
        }
        if trimmed.is_empty() {
            out.push(String::new());
            continue;
        }
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let text = inline(trimmed[hashes..].trim());
            out.push(match hashes {
                1 => format!("  {}", bold(t.accent, &strip_ansi(&text))),
                2 => format!("  {}", bold(t.paper, &strip_ansi(&text))),
                _ => format!("  {}", bold(t.ink, &strip_ansi(&text))),
            });
            continue;
        }
        if trimmed.len() >= 3 && trimmed.chars().all(|c| matches!(c, '-' | '*' | '_' | ' ')) && trimmed.chars().filter(|c| !c.is_whitespace()).count() >= 3 {
            out.push(format!("  {}", rule(term_width().saturating_sub(4).min(60))));
            continue;
        }
        if let Some(q) = trimmed.strip_prefix('>') {
            out.push(format!("  {} {}", paint(t.ink, "▌"), dim(&strip_ansi(&inline(q.trim_start())))));
            continue;
        }
        let indent = line.len() - trimmed.len();
        let pad = " ".repeat(2 + indent);
        if let Some(item) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).or_else(|| trimmed.strip_prefix("+ ")) {
            // A task list's box, if it has one.
            let (mark, item) = match item.strip_prefix("[ ] ") {
                Some(rest) => (dim("☐"), rest),
                None => match item.strip_prefix("[x] ").or_else(|| item.strip_prefix("[X] ")) {
                    Some(rest) => (paint(t.progress, "☑"), rest),
                    None => (paint(t.accent, "•"), item),
                },
            };
            out.push(format!("{pad}{mark} {}", inline(item)));
            continue;
        }
        let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && trimmed[digits..].starts_with(". ") {
            out.push(format!("{pad}{} {}", paint(t.accent, &trimmed[..digits + 1]), inline(&trimmed[digits + 2..])));
            continue;
        }
        out.push(format!("{pad}{}", inline(trimmed)));
    }
    if fence.is_some() {
        out.push(format!("    {}", faint("└")));
    }
    out.join("\n")
}

/// Inline markdown in one line: `code` (nothing inside it is markup),
/// **bold**, and [links](url) — the text, then the address, dim.
fn inline(s: &str) -> String {
    let t = theme();
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut plain_run = String::new();
    let mut i = 0;
    let flush = |run: &mut String, out: &mut String| {
        if !run.is_empty() {
            out.push_str(&plain(run));
            run.clear();
        }
    };
    let find = |from: usize, pat: &[char]| (from..chars.len().saturating_sub(pat.len() - 1)).find(|&j| chars[j..j + pat.len()] == *pat);
    while i < chars.len() {
        if chars[i] == '`' {
            if let Some(end) = find(i + 1, &['`']) {
                flush(&mut plain_run, &mut out);
                out.push_str(&paint(t.warm, &chars[i + 1..end].iter().collect::<String>()));
                i = end + 1;
                continue;
            }
        }
        if chars[i..].starts_with(&['*', '*']) || chars[i..].starts_with(&['_', '_']) {
            let pat = [chars[i], chars[i]];
            if let Some(end) = find(i + 2, &pat).filter(|&e| e > i + 2) {
                flush(&mut plain_run, &mut out);
                out.push_str(&bold(t.paper, &strip_ansi(&inline(&chars[i + 2..end].iter().collect::<String>()))));
                i = end + 2;
                continue;
            }
        }
        if chars[i] == '[' {
            if let Some(close) = find(i + 1, &[']', '(']) {
                if let Some(end) = find(close + 2, &[')']) {
                    flush(&mut plain_run, &mut out);
                    let text: String = chars[i + 1..close].iter().collect();
                    let url: String = chars[close + 2..end].iter().collect();
                    out.push_str(&format!("{} {}", paint(t.accent, &text), dim(&format!("({url})"))));
                    i = end + 1;
                    continue;
                }
            }
        }
        plain_run.push(chars[i]);
        i += 1;
    }
    flush(&mut plain_run, &mut out);
    out
}

#[cfg(test)]
mod markdown_tests {
    use super::*;

    fn md(s: &str) -> String {
        strip_ansi(&markdown(s))
    }

    #[test]
    fn headings_emphasis_and_inline_code_lose_their_markup() {
        let r = md("# /tmp/meow-greet: Rebuild\n\nBuilt with `rustc hello.rs` — **exit 0**, see [the log](http://x/y).");
        assert!(r.contains("  /tmp/meow-greet: Rebuild"), "{r}");
        assert!(r.contains("Built with rustc hello.rs — exit 0, see the log (http://x/y)."), "{r}");
        assert!(!r.contains("**") && !r.contains('`') && !r.contains("# "), "{r}");
    }

    #[test]
    fn code_blocks_keep_their_contents_verbatim_behind_a_gutter() {
        let r = md("```rust\nfn main() { println!(\"**not bold**\"); }\n```\nafter");
        assert!(r.contains("┌ rust"), "{r}");
        assert!(r.contains("│ fn main() { println!(\"**not bold**\"); }"), "markup inside code is left alone: {r}");
        assert!(r.contains("└") && r.contains("  after"), "{r}");
        // An unclosed fence still closes at the end.
        assert!(md("```\nx").ends_with('└'));
    }

    #[test]
    fn lists_quotes_and_rules() {
        let r = md("- one\n  - nested `x`\n1. first\n12. twelfth\n- [ ] open\n- [x] done\n> quoted **text**\n---");
        assert!(r.contains("  • one") && r.contains("    • nested x"), "{r}");
        assert!(r.contains("  1. first") && r.contains("  12. twelfth"), "{r}");
        assert!(r.contains("☐ open") && r.contains("☑ done"), "{r}");
        assert!(r.contains("▌ quoted text"), "{r}");
        assert!(r.contains("──"), "{r}");
    }

    #[test]
    fn unmatched_markers_are_left_as_written() {
        let r = md("a ** b and a lone ` tick and [not a link]");
        assert!(r.contains("a ** b and a lone ` tick and [not a link]"), "{r}");
    }
}

#[cfg(test)]
mod directed_tests {
    use super::*;

    #[test]
    fn a_directive_names_word_boundary_survives_shouting() {
        // `Directive::PlanNeeded` etc. (`miot_primitives`) arrive as one run
        // of PascalCase letters — shouted as-is (`theme().upper`), that
        // collapses the case distinction that marked the word boundary into
        // an unreadable "PLANNEEDED". `split_words` puts the boundary back
        // as a literal space before `shout` ever sees it, so it survives
        // uppercasing either way.
        assert_eq!(split_words("PlanNeeded"), "Plan Needed");
        assert_eq!(split_words("ClearanceNeeded"), "Clearance Needed");
        assert_eq!(split_words("ReassignNeeded"), "Reassign Needed");
        assert_eq!(split_words("Plan"), "Plan", "a single word gains no stray space");
    }

    #[test]
    fn directed_keeps_the_word_gap_whichever_theme_cases_it() {
        let r = strip_ansi(&directed("PlanNeeded"));
        assert!(r.to_uppercase().contains("PLAN NEEDED"), "{r}");
    }
}
