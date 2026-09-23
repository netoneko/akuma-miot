//! A thin driver over `kot::ui` — the *look*, standalone. Renders what a
//! node holds (its event log, head and mesh) with the same code the real
//! REPL (`client::repl`) now uses, draws the composer, and exits.
//! Read-only: no signing, no input.
//!
//!     cargo run --release -p kot --bin repl-mock -- --node http://192.168.1.123:9944
//!     cargo run --release -p kot --bin repl-mock -- neon --node http://192.168.1.123:9944 --last 50
//!     cargo run --release -p kot --bin repl-mock -- ink --demo        # the canned session, no node
//!
//! Skins: `bund` (the Bund after dark, default), `neon` (Lujiazui in the
//! rain), `ink` (水墨). `KOT_THEME`, `MIOT_NODE` and `MIOT_ROSTER` are
//! honoured like `kot` does; with no roster the fleet's from
//! `overlays/deploy/mesh.env` is used, so the fleet's keys get names.
//!
//! Blocks carry no timestamp, so times are estimated: the head is "now"
//! and every block is `BLOCK_MS` earlier — marked `≈`. Gaps between events
//! are exact in blocks, so `(+12s)` is two blocks.
//!
//! Now that `kot::ui` is shared, this binary exists to iterate on the look
//! without a live litter (`--demo`) or against a real node with no risk of
//! sending anything (it never submits, never even connects for writes). It
//! just `println!`s what `kot::ui` returns; the real REPL feeds the same
//! strings to a ratatui inline viewport instead (`client::repl`).

use kot::common::Roster;
use kot::ui;
use std::sync::OnceLock;

fn roster() -> &'static Roster {
    static R: OnceLock<Roster> = OnceLock::new();
    R.get_or_init(|| {
        let spec = arg("--roster")
            .or_else(|| std::env::var("MIOT_ROSTER").ok())
            .or_else(|| include_str!("../../../../overlays/deploy/mesh.env").lines().find_map(|l| l.strip_prefix("MIOT_ROSTER=").map(str::to_string)))
            .unwrap_or_else(|| "root=1,mimi=2,tama=3,kuro=4,sora=5".into());
        Roster::parse(&spec).unwrap_or_else(|e| {
            eprintln!("bad roster: {e}");
            std::process::exit(2)
        })
    })
}

/// `--flag value` from argv.
fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1).cloned())
}

async fn get(http: &reqwest::Client, node: &str, path: &str) -> Result<serde_json::Value, String> {
    let r = http.get(format!("{node}{path}")).send().await.map_err(|e| format!("{node}{path}: {e}"))?;
    r.json().await.map_err(|e| format!("{node}{path}: bad json: {e}"))
}

