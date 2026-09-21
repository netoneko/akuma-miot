//! `--rpc` — talk to a **real** `miot-node` instead of an in-process chain.
//!
//! Everything else in this binary (`--live`, `--chat`, the default scripted
//! demo) drives its own `TestExternalities` in this process, the way this
//! crate always has. This module is the one path that signs a real
//! [`miot_runtime::UncheckedExtrinsic`] and POSTs it to a node over HTTP —
//! the same wire `miot-cat` uses, reused rather than re-derived
//! (`miot_runtime::client::sign` is the shared piece).
//!
//! # What this is not
//!
//! There is no OpenSSH **private**-key loading here. `miot-keys` only ever
//! reads a *public* key line (`account_from_ssh`) — deriving an account, not
//! a signing capability — because parsing an OpenSSH private key is its own
//! security-sensitive piece of work that hasn't been built. `--seed` is a
//! deterministic ed25519 seed (a small int, `[n; 32]`, or 64 hex chars),
//! exactly what `miot-cat`'s `MIOT_SEED` accepts. Signing as the *real*
//! operator key still means running this against a node whose
//! `MIOT_ROOT_PUBKEY`/`MIOT_ROOT` was set to match a seed you actually hold,
//! same as any other account here — there is no special root path.

use codec::Encode;
use miot_keys::Identity;
use miot_runtime::{client, AccountId, RuntimeCall};
use polkadot_sdk::*;
use sp_core::H256;
use std::io::Write;
use tokio::io::AsyncBufReadExt;

use crate::{DIM, OFF};

fn parse_seed(spec: &str) -> [u8; 32] {
    if let Ok(n) = spec.parse::<u8>() {
        return [n; 32];
    }
    let bytes = hex::decode(spec.trim_start_matches("0x"))
        .unwrap_or_else(|_| panic!("seed {spec:?} is neither a small int nor 64 hex chars"));
    bytes.try_into().unwrap_or_else(|_| panic!("seed {spec:?} is not 32 bytes"))
}

/// `~/.akuma/miot/id_ed25519.seed` — named after the ssh convention it stands
/// in for, since this is the same idea: an identity you keep, not one you
/// retype. Not an OpenSSH key (see the module doc) — 64 hex chars, this
/// crate's own seed spelling.
fn identity_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    std::path::Path::new(&home).join(".akuma/miot/id_ed25519.seed")
}

/// The public half of [`identity_path`] — an `authorized_keys`-shaped line,
/// safe to read aloud or paste anywhere, since it's a public key. This is
/// what makes the persisted identity useful for more than signing:
/// `MIOT_ROOT_PUBKEY` on `miot-node` reads exactly this format
/// (`account_from_ssh`), so pasting this file's content there is how the
/// swarm learns *this* identity is the one with root.
fn pub_path() -> std::path::PathBuf {
    identity_path().with_file_name("id_ed25519.pub")
}

/// The persisted identity, creating one on first use.
///
/// Without this, every `--rpc` call with no `--identity-seed` would need a
/// fresh throwaway account — fine for one command, useless for "the same
/// swarm member every time." Generated with the system RNG (`getrandom`),
/// not a small int: this one is meant to be kept, not to double as a
/// human-readable placeholder like the `1,2,3,4,5` seed convention cats use.
///
/// Also (re)writes [`pub_path`] whenever it's missing or out of sync with the
/// seed, so the operator always has a fresh `MIOT_ROOT_PUBKEY` value to hand
/// the node — computing that from a private seed is not something anyone
/// should have to do by hand.
fn load_or_create_identity() -> Identity {
    let path = identity_path();
    let identity = if let Ok(text) = std::fs::read_to_string(&path) {
        Identity::from_seed(&parse_seed(text.trim()))
    } else {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("system RNG");
        std::fs::create_dir_all(path.parent().unwrap()).expect("create ~/.akuma/miot");
        std::fs::write(&path, hex::encode(seed)).expect("write identity seed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        println!(
            "  {DIM}new identity generated and saved to {}{OFF}",
            path.display(),
            DIM = "\x1b[2m",
            OFF = "\x1b[0m"
        );
        Identity::from_seed(&seed)
    };

    let pub_path = pub_path();
    let line = identity.ssh_public_line("miot-root");
    let stale = std::fs::read_to_string(&pub_path).map(|existing| existing.trim() != line.trim()).unwrap_or(true);
    if stale {
        std::fs::write(&pub_path, &line).expect("write public key");
        println!(
            "  {DIM}public key at {} — pass its content as MIOT_ROOT_PUBKEY to give this identity root{OFF}",
            pub_path.display(),
            DIM = "\x1b[2m",
            OFF = "\x1b[0m"
        );
    }

    identity
}

