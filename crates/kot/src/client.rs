//! The operator's side: a client of **any** node, holding no state.
//!
//! One-shot verbs (`kot task open`, `kot say`, `kot peers`, …) and the bare
//! `kot` REPL. Every write is a real signed
//! [`miot_runtime::UncheckedExtrinsic`] POSTed over HTTP — the same wire the
//! agent loop uses (`miot_runtime::client::sign` is the shared piece).
//!
//! **Any node will do** (`docs/CLI.md` §5a). The client tries `--node` and
//! then each of `MIOT_NODES` in order, and uses the first that answers. A
//! replica forwards writes to the primary itself, so the client never needs
//! to know which node that is.
//!
//! There is no OpenSSH **private**-key loading here: signing defaults to the
//! project-native seed at `~/.akuma/miot/id_ed25519.seed` (created on first
//! use), and `--as <name>` signs with a roster seed instead.

use ansi_to_tui::IntoText;
use codec::Encode;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures_util::StreamExt;
use miot_keys::Identity;
use miot_runtime::{client, AccountId, RuntimeCall};
use polkadot_sdk::*;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use ratatui_textarea::{CursorMove, TextArea};
use sp_core::H256;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

use crate::common::{Roster, DIM, OFF};
use crate::ui;

pub struct Client {
    http: reqwest::Client,
    candidates: Vec<String>,
    pub node: String,
    pub identity: Identity,
    pub roster: Roster,
}

impl Client {
    /// Pick the first node in `candidates` that answers `/head`.
    pub async fn connect(candidates: Vec<String>, identity: Identity, roster: Roster) -> Result<Self, String> {
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().unwrap();
        let mut c = Client { http, candidates, node: String::new(), identity, roster };
        c.reconnect().await?;
        Ok(c)
    }

    /// Move to the next live node. A dead endpoint is a reconnect, not an
    /// outage.
    pub async fn reconnect(&mut self) -> Result<(), String> {
        for n in &self.candidates {
            let probe = self.http.get(format!("{n}/head")).timeout(std::time::Duration::from_secs(3)).send().await;
            if matches!(probe, Ok(ref r) if r.status().is_success()) {
                if self.node != *n && !self.node.is_empty() {
                    eprintln!("  {DIM}switched to {n}{OFF}");
                }
                self.node = n.clone();
                return Ok(());
            }
        }
        Err(format!("no node answered (tried {})", self.candidates.join(", ")))
    }

    async fn get_json(&mut self, path: &str) -> Result<serde_json::Value, String> {
        for attempt in 0..2 {
            match self.http.get(format!("{}{path}", self.node)).send().await {
                Ok(r) => return r.json().await.map_err(|e| format!("bad response from {}{path}: {e}", self.node)),
                Err(e) if attempt == 0 => {
                    eprintln!("  {DIM}{} unreachable ({e}); trying the next node{OFF}", self.node);
                    self.reconnect().await?;
                }
                Err(e) => return Err(format!("node unreachable: {e}")),
            }
        }
        unreachable!()
    }

    async fn meta(&mut self) -> Result<client::Meta, String> {
        let v = self.get_json("/meta").await?;
        let gh = hex::decode(v["genesis_hash"].as_str().ok_or("no genesis_hash")?).map_err(|e| e.to_string())?;
        Ok(client::Meta {
            genesis_hash: H256::from_slice(&gh),
            spec_version: v["spec_version"].as_u64().ok_or("no spec_version")? as u32,
            tx_version: v["tx_version"].as_u64().ok_or("no tx_version")? as u32,
        })
    }

    /// Sign and submit, no printing — for a caller (the ratatui REPL) that
    /// renders the outcome itself. [`Client::submit`] is the printing
    /// wrapper the one-shot verbs use.
    pub async fn try_submit(&mut self, call: RuntimeCall) -> Result<(), String> {
        let m = self.meta().await?;
        let who = miot_keys::to_hex(&self.identity.account());
        let nonce = self.get_json(&format!("/account/{who}")).await?["nonce"].as_u64().unwrap_or(0) as u32;
        let uxt = client::sign(&self.identity, call, nonce, &m);
        let r = self.http.post(format!("{}/submit", self.node)).body(uxt.encode()).send().await.map_err(|e| format!("node unreachable: {e}"))?;
        if r.status().is_success() {
            return Ok(());
        }
        let e: serde_json::Value = r.json().await.unwrap_or_default();
        let msg = e.get("error").unwrap_or(&e).to_string();
        if msg.contains("Payment") {
            return Err(format!(
                "{msg} — {} isn't a member of this chain (no `providers`; see HANDOFF.md's \"catnip\"). \
                 Add its account to MIOT_MEMBERS/--root on every node, or sign --as a member.",
                miot_keys::short(&self.identity.account())
            ));
        }
        Err(msg)
    }

