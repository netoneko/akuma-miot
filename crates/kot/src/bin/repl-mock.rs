//! The `kot` REPL, the *look* — a mock. Prints one fake session the way the
//! real REPL (`client::repl`) should render it, then exits. No node, no
//! network, no input.
//!
//!     cargo run -p kot --bin repl-mock -- bund     # the Bund after dark (default)
//!     cargo run -p kot --bin repl-mock -- neon     # Lujiazui in the rain, full cyberpunk
//!     cargo run -p kot --bin repl-mock -- ink      # 水墨 — ink wash, one vermilion seal
//!
//! (`KOT_THEME=neon` works too.) Same session, same layout, three skins.
//!
//! Rules every skin obeys, all from `docs/CLI.md`:
//!  §0 ordinary stdout lines, no alternate screen — the terminal owns
//!     scrollback, select/copy and tmux copy-mode keep working;
//!  §1 the log grows upward; the composer is pinned last and, in the real
//!     thing, redrawn with relative moves only (clear its rows, print the
//!     new output *above*, draw it again) — output already printed never
//!     moves. Long lines are wrapped by us to the terminal width with a
//!     hanging indent, never left to the terminal's ragged hard wrap;
//!  §2 the prompt shows the resolved target (`root → tama ▸`);
//!  §6 banner once (`akuma_40`), one colour per sender, the `akuma_20`
//!     avatar beside a cat's `said`, no box-drawing around the log.
//!
//! Line editing (↑↓ history, ←→, ⌃r, tab-complete `@name`) is the real
//! REPL's job via a readline crate; here it is a hint row.

use std::sync::OnceLock;

const BANNER: &str = include_str!("../../../../assets/akuma_40.txt");
const AVATAR: &str = include_str!("../../../../assets/akuma_20.txt");

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

const OFF: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";

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