/// `name=seed,name=seed,...` — the same spelling `MIOT_ROSTER` uses, resolved
/// client-side only. A tag never goes on the wire as a name; it is a lookup
/// against this, same as `docs/CLI.md` §2 describes.
fn resolve(roster: &str, name: &str) -> Option<AccountId> {
    let name = name.trim().trim_start_matches('@').to_ascii_lowercase();
    roster.split(',').find_map(|p| {
        let (n, seed) = p.split_once('=')?;
        (n.trim() == name).then(|| Identity::from_seed(&parse_seed(seed.trim())).account())
    })
}

const BROADCAST_ALIASES: [&str; 3] = ["all", "cats", "litter"];

/// `@name` tags in a line → who to `say` to, against `roster`. Mirrors
/// `chat.rs`'s `parse_targets`: `@all`/`@cats`/`@litter` anywhere in the line
/// forces an explicit broadcast (empty target list); otherwise every
/// resolved `@name` becomes a target, in appearance order, deduplicated.
/// Unrecognized tags come back separately as a soft warning, not a refusal.
fn parse_targets(roster: &str, line: &str) -> (Vec<AccountId>, Vec<String>) {
    let mut targets = Vec::new();
    let mut unknown = Vec::new();
    for word in line.split_whitespace() {
        let Some(tag) = word.strip_prefix('@') else { continue };
        let lower = tag.trim_end_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
        if BROADCAST_ALIASES.contains(&lower.as_str()) {
            return (Vec::new(), Vec::new());
        }
        match resolve(roster, &lower) {
            Some(a) => {
                if !targets.contains(&a) {
                    targets.push(a);
                }
            }
            None => unknown.push(lower),
        }
    }
    (targets, unknown)
}

/// The reverse of [`resolve`] — every roster entry as `(name, account)`, so
/// replies can be printed by name instead of a bare hex string.
fn roster_map(roster: &str) -> Vec<(String, AccountId)> {
    roster
        .split(',')
        .filter_map(|p| {
            let (n, seed) = p.split_once('=')?;
            Some((n.trim().to_string(), Identity::from_seed(&parse_seed(seed.trim())).account()))
        })
        .collect()
}

fn name_of(map: &[(String, AccountId)], who: &AccountId) -> String {
    map.iter().find(|(_, a)| a == who).map(|(n, _)| n.clone()).unwrap_or_else(|| miot_keys::short(who))
}

async fn meta(http: &reqwest::Client, node: &str) -> client::Meta {
    let v: serde_json::Value = http
        .get(format!("{node}/meta"))
        .send()
        .await
        .expect("node unreachable (meta)")
        .json()
        .await
        .expect("bad /meta response");
    let genesis_hash = H256::from_slice(&hex::decode(v["genesis_hash"].as_str().unwrap()).unwrap());
    client::Meta {
        genesis_hash,
        spec_version: v["spec_version"].as_u64().unwrap() as u32,
        tx_version: v["tx_version"].as_u64().unwrap() as u32,
    }
}

async fn nonce(http: &reqwest::Client, node: &str, who: &AccountId) -> u32 {
    let v: serde_json::Value = http
        .get(format!("{node}/account/{}", miot_keys::to_hex(who)))
        .send()
        .await
        .expect("node unreachable (nonce)")
        .json()
        .await
        .expect("bad /account response");
    v["nonce"].as_u64().unwrap_or(0) as u32
}

async fn submit(http: &reqwest::Client, node: &str, identity: &Identity, call: RuntimeCall) {
    let m = meta(http, node).await;
    let n = nonce(http, node, &identity.account()).await;
    let uxt = client::sign(identity, call, n, &m);
    let r = http.post(format!("{node}/submit")).body(uxt.encode()).send().await.expect("submit failed");
    if r.status().is_success() {
        println!("  submitted, signed as {}", miot_keys::short(&identity.account()));
    } else {
        let e: serde_json::Value = r.json().await.unwrap_or_default();
        let msg = e.get("error").unwrap_or(&e).to_string();
        if msg.contains("Payment") {
            println!(
                "  refused: {msg} — {} isn't a member of this chain yet (no `providers`, \
                 see HANDOFF.md's \"catnip\" note). Pass --identity-seed matching one of \
                 MIOT_MEMBERS (e.g. --identity-seed 1), or add this account's pubkey to \
                 MIOT_MEMBERS/MIOT_ROOT_PUBKEY on the node.",
                miot_keys::short(&identity.account()),
            );
        } else {
            println!("  refused: {msg}");
        }
    }
}