    /// Sign and submit. Prints the outcome; returns whether it landed.
    pub async fn submit(&mut self, call: RuntimeCall) -> bool {
        match self.try_submit(call).await {
            Ok(()) => {
                println!("  submitted, signed as {}", self.roster.name_of(&self.identity.account()));
                true
            }
            Err(e) => {
                println!("  refused: {e}");
                false
            }
        }
    }

    pub async fn head_seq(&mut self) -> u64 {
        self.get_json("/head").await.ok().and_then(|h| h["seq"].as_u64()).unwrap_or(0)
    }

    pub async fn print_tasks(&mut self) {
        println!("{}", self.tasks_text().await);
    }

    /// [`Client::print_tasks`]'s rows, joined by `\n` instead of printed —
    /// so the ratatui REPL can feed it through an inline-viewport insert.
    pub async fn tasks_text(&mut self) -> String {
        let rows = match self.get_json("/tasks").await {
            Ok(serde_json::Value::Array(rows)) => rows,
            Ok(_) => Vec::new(),
            Err(e) => return format!("  {}", ui::alert(&e)),
        };
        if rows.is_empty() {
            return format!("  {}", ui::dim("no tasks"));
        }
        rows.iter()
            .map(|t| {
                let id = t["id"].as_str().unwrap_or("?");
                let status = t["status"].as_str().unwrap_or("?");
                let who = t["holder"]
                    .as_str()
                    .or_else(|| t["assignee"].as_str())
                    .and_then(|s| miot_keys::from_hex(s).ok())
                    .map(|a| self.roster.name_of(&a));
                let lease = t["lease_until"].as_u64().map(|b| format!(" lease→{b}")).unwrap_or_default();
                let text = t["text"].as_str().unwrap_or("");

                let id_plain = format!("{id:<6}");
                let status_plain = format!("{status:<18}");
                let who_plain = format!("{:<10}", who.as_deref().unwrap_or("-"));
                let indent = 2 + ui::vcells(&id_plain) + 1 + ui::vcells(&status_plain) + 1 + ui::vcells(&who_plain) + ui::vcells(&lease) + 2;

                let status_c = match status {
                    "Open" | "Planned" => ui::ok(&status_plain),
                    "Failed" => ui::alert(&status_plain),
                    "Closed" | "Cleared" => ui::dim(&status_plain),
                    _ => ui::warn(&status_plain), // Pending, AwaitingClearance, ...
                };
                let who_c = match &who {
                    Some(n) => ui::pad(&ui::who(n), 10),
                    None => ui::dim(&who_plain),
                };

                format!("  {}  {status_c} {who_c}{}  {}", ui::task(&id_plain), ui::dim(&lease), ui::hang(indent, text))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Markdown on stdout, nothing else — so it pipes (`docs/CLI.md` §5).
    pub async fn print_artifact(&mut self, id: &str) -> bool {
        match self.get_json(&format!("/artifact/{id}")).await {
            Ok(a) if a["found"] == true => {
                println!("{}", a["body"].as_str().unwrap_or(""));
                true
            }
            Ok(_) => {
                eprintln!("no artifact for {id} (not closed yet, or no such task)");
                false
            }
            Err(e) => {
                eprintln!("{e}");
                false
            }
        }
    }

    /// The litter roster, then the mesh as seen from the connected node:
    /// who's primary, each peer's term, head and how recently it answered.
    pub async fn print_peers(&mut self) {
        println!("{}", self.peers_text().await);
    }

    /// [`Client::print_peers`], joined by `\n` instead of printed — so the
    /// ratatui REPL can feed it through an inline-viewport insert.
    pub async fn peers_text(&mut self) -> String {
        let lit = self.get_json("/head").await.ok().and_then(|h| h["leader"].as_str().map(str::to_string));
        let mut out = vec![format!("  {}", ui::dim("litter"))];
        for (n, a) in &self.roster.0 {
            let tag = if lit.as_deref() == Some(miot_keys::to_hex(a).as_str()) { format!("  {}", ui::ok("leader")) } else { String::new() };
            out.push(format!("    {}  {}{tag}", ui::pad(&ui::who(n), 14), ui::dim(&miot_keys::short(a))));
        }
        let m = match self.get_json("/mesh/peers").await {
            Ok(m) => m,
            Err(e) => {
                out.push(format!("  mesh: {}", ui::alert(&e)));
                return out.join("\n");
            }
        };
        let me = &m["me"];
        let names = mesh_names(&m, &self.roster);
        let cat_of = |mesh_name: &str| names.get(mesh_name).cloned().unwrap_or_else(|| mesh_name.to_string());

        out.push(format!(
            "  {} {} mesh, from {} ({})  quorum {}  last checkpoint #{}",
            ui::dim("网"),
            ui::dim("mesh,"),
            ui::who(&cat_of(me["name"].as_str().unwrap_or("?"))),
            ui::dim(&self.node),
            ui::plain(&m["quorum"].to_string()),
            m["last_checkpoint"]
        ));
        let row = |name: String, st: &serde_json::Value, seen: String| {
            let role = st["role"].as_str().unwrap_or("?");
            let role_c = if role == "leader" { ui::ok(&format!("{role:<13}")) } else { ui::dim(&format!("{role:<13}")) };
            format!(
                "    {}  {role_c} term {:<4} head {:<7} leader {}  {}",
                ui::pad(&ui::who(&name), 14),
                st["term"],
                st["head"],
                ui::pad(&st["leader"].as_str().map(&cat_of).map(|n| ui::who(&n)).unwrap_or_else(|| ui::dim("-")), 14),
                ui::dim(&seen),
            )
        };
        out.push(row(cat_of(me["name"].as_str().unwrap_or("?")), me, "(this node)".into()));
        for p in m["peers"].as_array().into_iter().flatten() {
            let route = p["route"].as_str().unwrap_or("?");
            match p["status"].as_object() {
                Some(_) => {
                    let ago = p["seen_ms_ago"].as_u64().unwrap_or(0);
                    let stale = if ago > 5_000 { format!("  {}", ui::warn("STALE")) } else { String::new() };
                    out.push(row(cat_of(p["status"]["name"].as_str().unwrap_or("?")), &p["status"], format!("{route}  seen {:.1}s ago{stale}", ago as f64 / 1000.0)));
                }
                None => out.push(format!("    {:<14} {}", "?", ui::warn(&format!("{route}  never answered")))),
            }
        }
        out.join("\n")
    }

    /// Every hex-looking string in `v`, resolved through the roster in
    /// place — not field-name-specific, since an effect's account-carrying
    /// fields differ by type (`who`, `to`, `from`, `author`, `holder`, ...)
    /// and a new `Effect` variant shouldn't need a matching new case here.
    /// `from_hex` only accepts exactly 32 bytes of hex, so a task id like
    /// `"t1.1"` or a directive name like `"PlanNeeded"` never matches.
    fn resolve_accounts(&self, v: &mut serde_json::Value) {
        match v {
            serde_json::Value::String(s) => {
                if let Ok(a) = miot_keys::from_hex(s) {
                    *s = self.roster.name_of(&a);
                }
            }
            serde_json::Value::Object(m) => m.values_mut().for_each(|vv| self.resolve_accounts(vv)),
            serde_json::Value::Array(a) => a.iter_mut().for_each(|vv| self.resolve_accounts(vv)),
            _ => {}
        }
    }

    /// `eff[field]`, resolved through the roster if it's an account, else
    /// the raw string (or `"?"` if absent/null — `from`/`to` on a broadcast
    /// or a root-authored effect are `null` in the wire JSON).
    fn name(&self, eff: &serde_json::Value, field: &str) -> String {
        eff[field]
            .as_str()
            .map(|s| miot_keys::from_hex(s).map(|a| self.roster.name_of(&a)).unwrap_or_else(|_| s.to_string()))
            .unwrap_or_else(|| "?".into())
    }

    /// One effect, in prose — matches `render()` in `crates/kot/src/node.rs`
    /// field-for-field. A raw JSON dump (even with hex resolved to names)
    /// reads as noise; every effect type gets an actual sentence, the way
    /// `"said"` always has.
    fn render_effect(&self, eff: &serde_json::Value) -> String {
        let task = || eff["task"].as_str().unwrap_or("?").to_string();
        let text = |field: &str| eff[field].as_str().unwrap_or("");
        match eff["t"].as_str().unwrap_or("") {
            "said" => format!("{}: {}", self.name(eff, "from"), text("body")),
            "opened" => format!("{} opened {}: {}", self.name(eff, "who"), task(), text("text")),
            "planned" => format!("{} planned {} into {} subtask(s)", self.name(eff, "who"), task(), eff["count"]),
            "assigned" => format!("{} assigned to {}: {}", task(), self.name(eff, "to"), text("what")),
            "directed" => format!("{} directed on {}: {}", self.name(eff, "to"), task(), eff["directive"].as_str().unwrap_or("?")),
            "nudge" => {
                let last = if eff["last"].as_bool().unwrap_or(false) { ", last" } else { "" };
                format!("{} nudged on {} ({} left{last})", self.name(eff, "to"), task(), eff["remaining"])
            }
            "record" => {
                let t = text("text");
                let suffix = if t.is_empty() { String::new() } else { format!(": {t}") };
                format!("{} {} on {}{suffix}", self.name(eff, "who"), eff["act"].as_str().unwrap_or("?"), task())
            }
            "requeued" => format!("{} requeued from {}: {}", task(), self.name(eff, "from"), eff["why"].as_str().unwrap_or("?")),
            "budget_spent" => format!("{} spent its nudge budget on {}", self.name(eff, "holder"), task()),
            "closed" => format!("{} closed by {}: {}", task(), self.name(eff, "author"), text("title")),
            "failed" => format!("{} failed", task()),
            "rehomed" => format!("{} rehomed from {} to {}", task(), self.name(eff, "from"), self.name(eff, "to")),
            // A future Effect variant lands here until it earns its own
            // sentence above — still resolves accounts, just not to prose.
            _ => {
                let mut v = eff.clone();
                self.resolve_accounts(&mut v);
                v.to_string()
            }
        }
    }

    fn print_event(&self, e: &serde_json::Value) {
        println!("  {DIM}block {}{OFF}  {}", e["block"], self.render_effect(&e["effect"]));
    }

    /// `kot log`. With `task`, only events about it (or its sub-tasks).
    /// `follow` keeps polling; without it, prints what the node holds and
    /// exits. `seconds` bounds a follow (one-shot verbs watch briefly).
    pub async fn log(&mut self, since: u64, task: Option<&str>, follow: bool, seconds: Option<u64>) {
        let deadline = seconds.map(|s| tokio::time::Instant::now() + std::time::Duration::from_secs(s));
        let task = task.map(|t| t.trim_start_matches('t').to_string());
        let mut cursor = since;
        loop {
            let batch = match self.get_json(&format!("/events?since={cursor}")).await {
                Ok(serde_json::Value::Array(b)) => b,
                _ => Vec::new(),
            };
            for e in &batch {
                cursor = cursor.max(e["seq"].as_u64().unwrap_or(cursor));
                if let Some(t) = &task {
                    let et = e["effect"]["task"].as_str().unwrap_or("").trim_start_matches('t');
                    if et != t && !et.starts_with(&format!("{t}.")) {
                        continue;
                    }
                }
                self.print_event(e);
            }
            if !follow || deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
}

const BROADCAST_ALIASES: [&str; 3] = ["all", "cats", "litter"];

/// `@name` tags in a line → who to `say` to. `@all`/`@cats`/`@litter`
/// anywhere forces a broadcast (empty target list); otherwise every resolved
/// `@name` is a target, in order, deduplicated. Unknown tags come back as a
/// soft warning, not a refusal.
fn parse_targets(roster: &Roster, line: &str) -> (Vec<AccountId>, Vec<String>) {
    let mut targets = Vec::new();
    let mut unknown = Vec::new();
    for word in line.split_whitespace() {
        let Some(tag) = word.strip_prefix('@') else { continue };
        let lower = tag.trim_end_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
        if BROADCAST_ALIASES.contains(&lower.as_str()) {
            return (Vec::new(), Vec::new());
        }
        match roster.account(&lower) {
            Some(a) if !targets.contains(&a) => targets.push(a),
            Some(_) => {}
            None => unknown.push(lower),
        }
    }
    (targets, unknown)
}

pub fn say_call(to: Option<AccountId>, body: &str) -> RuntimeCall {
    RuntimeCall::Litter(pallet_litter::Call::say { to, body: body.to_string() })
}

/// `Status.name` (a mesh/routing label, e.g. `ryzen-fc`) → cat name,
/// resolved through each status's `account` (hex) and the roster. Shared
/// between `kot peers` and the REPL's background mesh poll so both agree on
/// what to call a peer.
fn mesh_names(m: &serde_json::Value, roster: &Roster) -> std::collections::HashMap<String, String> {
    let mut names = std::collections::HashMap::new();
    let statuses = std::iter::once(&m["me"]).chain(m["peers"].as_array().into_iter().flatten().map(|p| &p["status"]).filter(|s| s.is_object()));
    for st in statuses {
        let Some(mesh_name) = st["name"].as_str() else { continue };
        let cat = st["account"]
            .as_str()
            .and_then(|s| miot_keys::from_hex(s).ok())
            .map(|a| roster.name_of(&a))
            .unwrap_or_else(|| mesh_name.to_string());
        names.insert(mesh_name.to_string(), cat);
    }
    names
}

/// Background, spawned by the REPL: watches `/mesh/peers` and, into `tx`,
/// sends a line only when something changes — leader, quorum, or a peer
/// going stale/coming back — never a full table on a timer (`/peers` still
/// gives the full picture on demand). Also keeps `state.primary` current
/// for the composer's hairline.
async fn poll_mesh_ui(http: reqwest::Client, node: String, roster: Roster, tx: mpsc::UnboundedSender<String>, state: Arc<AsyncMutex<ComposerState>>) {
    let mut last_leader: Option<String> = None;
    let mut had_quorum: Option<bool> = None;
    let mut stale: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;
        let Ok(r) = http.get(format!("{node}/mesh/peers")).send().await else { continue };
        let Ok(m) = r.json::<serde_json::Value>().await else { continue };
        let names = mesh_names(&m, &roster);
        let cat_of = |n: &str| names.get(n).cloned().unwrap_or_else(|| n.to_string());

        let primary_name = std::iter::once(&m["me"])
            .chain(m["peers"].as_array().into_iter().flatten().map(|p| &p["status"]))
            .find(|st| st["role"].as_str() == Some("leader"))
            .and_then(|st| st["name"].as_str())
            .map(&cat_of);
        if let Some(p) = &primary_name {
            state.lock().await.primary = p.clone();
        }

        let leader = m["me"]["leader"].as_str().map(&cat_of);
        if leader != last_leader {
            let who = leader.as_deref().map(ui::who).unwrap_or_else(|| ui::dim("nobody (election)"));
            let _ = tx.send(ui::mesh(format!("leader: {who}")));
            last_leader = leader;
        }

        let total = 1 + m["peers"].as_array().map(Vec::len).unwrap_or(0);
        let quorum = m["quorum"].as_u64().unwrap_or(0) as usize;
        let alive = 1 + m["peers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["status"].is_object() && p["seen_ms_ago"].as_u64().unwrap_or(u64::MAX) <= 5_000)
            .count();
        let has_quorum = alive >= quorum;
        if had_quorum != Some(has_quorum) {
            let tag = format!("{}/{total} alive", alive);
            let styled = if has_quorum { ui::ok(&format!("quorum ok, {tag}")) } else { ui::alert(&format!("NO QUORUM, {tag}")) };
            let _ = tx.send(ui::mesh(styled));
            had_quorum = Some(has_quorum);
        }

        for p in m["peers"].as_array().into_iter().flatten() {
            if !p["status"].is_object() {
                continue;
            }
            let name = cat_of(p["status"]["name"].as_str().unwrap_or("?"));
            let now_stale = p["seen_ms_ago"].as_u64().unwrap_or(u64::MAX) > 5_000;
            if stale.insert(name.clone(), now_stale).is_some_and(|was| was != now_stale) {
                let msg = if now_stale { ui::warn("went stale") } else { ui::ok("back") };
                let _ = tx.send(ui::mesh(format!("{} {msg}", ui::who(&name))));
            }
        }
    }
}

/// Background, spawned by the REPL: polls `/events` and, into `tx`, sends
/// every new one rendered through `kot::ui::render` — our own `said`
/// effects included, so `render`'s `me()` path (echo + "✓ sealed") is the
/// *only* echo of what we typed; raw mode means the terminal isn't echoing
/// it locally. Also keeps `state.head` current for the composer's hairline.
async fn tail_events(http: reqwest::Client, node: String, roster: Roster, me_name: String, since: u64, tx: mpsc::UnboundedSender<String>, state: Arc<AsyncMutex<ComposerState>>) {
    let mut cursor = since;
    loop {
        let batch: Vec<serde_json::Value> = match http.get(format!("{node}/events?since={cursor}")).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        if !batch.is_empty() {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            let mut head = None;
            for e in &batch {
                cursor = cursor.max(e["seq"].as_u64().unwrap_or(cursor));
                let block = e["block"].as_u64().unwrap_or(0);
                head = Some(block);
                let _ = tx.send(ui::render(&ui::hhmm(now), block, &e["effect"], &roster, &me_name));
            }
            if let Some(h) = head {
                state.lock().await.head = h;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// What the composer's hairline needs, kept current by the background
/// pollers and read fresh on every redraw.
struct ComposerState {
    node: String,
    primary: String,
    head: u64,
}

const COMPOSER_HEIGHT: u16 = 2;

/// Feeds one already-ANSI-colored block from `kot::ui` (possibly several
/// `\n`-joined lines) into the inline viewport's scrollback, above the
/// composer — `docs/CLI.md` §1's "clear its rows, print the new output
/// above, draw it again", done by ratatui instead of by hand.
fn insert_ansi(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>, s: &str) {
    if s.is_empty() {
        return;
    }
    let text: Text = s.into_text().unwrap_or_else(|_| Text::raw(s.to_string()));
    let height = text.lines.len().max(1) as u16;
    let _ = terminal.insert_before(height, |buf| {
        Paragraph::new(text).render(buf.area, buf);
    });
}

/// Raw mode, restored on drop (including an early return or a panic
/// unwinding through here) so a crash never leaves the operator's shell
/// broken.
struct RawGuard;
impl RawGuard {
    fn new() -> std::io::Result<Self> {
        enable_raw_mode()?;
        Ok(RawGuard)
    }
}
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// What handling one key did.
enum Outcome {
    None,
    Submit(String),
    Quit,
}

/// The composer's draft. Always exactly one logical line — `Enter` is
/// intercepted before it ever reaches the textarea — which is also why the
/// composer's `Viewport::Inline` height can stay fixed: ratatui 0.30 fixes
/// that height at construction, with no public way to grow it later.
/// `ratatui_textarea::TextArea` gives real emacs-style editing (kill/yank,
/// word motion, undo) for free; history, backward search and `@name`/
/// `/cmd` tab-completion are layered on top here.
struct Input {
    area: TextArea<'static>,
    history: Vec<String>,
    hist_idx: Option<usize>,
    saved: Option<String>,
    tab: Option<(usize, Vec<String>, usize)>,
}

impl Input {
    fn new() -> Self {
        let mut area = TextArea::new(vec![String::new()]);
        // The default underlines the whole line the cursor is on — since the
        // composer is always exactly one line, that's every character typed.
        // Just the cursor cell (already reversed-video by default) is enough.
        area.set_cursor_line_style(ratatui::style::Style::default());
        Input { area, history: Vec::new(), hist_idx: None, saved: None, tab: None }
    }

    fn draft(&self) -> &str {
        &self.area.lines()[0]
    }

    fn cursor_chars(&self) -> usize {
        self.area.cursor().1
    }

    fn widget(&self) -> &TextArea<'static> {
        &self.area
    }

    fn set_draft(&mut self, s: &str) {
        self.area.move_cursor(CursorMove::Jump(0, 0));
        self.area.delete_line_by_end();
        self.area.insert_str(s);
    }

    fn history_back(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.hist_idx.is_none() {
            self.saved = Some(self.draft().to_string());
            self.hist_idx = Some(self.history.len());
        }
        if let Some(i) = self.hist_idx {
            if i > 0 {
                self.hist_idx = Some(i - 1);
                let s = self.history[i - 1].clone();
                self.set_draft(&s);
            }
        }
    }

    fn history_forward(&mut self) {
        let Some(i) = self.hist_idx else { return };
        if i + 1 < self.history.len() {
            self.hist_idx = Some(i + 1);
            let s = self.history[i + 1].clone();
            self.set_draft(&s);
        } else {
            self.hist_idx = None;
            let s = self.saved.take().unwrap_or_default();
            self.set_draft(&s);
        }
    }

    /// `⌃r`: jump to the most recent history entry containing the draft as
    /// a substring, then one further back on each repeat. Not incremental
    /// readline-style search (the query has no edit buffer of its own) —
    /// simpler, still gets you to an old line fast.
    fn search_history(&mut self) {
        let query = self.draft().to_string();
        if query.is_empty() {
            return;
        }
        let start = self.hist_idx.unwrap_or(self.history.len());
        if self.hist_idx.is_none() {
            self.saved = Some(query.clone());
        }
        for i in (0..start).rev() {
            if self.history[i].contains(&query) {
                self.hist_idx = Some(i);
                let s = self.history[i].clone();
                self.set_draft(&s);
                return;
            }
        }
    }

    fn replace_current_word(&mut self, start: usize, cursor: usize, replacement: &str) {
        let draft = self.draft().to_string();
        let before: String = draft.chars().take(start).collect();
        let after: String = draft.chars().skip(cursor).collect();
        let new_cursor = start + replacement.chars().count();
        self.set_draft(&format!("{before}{replacement}{after}"));
        self.area.move_cursor(CursorMove::Jump(0, new_cursor as u16));
    }

    /// `⇥`/`⇧⇥`: complete the `@name` or `/command` under the cursor,
    /// cycling through matches on repeat presses.
    fn complete(&mut self, roster: &Roster, backward: bool) {
        if let Some((start, candidates, idx)) = &mut self.tab {
            if candidates.is_empty() {
                return;
            }
            *idx = if backward { (*idx + candidates.len() - 1) % candidates.len() } else { (*idx + 1) % candidates.len() };
            let start = *start;
            let repl = candidates[*idx].clone();
            let cursor = self.cursor_chars();
            self.replace_current_word(start, cursor, &repl);
            return;
        }
        let draft = self.draft().to_string();
        let cursor = self.cursor_chars();
        let (start, word) = current_word(&draft, cursor);
        if word.is_empty() {
            return;
        }
        let candidates: Vec<String> = if let Some(prefix) = word.strip_prefix('/') {
            ["/task", "/tasks", "/peers", "/artifact", "/clear", "/keys", "/quit", "/exit"].iter().filter(|c| c[1..].starts_with(prefix)).map(|s| s.to_string()).collect()
        } else if let Some(prefix) = word.strip_prefix('@') {
            let mut v: Vec<String> = roster.names().filter(|n| n.starts_with(prefix)).map(|n| format!("@{n}")).collect();
            for alias in ["all", "litter", "cats"] {
                if alias.starts_with(prefix) {
                    v.push(format!("@{alias}"));
                }
            }
            v
        } else {
            Vec::new()
        };
        if candidates.is_empty() {
            return;
        }
        let repl = candidates[0].clone();
        self.replace_current_word(start, cursor, &repl);
        self.tab = Some((start, candidates, 0));
    }

    /// Handle one key. `Outcome::Submit` has already cleared the draft and
    /// pushed it to history; the caller still owns actually sending it.
    fn handle(&mut self, key: KeyEvent, roster: &Roster) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::None;
        }
        if !matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.tab = None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => {
                let line = self.draft().trim().to_string();
                if line.is_empty() {
                    return Outcome::None;
                }
                self.history.push(line.clone());
                self.hist_idx = None;
                self.saved = None;
                self.set_draft("");
                Outcome::Submit(line)
            }
            KeyCode::Char('d') if ctrl => {
                if self.draft().is_empty() {
                    Outcome::Quit
                } else {
                    self.area.delete_next_char();
                    Outcome::None
                }
            }
            KeyCode::Char('c') if ctrl => {
                self.set_draft("");
                self.hist_idx = None;
                self.saved = None;
                Outcome::None
            }
            KeyCode::Up => {
                self.history_back();
                Outcome::None
            }
            KeyCode::Char('p') if ctrl => {
                self.history_back();
                Outcome::None
            }
            KeyCode::Down => {
                self.history_forward();
                Outcome::None
            }
            KeyCode::Char('n') if ctrl => {
                self.history_forward();
                Outcome::None
            }
            KeyCode::Char('r') if ctrl => {
                self.search_history();
                Outcome::None
            }
            KeyCode::Tab => {
                self.complete(roster, false);
                Outcome::None
            }
            KeyCode::BackTab => {
                self.complete(roster, true);
                Outcome::None
            }
            _ => {
                let _ = self.area.input(key);
                Outcome::None
            }
        }
    }
}

/// The word touching `cursor` (a char index) in `s`: its start char index
/// and text, split on whitespace. Used for `@name`/`/cmd` completion.
fn current_word(s: &str, cursor: usize) -> (usize, String) {
    let chars: Vec<char> = s.chars().collect();
    let mut start = cursor.min(chars.len());
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = cursor.min(chars.len());
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    (start, chars[start..end].iter().collect())
}

/// One submitted line: a slash command or a plain `say` (with `@name`
/// tags). Every line here is a real signed extrinsic; output goes through
/// `tx` so it lands through the same insert as everything else — no
/// separate, racing print path. Returns whether the session should end.
async fn run_command(c: &mut Client, line: &str, tx: &mpsc::UnboundedSender<String>) -> bool {
    let send = |s: String| {
        let _ = tx.send(s);
    };
    match line.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((line, "")) {
        ("/quit" | "/exit", _) => return true,
        ("/keys", _) => send(ui::keys()),
        ("/clear", _) => match c.try_submit(RuntimeCall::Litter(pallet_litter::Call::clear_all {})).await {
            Ok(()) => send(format!("  {}", ui::ok("✓ cleared"))),
            Err(e) => send(format!("  {}", ui::alert(&format!("refused: {e}")))),
        },
        ("/tasks", _) => {
            let t = c.tasks_text().await;
            send(t);
        }
        ("/peers", _) => {
            let t = c.peers_text().await;
            send(t);
        }
        ("/artifact", id) if !id.is_empty() => match c.get_json(&format!("/artifact/{id}")).await {
            Ok(a) if a["found"] == true => send(a["body"].as_str().unwrap_or("").to_string()),
            _ => send(format!("  {}", ui::alert(&format!("no artifact for {id} (not closed yet, or no such task)")))),
        },
        ("/task", text) if !text.is_empty() => match c.try_submit(RuntimeCall::Litter(pallet_litter::Call::open { text: text.to_string() })).await {
            Ok(()) => send(format!("  {}", ui::ok("✓ task opened"))),
            Err(e) => send(format!("  {}", ui::alert(&format!("refused: {e}")))),
        },
        (cmd, _) if cmd.starts_with('/') => send(format!("  {}", ui::dim(&format!("unknown command {cmd}")))),
        _ => {
            let (targets, unknown) = parse_targets(&c.roster, line);
            for bad in &unknown {
                send(format!("  {}", ui::warn(&format!("no such cat: @{bad}"))));
            }
            // No success message here on purpose: `tail_events` will render
            // the committed `said` effect through `ui::render`'s `me()`
            // path once it comes back — that's the only echo (raw mode
            // means the terminal isn't echoing what was typed).
            let calls: Vec<Option<AccountId>> = if targets.is_empty() { vec![None] } else { targets.into_iter().map(Some).collect() };
            for t in calls {
                if let Err(e) = c.try_submit(say_call(t, line)).await {
                    send(format!("  {}", ui::alert(&format!("refused: {e}"))));
                }
            }
        }
    }
    false
}

/// Bare `kot`: the interactive session, `kot::ui`'s look wired to a live
/// node — raw mode, a ratatui inline viewport pinning the composer to the
/// bottom (`docs/CLI.md` §0/§1: ordinary scrollback above it, nothing
/// alt-screen). Every line is a real signed extrinsic, and replies come
/// from whatever cats are actually running.
pub async fn repl(mut c: Client) {
    let me = c.identity.account();
    let me_name = c.roster.name_of(&me);

    let t0 = std::time::Instant::now();
    let head = c.get_json("/head").await.unwrap_or_default();
    let latency = t0.elapsed().as_millis();
    let head_block = head["block"].as_u64().unwrap_or(0);

    let mesh_v = c.get_json("/mesh/peers").await.ok();
    let role = mesh_v.as_ref().and_then(|m| m["me"]["role"].as_str()).unwrap_or("?").to_string();
    let primary = mesh_v.as_ref().and_then(|m| {
        std::iter::once(&m["me"])
            .chain(m["peers"].as_array().into_iter().flatten().map(|p| &p["status"]))
            .find(|st| st["role"].as_str() == Some("leader"))
            .and_then(|st| st["account"].as_str())
            .and_then(|s| miot_keys::from_hex(s).ok())
            .map(|a| c.roster.name_of(&a))
    });
    let fwd = if role == "leader" { "primary, seals blocks itself" } else { "forwards writes to the primary" };

    let mut header = vec![
        ui::banner(),
        ui::kv("节点", "node", format!("{}  {}", ui::plain(&c.node), ui::dim(&format!("{role} · {latency}ms · {fwd}")))),
        ui::kv("身份", "you", format!("{}  {}", ui::sealed(&me_name), ui::dim(&miot_keys::short(&me)))),
        ui::kv("猫群", "litter", c.roster.names().filter(|n| *n != me_name).map(ui::sealed).collect::<Vec<_>>().join("   ")),
    ];

    // Replay what the node still holds, so scrollback has context.
    let all = match c.get_json("/events?since=0").await {
        Ok(serde_json::Value::Array(b)) => b,
        _ => Vec::new(),
    };
    let cursor = all.iter().filter_map(|e| e["seq"].as_u64()).max().unwrap_or(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let shown = all.len().min(30);
    header.push(ui::section("回放", &format!("replay · last {shown} of {} events", all.len())));
    if all.is_empty() {
        header.push(format!("  {}", ui::dim("nothing on this chain yet")));
    }
    let start = all.len() - shown;
    let mut prev: Option<u64> = if start > 0 { all[start - 1]["block"].as_u64() } else { None };
    for e in &all[start..] {
        let block = e["block"].as_u64().unwrap_or(0);
        let time = ui::when(block, prev, head_block, now, crate::node::BLOCK_MS);
        header.push(ui::render(&time, block, &e["effect"], &c.roster, &me_name));
        prev = Some(block);
    }
    header.push(ui::section("回放结束", "end replay"));
    header.push(ui::keys());

    // Everything above is plain, newline-safe stdout — fine to `println!`
    // before raw mode changes what a bare `\n` does to the cursor.
    for h in &header {
        println!("{h}");
    }
    println!();

    let guard = match RawGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("  raw mode: {e} (not a real terminal? try `kot log --follow` instead)");
            return;
        }
    };
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = match Terminal::with_options(backend, TerminalOptions { viewport: Viewport::Inline(COMPOSER_HEIGHT) }) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("  terminal: {e}");
            return;
        }
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let state = Arc::new(AsyncMutex::new(ComposerState { node: c.node.clone(), primary: primary.unwrap_or_else(|| "?".into()), head: head_block }));

    tokio::spawn(tail_events(c.http.clone(), c.node.clone(), c.roster.clone(), me_name.clone(), cursor, tx.clone(), state.clone()));
    tokio::spawn(poll_mesh_ui(c.http.clone(), c.node.clone(), c.roster.clone(), tx.clone(), state.clone()));

    let mut input = Input::new();
    let mut events = EventStream::new();
    let target = "litter".to_string();

    loop {
        let quit = tokio::select! {
            Some(text) = rx.recv() => { insert_ansi(&mut terminal, &text); false }
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(Event::Key(key))) => match input.handle(key, &c.roster) {
                    Outcome::Quit => true,
                    Outcome::Submit(line) => run_command(&mut c, &line, &tx).await,
                    Outcome::None => false,
                },
                Some(Ok(Event::Resize(_, _))) => { let _ = terminal.autoresize(); false }
                Some(Ok(_)) => false,
                Some(Err(_)) | None => true,
            },
        };
        if quit {
            break;
        }

        let (node, primary, head) = {
            let s = state.lock().await;
            (s.node.clone(), s.primary.clone(), s.head)
        };
        let status = ui::composer_status(&node, &primary, head);
        let prompt = ui::prompt(&me_name, Some(&target));
        let prompt_w = ui::vcells(&prompt) as u16;
        let _ = terminal.draw(|f| {
            let area = f.area();
            let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
            let status_text: Text = status.as_str().into_text().unwrap_or_else(|_| Text::raw(status.clone()));
            f.render_widget(Paragraph::new(status_text), rows[0]);
            let cols = Layout::horizontal([Constraint::Length(prompt_w), Constraint::Min(1)]).split(rows[1]);
            let prompt_text: Text = prompt.as_str().into_text().unwrap_or_else(|_| Text::raw(prompt.clone()));
            f.render_widget(Paragraph::new(prompt_text), cols[0]);
            f.render_widget(input.widget(), cols[1]);
        });
    }

    drop(terminal);
    drop(guard);
    println!("\n  {}", ui::dim("bye."));
}
