//! The chain, as a process.
//!
//! One writer, many readers. The node owns the state outright — no cat ever
//! touches it — and cats reach it over HTTP, which is the whole difference
//! between this and `miot-sim`: there, four cats were four integers inside one
//! process sharing one `TestExternalities`. Here they are four processes on
//! four containers that have never heard of each other.
//!
//! # Two loops, and only one of them is here
//!
//! **The block loop** ticks on a real interval and never waits for anybody.
//! That is Law I made literal: `on_initialize` runs on the clock, leases
//! expire, offers are re-made and directives repeat whether or not a single cat
//! is connected — or even alive. A cat mid-turn is not a thing the node knows
//! about or slows down for.
//!
//! The agent loop is in `miot-cat`, in another container, and the only contact
//! between them is:
//!
//! | | |
//! |---|---|
//! | `POST /submit` | a signed extrinsic; checked, dispatched into the *currently open* block |
//! | `GET /meta` | genesis hash + spec/tx version — what a signer needs to build a valid extension set |
//! | `GET /account/:id` | that account's current nonce |
//! | `GET /events?since=N` | everything the chain emitted after cursor `N` |
//! | `GET /head` | height, leader, and whether the parent is closed |
//! | `GET /artifact/:id` | a closed parent's report, out of chain state |
//!
//! # What changed
//!
//! Calls used to arrive as JSON naming an account, and this node trusted
//! `who` — the exact thing the whole project existed to fix. `/call` is gone.
//! `/submit` takes a SCALE-encoded [`miot_runtime::UncheckedExtrinsic`] and
//! runs it through [`miot_runtime::Executive`]: signature recovery, nonce,
//! mortality and genesis/version binding, *then* dispatch. A forged sender is
//! now a signature that does not verify, not a policy failure.

use std::collections::VecDeque;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State as AxState};
use axum::routing::{get, post};
use axum::{Json, Router};
use codec::Decode;
use miot_primitives::{Effect, TaskId};
use miot_runtime::{AccountId, Executive, Header, Litter, Runtime, System, UncheckedExtrinsic, VERSION};
use polkadot_sdk::*;
use serde::{Deserialize, Serialize};
use sp_core::H256;
use sp_runtime::traits::Header as HeaderT;
use tokio::sync::Mutex;

/// How long a block takes. Six seconds — the Polkadot default, and a round
/// number to reason in.
///
/// The block time is not the interesting number; the **wake cadence** on top of
/// it is, and that lives per task in `miot-runtime`'s timer constants. What
/// matters is only that a wake interval is longer than one LLM turn — measured
/// at 30-230 s here — because a cadence shorter than a turn re-wakes a cat
/// eleven times while it is still thinking about the first copy. That is the
/// litter's oldest timer finding, and this project reproduced it before fixing
/// it.
const BLOCK_MS: u64 = 6000;

/// An effect plus the cursor position it sits at, so a cat can resume.
#[derive(Serialize, Clone)]
struct Entry {
    seq: u64,
    block: u64,
    /// Rendered rather than raw: the cat needs to act on this, and a JSON
    /// rendering of `Effect` is what it reads. Accounts render as hex —
    /// [`miot_keys::to_hex`] — never as anything that looks like a name.
    effect: serde_json::Value,
    /// Who must take a turn because of it, as hex. `null` means nobody — the
    /// waking rule is decided here, by the protocol, not by each cat.
    wakes: Option<String>,
}

struct Node {
    ext: sp_io::TestExternalities,
    log: VecDeque<Entry>,
    seq: u64,
    block: u64,
    /// The hash the block *currently open* for extrinsics will chain to when
    /// it closes. Updated only in [`Node::advance`], which only the block
    /// timer calls.
    parent_hash: H256,
}

const LOG_CAP: usize = 4096;

fn parse_task(s: &str) -> Option<TaskId> {
    let s = s.trim().trim_start_matches('t');
    let mut it = s.split('.');
    let p: u32 = it.next()?.parse().ok()?;
    match it.next() {
        None => Some(TaskId::parent(p)),
        Some(sub) => Some(TaskId::sub(p, sub.parse().ok()?)),
    }
}

