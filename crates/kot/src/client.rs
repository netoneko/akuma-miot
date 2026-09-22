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

use codec::Encode;
use miot_keys::Identity;
use miot_runtime::{client, AccountId, RuntimeCall};
use polkadot_sdk::*;
use sp_core::H256;
use std::io::Write;
use tokio::io::AsyncBufReadExt;

use crate::common::{Roster, DIM, OFF};

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

    /// Sign and submit. Prints the outcome; returns whether it landed.
    pub async fn submit(&mut self, call: RuntimeCall) -> bool {
        let res: Result<(), String> = async {
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
        .await;
        match res {
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
        let rows = match self.get_json("/tasks").await {
            Ok(serde_json::Value::Array(rows)) => rows,
            Ok(_) => Vec::new(),
            Err(e) => return println!("  {e}"),
        };
        if rows.is_empty() {
            return println!("{DIM}  no tasks{OFF}");
        }
        for t in &rows {
            let who = t["holder"]
                .as_str()
                .or_else(|| t["assignee"].as_str())
                .and_then(|s| miot_keys::from_hex(s).ok())
                .map(|a| self.roster.name_of(&a));
            let lease = t["lease_until"].as_u64().map(|b| format!(" lease→{b}")).unwrap_or_default();
            println!(
                "  {DIM}{:<6}{OFF} {:<12} {:<12}{}  {DIM}{}{OFF}",
                t["id"].as_str().unwrap_or("?"),
                t["status"].as_str().unwrap_or("?"),
                who.unwrap_or_default(),
                lease,
                t["text"].as_str().unwrap_or("").chars().take(60).collect::<String>(),
            );
        }
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
        let lit = self.get_json("/head").await.ok().and_then(|h| h["leader"].as_str().map(str::to_string));
        println!("  {DIM}litter{OFF}");
        for (n, a) in &self.roster.0 {
            let tag = if lit.as_deref() == Some(miot_keys::to_hex(a).as_str()) { "  leader" } else { "" };
            println!("    {n:<14} {DIM}{}{OFF}{tag}", miot_keys::short(a));
        }
        let m = match self.get_json("/mesh/peers").await {
            Ok(m) => m,
            Err(e) => return println!("  mesh: {e}"),
        };
        let me = &m["me"];
        println!(
            "  {DIM}mesh, from {} ({}){OFF}  quorum {}  last checkpoint {}",
            me["name"].as_str().unwrap_or("?"),
            self.node,
            m["quorum"],
            m["last_checkpoint"]
        );
        let row = |name: &str, st: &serde_json::Value, seen: String| {
            println!(
                "    {name:<14} {:<13} term {:<4} head {:<7} leader {:<14} {DIM}{seen}{OFF}",
                st["role"].as_str().unwrap_or("?"),
                st["term"],
                st["head"],
                st["leader"].as_str().unwrap_or("-"),
            )
        };
        row(me["name"].as_str().unwrap_or("?"), me, "(this node)".into());
        for p in m["peers"].as_array().into_iter().flatten() {
            let route = p["route"].as_str().unwrap_or("?");
            match p["status"].as_object() {
                Some(_) => {
                    let ago = p["seen_ms_ago"].as_u64().unwrap_or(0);
                    let stale = if ago > 5_000 { "  STALE" } else { "" };
                    row(p["status"]["name"].as_str().unwrap_or("?"), &p["status"], format!("{route}  seen {:.1}s ago{stale}", ago as f64 / 1000.0));
                }
                None => println!("    {:<14} {DIM}{route}  never answered{OFF}", "?"),
            }
        }
    }

    fn print_event(&self, e: &serde_json::Value) {
        let eff = &e["effect"];
        if eff["t"] == "said" {
            let from = eff["from"].as_str().and_then(|s| miot_keys::from_hex(s).ok());
            let label = from.map(|a| self.roster.name_of(&a)).unwrap_or_else(|| "?".into());
            println!("  {DIM}block {}{OFF}  {label}: {}", e["block"], eff["body"].as_str().unwrap_or(""));
        } else {
            println!("  {DIM}block {}{OFF}  {eff}", e["block"]);
        }
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

fn prompt(label: &str) {
    print!("\n{DIM}{label}{OFF} ▸ ");
    let _ = std::io::stdout().flush();
}

/// Poll `/events` for the life of the session, printing every `said` not
/// from us as it lands. A background task, not something the prompt waits
/// on: a cat's turn is 30–150 s, and blocking the prompt for that was the
/// bug this replaced.
async fn print_replies(http: reqwest::Client, node: String, roster: Roster, me: AccountId, since: u64) {
    let mut cursor = since;
    loop {
        let batch: Vec<serde_json::Value> = match http.get(format!("{node}/events?since={cursor}")).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        for e in &batch {
            cursor = cursor.max(e["seq"].as_u64().unwrap_or(cursor));
            let eff = &e["effect"];
            if eff["t"] != "said" {
                continue;
            }
            let Some(from) = eff["from"].as_str().and_then(|s| miot_keys::from_hex(s).ok()) else { continue };
            if from != me {
                println!("{DIM}  {}{OFF}  {}", roster.name_of(&from), eff["body"].as_str().unwrap_or(""));
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Bare `kot`: the interactive session. A line is a `say` (with `@name`
/// tags); slash commands are the one-shot verbs (`docs/CLI.md` §5). Every
/// line is a real signed extrinsic, and replies come from whatever cats are
/// actually running.
pub async fn repl(mut c: Client) {
    let me = c.identity.account();
    println!(
        "  {DIM}{} — /task <text>, /tasks, /artifact <id>, /peers, /clear, /quit{OFF}",
        c.node
    );
    for (n, a) in &c.roster.0 {
        if *a != me {
            println!("    {n}  {DIM}{}{OFF}", miot_keys::short(a));
        }
    }

    // Replay what the node still holds, so scrollback has context (§4).
    let all = match c.get_json("/events?since=0").await {
        Ok(serde_json::Value::Array(b)) => b,
        _ => Vec::new(),
    };
    let cursor = all.iter().filter_map(|e| e["seq"].as_u64()).max().unwrap_or(0);
    if !all.is_empty() {
        println!("{DIM}  — replaying ({} of {} events) —{OFF}", all.len().min(30), all.len());
        for e in all.iter().rev().take(30).rev() {
            c.print_event(e);
        }
        println!("{DIM}  — end replay —{OFF}");
    }
    tokio::spawn(print_replies(c.http.clone(), c.node.clone(), c.roster.clone(), me.clone(), cursor));

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    loop {
        prompt(&c.roster.name_of(&me));
        let Ok(Some(line)) = lines.next_line().await else { break };
        let line = line.trim().to_string();
        match line.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((line.as_str(), "")) {
            ("" | "/quit" | "/exit", _) => break,
            ("/clear", _) => {
                c.submit(RuntimeCall::Litter(pallet_litter::Call::clear_all {})).await;
            }
            ("/tasks", _) => c.print_tasks().await,
            ("/peers", _) => c.print_peers().await,
            ("/artifact", id) => {
                c.print_artifact(id).await;
            }
            ("/task", text) if !text.is_empty() => {
                c.submit(RuntimeCall::Litter(pallet_litter::Call::open { text: text.to_string() })).await;
            }
            (cmd, _) if cmd.starts_with('/') => println!("{DIM}  unknown command {cmd}{OFF}"),
            _ => {
                let (targets, unknown) = parse_targets(&c.roster, &line);
                for bad in &unknown {
                    println!("{DIM}  no such cat: @{bad}{OFF}");
                }
                if targets.is_empty() {
                    c.submit(say_call(None, &line)).await;
                } else {
                    for t in targets {
                        c.submit(say_call(Some(t), &line)).await;
                    }
                }
            }
        }
    }
    println!("\n{DIM}  bye.{OFF}");
}