/// The session, from a real node: header, the last `--last` events, the
/// mesh as it stands, the composer. Then exit — the real REPL keeps tailing
/// `/events` and redraws the composer in its inline viewport instead.
async fn live(node: &str, last: usize) -> Result<(), String> {
    let node = node.trim_end_matches('/');
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().unwrap();

    let t0 = std::time::Instant::now();
    let head = get(&http, node, "/head").await?;
    let latency = t0.elapsed().as_millis();
    let head_block = head["block"].as_u64().unwrap_or(0);
    let leader = head["leader"].as_str().and_then(|s| miot_keys::from_hex(s).ok()).map(|a| roster().name_of(&a));
    let events = match get(&http, node, "/events?since=0").await? {
        serde_json::Value::Array(v) => v,
        _ => Vec::new(),
    };
    let mesh_v = get(&http, node, "/mesh/peers").await.ok();
    // Two leaders, deliberately: `/head`'s is the litter's leader *cat*
    // (pallet `set_leader`, who plans), the mesh's is the elected node that
    // seals blocks. Writes go to the second.
    let primary = mesh_v.as_ref().and_then(|m| {
        std::iter::once(&m["me"])
            .chain(m["peers"].as_array().into_iter().flatten().map(|p| &p["status"]))
            .find(|st| st["role"].as_str() == Some("leader"))
            .and_then(|st| st["account"].as_str())
            .and_then(|s| miot_keys::from_hex(s).ok())
            .map(|a| roster().name_of(&a))
    });
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);

    let me_id = kot::common::load_or_create_identity(&kot::common::root_identity_path(), "miot-root");
    let me_name = roster().name_of(&me_id.account());

    println!("{}", ui::banner());
    let role = mesh_v.as_ref().and_then(|m| m["me"]["role"].as_str()).unwrap_or("?").to_string();
    let fwd = if role == "leader" { "primary, seals blocks itself" } else { "forwards writes to the primary" };
    println!("{}", ui::kv("节点", "node", format!("{}  {}", ui::plain(node), ui::dim(&format!("{role} · {latency}ms · {fwd}")))));
    println!(
        "{}",
        ui::kv(
            "身份",
            "you",
            format!("{}  {}", ui::sealed(&me_name), ui::dim(&format!("{}  {}", miot_keys::short(&me_id.account()), kot::common::root_identity_path().display()))),
        )
    );
    println!("{}", ui::kv("猫群", "litter", roster().names().filter(|n| *n != me_name).map(ui::sealed).collect::<Vec<_>>().join("   ")));
    let blocks: Vec<u64> = events.iter().filter_map(|e| e["block"].as_u64()).collect();
    let gaps: Vec<u64> = blocks.windows(2).map(|w| w[1].saturating_sub(w[0])).rev().take(16).collect::<Vec<_>>().into_iter().rev().collect();
    let term = mesh_v.as_ref().and_then(|m| m["me"]["term"].as_u64()).unwrap_or(0);
    println!(
        "{}",
        ui::kv(
            "链",
            "chain",
            format!(
                "{}  {}  {}",
                ui::dim(&format!("head #{head_block}")),
                if gaps.is_empty() { ui::dim("no events yet") } else { ui::sparkline(&gaps) },
                ui::dim(&format!(
                    "{}s blocks · primary {} · term {} · leader cat {}",
                    kot::node::BLOCK_MS / 1000,
                    primary.clone().unwrap_or_else(|| "nobody".into()),
                    term,
                    leader.clone().unwrap_or_else(|| "unset".into())
                ))
            ),
        )
    );

    let shown = events.len().min(last);
    println!("{}", ui::section("回放", &format!("replay · last {shown} of {} events · ≈ times from {}s blocks", events.len(), kot::node::BLOCK_MS / 1000)));
    println!();
    if events.is_empty() {
        println!("{}", ui::dim("nothing on this chain yet"));
    }
    let start = events.len() - shown;
    let mut prev: Option<u64> = if start > 0 { events[start - 1]["block"].as_u64() } else { None };
    for e in &events[start..] {
        let block = e["block"].as_u64().unwrap_or(0);
        let time = ui::when(block, prev, head_block, now, kot::node::BLOCK_MS);
        println!("{}", ui::render(&time, block, &e["effect"], roster(), &me_name));
        prev = Some(block);
    }
    println!("{}", ui::section("回放结束", "end replay"));
    println!();

    if let Some(m) = &mesh_v {
        let peers = m["peers"].as_array().cloned().unwrap_or_default();
        let total = 1 + peers.len();
        let mut alive = 1;
        let mut stale = Vec::new();
        for p in &peers {
            let seen = p["seen_ms_ago"].as_u64();
            let name = p["status"]["account"].as_str().and_then(|s| miot_keys::from_hex(s).ok()).map(|a| roster().name_of(&a));
            match (seen, name) {
                (Some(ms), Some(n)) if ms <= 5_000 => {
                    alive += 1;
                    let _ = n;
                }
                (Some(_), Some(n)) => stale.push(format!("{} {}", ui::who(&n), ui::warn("stale"))),
                (_, _) => stale.push(format!("{} {}", ui::dim(p["route"].as_str().unwrap_or("?")), ui::warn("never answered"))),
            }
        }
        println!(
            "{}",
            ui::mesh(format!(
                "primary {} · term {} · {} alive · quorum {} · checkpoint #{}",
                primary.as_deref().map(ui::who).unwrap_or_else(|| ui::dim("nobody (election)")),
                ui::plain(&term.to_string()),
                ui::plain(&format!("{alive}/{total}")),
                ui::plain(&m["quorum"].to_string()),
                m["last_checkpoint"].as_u64().unwrap_or(0)
            ))
        );
        for s in stale {
            println!("{}", ui::mesh(s));
        }
    }

    let draft = "";
    for l in ui::composer_lines(
        node.trim_start_matches("http://"),
        primary.as_deref().unwrap_or("?"),
        head_block,
        &me_name,
        "litter",
        draft,
        draft.chars().count(),
        roster(),
        &ui::hint(),
    ) {
        println!("{l}");
    }
    Ok(())
}