fn render(e: &Effect<AccountId>) -> serde_json::Value {
    use miot_keys::to_hex;
    use serde_json::json;
    match e {
        Effect::Said { from, to, body, from_root } => {
            json!({"t":"said","from":to_hex(from),"to":to.as_ref().map(to_hex),"body":body,"root":from_root})
        }
        Effect::Opened { who, task } => json!({"t":"opened","who":to_hex(who),"task":task.to_string()}),
        Effect::Planned { who, task, count } => {
            json!({"t":"planned","who":to_hex(who),"task":task.to_string(),"count":count})
        }
        Effect::Assigned { to, task, what, expect } => {
            json!({"t":"assigned","to":to_hex(to),"task":task.to_string(),"what":what,"expect":expect})
        }
        Effect::Directed { to, task, directive } => {
            json!({"t":"directed","to":to_hex(to),"task":task.to_string(),"directive":format!("{directive:?}")})
        }
        Effect::Nudge { to, task, remaining, last } => {
            json!({"t":"nudge","to":to_hex(to),"task":task.to_string(),"remaining":remaining,"last":last})
        }
        Effect::Record { who, task, act } => {
            json!({"t":"record","who":to_hex(who),"task":task.to_string(),"act":act.as_str()})
        }
        Effect::Requeued { task, from, why } => {
            json!({"t":"requeued","task":task.to_string(),"from":from.as_ref().map(to_hex),"why":format!("{why:?}")})
        }
        Effect::NudgeBudgetSpent { holder, task } => {
            json!({"t":"budget_spent","holder":to_hex(holder),"task":task.to_string()})
        }
        Effect::Closed { task, title } => {
            json!({"t":"closed","task":task.to_string(),"title":title})
        }
        Effect::Rehomed { task, from, to } => {
            json!({"t":"rehomed","task":task.to_string(),"from":from.as_ref().map(to_hex),"to":to_hex(to)})
        }
    }
}

impl Node {
    fn absorb(&mut self, effects: Vec<Effect<AccountId>>) {
        for e in effects {
            self.seq += 1;
            // The waking rule lives in the protocol. A cat is told whether to
            // take a turn; it does not re-derive that from an event name.
            let wakes = if e.wakes() { e.to().map(miot_keys::to_hex) } else { None };
            let entry = Entry { seq: self.seq, block: self.block, effect: render(&e), wakes };
            if self.log.len() >= LOG_CAP {
                self.log.pop_front();
            }
            self.log.push_back(entry);
        }
    }

    /// Everything the pallet emitted since the last drain, in this block.
    fn drain(&mut self) -> Vec<Effect<AccountId>> {
        self.ext.execute_with(|| {
            let out: Vec<_> = System::events()
                .into_iter()
                .filter_map(|r| match r.event {
                    miot_runtime::RuntimeEvent::Litter(pallet_litter::Event::Happened(e)) => Some(e),
                    _ => None,
                })
                .collect();
            System::reset_events();
            out
        })
    }

    /// Close the currently-open block and open the next one. Called only by
    /// the timer loop — nothing here consults a cat, which is Law I.
    /// `on_initialize` (leases, re-offers, nags, directives) fires inside
    /// [`Executive::initialize_block`] for the new block, same as it always
    /// did, just through the real block lifecycle instead of a hand call.
    fn advance(&mut self) {
        let header = self.ext.execute_with(Executive::finalize_block);
        self.parent_hash = header.hash();
        self.block += 1;
        let b = self.block;
        let next = Header::new(b, Default::default(), Default::default(), self.parent_hash, Default::default());
        self.ext.execute_with(|| {
            Executive::initialize_block(&next);
        });
        let fx = self.drain();
        self.absorb(fx);
    }

    /// Check and dispatch one signed extrinsic into the block that is
    /// currently open. Synchronous, because a cat's own refusal-handling
    /// (`AlreadySubmitted`, `NotYours`, …) depends on an immediate answer —
    /// the same guarantee `/call` used to give, now backed by a real check.
    fn submit(&mut self, uxt: UncheckedExtrinsic) -> Result<(), String> {
        let r = self.ext.execute_with(|| Executive::apply_extrinsic(uxt));
        let fx = self.drain();
        self.absorb(fx);
        match r {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("{e:?}")),
            // Bad signature, stale nonce, wrong genesis/spec/tx version —
            // the extrinsic never reached the pallet at all.
            Err(e) => Err(format!("rejected: {e:?}")),
        }
    }
}

