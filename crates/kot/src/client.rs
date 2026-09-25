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

use crate::activity::Seen;
use crate::common::{EventCursor, Roster, DIM, OFF};
use crate::ui;

pub struct Client {
    http: reqwest::Client,
    candidates: Vec<String>,
    pub node: String,
    pub identity: Identity,
    pub roster: Roster,
    /// The REPL's sticky `/dm` target: once set, a bare line goes only to
    /// this account (no `@name` needed) until `/dm` turns it off again.
    /// Lives here rather than as a local in `repl()` so `run_command`
    /// (which only ever sees `&mut Client`) can set and clear it.
    pub dm_target: Option<AccountId>,
    /// Where a housekeeping line (a reconnect, a retry, a "queued locally")
    /// goes: the REPL's inline-viewport scrollback channel once `repl()` has
    /// one (raw mode owns the terminal from then on — a bare `eprintln!`
    /// after that point doesn't get skipped, it corrupts the display, since
    /// ratatui's own buffer no longer matches what's actually on screen).
    /// `None` before that (a one-shot verb, or `repl()`'s own pre-raw-mode
    /// setup calls), where a bare `eprintln!` is the right thing.
    pub notice: Option<mpsc::UnboundedSender<String>>,
}

/// See [`Client::notice`]. A free function (not a method) so [`watch_seal`],
/// spawned independently of any live `&Client` borrow, can use the same
/// fallback without needing one.
fn emit(sink: &Option<mpsc::UnboundedSender<String>>, line: String) {
    match sink {
        Some(tx) => {
            let _ = tx.send(line);
        }
        None => eprintln!("{line}"),
    }
}

/// The confirmation half of `try_submit`'s "pending"/"applied" ack: polls
/// `/tx/{hash}` (routed through whoever's primary, same as `/submit`
/// itself) until it reports sealed, and reports exactly one line when it
/// does — through `sink` ([`emit`]), never a bare `eprintln!`: this runs
/// spawned, well past the call that started it, so it must keep respecting
/// whatever `Client::notice` was in force at that point (a live raw-mode
/// REPL, or a plain one-shot verb) for as long as it keeps polling.
/// Spawned rather than awaited so `try_submit` stays non-blocking — sealing
/// can take several seconds (one block tick) or, if this went through the
/// mempool, however long a relay hop takes to find a route. Gives up
/// silently-ish (one line) after ~30s; the extrinsic itself isn't lost —
/// `Cat::submit`/`Client::try_submit`'s own retry, or a later
/// `mempool_round`, still owns getting it there. `"unknown"` (the node that
/// resolved it lost leadership before this caught up, or was itself the one
/// asked and its own tracking evicted the hash) is reported the same way as
/// a timeout.
async fn watch_seal(http: reqwest::Client, node: String, identity: Identity, hash: String, sink: Option<mpsc::UnboundedSender<String>>) {
    let headers = crate::node::sign_headers(&identity, b"");
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let Ok(r) = http.get(format!("{node}/tx/{hash}")).headers(headers.clone()).send().await else {
            continue;
        };
        let Ok(v) = r.json::<serde_json::Value>().await else {
            continue;
        };
        match v.get("status").and_then(|s| s.as_str()) {
            Some("sealed") => {
                let height = v.get("height").and_then(|h| h.as_u64()).unwrap_or(0);
                emit(&sink, format!("  {DIM}{hash} sealed at block {height}{OFF}"));
                return;
            }
            Some("unknown") => {
                emit(&sink, format!("  {DIM}{hash}: lost track of it (leadership likely moved before it sealed){OFF}"));
                return;
            }
            _ => {} // still pending or applied-but-unsealed — keep polling
        }
    }
    emit(&sink, format!("  {DIM}{hash}: still not sealed after 30s — it may land later{OFF}"));
}

