//! The `kot` REPL, the *look* — a mock. Prints one fake session the way the
//! real REPL (`client::repl`) should render it, then exits. No node, no
//! network, no input. `cargo run -p kot --bin repl-mock`.
//!
//! Rules it obeys, all from `docs/CLI.md`:
//!  §0 ordinary stdout lines, no alternate screen — the terminal owns
//!     scrollback, select/copy and tmux copy-mode keep working;
//!  §1 the log grows upward; the composer is pinned last and, in the real
//!     thing, redrawn with relative moves only (clear its lines, print the
//!     new output *above*, draw it again) — output already printed never
//!     moves;
//!  §2 the prompt shows the resolved target (`root → tama ▸`);
//!  §6 banner once (`akuma_40`), one colour per sender, the `akuma_20`
//!     avatar beside a cat's `said`, no box-drawing around the log.
//!
//! Palette: the Bund after dark. Pearl Tower pink at the top of the cat
//! fading into Lujiazui cyan, gold for the operator, and one neon per cat.
//! Line editing (↑↓ history, ←→, ⌃r, tab-complete `@name`) is the real
//! REPL's job via a readline crate; here it is a hint line.

const BANNER: &str = include_str!("../../../../assets/akuma_40.txt");
const AVATAR: &str = include_str!("../../../../assets/akuma_20.txt");

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

const PINK: Rgb = Rgb(255, 45, 149); // 东方明珠 — the Pearl Tower's lights
const CYAN: Rgb = Rgb(0, 229, 255); // 陆家嘴 — Lujiazui glass
const GOLD: Rgb = Rgb(255, 194, 51); // 外滩 — the Bund's facades
const JADE: Rgb = Rgb(46, 230, 166);
const VIOLET: Rgb = Rgb(177, 124, 255);
const TANGERINE: Rgb = Rgb(255, 138, 61);
const RED: Rgb = Rgb(255, 75, 62); // 红灯笼
const SMOKE: Rgb = Rgb(107, 111, 122); // Huangpu haze, the dim tone
const PAPER: Rgb = Rgb(232, 232, 236);

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
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb(m(self.0, o.0), m(self.1, o.1), m(self.2, o.2))
    }
}

fn paint(c: Rgb, s: &str) -> String {
    format!("{}{s}{OFF}", c.fg())
}
fn bold(c: Rgb, s: &str) -> String {
    format!("{BOLD}{}{s}{OFF}", c.fg())
}
fn dim(s: &str) -> String {
    paint(SMOKE, s)
}

/// Columns of the terminal we are attached to. The mock shells out to
/// `stty` on the inherited stdin; the real REPL would ask the tty directly
/// (and re-ask on SIGWINCH). 80 when there is no tty.
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
    s.chars().map(|c| if matches!(c as u32, 0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6) { 2 } else { 1 }).sum()
}

/// One colour per sender, forever — that is what makes the scroll readable.
fn cat(name: &str) -> Rgb {
    match name {
        "root" | "you" => GOLD,
        "meow" => PINK,
        "tama" => CYAN,
        "kuro" => VIOLET,
        "sora" => JADE,
        "mimi" => TANGERINE,
        _ => PAPER,
    }
}
fn who(name: &str) -> String {
    bold(cat(name), name)
}
fn task(id: &str) -> String {
    paint(GOLD, id)
}

/// A hairline that shades pink → cyan over `n` cells. The only rule we draw.
fn rule(n: usize) -> String {
    (0..n).map(|i| paint(PINK.lerp(CYAN, i as f32 / n as f32), "─")).collect()
}

/// The banner, top to bottom: Pearl Tower pink into river-glass cyan, with
/// the title block sitting to its right, neofetch-style.
fn banner() {
    let lines: Vec<&str> = BANNER.lines().collect();
    let n = lines.len().max(1) as f32;
    let side = [
        format!("{}  {}", bold(PINK, "恶魔猫窝"), dim("· akuma miot")),
        dim("distributed cat system"),
        format!("{}  {}", dim("a litter of models, coordinating"), dim("on chain")),
        String::new(),
        format!("{} {}", dim("kot"), dim(env!("CARGO_PKG_VERSION"))),
    ];
    for (i, l) in lines.iter().enumerate() {
        let c = PINK.lerp(CYAN, i as f32 / n);
        let right = side.get(i.wrapping_sub(5)).cloned().unwrap_or_default();
        println!("{}  {right}", paint(c, &format!("{l:<42}")));
    }
    println!();
}