struct Theme {
    name: &'static str,
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

fn theme() -> &'static Theme {
    static T: OnceLock<Theme> = OnceLock::new();
    T.get_or_init(|| {
        let pick = std::env::args().nth(1).or_else(|| std::env::var("KOT_THEME").ok()).unwrap_or_default();
        match pick.as_str() {
            "neon" => NEON,
            "ink" => INK_WASH,
            _ => BUND,
        }
    })
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

fn paint(c: Rgb, s: &str) -> String {
    format!("{}{s}{OFF}", c.fg())
}
fn bold(c: Rgb, s: &str) -> String {
    format!("{BOLD}{}{s}{OFF}", c.fg())
}
fn dim(s: &str) -> String {
    paint(theme().smoke, s)
}
fn faint(s: &str) -> String {
    paint(theme().ink, s)
}
fn plain(s: &str) -> String {
    paint(theme().paper, s)
}
fn shout(s: &str) -> String {
    if theme().upper { s.to_uppercase() } else { s.to_string() }
}

/// Columns of the terminal we are attached to. The mock honours `COLUMNS`,
/// then asks `stty` on the inherited stdin; the real REPL would ask the tty
/// directly (and re-ask on SIGWINCH). 80 when there is no tty.
fn term_width() -> usize {
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
fn cells(s: &str) -> usize {
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

fn vcells(s: &str) -> usize {
    cells(&strip_ansi(s))
}

/// Word-wrap by display cells, colour codes counting for nothing. A colour
/// that spans a break simply stays on into the next line.
fn wrap(s: &str, width: usize) -> Vec<String> {
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
    theme().cats.iter().find(|(n, _, _)| *n == name).map(|c| c.1).unwrap_or(theme().paper)
}
/// Each cat's chop — one hanzi, stamped beside the name where there is room.
fn chop(name: &str) -> &'static str {
    theme().cats.iter().find(|(n, _, _)| *n == name).map(|c| c.2).unwrap_or("猫")
}
fn who(name: &str) -> String {
    bold(cat(name), name)
}
/// Name with its chop: `玉 tama`.
fn sealed(name: &str) -> String {
    format!("{} {}", paint(cat(name), chop(name)), who(name))
}
fn task(id: &str) -> String {
    paint(theme().done, id)
}

/// Message text with every `@name` lit in that cat's colour — in the log
/// and, live, in the composer as you type it.
fn tags(body: &str) -> String {
    let names = ["meow", "tama", "kuro", "sora", "mimi", "root"];
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
            format!("{}  {}", shade_row("▓▒░ SYNC ░▒▓"), dim("0x04A4 · 5 peers · term 12")),
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
fn banner() {
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
    for (r, l) in lines.iter().enumerate() {
        let mut out = String::new();
        for (c, ch) in l.chars().enumerate() {
            if ch == ' ' {
                out.push(' ');
            } else {
                let pos = (r as f32 / rows) * (1.0 - t.diagonal) + (c as f32 / cols) * t.diagonal;
                out.push_str(&paint(ramp(pos), &ch.to_string()));
            }
        }
        let pad = " ".repeat((42usize).saturating_sub(l.chars().count()));
        let right = side.get(r.wrapping_sub(5)).cloned().unwrap_or_default();
        println!("{out}{pad}  {right}");
    }
    println!();
}

fn kv(zh: &str, en: &str, val: String) {
    let t = theme();
    let label = if t.bracket_labels { format!("[{zh}]") } else { zh.to_string() };
    println!("  {} {} {val}", paint(t.accent, &label), dim(&format!("{:<7}", shout(en))));
}

/// Recent gaps between blocks as bars — the chain's pulse at a glance.
fn sparkline(gaps: &[u64]) -> String {
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

fn section(zh: &str, en: &str) {
    let t = theme();
    let en = shout(en);
    let lead = if t.bracket_labels { format!("{} ", faint("//")) } else { format!("{} ", rule(3)) };
    let label_w = cells(zh) + 1 + cells(&en);
    let rest = term_width().saturating_sub(2 + vcells(&lead) + label_w + 1).max(3);
    println!("\n  {lead}{} {} {}", bold(t.paper, zh), dim(&en), rule(rest));
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
    let pad = " ".repeat(14usize.saturating_sub(cells(time)));
    format!("  {t}{pad}  {}  ", dim(&format!("{:<6}", block_id(block))))
}

/// Cells the stamp occupies: two of margin, the time column, block column
/// and their gaps. Continuation lines hang under the text, not column 0.
const STAMP_W: usize = 2 + 14 + 2 + 6 + 2;

/// A protocol observation — the effect in prose, the protocol's own verbs,
/// wrapped to the terminal under its own column.
fn obs(time: &str, block: u64, text: String) {
    let width = term_width().saturating_sub(STAMP_W).max(20);
    for (i, l) in wrap(&text, width).into_iter().enumerate() {
        if i == 0 {
            println!("{}{l}", stamp(time, block));
        } else {
            println!("{}{l}", " ".repeat(STAMP_W));
        }
    }
}

/// The verbs, each with a glyph and a colour so a scroll can be scanned by
/// shape alone: progress, done, a demand, a poke. The words are the
/// protocol's own; only the case is the skin's.
fn verb(glyph: &str, word: &str, c: Rgb) -> String {
    paint(c, &format!("{glyph} {}", shout(word)))
}
fn claimed() -> String {
    verb("◇", "claimed", theme().progress)
}
fn submitted() -> String {
    verb("◆", "submitted", theme().progress)
}
fn closed() -> String {
    verb("✓", "closed", theme().done)
}
fn directed(what: &str) -> String {
    bold(theme().alarm, &format!("⚑ {}", shout(what)))
}
fn nudged() -> String {
    verb("↻", "nudged", theme().warm)
}

fn mesh(text: String) {
    let t = theme();
    let tag = if t.bracket_labels { "[网] MESH" } else { "网 mesh" };
    println!("  {} {}  {text}", " ".repeat(14), paint(t.cats[3].1, tag));
}

/// A cat speaking: the small Akuma shaded from the sender's colour down
/// into haze, header on its first row, the message flowing beside it.
/// `--no-avatars` collapses this to one `obs` line with a coloured name.
fn said(time: &str, block: u64, from: &str, to: &str, body: &str) {
    let art: Vec<&str> = AVATAR.lines().collect();
    let c = cat(from);
    let arrow = if to == "litter" { dim("· to the litter") } else { format!("{} {}", dim("→"), who(to)) };
    let head = format!("{} {arrow}   {}", sealed(from), stamp(time, block).trim_start());
    let mut rows: Vec<String> = vec![head];
    let width = term_width().saturating_sub(2 + 20 + 2 + 1).max(20);
    rows.extend(wrap(body, width).into_iter().map(|l| tags(&l)));
    let n = rows.len().max(art.len());
    println!();
    for i in 0..n {
        let a = art.get(i).copied().unwrap_or("");
        let r = rows.get(i).cloned().unwrap_or_default();
        let shade = c.lerp(theme().ink, i as f32 / art.len() as f32 * 0.7);
        println!("  {}  {r}", paint(shade, &format!("{a:<20}")));
    }
    println!();
}

/// The prompt as it stands for `target`: `根 root → 玉 tama ▸ `. Slash
/// commands have no target, so just `根 root ▸ `.
fn prompt(target: Option<&str>) -> String {
    let t = theme();
    let to = match target {
        Some("litter") => format!("{} {} ", dim("→"), paint(t.paper, "猫群 litter")),
        Some(n) => format!("{} {} ", dim("→"), sealed(n)),
        None => String::new(),
    };
    format!("{} {to}{} ", sealed("root"), paint(t.accent, t.prompt))
}

/// What you typed, left in scrollback exactly where the composer stood when
/// you hit ⏎ — at the margin, behind its prompt, like any shell.
fn typed(target: Option<&str>, line: &str) {
    println!("  {}{}", prompt(target), tags(line));
}

/// Your own line: the echo, then the chain's word that it was sealed.
fn me(time: &str, block: u64, to: &str, body: &str) {
    typed(Some(to), body);
    obs(time, block, paint(theme().progress, "✓ sealed"));
}

// ── keys ────────────────────────────────────────────────────────────────

/// A key and what it does: the key legible, the label quiet.
fn key(k: &str, what: &str) -> String {
    format!("{} {}", plain(k), dim(what))
}

/// `/keys` — the whole control scheme, printed into the log like any other
/// output. Shell-style: emacs editing keys, no arrows required. Same
/// bindings as bash/zsh in emacs mode, so muscle memory carries over.
fn keys() {
    let col = |a: &str, b: &str| println!("      {a}{}{b}", " ".repeat(34usize.saturating_sub(vcells(a))));
    println!();
    col(&key("⌃a  ⌃e", "start · end of line"), &key("⌃p  ⌃n", "older · newer line"));
    col(&key("⌥b  ⌥f", "word back · forward"), &key("⌃r", "search history"));
    col(&key("⌃b  ⌃f", "char back · forward"), &key("⇥  ⇧⇥", "complete @name /cmd, cycle"));
    col(&key("⌃w  ⌥d", "kill word back · forward"), &key("⏎", "send"));
    col(&key("⌃u  ⌃k", "kill to start · to end"), &key("⌥⏎", "newline"));
    col(&key("⌃y", "yank"), &key("⌃c", "clear the draft"));
    col(&key("⌃t", "swap chars"), &key("⌃d", "quit on an empty line"));
    println!();
    println!("      {}", dim("arrows work too, they are just not needed."));
    println!();
}

// ── the composer ────────────────────────────────────────────────────────

/// One hairline with the connection on its right — the node this client
/// talks to, the primary that will actually seal the block, the head it has
/// seen — then the prompt with the resolved target, the draft with its
/// `@name`s already lit, a hint row. In the real REPL these three rows are
/// the only thing ever redrawn.
fn composer(node: &str, primary: &str, head: u64, target: &str, draft: &str) {
    let t = theme();
    println!();
    let status = format!(
        "{} {} {} {} {} {}",
        paint(t.progress, "●"),
        dim(node),
        dim("→"),
        who(primary),
        dim("·"),
        dim(&format!("head {}", block_id(head)))
    );
    println!("  {}  {status}", rule(term_width().saturating_sub(4 + vcells(&status)).max(3)));
    let prompt = prompt(Some(target));
    println!("  {prompt}{}{}", tags(draft), paint(t.accent, t.cursor));
    let hint = [key("⇥", "complete"), key("⌃r", "history"), key("⌥⏎", "newline"), key("⌃d", "quit"), key("/keys", "all bindings")].join("   ");
    println!("  {}{hint}", " ".repeat(vcells(&prompt)));
}

fn main() {
    let t = theme();
    banner();
    kv("节点", "node", format!("{}  {}", plain("http://192.168.1.123:9944"), dim("replica · 12ms · forwards to the primary")));
    kv("身份", "you", format!("{}  {}", sealed("root"), dim("fe10d60e…3b25a  ~/.akuma/miot/id_ed25519.seed")));
    kv("猫群", "litter", ["meow", "tama", "kuro", "sora", "mimi"].iter().map(|n| sealed(n)).collect::<Vec<_>>().join("   "));
    kv(
        "链",
        "chain",
        format!(
            "{}  {}  {}",
            dim(&format!("head {}", block_id(1188))),
            sparkline(&[6, 40, 1, 52, 3, 108, 51, 62, 69, 2, 14, 41]),
            dim("one block every 37s · leader meow · term 12")
        ),
    );

    section("回放", "replay · last 9 of 1184 events");
    println!();
    obs("21:40", 1176, format!("{} opened {}: {}", who("root"), task("t7"), plain("audit the leader-wins rewind — can a block the primary produced be lost?")));
    obs("21:40 (+6s)", 1177, format!("{} planned {} into {} subtasks", who("meow"), task("t7"), plain("2")));
    obs("21:41 (+40s)", 1178, format!("{} assigned to {}: {}", task("t7.1"), who("tama"), dim("read rewind_for_fork in miot-store, list what it drops")));
    obs("21:41 (+1s)", 1179, format!("{} assigned to {}: {}", task("t7.2"), who("kuro"), dim("check /events seq reset in the agent loop cursor")));
    obs("21:42 (+52s)", 1180, format!("{} {} {}", who("tama"), claimed(), task("t7.1")));
    obs("21:42 (+3s)", 1181, format!("{} {} {}", who("kuro"), claimed(), task("t7.2")));
    said(
        "21:44 (+1m48s)",
        1182,
        "tama",
        "root",
        "Yes. rewind_for_fork truncates back to the last compaction and re-pulls from the new primary; \
         a block only the dead primary held is gone. Records, not work — the cat re-submits. There is no commit index.",
    );
    obs("21:45 (+51s)", 1183, format!("{} {} {}: {}", who("kuro"), submitted(), task("t7.2"), dim("cursor resets on seq going backwards, one case unhandled")));
    obs("21:46 (+1m02s)", 1184, format!("{} {} by {}: {}", task("t7.1"), closed(), who("tama"), plain("Leader-wins rewind: what is lost and when")));
    section("回放结束", "end replay");

    println!();
    mesh(format!("leader {} · term {} · {} alive", who("meow"), plain("12"), plain("5/5")));
    me("21:47 (+1m09s)", 1185, "litter", "nice. @kuro what was the unhandled case?");
    obs("21:47 (+2s)", 1186, format!("{} directed on {}: {}", who("kuro"), task("t7.2"), directed("ArtifactNeeded")));
    said(
        "21:47 (+14s)",
        1187,
        "kuro",
        "root",
        "A node that rebuilt its log after an adopted checkpoint hands out seq starting at 0 again — the agent loop \
         handles the plain rewind but not that one. Writing it up as the artifact now, @tama you may want to read it.",
    );
    obs("21:48 (+41s)", 1188, format!("{} {} {} {}", who("sora"), nudged(), task("t7.2"), dim("(2 left)")));
    mesh(format!("{} {}", who("mimi"), paint(t.warm, "went stale")));
    typed(None, "/keys");
    keys();

    composer("192.168.1.123:9944", "meow", 1188, "tama", "@tama can you re-run the build with -j1 and paste ");
    let others = ["bund", "neon", "ink"].iter().filter(|s| **s != t.name).cloned().collect::<Vec<_>>().join(" · ");
    println!("\n  {}  {}", plain("再见 · bye."), dim(&format!("skin {}  ·  try {others}  ·  repl-mock <skin> or KOT_THEME", t.name)));
}