/// Give an account "provider" standing so [`frame_system::CheckNonce`] will
/// even look at its nonce.
///
/// `CheckNonce::validate_nonce_for_account` refuses **every** account whose
/// `providers`/`sufficients` are both zero with `InvalidTransaction::Payment`
/// — that gate exists for `pallet-balances` to say "this account has been
/// credited, it exists." We have no balances pallet and nothing pays for
/// anything here, so nothing was ever going to bump those counters, and
/// every signature — however correct — would be refused forever. Feeding it
/// a name it does not deserve: an account that has not been given catnip
/// cannot be nonce-checked, and a litter without balances still needs to
/// mark who is actually in it.
fn catnip(who: &AccountId) {
    frame_system::Pallet::<Runtime>::inc_providers(who);
}

/// Membership is operator-decided, not open: `MIOT_MEMBERS` (default the
/// same `1,2,3,4,5` seed convention `MIOT_ROSTER`/`MIOT_SEED` already use)
/// lists every account that gets [`catnip`] at genesis. Root and leader are
/// always included even if left out of the list — the operator and the
/// leader are never accidentally unable to speak.
fn members(root: &AccountId, leader: &AccountId) -> Vec<AccountId> {
    let mut out: Vec<AccountId> = std::env::var("MIOT_MEMBERS")
        .unwrap_or_else(|_| "1,2,3,4,5".to_string())
        .split(',')
        .filter_map(|spec| {
            let spec = spec.trim();
            if spec.is_empty() {
                return None;
            }
            if let Ok(n) = spec.parse::<u8>() {
                return Some(miot_keys::Identity::from_seed(&[n; 32]).account());
            }
            miot_keys::from_hex(spec).ok()
        })
        .collect();
    for a in [root, leader] {
        if !out.contains(a) {
            out.push(a.clone());
        }
    }
    out
}

fn genesis(root: AccountId, leader: AccountId) -> (sp_io::TestExternalities, H256) {
    use sp_runtime::BuildStorage;
    let mut t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
    pallet_litter::GenesisConfig::<Runtime> { root: Some(root.clone()), leader: Some(leader.clone()) }
        .assimilate_storage(&mut t)
        .unwrap();
    let mut ext: sp_io::TestExternalities = t.into();
    // Block 1's parent is "genesis" by definition. Whatever hash we hand
    // `initialize_block` here becomes `block_hash(0)` forever — that is what
    // `CheckGenesis` binds a signature to, and what `/meta` reports.
    let genesis_hash = H256::zero();
    let first = Header::new(1, Default::default(), Default::default(), genesis_hash, Default::default());
    let member_list = members(&root, &leader);
    ext.execute_with(|| {
        Executive::initialize_block(&first);
        for m in &member_list {
            catnip(m);
        }
    });
    (ext, genesis_hash)
}

type Shared = Arc<Mutex<Node>>;

#[derive(Deserialize)]
struct Since {
    #[serde(default)]
    since: u64,
}