impl Client {
    /// Pick the first node in `candidates` that answers `/head`, then take
    /// the roster from it. The node isn't pinned: it's a private chain the
    /// operator runs, so whichever node answers is trusted, and it's the
    /// source of the roster rather than something checked against one
    /// (`crate::tls`'s module docs). `roster` is only a fallback for names
    /// from a node too old to serve `/roster`.
    pub async fn connect(candidates: Vec<String>, identity: Identity, roster: Roster) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .use_preconfigured_tls(crate::tls::client_config_any_node(&identity))
            .build()
            .unwrap();
        let mut c = Client { http, candidates, node: String::new(), identity, roster, dm_target: None, notice: None };
        c.reconnect().await?;
        c.adopt_chain_roster().await;
        Ok(c)
    }

    /// Name accounts by the chain's own genesis roster (`/roster`), not by
    /// any local copy. A node too old to have `/roster` leaves the local
    /// (`--roster`) names in place.
    async fn adopt_chain_roster(&mut self) {
        let Ok(v) = self.get_json("/roster").await else { return };
        let rows: Vec<(String, AccountId)> = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| Some((r["name"].as_str()?.to_string(), miot_keys::from_hex(r["account"].as_str()?).ok()?)))
            .collect();
        if !rows.is_empty() {
            self.roster = Roster(rows);
        }
    }

    /// Move to the next live node. A dead endpoint is a reconnect, not an
    /// outage.
    pub async fn reconnect(&mut self) -> Result<(), String> {
        for n in &self.candidates {
            let headers = crate::node::sign_headers(&self.identity, b"");
            let probe = self.http.get(format!("{n}/head")).headers(headers).timeout(std::time::Duration::from_secs(3)).send().await;
            if matches!(probe, Ok(ref r) if r.status().is_success()) {
                if self.node != *n && !self.node.is_empty() {
                    emit(&self.notice, format!("  {DIM}switched to {n}{OFF}"));
                }
                self.node = n.clone();
                return Ok(());
            }
        }
        Err(format!("no node answered (tried {})", self.candidates.join(", ")))
    }

    async fn get_json(&mut self, path: &str) -> Result<serde_json::Value, String> {
        // Signed over the raw query string (or `b""` for a parameterless
        // path) — same rule `node.rs`'s handlers check against, so this
        // must match `require_client_auth`'s bytes exactly.
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let headers = crate::node::sign_headers(&self.identity, query.as_bytes());
        for attempt in 0..2 {
            match self.http.get(format!("{}{path}", self.node)).headers(headers.clone()).send().await {
                Ok(r) => return r.json().await.map_err(|e| format!("bad response from {}{path}: {e}", self.node)),
                Err(e) if attempt == 0 => {
                    emit(&self.notice, format!("  {DIM}{} unreachable ({e}); trying the next node{OFF}", self.node));
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

    /// Sign and submit — for a caller (the ratatui REPL) that renders the
    /// outcome itself. Any housekeeping line this needs to say (a retry, a
    /// "queued locally," eventually a "sealed") goes through [`Client::
    /// notice`], never a bare `eprintln!`: once `repl()` has put the
    /// terminal in raw mode, that corrupts the display rather than getting
    /// silently skipped. [`Client::submit`] is the printing wrapper the
    /// one-shot verbs use, where `notice` is `None` and a bare `eprintln!`
    /// is exactly right.
    ///
    /// Retries a transient refusal instead of surfacing it on the first
    /// try: a `Stale`/`Future` nonce might resync, and "no route to it"
    /// (this node follows the primary by push and can't forward — see
    /// `node.rs::no_primary`) might resolve if the term rolls over to a
    /// leader this node can actually reach. Same retryable set as
    /// `agent.rs::Cat::submit`; a business-logic refusal
    /// (`NotAuthorized`, `WrongKind`, ...) still fails on the first attempt.
    pub async fn try_submit(&mut self, call: RuntimeCall) -> Result<(), String> {
        const ATTEMPTS: u32 = 4;
        const BACKOFF_MS: [u64; 3] = [1000, 2000, 4000];

        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(BACKOFF_MS[attempt as usize - 1])).await;
                emit(&self.notice, format!("  {DIM}retrying submit (attempt {}/{ATTEMPTS}) — {last}{OFF}", attempt + 1));
            }

            let m = self.meta().await?;
            let who = miot_keys::to_hex(&self.identity.account());
            let nonce = self.get_json(&format!("/account/{who}")).await?["nonce"].as_u64().unwrap_or(0) as u32;
            let uxt = client::sign(&self.identity, call.clone(), nonce, &m);
            let r = match self.http.post(format!("{}/submit", self.node)).body(uxt.encode()).send().await {
                Ok(r) => r,
                Err(e) => {
                    last = format!("node unreachable: {e}");
                    continue;
                }
            };
            let ok = r.status().is_success();
            let v: serde_json::Value = r.json().await.unwrap_or_default();
            if ok {
                // "pending" (queued in the mempool, no route to the primary
                // yet) or "applied" (landed in the currently open block) —
                // either way, not durable until it seals. Say so once, then
                // watch in the background so this call stays non-blocking;
                // `watch_seal` gets its own clone of `notice` so it keeps
                // reporting correctly (channel or plain print) however long
                // it runs past this call returning.
                if let Some(note) = v.get("note").and_then(|n| n.as_str()) {
                    emit(&self.notice, format!("  {DIM}{note}{OFF}"));
                }
                if let Some(hash) = v.get("tx_hash").and_then(|h| h.as_str()).map(str::to_string) {
                    tokio::spawn(watch_seal(self.http.clone(), self.node.clone(), self.identity, hash, self.notice.clone()));
                }
                return Ok(());
            }
            let msg = v.get("error").unwrap_or(&v).to_string();
            if msg.contains("Payment") {
                return Err(format!(
                    "{msg} — {} isn't a member of this chain (no `providers`; see HANDOFF.md's \"catnip\"). \
                     Add it to MIOT_ROSTER on every node (a new genesis), or sign --as a member.",
                    miot_keys::short(&self.identity.account())
                ));
            }
            if !(msg.contains("Stale") || msg.contains("Future") || msg.contains("no route to it")) {
                return Err(msg);
            }
            last = format!("refused: {msg}");
        }
        Err(format!("gave up after {ATTEMPTS} attempts — {last}"))
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
    ///
    /// Routes by id shape, same convention `ArtifactRead` (the LLM tool,
    /// `agent.rs`/`chat.rs`) already uses: a `t`-prefixed id (as
    /// `/artifacts`' merged listing renders a closed task's) hits
    /// `/artifact/{id}`, anything else hits `/note/{id}`. Before this, a
    /// human at the REPL had to already know which of `/artifact <id>` or
    /// `/note <id>` an id from that merged list needed — the exact
    /// distinction the tool-calling side never had to make.
    ///
    /// The body is the artifact; below it come its votes and this epoch's
    /// comment thread (each epoch gains its own — `docs/MESSAGING.md`).
    pub async fn print_artifact(&mut self, id: &str) -> bool {
        let path = if id.trim_start().starts_with(['t', 'T']) { format!("/artifact/{id}") } else { format!("/note/{id}") };
        match self.get_json(&path).await {
            Ok(a) if a["found"] == true => {
                println!("{}", a["body"].as_str().unwrap_or(""));
                self.print_feedback(&a);
                true
            }
            Ok(_) => {
                eprintln!("no artifact for {id}");
                false
            }
            Err(e) => {
                eprintln!("{e}");
                false
            }
        }
    }

    /// Votes and the current epoch's comments, under an artifact body —
    /// the shared tail of `print_artifact`/`print_note`.
    fn print_feedback(&self, a: &serde_json::Value) {
        let (ups, downs) = (
            a["votes"]["up"].as_array().map(Vec::len).unwrap_or(0),
            a["votes"]["down"].as_array().map(Vec::len).unwrap_or(0),
        );
        if ups + downs > 0 {
            println!("\n▲ {ups}  ▼ {downs}");
        }
        let comments = a["comments"].as_array();
        if comments.is_some_and(|c| !c.is_empty()) {
            println!("\n--- comments (this epoch) ---");
            for c in comments.unwrap() {
                let who = c["who"]
                    .as_str()
                    .and_then(|h| miot_keys::from_hex(h).ok())
                    .map(|acc| self.roster.name_of(&acc))
                    .unwrap_or_else(|| "?".into());
                println!("  {who}: {}", c["body"].as_str().unwrap_or(""));
            }
        }
    }

    /// A standalone note's markdown on stdout, no task behind it.
    pub async fn print_note(&mut self, id: &str) -> bool {
        match self.get_json(&format!("/note/{id}")).await {
            Ok(a) if a["found"] == true => {
                println!("{}", a["body"].as_str().unwrap_or(""));
                self.print_feedback(&a);
                true
            }
            Ok(_) => {
                eprintln!("no note {id}");
                false
            }
            Err(e) => {
                eprintln!("{e}");
                false
            }
        }
    }

    /// Every standalone note: id, title, author.
    pub async fn print_notes(&mut self) -> bool {
        match self.get_json("/notes").await {
            Ok(serde_json::Value::Array(rows)) => {
                for r in &rows {
                    let id = r["id"].as_u64().unwrap_or(0);
                    let title = r["title"].as_str().unwrap_or("");
                    let author = r["author"]
                        .as_str()
                        .and_then(|h| miot_keys::from_hex(h).ok())
                        .map(|a| self.roster.name_of(&a))
                        .unwrap_or_else(|| "someone".into());
                    println!("  {id}  {title}  ({author})");
                }
                true
            }
            Ok(_) => true,
            Err(e) => {
                eprintln!("{e}");
                false
            }
        }
    }

    /// Every artifact — a closed task's report and a standalone one alike,
    /// one merged id-addressed list — unlike `print_notes`, which is only
    /// ever the standalone half of that (`/notes`, not `/artifacts`). Read
    /// one back with `kot artifact <id>` (task) or `kot note <id>`
    /// (standalone) — same split the ids themselves already carry (a `t`
    /// prefix means the first).
    pub async fn print_artifacts(&mut self) -> bool {
        match self.get_json("/artifacts").await {
            Ok(serde_json::Value::Array(rows)) => {
                for r in &rows {
                    let id = r["id"].as_str().unwrap_or("?");
                    let title = r["title"].as_str().unwrap_or("");
                    let author = r["author"]
                        .as_str()
                        .and_then(|h| miot_keys::from_hex(h).ok())
                        .map(|a| self.roster.name_of(&a))
                        .unwrap_or_else(|| "someone".into());
                    println!("  {id}  {title}  ({author})");
                }
                true
            }
            Ok(_) => true,
            Err(e) => {
                eprintln!("{e}");
                false
            }
        }
    }

    /// The litter roster, then the mesh as seen from the connected node:
    /// who's primary, each peer's term, head and how recently it answered.
    /// What every cat this node can hear is doing right now (`GET
    /// /activity`), or just `only`.
    pub async fn activity_text(&mut self, only: Option<&str>) -> String {
        match self.get_json("/activity").await.and_then(|v| serde_json::from_value::<Vec<Seen>>(v).map_err(|e| e.to_string())) {
            Ok(seen) => ui::activity_text(&seen, &self.roster, only),
            Err(e) => format!("  {}", ui::alert(&format!("no /activity from {} ({e}) — an older build?", self.node))),
        }
    }

    /// One cat's own local task list (`LocalTask`), from its live record.
    pub async fn local_tasks_text(&mut self, cat: &str) -> String {
        match self.get_json("/activity").await.and_then(|v| serde_json::from_value::<Vec<Seen>>(v).map_err(|e| e.to_string())) {
            Ok(seen) => ui::local_tasks_text(&seen, &self.roster, cat),
            Err(e) => format!("  {}", ui::alert(&format!("no /activity from {} ({e}) — an older build?", self.node))),
        }
    }

    pub async fn print_peers(&mut self) {
        println!("{}", self.peers_text().await);
    }

    /// [`Client::print_peers`], joined by `\n` instead of printed — so the
    /// ratatui REPL can feed it through an inline-viewport insert.
    pub async fn peers_text(&mut self) -> String {
        let lit = self.get_json("/head").await.ok().and_then(|h| h["leader"].as_str().map(str::to_string));
        let mesh = self.get_json("/mesh/peers").await;
        // Cat name -> the address the connected node reaches it at (its
        // `MIOT_PEERS` route, scheme dropped). The connected node itself is
        // wherever this client dialed it. External members and anything the
        // node has never heard from still show their configured route.
        let mut addr_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if let Ok(m) = &mesh {
            let names = mesh_names(m, &self.roster);
            if let Some(n) = m["me"]["name"].as_str() {
                addr_of.insert(names.get(n).cloned().unwrap_or_else(|| n.to_string()), host_port(&self.node));
            }
            for p in m["peers"].as_array().into_iter().flatten() {
                if let Some(n) = p["status"]["name"].as_str() {
                    addr_of.insert(names.get(n).cloned().unwrap_or_else(|| n.to_string()), host_port(p["route"].as_str().unwrap_or("?")));
                }
            }
        }

        let mut out = vec![format!("  {}", ui::dim("litter"))];
        for (n, a) in &self.roster.0 {
            let tag = if lit.as_deref() == Some(miot_keys::to_hex(a).as_str()) { format!("  {}", ui::ok("leader")) } else { String::new() };
            let addr = addr_of.get(n).map(|s| ui::plain(s)).unwrap_or_else(|| ui::dim("—"));
            out.push(format!("    {}  {}  {}{tag}", ui::pad(&ui::who(n), 14), ui::dim(&miot_keys::short(a)), ui::pad(&addr, 24)));
        }
        let m = match mesh {
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
        let row = |name: String, addr: &str, st: &serde_json::Value, seen: String| {
            let role = st["role"].as_str().unwrap_or("?");
            let role_c = if role == "leader" { ui::ok(&format!("{role:<13}")) } else { ui::dim(&format!("{role:<13}")) };
            format!(
                "    {}  {}  {role_c} term {:<4} head {:<7} leader {}  {}",
                ui::pad(&ui::who(&name), 14),
                ui::pad(&ui::plain(addr), 24),
                st["term"],
                st["head"],
                ui::pad(&st["leader"].as_str().map(&cat_of).map(|n| ui::who(&n)).unwrap_or_else(|| ui::dim("-")), 14),
                seen,
            )
        };
        out.push(row(cat_of(me["name"].as_str().unwrap_or("?")), &host_port(&self.node), me, ui::dim("(this node)")));
        for p in m["peers"].as_array().into_iter().flatten() {
            let route = host_port(p["route"].as_str().unwrap_or("?"));
            match p["status"].as_object() {
                Some(_) => {
                    let ago = p["seen_ms_ago"].as_u64().unwrap_or(0);
                    let seen = if ago > 5_000 {
                        format!("{}  {}", ui::dim(&format!("seen {:.1}s ago", ago as f64 / 1000.0)), ui::warn("STALE"))
                    } else {
                        ui::dim(&format!("seen {:.1}s ago", ago as f64 / 1000.0))
                    };
                    out.push(row(cat_of(p["status"]["name"].as_str().unwrap_or("?")), &route, &p["status"], seen));
                }
                None => out.push(format!("    {}  {}  {}", ui::pad(&ui::dim("?"), 14), ui::pad(&ui::plain(&route), 24), ui::warn("never answered"))),
            }
        }
        // Peers that call this node but that it can't call back: known only
        // by what they sent (their polls, a leader's pushes).
        for p in m["inbound"].as_array().into_iter().flatten() {
            let ago = p["seen_ms_ago"].as_u64().unwrap_or(0);
            let seen = ui::dim(&format!("heard {:.1}s ago, calls us (no route from here)", ago as f64 / 1000.0));
            out.push(row(cat_of(p["status"]["name"].as_str().unwrap_or("?")), "(inbound only)", &p["status"], seen));
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
            "said" => {
                let otr = if eff["off_record"].as_bool().unwrap_or(false) { " (off the record)" } else { "" };
                format!("{}: {}{otr}", self.name(eff, "from"), text("body"))
            }
            "message" => {
                let otr = if eff["off_record"].as_bool().unwrap_or(false) { " (off the record)" } else { "" };
                let mut extra = String::new();
                if let Some(p) = eff["parent"].as_u64() {
                    extra.push_str(&format!(" ↩#{p}"));
                }
                for t in eff["tags"].as_array().into_iter().flatten() {
                    if let Some(t) = t.as_str() {
                        extra.push_str(&format!(" #{t}"));
                    }
                }
                format!("{}: {}{otr}{extra}", self.name(eff, "from"), text("body"))
            }
            "reacted" => format!("{} reacted {} on #{}", self.name(eff, "who"), text("emoji"), eff["target"]),
            "voted" => format!("{} voted {} on §{}", self.name(eff, "who"), if eff["up"].as_bool().unwrap_or(false) { "up" } else { "down" }, eff["artifact"].as_str().unwrap_or("?")),
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
            "standalone_artifact" => format!("{} published artifact {}: {}", self.name(eff, "author"), eff["id"], text("title")),
            "stats_reported" => format!("{} ∑ {}", self.name(eff, "who"), ui::stats_phrase(eff)),
            // A future Effect variant lands here until it earns its own
            // sentence above — still resolves accounts, just not to prose.
            _ => {
                let mut v = eff.clone();
                self.resolve_accounts(&mut v);
                v.to_string()
            }
        }
    }

    /// One line per event, grep-friendly — what `kot log` prints when its
    /// stdout isn't a terminal (piped, redirected, a script watching).
    fn print_event(&self, e: &serde_json::Value) {
        let at = e["at"].as_u64().map(|t| format!("{}  ", ui::clock(t))).unwrap_or_default();
        println!("  {at}block {}  {}", e["block"], self.render_effect(&e["effect"]));
    }

    /// `kot log`. With `task`, only events about it (or its sub-tasks).
    /// `follow` keeps polling; without it, prints what the node holds and
    /// exits. `seconds` bounds a follow (one-shot verbs watch briefly).
    ///
    /// On a terminal every event goes through `ui::render` — the REPL's own
    /// look: avatars beside speech, verbs by glyph, each cat's `∑` running
    /// tally of turns/tools/tokens. Piped, it stays one plain line each.
    pub async fn log(&mut self, since: u64, task: Option<&str>, follow: bool, seconds: Option<u64>) {
        use std::io::IsTerminal;
        let pretty = std::io::stdout().is_terminal();
        let me_name = self.roster.name_of(&self.identity.account());
        let mut prev_at: Option<u64> = None;
        let mut replaying = true;

        let deadline = seconds.map(|s| tokio::time::Instant::now() + std::time::Duration::from_secs(s));
        let task = task.map(|t| t.trim_start_matches('t').to_string());
        let mut cursor = EventCursor::new(since);
        loop {
            if !replaying {
                if let Ok(h) = self.get_json("/head").await {
                    if h["seq"].as_u64().is_some_and(|s| cursor.check_head(s)) && pretty {
                        println!("{}", ui::note("the node rebuilt its log (a /clear, or a rewind) — following it"));
                    }
                }
            }
            let batch = match self.get_json(&format!("/events?since={}", cursor.seq)).await {
                Ok(serde_json::Value::Array(b)) => b,
                _ => Vec::new(),
            };
            for e in &batch {
                if !cursor.accept(e) {
                    continue;
                }
                if let Some(t) = &task {
                    let et = e["effect"]["task"].as_str().unwrap_or("").trim_start_matches('t');
                    if et != t && !et.starts_with(&format!("{t}.")) {
                        continue;
                    }
                }
                if pretty {
                    let block = e["block"].as_u64().unwrap_or(0);
                    // The block's real seal time. A replayed block without
                    // one gets no time; a live one still in its open block
                    // is happening now.
                    let at = match e["at"].as_u64() {
                        Some(t) => Some(t),
                        None if !replaying => Some(unix_ms_now()),
                        None => None,
                    };
                    println!("{}", ui::render(&ui::stamp_at(at, prev_at), block, &e["effect"], &self.roster, &me_name));
                    prev_at = at.or(prev_at);
                } else {
                    self.print_event(e);
                }
            }
            replaying = false;
            if !follow || deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    /// `kot log --tree`: the conversation log as the tree it actually is.
    ///
    /// Only speech is threaded — `said` and `message` events, edges drawn
    /// from `Effect::Message`'s `parent` (the id of the message being
    /// answered). Task transitions, stats and everything else are skipped:
    /// the point is to follow one discussion in isolation without paging
    /// past a day of lifecycle records (artifact 5 §1.2). Reactions
    /// collapse onto the line they answered, so an agreement is one glyph
    /// under the message instead of a block of its own.
    ///
    /// A parent this window doesn't hold (compacted away, or a reply to a
    /// pre-epoch message) makes its node a root, marked `…` — better a
    /// rooted orphan than a silently dropped branch. Cycles can't occur on
    /// chain (a parent is always an earlier message) but the walk is
    /// guarded anyway, since it renders arbitrary wire data.
    pub async fn log_tree(&mut self) {
        use std::collections::HashMap;
        let batch = match self.get_json("/events?since=0").await {
            Ok(serde_json::Value::Array(b)) => b,
            _ => {
                eprintln!("could not fetch events");
                return;
            }
        };
        #[derive(Clone)]
        struct Msg {
            from: String,
            to: String,
            body: String,
            parent: Option<String>,
            tags: Vec<String>,
            at: Option<u64>,
            reactions: Vec<String>,
        }
        let mut msgs: HashMap<String, Msg> = HashMap::new();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut roots: Vec<String> = Vec::new();
        for e in &batch {
            let block = e["block"].as_u64().unwrap_or(0);
            let eff = &e["effect"];
            match eff["t"].as_str().unwrap_or("") {
                "said" | "message" => {
                    // `said` has no id (legacy path) — key it by block so it
                    // still shows, as a root, with nothing hanging off it.
                    let id = match eff["id"].as_str() {
                        Some(i) => i.to_string(),
                        None => format!("b{block}"),
                    };
                    let m = Msg {
                        from: self.name(eff, "from"),
                        to: if eff["to"].is_null() { "litter".into() } else { self.name(eff, "to") },
                        body: eff["body"].as_str().unwrap_or("").to_string(),
                        parent: eff["parent"].as_str().map(str::to_string),
                        tags: eff["tags"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect()).unwrap_or_default(),
                        at: e["at"].as_u64(),
                        reactions: Vec::new(),
                    };
                    if !msgs.contains_key(&id) {
                        order.push(id.clone());
                    }
                    msgs.insert(id, m);
                }
                "reacted" => {
                    let target = eff["target"].as_str().unwrap_or("").to_string();
                    let emoji = eff["emoji"].as_str().unwrap_or("·").to_string();
                    let who = self.name(eff, "who");
                    if let Some(m) = msgs.get_mut(&target) {
                        m.reactions.push(format!("{emoji}{}", if emoji.chars().count() == 1 { format!(" {who}") } else { String::new() }));
                    }
                }
                _ => {}
            }
        }
        // Parents, in log order; a reply to an unseen parent is a root.
        for id in &order {
            match msgs[id].parent.clone() {
                Some(p) if msgs.contains_key(&p) => children.entry(p).or_default().push(id.clone()),
                Some(p) => {
                    roots.push(id.clone());
                    println!("  {}", ui::dim(&format!("… #{p} (not in this node's window)")));
                }
                None => roots.push(id.clone()),
            }
        }
        for m in msgs.values_mut() {
            m.reactions.dedup();
        }
        let printed = std::cell::Cell::new(0usize);
        fn walk(
            id: &str,
            prefix: &str,
            last: bool,
            msgs: &HashMap<String, Msg>,
            children: &HashMap<String, Vec<String>>,
            printed: &std::cell::Cell<usize>,
        ) {
            let m = &msgs[id];
            let branch = if printed.get() == 0 { "" } else if last { "└─" } else { "├─" };
            let mut line = format!(
                "{prefix}{branch} {} {}{}",
                ui::who(&format!("#{id}")),
                ui::who(&m.from),
                ui::dim(&format!(" → {}", m.to))
            );
            let time = m.at.map(|t| ui::dim(&format!(" {}", ui::clock(t)))).unwrap_or_default();
            line.push_str(&time);
            let first = m.body.lines().next().unwrap_or("");
            let rest = m.body.lines().count().saturating_sub(1);
            line.push_str(&format!(" {}", ui::plain(first)));
            if rest > 0 {
                line.push_str(&ui::dim(&format!(" (+{rest} more lines)")));
            }
            for t in &m.tags {
                line.push_str(&ui::dim(&format!(" #{t}")));
            }
            if !m.reactions.is_empty() {
                line.push_str(&format!("  {}", ui::dim(&m.reactions.join(" "))));
            }
            println!("{line}");
            printed.set(printed.get() + 1);
            let kids = children.get(id).map(Vec::as_slice).unwrap_or(&[]);
            let next = if last { format!("{prefix}   ") } else { format!("{prefix}│  ") };
            for (i, k) in kids.iter().enumerate() {
                walk(k, &next, i + 1 == kids.len(), msgs, children, printed);
            }
        }
        for r in &roots {
            walk(r, "", true, &msgs, &children, &printed);
        }
    }
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `https://192.168.1.126:9944` -> `192.168.1.126:9944` — the scheme is
/// always https on this mesh, so it's just width.
fn host_port(route: &str) -> String {
    route.split_once("://").map(|(_, r)| r).unwrap_or(route).trim_end_matches('/').to_string()
}

const BROADCAST_ALIASES: [&str; 3] = ["all", "cats", "litter"];

/// A fresh `MessageId` for a message we're about to post — minted from a
/// randomly-seeded hasher over the body and the clock, which is plenty:
/// ids only have to be unique within one epoch (a handful of cats, a few
/// thousand messages — `docs/MESSAGING.md`).
pub fn fresh_id(body: &str) -> miot_primitives::MessageId {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write(body.as_bytes());
    h.write_u64(n);
    h.write_u64(unix_ms_now());
    miot_primitives::MessageId(h.finish())
}

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

pub fn say_call(to: Option<AccountId>, body: &str, off_record: bool) -> RuntimeCall {
    // An operator's own message always wants an answer — `no_ack` is a
    // sender's own signal that it doesn't, and a human at the keyboard is
    // never the one that needs it.
    RuntimeCall::Litter(pallet_litter::Call::say { to, body: body.to_string(), no_ack: false, off_record })
}

/// `Status.name` (a mesh/routing label, e.g. `ryzen-fc`) → cat name,
/// resolved through each status's `account` (hex) and the roster. Shared
/// between `kot peers` and the REPL's background mesh poll so both agree on
/// what to call a peer.
fn mesh_names(m: &serde_json::Value, roster: &Roster) -> std::collections::HashMap<String, String> {
    let mut names = std::collections::HashMap::new();
    // `inbound`: peers heard only by their calls to this node (no route
    // from here) — named the same way, so a push-only node's view reads
    // "kuro", not "mac-linux-aarch64".
    let statuses = std::iter::once(&m["me"]).chain(
        m["peers"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(m["inbound"].as_array().into_iter().flatten())
            .map(|p| &p["status"])
            .filter(|s| s.is_object()),
    );
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
async fn poll_mesh_ui(
    http: reqwest::Client,
    node: String,
    identity: Identity,
    roster: Roster,
    tx: mpsc::UnboundedSender<String>,
    state: Arc<AsyncMutex<ComposerState>>,
) {
    let mut last_leader: Option<String> = None;
    let mut had_quorum: Option<bool> = None;
    let mut stale: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;
        let headers = crate::node::sign_headers(&identity, b"");
        let Ok(r) = http.get(format!("{node}/mesh/peers")).headers(headers).send().await else { continue };
        let Ok(m) = r.json::<serde_json::Value>().await else { continue };
        let names = mesh_names(&m, &roster);
        let cat_of = |n: &str| names.get(n).cloned().unwrap_or_else(|| n.to_string());

        let primary_name = std::iter::once(&m["me"])
            .chain(m["peers"].as_array().into_iter().flatten().chain(m["inbound"].as_array().into_iter().flatten()).map(|p| &p["status"]))
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
async fn tail_events(
    http: reqwest::Client,
    node: String,
    identity: Identity,
    roster: Roster,
    me_name: String,
    mut cursor: EventCursor,
    tx: mpsc::UnboundedSender<String>,
    state: Arc<AsyncMutex<ComposerState>>,
) {
    loop {
        let head_seq = async {
            let r = http.get(format!("{node}/head")).headers(crate::node::sign_headers(&identity, b"")).send().await.ok()?;
            r.json::<serde_json::Value>().await.ok()?["seq"].as_u64()
        }
        .await;
        if let Some(s) = head_seq {
            if cursor.check_head(s) {
                let _ = tx.send(ui::note("the node rebuilt its log (a /clear, or a rewind) — following it"));
            }
        }
        let query = format!("since={}", cursor.seq);
        let headers = crate::node::sign_headers(&identity, query.as_bytes());
        let batch: Vec<serde_json::Value> = match http.get(format!("{node}/events?{query}")).headers(headers).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        if !batch.is_empty() {
            let mut head = None;
            for e in &batch {
                let block = e["block"].as_u64().unwrap_or(0);
                head = Some(block);
                if !cursor.accept(e) {
                    continue;
                }
                // Live, so an entry whose block is still open (no seal time
                // yet) really is happening now.
                let at = e["at"].as_u64().unwrap_or_else(unix_ms_now);
                let eff = &e["effect"];
                // Our own line, already echoed at ⏎ (`send_and_seal`): only
                // the seal is news. One sent some other way (`kot say` in
                // another terminal) wasn't echoed here, so it renders whole.
                let ours = eff["t"] == "said" && ui::name_of(eff, "from", &roster) == me_name;
                let echoed = ours && {
                    let mut s = state.lock().await;
                    let had = s.sealing > 0;
                    s.sealing = s.sealing.saturating_sub(1);
                    had
                };
                let line = if echoed {
                    ui::sealed_line(&ui::clock(at), block, eff["off_record"].as_bool().unwrap_or(false))
                } else {
                    ui::render(&ui::clock(at), block, eff, &roster, &me_name)
                };
                let _ = tx.send(line);
            }
            if let Some(h) = head {
                state.lock().await.head = h;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Background, spawned by the REPL: keeps `state.activity` current from
/// `/activity`, once a second — the composer's activity row reads it on
/// every redraw. A node too old to have the route just leaves it empty.
async fn poll_activity(http: reqwest::Client, node: String, identity: Identity, state: Arc<AsyncMutex<ComposerState>>) {
    loop {
        let headers = crate::node::sign_headers(&identity, b"");
        if let Ok(r) = http.get(format!("{node}/activity")).headers(headers).send().await {
            if let Ok(seen) = r.json::<Vec<Seen>>().await {
                state.lock().await.activity = seen;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
    }
}

/// What the composer's hairline needs, kept current by the background
/// pollers and read fresh on every redraw.
struct ComposerState {
    node: String,
    primary: String,
    head: u64,
    /// Lines this client echoed at ⏎ and is waiting to see sealed — shown
    /// as `◌ sealing…` in the status row until the chain's `said` comes
    /// back through `tail_events`, which then prints only `✓ sealed`.
    sealing: usize,
    /// Every cat's live record, as the node last served it.
    activity: Vec<Seen>,
}

/// The activity row, the hairline, the prompt.
const COMPOSER_HEIGHT: u16 = 3;

/// Feeds one already-ANSI-colored block from `kot::ui` (possibly several
/// `\n`-joined lines) into the inline viewport's scrollback, above the
/// composer — `docs/CLI.md` §1's "clear its rows, print the new output
/// above, draw it again", done by ratatui instead of by hand.
fn insert_ansi(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>, s: &str) {
    if s.is_empty() {
        return;
    }
    let text: Text = s.into_text().unwrap_or_else(|_| Text::raw(s.to_string()));
    let cols = terminal.size().map(|a| a.width).unwrap_or(100).max(20);
    // Wrapped here, not by the Paragraph: the insert's height has to be
    // the rows actually drawn. Unwrapped, every line past the terminal's
    // width was cut off at the edge — an artifact, mostly long markdown
    // paragraphs, came out as a column of truncated first lines.
    let text = wrap_text(text, cols);
    let height = text.lines.len().max(1) as u16;
    let _ = terminal.insert_before(height, |buf| {
        Paragraph::new(text).render(buf.area, buf);
    });
}

/// Word-wrap styled text to `cols` cells, keeping each character's style:
/// break at the last space that fits, or mid-word if a word alone is wider
/// than the line. Widths are ratatui's own (`Span::width`), so the row count
/// is exactly what gets drawn.
fn wrap_text(text: Text<'_>, cols: u16) -> Text<'static> {
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};
    let cols = cols.max(1) as usize;
    let width = |c: char| Span::raw(c.to_string()).width();
    // One styled row back into spans, runs of one style merged.
    let to_line = |cells: &[(char, Style)]| {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut style = cells.first().map(|c| c.1).unwrap_or_default();
        for &(ch, st) in cells {
            if st != style && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), style));
            }
            style = st;
            run.push(ch);
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, style));
        }
        Line::from(spans)
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in text.lines {
        let base = line.style;
        let cells: Vec<(char, Style)> = line.spans.iter().flat_map(|sp| {
            let st = base.patch(sp.style);
            sp.content.chars().filter(|c| *c != '\r').map(move |c| (c, st))
        }).collect();
        let mut row: Vec<(char, Style)> = Vec::new();
        let mut used = 0;
        for cell in cells {
            let w = width(cell.0);
            if used + w > cols && !row.is_empty() && cell.0 == ' ' {
                // The row is exactly full: break here, the space goes.
                out.push(to_line(&std::mem::take(&mut row)));
                used = 0;
                continue;
            }
            if used + w > cols && !row.is_empty() {
                // Back to the last space, if there is one past the start —
                // the rest of the word moves down with this character.
                match row.iter().rposition(|c| c.0 == ' ').filter(|&i| i > 0) {
                    Some(i) => {
                        let carry: Vec<(char, Style)> = row.split_off(i + 1);
                        row.pop();
                        out.push(to_line(&row));
                        row = carry;
                    }
                    None => out.push(to_line(&std::mem::take(&mut row))),
                }
                used = row.iter().map(|c| width(c.0)).sum();
            }
            row.push(cell);
            used += w;
        }
        out.push(to_line(&row));
    }
    Text::from(out)
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
            ["/task", "/tasks", "/peers", "/activity", "/artifact", "/artifacts", "/note", "/notes", "/clear", "/keys", "/reply", "/react", "/vote", "/quit", "/exit"].iter().filter(|c| c[1..].starts_with(prefix)).map(|s| s.to_string()).collect()
        } else if let Some(prefix) = word.strip_prefix('@') {
            let mut v: Vec<String> = roster.names().filter(|n| n.starts_with(prefix)).map(|n| format!("@{n}")).collect();
            for alias in ["all", "litter", "cats"] {
                if alias.starts_with(prefix) {
                    v.push(format!("@{alias}"));
                }
            }
            v
        } else if ["/tasks ", "/activity ", "/dm "].iter().any(|c| draft.starts_with(c)) && start == draft.find(' ').map(|i| i + 1).unwrap_or(0) {
            // A command that takes a cat's name as its argument: the bare
            // name, no `@`.
            roster.names().filter(|n| n.starts_with(word.as_str())).map(str::to_string).collect()
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
async fn run_command(c: &mut Client, line: &str, tx: &mpsc::UnboundedSender<String>, state: &Arc<AsyncMutex<ComposerState>>) -> bool {
    let send = |s: String| {
        let _ = tx.send(s);
    };
    let me_name = c.roster.name_of(&c.identity.account());
    match line.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((line, "")) {
        ("/quit" | "/exit", _) => return true,
        ("/keys", _) => send(ui::keys()),
        ("/clear", _) => match c.try_submit(RuntimeCall::Litter(pallet_litter::Call::clear_all {})).await {
            Ok(()) => send(format!("  {}", ui::ok("✓ cleared"))),
            Err(e) => send(format!("  {}", ui::alert(&format!("refused: {e}")))),
        },
        // Bare: the litter's tasks, on chain. With a cat's name: that
        // cat's own local list (`LocalTask`), from its live record.
        ("/tasks", "") => {
            let t = c.tasks_text().await;
            send(t);
        }
        ("/tasks", who) => {
            let name = who.trim_start_matches('@');
            if c.roster.account(name).is_some() {
                let t = c.local_tasks_text(name).await;
                send(t);
            } else {
                send(format!("  {}", ui::warn(&format!("no such cat: @{name} — /tasks alone lists the litter's tasks"))));
            }
        }
        ("/peers", _) => {
            let t = c.peers_text().await;
            send(t);
        }
        ("/activity", who) => {
            let t = c.activity_text(if who.is_empty() { None } else { Some(who) }).await;
            send(t);
        }
        // Same id-shape routing as `Client::print_artifact`/`ArtifactRead`:
        // a `t`-prefixed id is a closed task's, anything else standalone.
        // `/artifacts <id>` too — the list shows ids, and typing one after
        // the command that listed them is the natural next step.
        ("/artifact" | "/artifacts", id) if !id.is_empty() => {
            let path = if id.trim_start().starts_with(['t', 'T']) { format!("/artifact/{id}") } else { format!("/note/{id}") };
            match c.get_json(&path).await {
                Ok(a) if a["found"] == true => send(ui::markdown(a["body"].as_str().unwrap_or(""))),
                _ => send(format!("  {}", ui::alert(&format!("no artifact for {id}")))),
            }
        }
        ("/note", id) if !id.is_empty() => match c.get_json(&format!("/note/{id}")).await {
            Ok(a) if a["found"] == true => send(ui::markdown(a["body"].as_str().unwrap_or(""))),
            _ => send(format!("  {}", ui::alert(&format!("no note {id}")))),
        },
        ("/notes", _) => match c.get_json("/notes").await {
            Ok(serde_json::Value::Array(rows)) if rows.is_empty() => send(format!("  {}", ui::dim("no notes yet"))),
            Ok(serde_json::Value::Array(rows)) => {
                let t = rows
                    .iter()
                    .map(|r| {
                        let id = r["id"].as_u64().unwrap_or(0);
                        let title = r["title"].as_str().unwrap_or("");
                        let author = r["author"]
                            .as_str()
                            .and_then(|h| miot_keys::from_hex(h).ok())
                            .map(|a| c.roster.name_of(&a))
                            .unwrap_or_else(|| "someone".into());
                        format!("  {id}  {title}  ({author})")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                send(t);
            }
            _ => send(format!("  {}", ui::alert("could not fetch notes"))),
        },
        ("/artifacts", _) => match c.get_json("/artifacts").await {
            Ok(serde_json::Value::Array(rows)) if rows.is_empty() => send(format!("  {}", ui::dim("no artifacts yet"))),
            Ok(serde_json::Value::Array(rows)) => {
                let t = rows
                    .iter()
                    .map(|r| {
                        let id = r["id"].as_str().unwrap_or("?");
                        let title = r["title"].as_str().unwrap_or("");
                        let author = r["author"]
                            .as_str()
                            .and_then(|h| miot_keys::from_hex(h).ok())
                            .map(|a| c.roster.name_of(&a))
                            .unwrap_or_else(|| "someone".into());
                        format!("  {id}  {title}  ({author})")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                send(t);
            }
            _ => send(format!("  {}", ui::alert("could not fetch artifacts"))),
        },
        ("/task", text) if !text.is_empty() => match c.try_submit(RuntimeCall::Litter(pallet_litter::Call::open { text: text.to_string() })).await {
            Ok(()) => send(format!("  {}", ui::ok("✓ task opened"))),
            Err(e) => send(format!("  {}", ui::alert(&format!("refused: {e}")))),
        },
        // `/dm <name>` — sticky, not one-shot: every bare line after this
        // (no `@name` needed) goes only to `<name>` until a bare `/dm`
        // turns it back off. `<name>` may carry a first message right on
        // the same line. State lives on `Client::dm_target` (not a local
        // here) so it survives across calls to `run_command` and the
        // composer's prompt (`repl()`) can read it back for the `→ name`
        // hairline.
        ("/dm", rest) if rest.is_empty() => match c.dm_target.take() {
            Some(a) => send(format!("  {}", ui::dim(&format!("dm off — {} was the target", c.roster.name_of(&a))))),
            None => send(format!("  {}", ui::alert("usage: /dm <name> [message] (no target set to turn off)"))),
        },
        ("/dm", rest) => {
            let (name, body) = rest.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((rest, ""));
            match c.roster.account(name) {
                Some(a) => {
                    c.dm_target = Some(a.clone());
                    send(format!("  {}", ui::dim(&format!("dm → {} (stays until /dm turns it off)", c.roster.name_of(&a)))));
                    if !body.is_empty() {
                        if let Err(e) = c.try_submit(say_call(Some(a), body, false)).await {
                            send(format!("  {}", ui::alert(&format!("refused: {e}"))));
                        }
                    }
                }
                None => send(format!("  {}", ui::warn(&format!("no such cat: @{}", name.trim_start_matches('@'))))),
            }
        }
        // `/all` — the other direction from `/dm`: turns any sticky DM
        // target back off (same as a bare `/dm`) and, unlike a bare line,
        // never needs `@all`/`@cats`/`@litter` tagged in to broadcast —
        // it always goes to the whole litter, tags or not.
        ("/all", rest) if rest.is_empty() => match c.dm_target.take() {
            Some(a) => send(format!("  {}", ui::dim(&format!("back to the litter — {} is no longer the target", c.roster.name_of(&a))))),
            None => send(format!("  {}", ui::dim("already talking to the whole litter"))),
        },
        ("/all", text) => {
            c.dm_target = None;
            if let Err(e) = c.try_submit(say_call(None, text, false)).await {
                send(format!("  {}", ui::alert(&format!("refused: {e}"))));
            }
        }
        // Same `@name` targeting as a bare line, but the resulting `Said`
        // never joins the block body (`node.rs::absorb`) — it still wakes
        // whoever it's addressed to live, it just isn't there on replay, a
        // rewind, or for a peer that pulls the block later instead of
        // tailing it live.
        ("/otr", text) if !text.is_empty() => {
            let (targets, unknown) = parse_targets(&c.roster, text);
            for bad in &unknown {
                send(format!("  {}", ui::warn(&format!("no such cat: @{bad}"))));
            }
            let calls: Vec<Option<AccountId>> =
                if !targets.is_empty() { targets.into_iter().map(Some).collect() } else { vec![c.dm_target.clone()] };
            send_and_seal(c, &me_name, calls, text, true, &send, state).await;
        }
        // `/reply <id> <text>` — a threaded, optionally tagged message
        // (`Effect::Message`). `@name` targets as everywhere else; `#tag`
        // tokens come off the body into the effect's `tags`. `<id>` is the
        // 8-hex-digit id shown on every message (`MessageId::parse` also
        // takes decimal, `#`/`0x` prefixed forms).
        ("/reply", rest) if !rest.is_empty() => {
            let (raw_id, body) = match rest.split_once(' ') {
                Some((p, b)) if miot_primitives::MessageId::parse(p).is_some() => (p, b.trim()),
                _ => {
                    send(format!("  {}", ui::dim("usage: /reply <id> <text> — #tags and @targets allowed in the text")));
                    return false;
                }
            };
            let parent = miot_primitives::MessageId::parse(raw_id);
            let mut tags: Vec<String> = Vec::new();
            let body = body
                .split_whitespace()
                .filter(|w| {
                    if let Some(t) = w.trim_end_matches(|c: char| !c.is_alphanumeric()).strip_prefix('#') {
                        if !t.is_empty() && !tags.contains(&t.to_string()) {
                            tags.push(t.to_string());
                            return false;
                        }
                    }
                    true
                })
                .collect::<Vec<_>>()
                .join(" ");
            let (targets, unknown) = parse_targets(&c.roster, &body);
            for bad in &unknown {
                send(format!("  {}", ui::warn(&format!("no such cat: @{bad}"))));
            }
            let calls: Vec<Option<AccountId>> =
                if !targets.is_empty() { targets.into_iter().map(Some).collect() } else { vec![c.dm_target.clone()] };
            for t in calls {
                if let Err(e) = c
                    .try_submit(RuntimeCall::Litter(pallet_litter::Call::post {
                        id: fresh_id(&body),
                        to: t,
                        body: body.clone(),
                        parent,
                        artifact_id: None,
                        tags: tags.clone(),
                        no_ack: false,
                        off_record: false,
                    }))
                    .await
                {
                    send(format!("  {}", ui::alert(&format!("refused: {e}"))));
                }
            }
        }
        // `/react <id> <emoji>` — the acknowledgment that isn't a message.
        ("/react", rest) => match rest.split_once(' ') {
            Some((id, emoji)) if !emoji.trim().is_empty() && miot_primitives::MessageId::parse(id).is_some() => {
                if let Err(e) = c
                    .try_submit(RuntimeCall::Litter(pallet_litter::Call::react {
                        target: miot_primitives::MessageId::parse(id).unwrap(),
                        emoji: emoji.trim().to_string(),
                    }))
                    .await
                {
                    send(format!("  {}", ui::alert(&format!("refused: {e}"))));
                }
            }
            _ => send(format!("  {}", ui::dim("usage: /react <id> <emoji>"))),
        },
        // `/vote t5 up` / `/vote 7 down` — artifact votes; the id is
        // addressed exactly as `/artifact` addresses it.
        ("/vote", rest) => match rest.split_whitespace().collect::<Vec<_>>()[..] {
            [id, dir] if dir == "up" || dir == "down" => {
                match miot_primitives::ArtifactId::parse(id) {
                    Some(a) => {
                        if let Err(e) =
                            c.try_submit(RuntimeCall::Litter(pallet_litter::Call::vote { artifact: a, up: dir == "up" })).await
                        {
                            send(format!("  {}", ui::alert(&format!("refused: {e}"))));
                        }
                    }
                    None => send(format!("  {}", ui::warn(&format!("no such artifact id: {id} (t5 or 7)")))),
                }
            }
            _ => send(format!("  {}", ui::dim("usage: /vote <id> <up|down> — e.g. /vote t5 up"))),
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
            // No `@name` tag falls back to the sticky `/dm` target when one
            // is set, and only broadcasts to the whole litter when it isn't.
            let calls: Vec<Option<AccountId>> =
                if !targets.is_empty() { targets.into_iter().map(Some).collect() } else { vec![c.dm_target.clone()] };
            send_and_seal(c, &me_name, calls, line, false, &send, state).await;
        }
    }
    false
}

/// Echo a line the moment it's sent, count it as sealing (the status row
/// shows `◌ sealing…`), and submit it once per target. `tail_events` prints
/// `✓ sealed` under it when the chain's `said` comes back; a refusal says
/// so here and stops counting it.
async fn send_and_seal(
    c: &mut Client,
    me_name: &str,
    calls: Vec<Option<AccountId>>,
    text: &str,
    off_record: bool,
    send: &impl Fn(String),
    state: &Arc<AsyncMutex<ComposerState>>,
) {
    for t in calls {
        let to = t.as_ref().map(|a| c.roster.name_of(a)).unwrap_or_else(|| "litter".to_string());
        send(ui::typed(me_name, Some(&to), text, &c.roster));
        state.lock().await.sealing += 1;
        if let Err(e) = c.try_submit(say_call(t, text, off_record)).await {
            let mut s = state.lock().await;
            s.sealing = s.sealing.saturating_sub(1);
            send(format!("  {}", ui::alert(&format!("refused: {e}"))));
        }
    }
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
    let mut cursor = EventCursor::new(0);
    // The whole session the node holds (everything since the last
    // compaction, up to its log cap) — not a tail: scrollback is where the
    // operator reads what happened while they were away.
    header.push(ui::section("回放", &format!("replay · this session, {} events", all.len())));
    if all.is_empty() {
        header.push(format!("  {}", ui::dim("nothing on this chain yet")));
    }
    let mut prev_at: Option<u64> = None;
    for e in &all {
        cursor.accept(e);
        let block = e["block"].as_u64().unwrap_or(0);
        let at = e["at"].as_u64();
        header.push(ui::render(&ui::stamp_at(at, prev_at), block, &e["effect"], &c.roster, &me_name));
        prev_at = at.or(prev_at);
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
    // From here on the terminal is in raw mode (`guard`, above) and ratatui
    // owns it — any of `c`'s own housekeeping lines (a reconnect, a submit
    // retry, a mempool "queued locally") must go through this channel too,
    // not a bare `eprintln!` (`Client::notice`).
    c.notice = Some(tx.clone());
    let state = Arc::new(AsyncMutex::new(ComposerState { node: c.node.clone(), primary: primary.unwrap_or_else(|| "?".into()), head: head_block, sealing: 0, activity: Vec::new() }));

    tokio::spawn(tail_events(c.http.clone(), c.node.clone(), c.identity, c.roster.clone(), me_name.clone(), cursor, tx.clone(), state.clone()));
    tokio::spawn(poll_mesh_ui(c.http.clone(), c.node.clone(), c.identity, c.roster.clone(), tx.clone(), state.clone()));
    tokio::spawn(poll_activity(c.http.clone(), c.node.clone(), c.identity, state.clone()));

    let mut input = Input::new();
    let mut events = EventStream::new();
    // The activity row counts seconds up; redraw on a tick, not only when a
    // line arrives or a key is pressed.
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(1_000));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let quit = tokio::select! {
            _ = tick.tick() => false,
            Some(text) = rx.recv() => { insert_ansi(&mut terminal, &text); false }
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(Event::Key(key))) => match input.handle(key, &c.roster) {
                    Outcome::Quit => true,
                    Outcome::Submit(line) => run_command(&mut c, &line, &tx, &state).await,
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

        let (node, primary, head, sealing, activity) = {
            let s = state.lock().await;
            (s.node.clone(), s.primary.clone(), s.head, s.sealing, ui::activity_row(&s.activity, &c.roster))
        };
        let status = ui::composer_status(&node, &primary, head, sealing);
        // Reflects `/dm`'s sticky target fresh every redraw, since
        // `run_command` mutates `c.dm_target` rather than a local here.
        let target = c.dm_target.as_ref().map(|a| c.roster.name_of(a)).unwrap_or_else(|| "litter".to_string());
        let prompt = ui::prompt(&me_name, Some(&target));
        let prompt_w = ui::vcells(&prompt) as u16;
        let _ = terminal.draw(|f| {
            let area = f.area();
            let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)]).split(area);
            let activity_text: Text = activity.as_str().into_text().unwrap_or_else(|_| Text::raw(activity.clone()));
            f.render_widget(Paragraph::new(activity_text), rows[0]);
            let status_text: Text = status.as_str().into_text().unwrap_or_else(|_| Text::raw(status.clone()));
            f.render_widget(Paragraph::new(status_text), rows[1]);
            let cols = Layout::horizontal([Constraint::Length(prompt_w), Constraint::Min(1)]).split(rows[2]);
            let prompt_text: Text = prompt.as_str().into_text().unwrap_or_else(|_| Text::raw(prompt.clone()));
            f.render_widget(Paragraph::new(prompt_text), cols[0]);
            f.render_widget(input.widget(), cols[1]);
        });
    }

    drop(terminal);
    drop(guard);
    println!("\n  {}", ui::dim("bye."));
}

#[cfg(test)]
mod wrap_tests {
    use super::wrap_text;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span, Text};

    fn rows(t: &Text) -> Vec<String> {
        t.lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn long_lines_wrap_at_words_and_nothing_is_lost() {
        let t = wrap_text(Text::raw("Rebuilt from scratch after the upgrade/restart; all four LocalTask steps green"), 30);
        let r = rows(&t);
        assert_eq!(r, vec!["Rebuilt from scratch after the", "upgrade/restart; all four", "LocalTask steps green"]);
        assert!(r.iter().all(|l| Span::raw(l.as_str()).width() <= 30));
    }

    #[test]
    fn a_word_wider_than_the_line_is_broken_and_short_lines_are_untouched() {
        assert_eq!(rows(&wrap_text(Text::raw("abcdefghij"), 4)), vec!["abcd", "efgh", "ij"]);
        assert_eq!(rows(&wrap_text(Text::raw("short\n\nnext"), 40)), vec!["short", "", "next"]);
    }

    #[test]
    fn wide_characters_count_as_two() {
        // 喵 is two cells: three of them fill six.
        assert_eq!(rows(&wrap_text(Text::raw("喵喵喵喵"), 6)), vec!["喵喵喵", "喵"]);
    }

    #[test]
    fn styles_survive_the_break() {
        let red = Style::default().fg(Color::Red);
        let t = wrap_text(Text::from(Line::from(vec![Span::raw("aaa "), Span::styled("bbb ccc", red)])), 7);
        assert_eq!(rows(&t), vec!["aaa bbb", "ccc"]);
        assert_eq!(t.lines[1].spans[0].style, red);
        assert_eq!(t.lines[0].spans[1].style, red);
    }
}