/// Print the log from `since` forward, block/who/what — the same shape the
/// scripted demo and `--chat` already print, so `--rpc` output reads like
/// every other mode here rather than inventing a fourth spelling of it.
/// `seconds: None` would watch indefinitely; nothing calls it that way today
/// — `docker compose logs -f` / `curl .../events` already own that job.
async fn watch(http: &reqwest::Client, node: &str, since: u64, seconds: Option<u64>) {
    let deadline = seconds.map(|s| tokio::time::Instant::now() + std::time::Duration::from_secs(s));
    let mut cursor = since;
    loop {
        if let Some(d) = deadline {
            if tokio::time::Instant::now() >= d {
                break;
            }
        }
        let batch: Vec<serde_json::Value> = match http.get(format!("{node}/events?since={cursor}")).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        for e in &batch {
            cursor = cursor.max(e["seq"].as_u64().unwrap_or(cursor));
            println!("  {DIM}block {}{OFF}  {}", e["block"], e["effect"], DIM = "\x1b[2m", OFF = "\x1b[0m");
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

fn prompt(label: &str) {
    print!("\n{DIM}{label}{OFF} ▸ ");
    let _ = std::io::stdout().flush();
}

/// Poll `/events` forever from `since`, printing every `said` not from `me`
/// the moment it lands. Runs for the life of the chat session as a
/// background task rather than something the composer waits on — a real
/// cat's turn is 30-150 s (BLOCK_MS=6000 in `miot-node` alone makes a dozen
/// blocks a minute), and blocking the prompt for that was the bug: nothing
/// printed and nothing could be typed until a reply arrived or a fixed
/// timeout gave up. Printing here just interleaves with whatever the
/// operator is mid-typing, same as any other chat client without a
/// composer (see docs/CLI.md §0/§1 — no repainting, so this is the honest
/// version of that until a real composer exists).
async fn print_replies(http: reqwest::Client, node: String, roster: Vec<(String, AccountId)>, me: AccountId, since: u64) {
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
            if from == me {
                continue; // our own line, echoed back through the log
            }
            let body = eff["body"].as_str().unwrap_or("");
            println!("{DIM}  {}{OFF}  {body}", name_of(&roster, &from));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// `/tasks` — one line per live task, `docs/CLI.md` §5. `miot-node`'s
/// `/tasks` already hands back the pallet's own view, so this is print-only.
async fn print_tasks(http: &reqwest::Client, node: &str, roster: &[(String, AccountId)]) {
    let rows: Vec<serde_json::Value> = match http.get(format!("{node}/tasks")).send().await {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(e) => {
            println!("  node unreachable: {e}");
            return;
        }
    };
    if rows.is_empty() {
        println!("{DIM}  no tasks{OFF}");
        return;
    }
    for t in &rows {
        let id = t["id"].as_str().unwrap_or("?");
        let status = t["status"].as_str().unwrap_or("?");
        let who = t["holder"]
            .as_str()
            .or_else(|| t["assignee"].as_str())
            .and_then(|s| miot_keys::from_hex(s).ok())
            .map(|a| name_of(roster, &a));
        let lease = t["lease_until"].as_u64().map(|b| format!(" lease→{b}")).unwrap_or_default();
        println!(
            "  {DIM}{:<6}{OFF} {:<12} {}{}",
            id,
            status,
            who.unwrap_or_default(),
            lease,
        );
    }
}

/// On start, print what the node still holds — `docs/CLI.md` §4's "replay of
/// recent litter traffic... so scrollback has context," bounded to the last
/// `n`. `since=0` is the honest boundary today: `miot-node` keeps an
/// in-memory ring (`LOG_CAP`, 4096 entries) and nothing else, so this *is*
/// everything since boot. Once `miot-store` is wired in (HANDOFF item 2),
/// the real "since last compaction" boundary lands here unchanged — the
/// node deciding what it still holds is the node's business, not this
/// client's.
///
/// Returns the highest `seq` seen, so the caller's live polling starts
/// exactly where the replay left off rather than re-printing it.
async fn replay(http: &reqwest::Client, node: &str, roster: &[(String, AccountId)], n: usize) -> u64 {
    let batch: Vec<serde_json::Value> = match http.get(format!("{node}/events?since=0")).send().await {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let cursor = batch.iter().filter_map(|e| e["seq"].as_u64()).max().unwrap_or(0);
    if batch.is_empty() {
        return cursor;
    }
    println!("{DIM}  — replaying since boot ({} of {} events) —{OFF}", batch.len().min(n), batch.len());
    for e in batch.iter().rev().take(n).rev() {
        let eff = &e["effect"];
        if eff["t"] == "said" {
            let from = eff["from"].as_str().and_then(|s| miot_keys::from_hex(s).ok());
            let label = from.map(|a| name_of(roster, &a)).unwrap_or_else(|| "?".into());
            println!("{DIM}  {label}  {}{OFF}", eff["body"].as_str().unwrap_or(""));
        } else {
            println!("{DIM}  block {}  {eff}{OFF}", e["block"]);
        }
    }
    println!("{DIM}  — end replay —{OFF}");
    cursor
}

/// Interactive — `--rpc <url> --chat`. Same REPL as the in-process `--chat`
/// (`chat.rs`): type a line, it becomes a `say`, `@name` tags, `/clear`
/// fails every open task, blank line or `/quit` leaves. The difference is
/// entirely underneath: every line is a real signed extrinsic, and replies
/// come from whatever cats are actually running against this node, not a
/// simulated turn in this process.
pub async fn chat(node: &str, seed: Option<&str>, roster: &str) {
    let identity = match seed {
        Some(s) => Identity::from_seed(&parse_seed(s)),
        None => load_or_create_identity(),
    };
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().unwrap();
    let me = identity.account();
    let map = roster_map(roster);

    println!("  {DIM}chat (remote) — {node}, /clear to fail every open task, blank line, /quit, or /exit to leave{OFF}");
    for (n, a) in &map {
        if *a != me {
            println!("    {n}  {DIM}{}{OFF}", miot_keys::short(a));
        }
    }

    let cursor = replay(&http, node, &map, 30).await;

    // Replies print as they land, independent of the prompt — see
    // `print_replies`'s doc comment for why this used to block instead.
    tokio::spawn(print_replies(http.clone(), node.to_string(), map.clone(), me.clone(), cursor));

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    loop {
        prompt(&name_of(&map, &me));
        let Ok(Some(line)) = lines.next_line().await else { break };
        let line = line.trim().to_string();
        if line.is_empty() || line == "/quit" || line == "/exit" {
            break;
        }
        if line == "/clear" {
            submit(&http, node, &identity, RuntimeCall::Litter(pallet_litter::Call::clear_all {})).await;
            continue;
        }
        if line == "/tasks" {
            print_tasks(&http, node, &map).await;
            continue;
        }

        let (targets, unknown) = parse_targets(roster, &line);
        for bad in &unknown {
            println!("{DIM}  no such cat: @{bad}{OFF}");
        }
        if targets.is_empty() {
            submit(&http, node, &identity, RuntimeCall::Litter(pallet_litter::Call::say { to: None, body: line.clone() })).await;
        } else {
            for t in &targets {
                submit(
                    &http,
                    node,
                    &identity,
                    RuntimeCall::Litter(pallet_litter::Call::say { to: Some(t.clone()), body: line.clone() }),
                )
                .await;
            }
        }
    }
    println!("\n{DIM}  bye.{OFF}");
}

/// One-shot: sign, submit, watch the log for a few seconds, exit.
///
/// `--rpc <url> [--identity-seed <seed>] [--roster name=seed,...] (--open
/// "<text>" | --say "<body>" [--to <name>] | --clear)`. Without
/// `--identity-seed`, signs as the persisted identity at
/// `~/.akuma/miot/id_ed25519.seed` (created on first use) — the same
/// account every time, rather than a fresh throwaway one per call.
/// `--clear` needs that identity to actually be root on this chain
/// (`MIOT_ROOT_PUBKEY`/`MIOT_ROOT` on the node) — same authority as
/// `--open`, just root-only rather than root-or-leader.
pub async fn run(
    node: &str,
    seed: Option<&str>,
    roster: &str,
    open: Option<&str>,
    say: Option<&str>,
    to: Option<&str>,
    clear: bool,
) {
    let identity = match seed {
        Some(s) => Identity::from_seed(&parse_seed(s)),
        None => load_or_create_identity(),
    };
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().unwrap();

    println!("  {DIM}rpc {node} — signed as {}{OFF}", miot_keys::short(&identity.account()), DIM = "\x1b[2m", OFF = "\x1b[0m");

    let since = match http.get(format!("{node}/head")).send().await {
        Ok(r) => r.json::<serde_json::Value>().await.ok().and_then(|h| h["seq"].as_u64()).unwrap_or(0),
        Err(_) => 0,
    };

    if clear {
        submit(&http, node, &identity, RuntimeCall::Litter(pallet_litter::Call::clear_all {})).await;
    } else if let Some(text) = open {
        submit(&http, node, &identity, RuntimeCall::Litter(pallet_litter::Call::open { text: text.to_string() })).await;
    } else if let Some(body) = say {
        let to = to.and_then(|n| resolve(roster, n));
        if let Some(name) = to.as_ref() {
            println!("  {DIM}→ {}{OFF}", miot_keys::short(name), DIM = "\x1b[2m", OFF = "\x1b[0m");
        }
        submit(&http, node, &identity, RuntimeCall::Litter(pallet_litter::Call::say { to, body: body.to_string() })).await;
    } else {
        println!("  nothing to do — pass --open \"<text>\", --say \"<body>\", or --clear");
        return;
    }

    watch(&http, node, since, Some(8)).await;
}