/// `MIOT_ROOT_PUBKEY` (an `authorized_keys` line — root's real identity, per
/// `miot-keys`) wins; `MIOT_ROOT` (hex `AccountId32`) is the fallback for
/// anything that isn't the real operator (tests, a throwaway litter). Same
/// shape for `MIOT_LEADER`, minus the ssh-key path — a leader is just another
/// cat, not the operator.
fn account_env(pubkey_var: &str, hex_var: &str, default_seed: u8) -> AccountId {
    if let Ok(line) = std::env::var(pubkey_var) {
        if let Ok(a) = miot_keys::account_from_ssh(&line) {
            return a;
        }
        eprintln!("[node] {pubkey_var} set but not a valid ssh-ed25519 line — falling back");
    }
    if let Ok(hex) = std::env::var(hex_var) {
        if let Ok(a) = miot_keys::from_hex(&hex) {
            return a;
        }
        eprintln!("[node] {hex_var} set but not valid hex — falling back");
    }
    miot_keys::Identity::from_seed(&[default_seed; 32]).account()
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("MIOT_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(9944);
    let root = account_env("MIOT_ROOT_PUBKEY", "MIOT_ROOT", 1);
    let leader = account_env("MIOT_LEADER_PUBKEY", "MIOT_LEADER", 2);

    let (ext, genesis_hash) = genesis(root.clone(), leader.clone());
    let node: Shared =
        Arc::new(Mutex::new(Node { ext, log: VecDeque::new(), seq: 0, block: 1, parent_hash: genesis_hash }));

    // The block loop. Its own task, its own clock, and nothing in it waits for
    // a cat — that is the whole of Law I.
    {
        let node = node.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_millis(BLOCK_MS));
            loop {
                iv.tick().await;
                node.lock().await.advance();
            }
        });
    }

    let app = Router::new()
        .route("/head", get(head))
        .route("/events", get(events))
        .route("/submit", post(submit))
        .route("/meta", get(meta))
        .route("/account/{id}", get(account))
        .route("/artifact/{id}", get(artifact))
        .with_state(node);

    let addr = format!("0.0.0.0:{port}");
    println!(
        "[node] chain on {addr}  root={}  leader={}  block={BLOCK_MS}ms",
        miot_keys::short(&root),
        miot_keys::short(&leader)
    );
    let l = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(l, app).await.unwrap();
}

async fn head(AxState(n): AxState<Shared>) -> Json<serde_json::Value> {
    let mut n = n.lock().await;
    let block = n.block;
    let seq = n.seq;
    let (leader, closed) = n.ext.execute_with(|| {
        let t = Litter::table();
        (t.leader().map(miot_keys::to_hex), Litter::artifact(TaskId::parent(1)).is_some())
    });
    Json(serde_json::json!({"block":block,"seq":seq,"leader":leader,"closed":closed}))
}

async fn events(AxState(n): AxState<Shared>, Query(q): Query<Since>) -> Json<Vec<Entry>> {
    let n = n.lock().await;
    Json(n.log.iter().filter(|e| e.seq > q.since).cloned().collect())
}

/// What a signer needs to build a valid `SignedExtra` without trusting
/// anything hardcoded: `miot_runtime::client::sign` consumes exactly this.
async fn meta(AxState(n): AxState<Shared>) -> Json<serde_json::Value> {
    let mut n = n.lock().await;
    let genesis_hash = n.ext.execute_with(|| System::block_hash(0u64));
    Json(serde_json::json!({
        "genesis_hash": hex::encode(genesis_hash.as_bytes()),
        "spec_version": VERSION.spec_version,
        "tx_version": VERSION.transaction_version,
    }))
}

async fn account(AxState(n): AxState<Shared>, Path(id): Path<String>) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let Ok(who) = miot_keys::from_hex(&id) else {
        return (axum::http::StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"bad account hex"})));
    };
    let mut n = n.lock().await;
    let nonce = n.ext.execute_with(|| frame_system::Pallet::<Runtime>::account_nonce(&who));
    (axum::http::StatusCode::OK, Json(serde_json::json!({"nonce": nonce})))
}

/// The extrinsic arrives as raw SCALE bytes, not JSON — there is no `who`
/// field to trust or distrust; the signature over these exact bytes is what
/// decides the sender, inside [`Node::submit`].
async fn submit(
    AxState(n): AxState<Shared>,
    body: Bytes,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let uxt = match UncheckedExtrinsic::decode(&mut &body[..]) {
        Ok(u) => u,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok":false,"error":format!("bad extrinsic: {e}")})),
            );
        }
    };
    let mut n = n.lock().await;
    match n.submit(uxt) {
        Ok(()) => (axum::http::StatusCode::OK, Json(serde_json::json!({"ok":true}))),
        // A refusal is the chain's answer, not an error in the cat. It is
        // reported and carries the typed reason.
        Err(e) => (
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"ok":false,"error":e})),
        ),
    }
}

async fn artifact(
    AxState(n): AxState<Shared>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let mut n = n.lock().await;
    let a = parse_task(&id).and_then(|t| n.ext.execute_with(|| Litter::artifact(t)));
    Json(match a {
        Some(a) => {
            serde_json::json!({"found":true,"title":a.title,"body":a.body,"author":miot_keys::to_hex(&a.author)})
        }
        None => serde_json::json!({"found":false}),
    })
}