/// The canned session — the same rendering with invented events, for
/// working on the look without a node.
fn demo() {
    println!("{}", ui::banner());
    println!("{}", ui::kv("节点", "node", format!("{}  {}", ui::plain("http://192.168.1.123:9944"), ui::dim("replica · 12ms · forwards to the primary"))));
    println!("{}", ui::kv("身份", "you", format!("{}  {}", ui::sealed("root"), ui::dim("fe10d60e…3b25a  ~/.akuma/miot/id_ed25519.seed"))));
    println!("{}", ui::kv("猫群", "litter", ["meow", "tama", "kuro", "sora", "mimi"].iter().map(|n| ui::sealed(n)).collect::<Vec<_>>().join("   ")));
    println!(
        "{}",
        ui::kv(
            "链",
            "chain",
            format!(
                "{}  {}  {}",
                ui::dim("head #1188"),
                ui::sparkline(&[6, 40, 1, 52, 3, 108, 51, 62, 69, 2, 14, 41]),
                ui::dim("one block every 37s · leader meow · term 12")
            ),
        )
    );

    println!("{}", ui::section("回放", "replay · last 9 of 1184 events"));
    println!();
    let r = roster();
    println!(
        "{}",
        ui::obs("21:40", 1176, format!("{} opened {}: {}", ui::who("root"), ui::task("t7"), ui::plain("audit the leader-wins rewind — can a block the primary produced be lost?")))
    );
    println!("{}", ui::obs("21:40 (+6s)", 1177, format!("{} planned {} into {} subtasks", ui::who("meow"), ui::task("t7"), ui::plain("2"))));
    println!(
        "{}",
        ui::obs(
            "21:41 (+40s)",
            1178,
            format!("{} assigned to {}: {}", ui::task("t7.1"), ui::who("tama"), ui::dim("read rewind_for_fork in miot-store, list what it drops")),
        )
    );
    println!(
        "{}",
        ui::obs(
            "21:41 (+1s)",
            1179,
            format!("{} assigned to {}: {}", ui::task("t7.2"), ui::who("kuro"), ui::dim("check /events seq reset in the agent loop cursor")),
        )
    );
    println!("{}", ui::obs("21:42 (+52s)", 1180, format!("{} {} {}", ui::who("tama"), ui::claimed(), ui::task("t7.1"))));
    println!("{}", ui::obs("21:42 (+3s)", 1181, format!("{} {} {}", ui::who("kuro"), ui::claimed(), ui::task("t7.2"))));
    println!(
        "{}",
        ui::said(
            "21:44 (+1m48s)",
            1182,
            "tama",
            "root",
            "Yes. rewind_for_fork truncates back to the last compaction and re-pulls from the new primary; \
             a block only the dead primary held is gone. Records, not work — the cat re-submits. There is no commit index.",
            r,
        )
    );
    println!(
        "{}",
        ui::obs(
            "21:45 (+51s)",
            1183,
            format!("{} {} {}: {}", ui::who("kuro"), ui::submitted(), ui::task("t7.2"), ui::dim("cursor resets on seq going backwards, one case unhandled")),
        )
    );
    println!(
        "{}",
        ui::obs("21:46 (+1m02s)", 1184, format!("{} {} by {}: {}", ui::task("t7.1"), ui::closed(), ui::who("tama"), ui::plain("Leader-wins rewind: what is lost and when")))
    );
    println!("{}", ui::section("回放结束", "end replay"));

    println!();
    println!("{}", ui::mesh(format!("leader {} · term {} · {} alive", ui::who("meow"), ui::plain("12"), ui::plain("5/5"))));
    println!("{}", ui::me("21:47 (+1m09s)", 1185, "root", "litter", "nice. @kuro what was the unhandled case?", r));
    println!("{}", ui::obs("21:47 (+2s)", 1186, format!("{} directed on {}: {}", ui::who("kuro"), ui::task("t7.2"), ui::directed("ArtifactNeeded"))));
    println!(
        "{}",
        ui::said(
            "21:47 (+14s)",
            1187,
            "kuro",
            "root",
            "A node that rebuilt its log after an adopted checkpoint hands out seq starting at 0 again — the agent loop \
             handles the plain rewind but not that one. Writing it up as the artifact now, @tama you may want to read it.",
            r,
        )
    );
    println!("{}", ui::obs("21:48 (+41s)", 1188, format!("{} {} {} {}", ui::who("sora"), ui::nudged(), ui::task("t7.2"), ui::dim("(2 left)"))));
    println!("{}", ui::mesh(format!("{} {}", ui::who("mimi"), ui::warn("went stale"))));
    println!("{}", ui::typed("root", None, "/keys", r));
    println!("{}", ui::keys());

    let draft = "@tama can you re-run the build with -j1 and paste ";
    for l in ui::composer_lines("192.168.1.123:9944", "meow", 1188, "root", "tama", draft, draft.chars().count(), r, &ui::hint()) {
        println!("{l}");
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(skin) = args.iter().skip(1).find(|a| ["bund", "neon", "ink"].contains(&a.as_str())) {
        std::env::set_var("KOT_THEME", skin);
    }
    if args.iter().any(|a| a == "--demo") {
        demo();
    } else {
        let node = arg("--node").or_else(|| std::env::var("MIOT_NODE").ok()).unwrap_or_else(|| "http://127.0.0.1:9944".into());
        let last = arg("--last").and_then(|n| n.parse().ok()).unwrap_or(30);
        if let Err(e) = live(&node, last).await {
            eprintln!("  {}", ui::alert(&format!("✗ {e}")));
            eprintln!("  {}", ui::dim("no node? `repl-mock --demo` shows the canned session."));
            std::process::exit(1);
        }
    }
    let name = ui::theme().name;
    let others = ["bund", "neon", "ink"].iter().filter(|s| **s != name).cloned().collect::<Vec<_>>().join(" · ");
    println!("\n  {}  {}", ui::plain("再见 · bye."), ui::dim(&format!("skin {name}  ·  try {others}  ·  repl-mock <skin> or KOT_THEME")));
}