fn kv(zh: &str, en: &str, val: String) {
    println!("  {} {} {val}", paint(PINK, zh), dim(&format!("{en:<7}")));
}

fn section(zh: &str, en: &str) {
    let label_w = cells(zh) + 1 + cells(en);
    let rest = term_width().saturating_sub(2 + 3 + 1 + label_w + 1).max(3);
    println!("\n  {} {} {} {}", rule(3), bold(PAPER, zh), dim(en), rule(rest));
}

/// Wall-clock time, plus the gap since the previous event in parentheses:
/// `21:40`, then `21:42 (+3s)`. The first line of a run has no gap.
fn stamp(time: &str, block: u64) -> String {
    format!("  {}  {}  ", dim(&format!("{time:<14}")), dim(&format!("#{block:<5}")))
}

/// Cells the stamp occupies: two of margin, the time column, block column
/// and their gaps. Continuation lines hang under the text, not column 0.
const STAMP_W: usize = 2 + 14 + 2 + 6 + 2;

/// A protocol observation — the effect in prose, the protocol's own verbs.
/// Wrapped to the terminal by us so a long `opened` reads as a paragraph
/// under its own column, not as the terminal's ragged hard wrap.
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

fn mesh(text: String) {
    println!("  {} {}  {text}", " ".repeat(14), paint(VIOLET, "网 mesh"));
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

/// Word-wrap by display cells, colour codes counting for nothing. A colour
/// that spans a break simply stays on into the next line.
fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut w_cells = 0usize;
    for w in s.split_whitespace() {
        let wc = cells(&strip_ansi(w));
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

/// A cat speaking: the small Akuma in the sender's colour, header on its
/// first row, the message flowing beside it. `--no-avatars` collapses this
/// to one `obs` line with a coloured name.
fn said(time: &str, block: u64, from: &str, to: &str, body: &str) {
    let art: Vec<&str> = AVATAR.lines().collect();
    let c = cat(from);
    let arrow = if to == "litter" { dim("· to the litter") } else { format!("{} {}", dim("→"), who(to)) };
    let head = format!("{} {arrow}   {} {}", who(from), dim(time), dim(&format!("· #{block}")));
    let mut rows: Vec<String> = vec![head];
    let width = term_width().saturating_sub(2 + 20 + 2 + 1).max(20);
    rows.extend(wrap(body, width).into_iter().map(|l| paint(PAPER, &l)));
    let n = rows.len().max(art.len());
    println!();
    for i in 0..n {
        let a = art.get(i).copied().unwrap_or("");
        let r = rows.get(i).cloned().unwrap_or_default();
        println!("  {}  {r}", paint(c, &format!("{a:<20}")));
    }
    println!();
}

fn me(time: &str, block: u64, to: &str, body: &str) {
    let arrow = if to == "litter" { dim("· to the litter") } else { format!("{} {}", dim("→"), who(to)) };
    obs(time, block, format!("{} {arrow}  {}", who("root"), paint(PAPER, body)));
}

/// The composer: one hairline with the connection on its right — the node
/// this client talks to, the primary that will actually seal the block, the
/// head it has seen — then the prompt with the resolved target, the line
/// being edited, a hint row. In the real REPL these three rows are the only
/// thing ever redrawn.
fn composer(node: &str, primary: &str, head: u64, target: &str, draft: &str) {
    println!();
    let status_plain = format!("● {node} → {primary} · head #{head}");
    let status = format!(
        "{} {} {} {} {} {}",
        paint(JADE, "●"),
        dim(node),
        dim("→"),
        who(primary),
        dim("·"),
        dim(&format!("head #{head}"))
    );
    println!("  {}  {status}", rule(term_width().saturating_sub(4 + cells(&status_plain)).max(3)));
    let prompt = format!("{} {} {} {} ", who("root"), dim("→"), who(target), paint(PINK, "▸"));
    println!("  {prompt}{}{}", paint(PAPER, draft), paint(PINK, "▍"));
    println!(
        "  {}",
        dim("               ↑↓ history   ←→ edit   ⌃r search   ⇥ complete @name   ⌃c quit")
    );
}

fn main() {
    banner();
    kv("节点", "node", format!("{}  {}", paint(PAPER, "http://192.168.1.123:9944"), dim("replica, forwards to the primary")));
    kv("身份", "you", format!("{}  {}", who("root"), dim("fe10d60e…3b25a  ~/.akuma/miot/id_ed25519.seed")));
    kv(
        "猫群",
        "litter",
        ["meow", "tama", "kuro", "sora", "mimi"].iter().map(|n| format!("{} {}", paint(cat(n), "●"), who(n))).collect::<Vec<_>>().join("  "),
    );

    section("回放", "replay · last 9 of 1184 events");
    println!();
    obs("21:40", 1176, format!("{} opened {}: {}", who("root"), task("t7"), paint(PAPER, "audit the leader-wins rewind — can a block the primary produced be lost?")));
    obs("21:40 (+6s)", 1177, format!("{} planned {} into {} subtasks", who("meow"), task("t7"), paint(PAPER, "2")));
    obs("21:41 (+40s)", 1178, format!("{} assigned to {}: {}", task("t7.1"), who("tama"), dim("read rewind_for_fork in miot-store, list what it drops")));
    obs("21:41 (+1s)", 1179, format!("{} assigned to {}: {}", task("t7.2"), who("kuro"), dim("check /events seq reset in the agent loop cursor")));
    obs("21:42 (+52s)", 1180, format!("{} {} on {}", who("tama"), paint(JADE, "claimed"), task("t7.1")));
    obs("21:42 (+3s)", 1181, format!("{} {} on {}", who("kuro"), paint(JADE, "claimed"), task("t7.2")));
    said(
        "21:44 (+1m48s)",
        1182,
        "tama",
        "root",
        "Yes. rewind_for_fork truncates back to the last compaction and re-pulls from the new primary; \
         a block only the dead primary held is gone. Records, not work — the cat re-submits. There is no commit index.",
    );
    obs("21:45 (+51s)", 1183, format!("{} {} on {}: {}", who("kuro"), paint(JADE, "submitted"), task("t7.2"), dim("cursor resets on seq going backwards, one case unhandled")));
    obs("21:46 (+1m02s)", 1184, format!("{} closed by {}: {}", task("t7.1"), who("tama"), paint(PAPER, "Leader-wins rewind: what is lost and when")));
    section("回放结束", "end replay");

    println!();
    mesh(format!("leader {} · term {} · {} alive", who("meow"), paint(PAPER, "12"), paint(PAPER, "5/5")));
    me("21:47 (+1m09s)", 1185, "litter", "nice. @kuro what was the unhandled case?");
    obs("21:47 (+2s)", 1186, format!("{} directed on {}: {}", who("kuro"), task("t7.2"), paint(RED, "ArtifactNeeded")));
    said(
        "21:47 (+14s)",
        1187,
        "kuro",
        "root",
        "A node that rebuilt its log after an adopted checkpoint hands out seq starting at 0 again — the agent loop \
         handles the plain rewind but not that one. Writing it up as the artifact now.",
    );
    obs("21:48 (+41s)", 1188, format!("{} nudged on {} {}", who("sora"), task("t7.2"), dim("(2 left)")));
    mesh(format!("{} {}", who("mimi"), dim("went stale")));

    composer("192.168.1.123:9944", "meow", 1188, "tama", "@tama can you re-run the build with -j1 and paste ");
    println!("\n  {}", dim("再见 · bye.  (mock — the real composer would wait on a line editor here)"));
}
