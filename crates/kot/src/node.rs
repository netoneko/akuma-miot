//! The chain, as part of a process — a mesh node.
//!
//! One writer, many readers, and now the writer is **elected**. Every node
//! in the mesh runs this, durably (every member has a store), and
//! [`miot_mesh::Mesh`] decides which one produces blocks. The leader is the
//! *primary*: it ticks the block loop and accepts `/submit`. Everyone else is
//! a *replica*: it pulls the primary's log and forwards `/submit` to it.
//! The role used to be an operator-set env var (`MIOT_ROLE`), changed by
//! restarting the process. It changes on its own now, whenever the mesh
//! elects someone new.
//!
//! # Two loops, and only one of them is here
//!
//! **The block loop** ticks on a real interval and never waits for anybody.
//! That's Law I made literal: `on_initialize` runs on the clock, leases
//! expire, offers are re-made and directives repeat whether or not a single
//! cat is connected, or even alive. The agent loop (`agent.rs`) may share
//! this process, but it reaches the node over HTTP like any other client
//! (`docs/CLI.md` §5a: one code path, co-located or not).
//!
//! Every route below except `/submit` requires the `x-miot-signer`/
//! `x-miot-sig` envelope from a trusted genesis account (`is_trusted_signer`)
//! — a private chain, so an unsigned or stranger-signed request gets a plain
//! 401, nothing served. See "mesh auth" further down and `docs/MESH_AUTH.md`.
//!
//! **Patrons** (`--patrons`, not genesis) are the one exception: accounts
//! outside the roster this node lets *read* — the TLS handshake, the status
//! poll, the block log, the client reads (`is_reader`). Never `/mesh/vote`,
//! never `/chain/push`, and a patron's status never reaches the election.
//! Their own node runs as a learner (`--patron`, [`Mesh::learner`]). So a
//! friend on another network can follow the chain with no new genesis.
//!
//! | | |
//! |---|---|
//! | `POST /submit` | a signed extrinsic; checked, dispatched into the *currently open* block (forwarded to the primary from a replica) — its own signature is the gate, no envelope needed |
//! | `GET /meta` | genesis hash + spec/tx version |
//! | `GET /roster` | the genesis roster, `[{name, account}]`, from chain state |
//! | `GET /account/{id}` | that account's nonce (the primary's, from a replica) |
//! | `GET /events?since=N` | everything the chain emitted after cursor `N` |
//! | `GET /head` | height, litter leader, closed |
//! | `GET /artifact/{id}` | a closed parent's report |
//! | `GET /note/{id}`, `GET /notes` | a standalone artifact (no task), and the list of them |
//! | `GET /artifacts` | task-closed and standalone artifacts, merged into one id-addressed list |
//! | `GET /tasks` | one row per live task |
//! | `GET /stats` | every cat's latest self-reported work stats (`report_stats`) |
//! | `GET /chain/{head,blocks,checkpoint}` | the block log, for replicas |
//! | `GET /mesh/status`, `POST /mesh/vote` | election (`miot-mesh`) |
//! | `GET /mesh/peers` | what this node sees of the mesh — `kot peers` |
//! | `POST /activity` | this node's own cat's live record (`crate::activity`), signed by that cat — no one else's |
//! | `GET /activity` | every cat's live record this node has heard: its own, and each peer's, carried on the status exchange |
//!
//! # Changing role
//!
//! - **Promoted** (replica → primary): the open block was opened while
//!   folding (`pallet_litter::Replaying` on, so no tick). Turn folding off
//!   and run that block's tick (`tick_now`) so the first block this node
//!   closes is a complete one.
//! - **Demoted** (primary → replica): the open block's effects were never
//!   persisted and may never reach anyone. Throw the in-memory state away
//!   and rebuild it from the store. Then reconcile against the new primary
//!   like any replica, which rewinds whatever this node produced that the
//!   new primary never pulled (*leader wins, back to the last compaction*).
//! - **New primary** (replica → replica of someone else): reconcile once
//!   against it before tailing, since the old and new primaries may disagree
//!   about the last few blocks.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State as AxState};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::serve::Listener;
use axum::{Json, Router};
use codec::{Decode, Encode};
use http::{HeaderMap, HeaderValue};
use miot_keys::Identity;
use miot_mesh::{Hard, Mesh, Status, Timing, VoteReply, VoteRequest};
use miot_primitives::{Effect, TaskId};
use miot_runtime::{AccountId, Executive, Header, Litter, Runtime, System, UncheckedExtrinsic, VERSION};
use polkadot_sdk::*;
use serde::{Deserialize, Serialize};
use sp_core::{ed25519, Pair as _, H256};
use sp_runtime::traits::Header as HeaderT;
use sp_runtime::traits::UniqueSaturatedInto;
use tokio::sync::Mutex;

use crate::activity::{Activity, Seen};
use crate::common::parse_task;
use crate::tls;

/// Everything a node needs to start. Every node in one mesh must agree on
/// `root`, `leader` and `roster` — they are genesis.
#[derive(Clone)]
pub struct NodeConfig {
    /// This node's mesh name — `--as`.
    pub name: String,
    /// This node's own keypair, derived from `--as` via the roster. Signs
    /// mesh-internal traffic (election, chain sync) — never `/submit`'s
    /// extrinsics, which are signed by whoever calls the client. See
    /// `docs/MESH_AUTH.md`.
    pub identity: Identity,
    pub bind: String,
    pub port: u16,
    pub db: PathBuf,
    /// The other members, as *this* node reaches them.
    pub peers: Vec<String>,
    pub root: AccountId,
    /// The *litter* leader at genesis (who plans) — unrelated to the mesh.
    pub leader: AccountId,
    /// Every member, by name — genesis, committed to chain state
    /// (`pallet_litter::Roster`, served as `/roster`). Its accounts are the
    /// ones given [`catnip`]; root and leader are always added.
    pub roster: Vec<(String, AccountId)>,
    /// How long a block takes. See [`BLOCK_MS`].
    pub block_ms: u64,
    /// How often a replica pulls the primary.
    pub sync_ms: u64,
    /// How often every node polls every peer's `/mesh/status`.
    pub poll_ms: u64,
    pub timing: Timing,
    /// Accounts outside the roster allowed to follow from this node: read,
    /// never vote or write. Per node, not genesis, so it can change with a
    /// restart of just the nodes a patron talks to. See the module doc.
    pub patrons: Vec<(String, AccountId)>,
    /// This node is a patron itself: it never campaigns and never votes
    /// ([`Mesh::learner`]), so it never produces.
    pub learner: bool,
}

/// Six seconds — the Polkadot default, and a round number to reason in.
///
/// The block time is not the interesting number; the **wake cadence** on top
/// of it is, and that lives per task in `miot-runtime`'s timer constants. What
/// matters is only that a wake interval is longer than one LLM turn.
pub const BLOCK_MS: u64 = 6000;

/// An effect plus the cursor position it sits at, so a cat can resume.
#[derive(Serialize, Clone)]
struct Entry {
    seq: u64,
    block: u64,
    /// Rendered rather than raw: the cat acts on this. Accounts render as
    /// hex, never as anything that looks like a name.
    effect: serde_json::Value,
    /// Who must take a turn because of it, as hex. Decided here, by the
    /// protocol, not by each cat.
    wakes: Option<String>,
    /// When its block was sealed, unix ms, by the primary's clock — carried
    /// in the block body ([`seal_body`]), so every node and every replay
    /// agree. `None` for a block sealed before this existed (or by a
    /// primary on an older build), and for an entry whose block is still
    /// open.
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<u64>,
}

const LOG_CAP: usize = 4096;
/// The mesh election's persisted state, in the store's aux space.
const AUX_MESH: &str = "mesh";
/// See [`genesis_fingerprint`].
const AUX_GENESIS: &str = "genesis";

pub struct Node {
    ext: sp_io::TestExternalities,
    log: VecDeque<Entry>,
    seq: u64,
    block: u64,
    /// The hash the currently open block will chain to when it closes.
    parent_hash: H256,
    /// The block log. Every mesh member is durable.
    store: miot_store::Store,
    /// Effects absorbed since the open block began; its body when it closes.
    pending: Vec<Effect<AccountId>>,
    /// Set by a successful `clear_all`; consumed when that block closes.
    pending_compaction: bool,
    genesis_root: AccountId,
    genesis_leader: AccountId,
    genesis_roster: Vec<(String, AccountId)>,
    /// The roster's accounts — what [`catnip`] and every trust check use.
    members: Vec<AccountId>,
    /// `--patrons`, by name. Readers, not members: see [`Node::is_reader`].
    patrons: Vec<(String, AccountId)>,
    /// When each patron last polled us, and the status it sent — kept
    /// here for `kot peers` only, never handed to [`Mesh`].
    patrons_seen: BTreeMap<String, (u64, Status)>,
    /// This node's own keypair — signs outgoing mesh-internal traffic.
    identity: Identity,
    mesh: Mesh,
    /// Whether this node is currently producing blocks — what the mesh last
    /// said, as applied. Differs from `mesh.is_leader()` only between an
    /// election result and [`Node::follow_mesh`].
    producing: bool,
    /// The primary's route, as last applied. `None` on the primary itself,
    /// and during an election.
    peer: Option<String>,
    /// A new primary was just adopted; compare logs before tailing it.
    needs_reconcile: bool,
    /// The leader we follow, by name, as last logged — so following one we
    /// have no route to (push-only) is announced once, like a routed one.
    followed: Option<String>,
    /// Primary only: per route, when a push session may next start
    /// (`u64::MAX` while one runs), and the last error logged for it.
    push_busy_until: BTreeMap<String, u64>,
    push_last_err: BTreeMap<String, String>,
    /// Primary only: `(route, term)` pairs whose log was already compared
    /// against ours, so a push session reconciles once per leadership.
    push_reconciled: BTreeSet<(String, u64)>,
    http: reqwest::Client,
    started: Instant,
    /// This node's own cat's live record, and when it came in — never on
    /// chain (`crate::activity`).
    activity: Option<(Instant, Activity)>,
    /// Each peer's cat's, by the account that signed the status carrying
    /// it: when it came in, and how old it already was then.
    peer_activity: BTreeMap<String, (Instant, u64, Activity)>,
    /// Extrinsics this node accepted but couldn't hand to the primary
    /// (`Route::Nobody` — a push-only follower with no route to today's
    /// leader). `mempool_round` retries these every mesh tick: forwards to
    /// the primary once a route exists, relays to reachable peers
    /// otherwise. Never persisted, never replicated — same status a block
    /// itself has before it's sealed. Bounded by `MEMPOOL_CAP`, oldest
    /// evicted first via `mempool_order`.
    mempool: HashMap<H256, MempoolEntry>,
    mempool_order: VecDeque<H256>,
    /// `/tx/{hash}`'s answer, from this node's own view only: set when this
    /// node itself applies an extrinsic (as primary) and flipped to
    /// `Sealed` when that block closes (`advance`), or set to `Pending`
    /// while the hash sits in `mempool` above. A leadership change loses an
    /// unsealed entry, same as the block it was riding would be. Bounded by
    /// `TX_STATUS_CAP`, oldest evicted first via `tx_status_order`.
    tx_status: HashMap<H256, TxState>,
    tx_status_order: VecDeque<H256>,
    /// Extrinsics this node took from a polled peer's queue and handed on
    /// (applied, forwarded, or queued here in turn), newest last, bounded by
    /// [`CARRIED_KEEP`]. Announced on every status this node sends, so the
    /// peer that offered them stops offering. See [`StatusWire::pending`].
    carried: VecDeque<H256>,
}

/// See [`Node::mempool`].
struct MempoolEntry {
    bytes: Vec<u8>,
    inserted_at: u64,
    /// A peer that polls us said it took this one on (`StatusWire::carried`).
    /// It stays only so `mempool_round` can still learn it sealed, which is
    /// what `/tx/{hash}` answers from; it is no longer offered or re-sent.
    carried: bool,
}

/// See [`Node::tx_status`]. `Copy` so `advance` can flip `Applied` entries
/// to `Sealed` in place without fighting the borrow checker over the map.
#[derive(Clone, Copy)]
enum TxState {
    Pending,
    Applied { height: u64 },
    Sealed { height: u64 },
}

impl TxState {
    fn json(self) -> serde_json::Value {
        match self {
            TxState::Pending => serde_json::json!({"status":"pending"}),
            TxState::Applied { height } => serde_json::json!({"status":"applied","height":height}),
            TxState::Sealed { height } => serde_json::json!({"status":"sealed","height":height}),
        }
    }
}

const MEMPOOL_CAP: usize = 512;
const TX_STATUS_CAP: usize = 1024;

fn tx_hash(bytes: &[u8]) -> H256 {
    H256::from(sp_io::hashing::blake2_256(bytes))
}

/// What actually goes over `/mesh/status`: the election's [`Status`], plus
/// this node's cat's live record riding along. Flattened, so to a node on
/// an older build — which parses a plain `Status` and ignores fields it
/// doesn't know — it's just a status; one that sends a plain `Status`
/// reads here as one with no activity.
#[derive(Serialize, Deserialize)]
struct StatusWire {
    #[serde(flatten)]
    status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    activity: Option<CarriedActivity>,
    /// Extrinsics this node queued for want of a route to the primary
    /// (`Node::mempool`), hex, oldest first, at most [`CARRY_PER_POLL`] — so
    /// a peer that polls it can take them the rest of the way. A node that
    /// can't call out has no other way to get a write off itself: its own
    /// `mempool_round` only reaches peers *it* can call, and for the AWS pair
    /// against a home primary that's each other (2026-09-25).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<String>,
    /// Hashes of extrinsics this node took from a peer's `pending` and
    /// handed on (`Node::carried`), hex — so that peer drops them from its
    /// queue instead of offering them on every poll until they expire.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    carried: Vec<String>,
}

/// How many queued extrinsics one status answer offers ([`StatusWire::pending`]).
const CARRY_PER_POLL: usize = 16;
/// How many carried hashes a node keeps announcing ([`StatusWire::carried`]).
const CARRIED_KEEP: usize = 64;

/// A record on the wire between nodes: how old it is as it leaves.
#[derive(Serialize, Deserialize)]
struct CarriedActivity {
    age_ms: u64,
    #[serde(flatten)]
    activity: Activity,
}

/// Records older than this aren't served: that cat's node has been out of
/// earshot long enough that "what it's doing" is no longer known.
const ACTIVITY_FORGET: Duration = Duration::from_secs(10 * 60);

/// A block body: its effects, SCALE-encoded, then the seal time (unix ms,
/// `u64`). The time is *appended* rather than wrapped around the effects so
/// a node on an older build — which reads a body with `Decode::decode`, not
/// `decode_all` — still reads the effects and ignores the rest; replicas
/// store the primary's bytes verbatim, so the fork check (a byte compare)
/// is unaffected either way.
fn seal_body(effects: &Vec<Effect<AccountId>>, at: u64) -> Vec<u8> {
    let mut body = effects.encode();
    body.extend(at.encode());
    body
}

/// The inverse of [`seal_body`]; a body from before seal times (nothing
/// after the effects) reads as `None`.
fn open_body(body: &[u8]) -> Result<(Vec<Effect<AccountId>>, Option<u64>), codec::Error> {
    let mut input = body;
    let effects = Vec::<Effect<AccountId>>::decode(&mut input)?;
    let at = if input.len() == 8 { u64::decode(&mut input).ok() } else { None };
    Ok((effects, at))
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Who an effect wakes, as it goes into `/events` — `None` for a record
/// nobody's loop should assemble a turn for, `"*"` for a broadcast.
///
/// This is the **node-level `no_ack` enforcement** (artifact 5 §1.3, kuro's
/// fix): a `Said`/`Message` flagged `no_ack` is *delivered* — it's in the
/// log, a human reading it sees it, an agent that happens to be awake for
/// another reason will find it — but it never **wakes** anyone. The old
/// behavior asked every model to honor the flag in its prompt and in its
/// auto-check-in; some did, some ping-ponged anyway, which is exactly the
/// "left to per-agent discipline" failure meow called out. Delivery is one
/// place, so the rule lives in one place.
fn wake_target(e: &Effect<AccountId>) -> Option<String> {
    use miot_keys::to_hex;
    let no_ack = matches!(e, Effect::Said { no_ack: true, .. } | Effect::Message { no_ack: true, .. });
    if no_ack || !e.wakes() {
        return None;
    }
    Some(e.to().map(to_hex).unwrap_or_else(|| "*".to_string()))
}

fn render(e: &Effect<AccountId>) -> serde_json::Value {
    use miot_keys::to_hex;
    use serde_json::json;
    match e {
        Effect::Said { from, to, body, from_root, no_ack, off_record } => {
            json!({"t":"said","from":to_hex(from),"to":to.as_ref().map(to_hex),"body":body,"root":from_root,"no_ack":no_ack,"off_record":off_record})
        }
        Effect::Opened { who, task, text } => json!({"t":"opened","who":to_hex(who),"task":task.to_string(),"text":text}),
        Effect::Planned { who, task, count } => json!({"t":"planned","who":to_hex(who),"task":task.to_string(),"count":count}),
        Effect::Assigned { to, task, what, expect } => {
            json!({"t":"assigned","to":to_hex(to),"task":task.to_string(),"what":what,"expect":expect})
        }
        Effect::Directed { to, task, directive } => {
            json!({"t":"directed","to":to_hex(to),"task":task.to_string(),"directive":format!("{directive:?}")})
        }
        Effect::Nudge { to, task, remaining, last } => {
            json!({"t":"nudge","to":to_hex(to),"task":task.to_string(),"remaining":remaining,"last":last})
        }
        Effect::Record { who, task, act, text } => {
            json!({"t":"record","who":to_hex(who),"task":task.to_string(),"act":act.as_str(),"text":text})
        }
        Effect::Requeued { task, from, why } => {
            json!({"t":"requeued","task":task.to_string(),"from":from.as_ref().map(to_hex),"why":format!("{why:?}")})
        }
        Effect::NudgeBudgetSpent { holder, task } => json!({"t":"budget_spent","holder":to_hex(holder),"task":task.to_string()}),
        Effect::Closed { task, title, body, author } => {
            json!({"t":"closed","task":task.to_string(),"title":title,"body":body,"author":to_hex(author)})
        }
        Effect::Failed { task } => json!({"t":"failed","task":task.to_string()}),
        Effect::Rehomed { task, from, to } => {
            json!({"t":"rehomed","task":task.to_string(),"from":from.as_ref().map(to_hex),"to":to_hex(to)})
        }
        Effect::StandaloneArtifact { author, id, title, body } => {
            json!({"t":"standalone_artifact","author":to_hex(author),"id":id,"title":title,"body":body})
        }
        Effect::StatsReported { who, turns, tool_calls, tokens, ms } => {
            json!({"t":"stats_reported","who":to_hex(who),"turns":turns,"tool_calls":tool_calls,"tokens":tokens,"ms":ms})
        }
        // Same `t` as the old variant — one kind of event to readers, with
        // `messages` present when the reporter counted them apart.
        Effect::StatsReported2 { who, turns, tool_calls, messages, tokens, ms } => {
            json!({"t":"stats_reported","who":to_hex(who),"turns":turns,"tool_calls":tool_calls,"messages":messages,"tokens":tokens,"ms":ms})
        }
        Effect::Message { id, from, to, body, parent, artifact_id, tags, from_root, no_ack, off_record } => {
            json!({"t":"message","id":id.to_string(),"from":to_hex(from),"to":to.as_ref().map(to_hex),"body":body,"parent":parent.map(|p| p.to_string()),"artifact_id":artifact_id.map(|a| a.to_string()),"tags":tags,"root":from_root,"no_ack":no_ack,"off_record":off_record})
        }
        Effect::Reacted { who, target, emoji } => json!({"t":"reacted","who":to_hex(who),"target":target.to_string(),"emoji":emoji}),
        Effect::Voted { who, artifact, up } => {
            json!({"t":"voted","who":to_hex(who),"artifact":artifact.to_string(),"up":up})
        }
    }
}

/// Give an account "provider" standing so [`frame_system::CheckNonce`] will
/// even look at its nonce.
///
/// `CheckNonce` refuses **every** account whose `providers`/`sufficients`
/// are both zero with `InvalidTransaction::Payment` — a gate that exists for
/// `pallet-balances` to trip. We have no balances pallet, so nothing ever
/// would. An account that hasn't had any catnip can't be nonce-checked.
fn catnip(who: &AccountId) {
    frame_system::Pallet::<Runtime>::inc_providers(who);
}

/// A fresh chain at genesis with block 1 open. `replaying` is set *before*
/// block 1 opens, so its `on_initialize` already knows whether to tick.
fn genesis(root: &AccountId, leader: &AccountId, roster: &[(String, AccountId)], replaying: bool) -> (sp_io::TestExternalities, H256) {
    use sp_runtime::BuildStorage;
    let mut t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
    pallet_litter::GenesisConfig::<Runtime> { root: Some(root.clone()), leader: Some(leader.clone()), roster: roster.to_vec() }
        .assimilate_storage(&mut t)
        .unwrap();
    let mut ext: sp_io::TestExternalities = t.into();
    // Block 1's parent is "genesis" by definition — what `CheckGenesis`
    // binds a signature to, and what `/meta` reports.
    let genesis_hash = H256::zero();
    let first = Header::new(1, Default::default(), Default::default(), genesis_hash, Default::default());
    let all = trusted_accounts(&members_of(roster), root, leader);
    ext.execute_with(|| {
        pallet_litter::Pallet::<Runtime>::set_replaying(replaying);
        Executive::initialize_block(&first);
        for m in &all {
            catnip(m);
        }
    });
    (ext, genesis_hash)
}

/// The roster's accounts, in order — genesis `members`.
fn members_of(roster: &[(String, AccountId)]) -> Vec<AccountId> {
    roster.iter().map(|(_, a)| a.clone()).collect()
}

/// What a store remembers about the genesis it was built under
/// ([`AUX_GENESIS`]): a hash over root, leader and the roster, names
/// included. The chain's own genesis hash can't do this job — it's
/// `H256::zero()` for every chain — so without it a node started on a new
/// genesis over an old block log would replay the old chain without a word.
fn genesis_fingerprint(root: &AccountId, leader: &AccountId, roster: &[(String, AccountId)]) -> [u8; 32] {
    sp_io::hashing::blake2_256(&(root, leader, roster).encode())
}

/// Genesis `members` plus root and leader — the account universe every
/// trust decision here draws from: [`Node::is_trusted_signer`]/`trusted_set`,
/// and the TLS pinning in [`tls`](crate::tls) (needed before a `Node` exists
/// at all, hence a free function rather than a method).
fn trusted_accounts(members: &[AccountId], root: &AccountId, leader: &AccountId) -> Vec<AccountId> {
    let mut v = members.to_vec();
    for a in [root, leader] {
        if !v.contains(a) {
            v.push(a.clone());
        }
    }
    v
}

/// [`Node::is_reader`]'s set, before a `Node` exists — what the TLS
/// listener accepts a handshake from.
fn reader_accounts(cfg: &NodeConfig) -> Vec<AccountId> {
    let mut v = trusted_accounts(&members_of(&cfg.roster), &cfg.root, &cfg.leader);
    for a in cfg.patrons.iter().map(|(_, a)| a.clone()).chain([cfg.identity.account()]) {
        if !v.contains(&a) {
            v.push(a);
        }
    }
    v
}

/// The full storage trie — `Store::compact`'s opaque `state` blob.
#[derive(Encode, Decode)]
struct Snapshot {
    raw: Vec<(Vec<u8>, (Vec<u8>, i32))>,
    root: H256,
    version: sp_storage::StateVersion,
}

impl Node {
    /// Open the store, rebuild state from it, load the election's vote.
    pub fn open(cfg: &NodeConfig) -> Result<Self, String> {
        let mut store = miot_store::Store::open(&cfg.db).map_err(|e| format!("open store at {:?}: {e}", cfg.db))?;
        let hard: Hard = match store.aux(AUX_MESH).map_err(|e| e.to_string())? {
            Some(b) => serde_json::from_slice(&b).map_err(|e| format!("corrupt mesh state: {e}"))?,
            None => Hard::default(),
        };
        let seed = cfg.name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
        let mut mesh = Mesh::new(cfg.name.clone(), cfg.peers.clone(), cfg.timing, hard, 0, seed);
        if cfg.learner {
            mesh = mesh.learner();
        }
        let fingerprint = genesis_fingerprint(&cfg.root, &cfg.leader, &cfg.roster);
        match store.aux(AUX_GENESIS).map_err(|e| e.to_string())? {
            Some(had) if had == fingerprint => {}
            Some(_) => {
                return Err(format!(
                    "the block log at {:?} was built under a different genesis (root, leader or roster changed). \
                     A new genesis is a new chain: move that directory aside and start again",
                    cfg.db
                ))
            }
            None => {
                // A store from before this check has no record: it can't be
                // told apart, so it's adopted, loudly.
                if !store.is_empty() {
                    eprintln!("[node] warning: {:?} predates genesis fingerprints; assuming it belongs to this genesis", cfg.db);
                }
                store.put_aux(AUX_GENESIS, &fingerprint).map_err(|e| e.to_string())?;
            }
        }
        let (ext, genesis_hash) = genesis(&cfg.root, &cfg.leader, &cfg.roster, true);
        let mut node = Node {
            ext,
            log: VecDeque::new(),
            seq: 0,
            block: 1,
            parent_hash: genesis_hash,
            store,
            pending: Vec::new(),
            pending_compaction: false,
            genesis_root: cfg.root.clone(),
            genesis_leader: cfg.leader.clone(),
            genesis_roster: cfg.roster.clone(),
            members: members_of(&cfg.roster),
            patrons: cfg.patrons.clone(),
            patrons_seen: BTreeMap::new(),
            identity: cfg.identity,
            mesh,
            producing: false,
            peer: None,
            needs_reconcile: false,
            followed: None,
            push_busy_until: BTreeMap::new(),
            push_last_err: BTreeMap::new(),
            push_reconciled: BTreeSet::new(),
            // mTLS pinned to the same genesis accounts `is_trusted_signer`
            // already trusts (`crate::tls`, `docs/MESH_AUTH.md`) — every
            // outbound call this node makes, peer traffic and forwarded
            // client requests alike, is both encrypted and authenticated by
            // the handshake itself. ALPN picks h2 when the peer's listener
            // (`tls::TlsListener`, same trusted set) offers it — no
            // `http2_prior_knowledge()` needed now that there's a real
            // handshake to negotiate over.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .use_preconfigured_tls(tls::client_config(&cfg.identity, trusted_accounts(&members_of(&cfg.roster), &cfg.root, &cfg.leader)))
                .build()
                .unwrap(),
            started: Instant::now(),
            activity: None,
            peer_activity: BTreeMap::new(),
            mempool: HashMap::new(),
            mempool_order: VecDeque::new(),
            tx_status: HashMap::new(),
            tx_status_order: VecDeque::new(),
            carried: VecDeque::new(),
        };
        if !node.store.is_empty() {
            println!(
                "[node] replaying {} block(s) (checkpoint {})",
                node.store.head() - node.store.last_checkpoint(),
                node.store.last_checkpoint()
            );
        }
        node.replay();
        Ok(node)
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Our status as it goes on the wire, our cat's record riding along.
    fn status_wire(&self) -> StatusWire {
        let status = self.mesh.status(self.store.head(), &miot_keys::to_hex(&self.identity.account()));
        let activity = self.activity.as_ref().map(|(got, a)| CarriedActivity { age_ms: got.elapsed().as_millis() as u64, activity: a.clone() });
        let pending = self
            .mempool_order
            .iter()
            .filter_map(|h| self.mempool.get(h))
            .filter(|e| !e.carried)
            .take(CARRY_PER_POLL)
            .map(|e| hex::encode(&e.bytes))
            .collect();
        let carried = self.carried.iter().map(|h| hex::encode(h.as_bytes())).collect();
        StatusWire { status, activity, pending, carried }
    }

    /// A peer's cat's record, from a status `signer` sent (or answered
    /// with). Only if the status names its own signer, and only if newer
    /// than what we have — two routes can deliver the same record twice.
    fn take_peer_activity(&mut self, signer: &AccountId, wire: &StatusWire) {
        let Some(c) = &wire.activity else { return };
        let key = miot_keys::to_hex(signer);
        if wire.status.account != key {
            return;
        }
        if self.peer_activity.get(&key).is_some_and(|(_, _, had)| had.at > c.activity.at) {
            return;
        }
        self.peer_activity.insert(key, (Instant::now(), c.age_ms, c.activity.clone()));
    }

    pub fn head(&self) -> u64 {
        self.store.head()
    }

    pub fn mesh(&self) -> &Mesh {
        &self.mesh
    }

    pub fn is_producing(&self) -> bool {
        self.producing
    }

    /// Queued extrinsics this node still offers to whoever polls it — not yet
    /// taken on by a carrier (see `StatusWire::pending`). For tests.
    pub fn mempool_offered(&self) -> usize {
        self.mempool.values().filter(|e| !e.carried).count()
    }

    /// Who mesh-internal traffic is allowed to come from: genesis `members`
    /// plus root and leader, the same set [`genesis`] gives chain standing.
    /// Not the same question as "who can sign a tx" — `/submit` accepts any
    /// account `CheckNonce` allows; this is narrower, and it's what was
    /// missing before (`docs/MESH_AUTH.md`).
    fn is_trusted_signer(&self, a: &AccountId) -> bool {
        self.members.contains(a) || *a == self.genesis_root || *a == self.genesis_leader
    }

    /// [`is_trusted_signer`](Self::is_trusted_signer)'s set, materialized —
    /// for a snapshot ([`PeerAuth`]) that outlives the lock.
    fn trusted_set(&self) -> Vec<AccountId> {
        trusted_accounts(&self.members, &self.genesis_root, &self.genesis_leader)
    }

    /// Who may *read* from this node: every trusted signer, plus
    /// `--patrons`, plus this node's own key — a learner isn't in the
    /// roster, and its operator's `kot` signs as it. Reading is the status
    /// poll, the block log and the client-facing GETs. Voting, pushing
    /// blocks and posting activity stay [`is_trusted_signer`]-only.
    fn is_reader(&self, a: &AccountId) -> bool {
        self.is_trusted_signer(a) || *a == self.identity.account() || self.patrons.iter().any(|(_, f)| f == a)
    }

    /// The whole litter table, SCALE-encoded — counters `/tasks` doesn't
    /// show included. Two nodes that agree on this agree on everything the
    /// next block's tick will read. For tests.
    pub fn state_fingerprint(&mut self) -> Vec<u8> {
        self.ext.execute_with(|| pallet_litter::Litter::<Runtime>::get().encode())
    }

    fn absorb(&mut self, effects: Vec<Effect<AccountId>>) {
        for e in effects {
            self.seq += 1;
            // `"*"` is the broadcast sentinel every agent's own filter
            // treats as "wakes me too" (`agent.rs`) — a genuine fan-out
            // (a `Vec` of every member's hex) was the other option, but it
            // meant changing `Entry.wakes`'s wire shape for every consumer
            // of `/events`; a client already ignoring a wake value it
            // doesn't recognize (a stricter, differently-versioned agent)
            // degrades to "didn't wake for this broadcast" rather than a
            // parse error, which a shape change would risk instead.
            let wakes = wake_target(&e);
            let entry = Entry { seq: self.seq, block: self.block, effect: render(&e), wakes, at: None };
            if self.log.len() >= LOG_CAP {
                self.log.pop_front();
            }
            self.log.push_back(entry);
            // An off-the-record `Said` still fans out live (the entry
            // above) but never joins `self.pending` — the Vec `persist()`
            // encodes as the block body — so it never reaches
            // `Store::append` and cannot come back on replay, rewind, or a
            // peer that pulls the block later instead of tailing it live.
            if !matches!(&e, Effect::Said { off_record: true, .. }) {
                self.pending.push(e);
            }
        }
    }

    /// Give every log entry of block `height` its seal time.
    fn stamp_block(&mut self, height: u64, at: Option<u64>) {
        for e in self.log.iter_mut().rev() {
            if e.block < height {
                break;
            }
            if e.block == height {
                e.at = at;
            }
        }
    }

    /// Everything the pallet emitted since the last drain.
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

    /// Persist the election's state if it changed. Called before any answer
    /// leaves this node: a vote must be on disk before it's told to anyone.
    fn save_hard(&mut self) {
        if let Some(h) = self.mesh.take_dirty() {
            let bytes = serde_json::to_vec(&h).expect("Hard serializes");
            if let Err(e) = self.store.put_aux(AUX_MESH, &bytes) {
                eprintln!("[mesh] failed to persist term/vote: {e}");
            }
        }
    }

    /// Record a hash's status, evicting the oldest tracked hash first once
    /// `TX_STATUS_CAP` is reached — see [`Node::tx_status`].
    fn set_tx_status(&mut self, hash: H256, st: TxState) {
        if !self.tx_status.contains_key(&hash) {
            if self.tx_status_order.len() >= TX_STATUS_CAP {
                if let Some(old) = self.tx_status_order.pop_front() {
                    self.tx_status.remove(&old);
                }
            }
            self.tx_status_order.push_back(hash);
        }
        self.tx_status.insert(hash, st);
    }

    /// Queue an extrinsic this node couldn't hand to the primary — see
    /// [`Node::mempool`]. A no-op if already queued (a relay can hear the
    /// same hash from more than one peer).
    fn mempool_insert(&mut self, hash: H256, bytes: Vec<u8>) {
        if self.mempool.contains_key(&hash) {
            return;
        }
        if self.mempool_order.len() >= MEMPOOL_CAP {
            if let Some(old) = self.mempool_order.pop_front() {
                self.mempool.remove(&old);
            }
        }
        self.mempool_order.push_back(hash);
        self.mempool.insert(hash, MempoolEntry { bytes, inserted_at: unix_ms(), carried: false });
        self.set_tx_status(hash, TxState::Pending);
    }

    /// Close the open block and open the next — the primary's block loop.
    fn advance(&mut self) {
        let closing = self.block;
        let header = self.ext.execute_with(Executive::finalize_block);
        self.persist(closing);
        for st in self.tx_status.values_mut() {
            if let TxState::Applied { height } = *st {
                if height == closing {
                    *st = TxState::Sealed { height };
                }
            }
        }
        if self.pending_compaction {
            self.compact_at(closing);
            self.pending_compaction = false;
        }
        self.parent_hash = header.hash();
        self.block += 1;
        let next = Header::new(self.block, Default::default(), Default::default(), self.parent_hash, Default::default());
        self.ext.execute_with(|| Executive::initialize_block(&next));
        let fx = self.drain();
        self.absorb(fx);
    }

    /// Write `height`'s effects as that block's body. Every block gets a row,
    /// even a quiet one — the store is append-only and gap-free.
    fn persist(&mut self, height: u64) {
        let at = unix_ms();
        let body = seal_body(&self.pending, at);
        self.stamp_block(height, Some(at));
        match self.store.append(height, &body) {
            Ok(()) => {
                self.mesh.appended();
                self.save_hard();
            }
            Err(e) => eprintln!("[node] store append failed at block {height}: {e}"),
        }
        self.pending.clear();
    }

    /// Fold one already-decided block into state. Assumes `height` is the
    /// open block; leaves `height + 1` open. The one place a stored or
    /// peer-fetched block gets folded — replay and sync both call this.
    fn apply_block(&mut self, height: u64, effects: Vec<Effect<AccountId>>, at: Option<u64>) {
        self.ext.execute_with(|| {
            let now: miot_primitives::BlockNumber = frame_system::Pallet::<Runtime>::block_number().unique_saturated_into();
            for e in &effects {
                pallet_litter::Pallet::<Runtime>::replay_effect(e, now);
            }
        });
        let header = self.ext.execute_with(Executive::finalize_block);
        self.parent_hash = header.hash();
        self.absorb(effects);
        self.stamp_block(height, at);
        self.pending.clear();
        self.block = height + 1;
        let next = Header::new(self.block, Default::default(), Default::default(), self.parent_hash, Default::default());
        self.ext.execute_with(|| Executive::initialize_block(&next));
        // Folding runs no tick, so nothing was emitted here; drain anyway so
        // a stray event can never leak into the next block's body.
        let _ = self.drain();
    }

    /// Rebuild state from the store: the checkpoint if there is one, then
    /// every block above it. Assumes `self.ext` is at genesis with block 1
    /// open, or gets there via the checkpoint.
    fn replay(&mut self) {
        let head = self.store.head();
        let cp = self.store.last_checkpoint();
        if cp > 0 {
            let blob = self.store.checkpoint_state().expect("store read").expect("checkpoint recorded, its state must exist");
            self.restore_from_snapshot(cp, &blob);
        }
        for h in (cp + 1)..=head {
            self.block = h;
            let body = self.store.block(h).expect("store read").expect("contiguous store above the checkpoint");
            let (effects, at) = open_body(&body).expect("corrupt block body in store");
            self.apply_block(h, effects, at);
        }
    }

    /// Throw away in-memory state and rebuild it from the store alone — on
    /// demotion (the open block's effects were never persisted) and after
    /// any change the store made underneath us (rewind, adopted checkpoint).
    fn reload_from_store(&mut self) {
        let (ext, genesis_hash) = genesis(&self.genesis_root, &self.genesis_leader, &self.genesis_roster, !self.producing);
        self.ext = ext;
        self.parent_hash = genesis_hash;
        self.block = 1;
        self.log.clear();
        self.seq = 0;
        self.pending.clear();
        self.pending_compaction = false;
        self.replay();
    }

    /// Snapshot state as of the just-finalized `height` and record it as a
    /// compaction checkpoint. Fires on root's `/clear`.
    ///
    /// `into_raw_snapshot` drains only the backend, not the overlay, hence
    /// `commit_all` first. Found live: without it the snapshot silently
    /// reflected genesis and the next block tripped frame_system's
    /// "block number must be strictly increasing".
    fn compact_at(&mut self, height: u64) {
        let mut ext = std::mem::replace(&mut self.ext, sp_io::TestExternalities::new_empty());
        ext.commit_all().expect("no open storage transactions to conflict with a plain commit");
        let (raw, root) = ext.into_raw_snapshot();
        let version = sp_storage::StateVersion::default();
        self.ext = sp_io::TestExternalities::from_raw_snapshot(raw.clone(), root, version);
        let blob = Snapshot { raw, root, version }.encode();
        match self.store.compact(height, &blob) {
            Ok(pruned) => println!("[node] compacted at block {height} ({pruned} block(s) pruned)"),
            Err(e) => eprintln!("[node] compact failed at block {height}: {e}"),
        }
    }

    /// Restore state from a checkpoint; leaves `height + 1` open.
    fn restore_from_snapshot(&mut self, height: u64, blob: &[u8]) {
        let Snapshot { raw, root, version } = Decode::decode(&mut &blob[..]).expect("corrupt checkpoint state");
        self.ext = sp_io::TestExternalities::from_raw_snapshot(raw, root, version);
        self.parent_hash = self.ext.execute_with(|| System::block_hash(height));
        self.log.clear();
        self.seq = 0;
        self.pending.clear();
        self.block = height + 1;
        let next = Header::new(self.block, Default::default(), Default::default(), self.parent_hash, Default::default());
        // The snapshot carries whatever `Replaying` its producer had; this
        // node's own role decides, and must before the block opens.
        let replaying = !self.producing;
        self.ext.execute_with(|| {
            pallet_litter::Pallet::<Runtime>::set_replaying(replaying);
            Executive::initialize_block(&next)
        });
        let _ = self.drain();
    }

    /// Check and dispatch one signed extrinsic into the open block.
    fn submit(&mut self, uxt: UncheckedExtrinsic) -> Result<(), String> {
        let wants_compaction = matches!(
            uxt.function,
            miot_runtime::RuntimeCall::Litter(pallet_litter::Call::clear_all {})
                | miot_runtime::RuntimeCall::Litter(pallet_litter::Call::request_compaction {})
        );
        let r = self.ext.execute_with(|| Executive::apply_extrinsic(uxt));
        let fx = self.drain();
        self.absorb(fx);
        match r {
            Ok(Ok(())) => {
                if wants_compaction {
                    self.pending_compaction = true;
                }
                Ok(())
            }
            Ok(Err(e)) => Err(format!("{e:?}")),
            Err(e) => Err(format!("rejected: {e:?}")),
        }
    }

    /// Apply whatever the mesh last decided. See the module doc's
    /// "Changing role". Called after every mesh interaction, under the lock.
    fn follow_mesh(&mut self) {
        self.save_hard();
        let lead = self.mesh.is_leader();
        if lead && !self.producing {
            self.producing = true;
            self.ext.execute_with(|| {
                pallet_litter::Pallet::<Runtime>::set_replaying(false);
                pallet_litter::Pallet::<Runtime>::tick_now();
            });
            let fx = self.drain();
            self.absorb(fx);
            println!("[mesh] {} elected primary (term {}, head {})", self.mesh.name(), self.mesh.term(), self.store.head());
        } else if !lead && self.producing {
            self.producing = false;
            println!("[mesh] {} no longer primary (term {}); rebuilding from the store", self.mesh.name(), self.mesh.term());
            self.reload_from_store();
        }
        // A member pulls from the leader. A learner pulls from any member
        // it can reach (`Mesh::pull_sources`), and keeps the one it has
        // while that's still a source, so two replicas trading places by a
        // block don't make it switch (and reconcile) every round.
        let sources = self.mesh.pull_sources(self.now_ms());
        let route = match &self.peer {
            Some(p) if self.mesh.is_learner() && sources.contains(p) => Some(p.clone()),
            _ => sources.into_iter().next(),
        };
        // A learner pulling from a replica sees a new leader without its
        // source changing. The new leader may rewind what that replica
        // had, so check the log again, and say who it follows now.
        let new_leader = self.mesh.is_learner() && route.is_some() && self.mesh.leader().is_some() && self.mesh.leader() != self.followed.as_deref();
        if route != self.peer || new_leader {
            if let Some(r) = &route {
                let via = if self.mesh.leader_route() == Some(r.as_str()) { "at" } else { "via a replica at" };
                println!("[mesh] following {} {via} {r} (term {})", self.mesh.leader().unwrap_or("?"), self.mesh.term());
                self.needs_reconcile = true;
            }
            self.peer = route;
        }
        let leader = if lead { None } else { self.mesh.leader().map(str::to_string) };
        if leader != self.followed {
            if let (Some(l), None) = (&leader, &self.peer) {
                println!("[mesh] following {l} by push: no route to it from here (term {})", self.mesh.term());
            }
            self.followed = leader;
        }
    }
}

impl Node {
    /// Primary only: which push sessions to start now — every route
    /// `Mesh::push_targets` names that doesn't have one running or backing
    /// off. Marks them busy; [`push_session`] clears that when it ends.
    fn start_pushes(&mut self, now: u64) -> Vec<String> {
        if !self.producing {
            self.push_busy_until.clear();
            self.push_reconciled.clear();
            return Vec::new();
        }
        let head = self.store.head();
        let targets = self.mesh.push_targets(now, head);
        let starts: Vec<String> =
            targets.into_iter().filter(|r| self.push_busy_until.get(r).is_none_or(|&until| now >= until)).collect();
        for r in &starts {
            self.push_busy_until.insert(r.clone(), u64::MAX);
        }
        starts
    }
}

pub type Shared = Arc<Mutex<Node>>;

// ---------------------------------------------------------------- the mesh

/// One election round: poll every peer's status, feed it in, tick, and run
/// a campaign if the tick started one. Every node runs this every
/// `poll_ms`, leader included — a leader polls too, which is how
/// check-quorum knows it can still see a majority.
/// What a call to a peer needs beyond the URL: how to sign what this node
/// sends, and who to trust in what comes back. Every mesh-internal call
/// (election, chain sync) takes one; a snapshot rather than holding the
/// lock, since these calls cross an await and the node mustn't be blocked
/// while a peer is slow to answer.
#[derive(Clone)]
struct PeerAuth {
    http: reqwest::Client,
    identity: Identity,
    trusted: Vec<AccountId>,
}

impl PeerAuth {
    async fn snapshot(shared: &Shared) -> Self {
        let n = shared.lock().await;
        PeerAuth { http: n.http.clone(), identity: n.identity, trusted: n.trusted_set() }
    }

    fn is_trusted(&self, a: &AccountId) -> bool {
        self.trusted.contains(a)
    }
}

pub async fn mesh_round(shared: &Shared, poll_ms: u64) {
    let (routes, mine) = {
        let n = shared.lock().await;
        let mine = serde_json::to_vec(&n.status_wire()).expect("Status serializes");
        (n.mesh.routes().to_vec(), mine)
    };
    let auth = PeerAuth::snapshot(shared).await;
    // Longer than the poll interval on purpose: a slow answer is still an
    // answer. Only a peer that misses every poll for a whole election
    // window counts as gone.
    let timeout = Duration::from_millis((poll_ms * 2).max(2_000));
    let mut set = tokio::task::JoinSet::new();
    for r in routes {
        let auth = auth.clone();
        let mine = mine.clone();
        set.spawn(async move {
            let st = async {
                // Our own status rides along, so the peer hears from us even
                // if it can't call us back (`Mesh::on_inbound`). A node from
                // before this answers POST with 405; ask it the old way.
                let url = format!("{r}/mesh/status");
                let posted = auth
                    .http
                    .post(&url)
                    .headers(sign_headers(&auth.identity, &mine))
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(mine)
                    .timeout(timeout)
                    .send()
                    .await
                    .ok()?;
                let resp = if posted.status() == StatusCode::METHOD_NOT_ALLOWED {
                    auth.http.get(&url).headers(sign_headers(&auth.identity, b"")).timeout(timeout).send().await.ok()?
                } else {
                    posted
                };
                let resp_headers = resp.headers().clone();
                let bytes = resp.bytes().await.ok()?;
                let signer = verify_headers(&resp_headers, &bytes, |a| auth.is_trusted(a)).ok()?;
                Some((signer, serde_json::from_slice::<StatusWire>(&bytes).ok()?))
            }
            .await;
            (r, st)
        });
    }
    let mut got = Vec::new();
    while let Some(Ok(x)) = set.join_next().await {
        got.push(x);
    }

    let mut offered: Vec<Vec<u8>> = Vec::new();
    let req = {
        let mut n = shared.lock().await;
        let now = n.now_ms();
        for (r, st) in got {
            if let Some((signer, mut wire)) = st {
                n.take_peer_activity(&signer, &wire);
                offered.extend(std::mem::take(&mut wire.pending).iter().filter_map(|x| hex::decode(x).ok()));
                n.mesh.on_status(&r, wire.status, now);
            }
        }
        let head = n.store.head();
        let req = n.mesh.tick(now, head);
        n.follow_mesh();
        let pushes = n.start_pushes(now);
        (req, pushes)
    };
    let (req, pushes) = req;
    for route in pushes {
        tokio::spawn(push_session(shared.clone(), route));
    }
    carry_offered(shared, offered).await;
    if let Some(req) = req {
        campaign(shared, req, timeout).await;
    }
}

/// Take the rest of the way what a polled peer couldn't send itself
/// ([`StatusWire::pending`]): each through the same door as a `/submit` here
/// (the signer check, then apply, forward to the primary, or queue), once.
/// Only answers from members get this far — `mesh_round` verified the signer
/// against the trusted set — and each extrinsic's own signature still decides
/// who it's from.
async fn carry_offered(shared: &Shared, offered: Vec<Vec<u8>>) {
    for bytes in offered {
        let hash = tx_hash(&bytes);
        {
            let n = shared.lock().await;
            if n.carried.contains(&hash) || n.mempool.contains_key(&hash) {
                continue;
            }
        }
        let (code, _) = accept_extrinsic(shared, Bytes::from(bytes)).await;
        // A refusal is final too: the chain's answer won't change on a retry,
        // so the offering node may as well stop offering it.
        if code.is_success() || code == StatusCode::UNPROCESSABLE_ENTITY || code == StatusCode::BAD_REQUEST {
            let mut n = shared.lock().await;
            if n.carried.len() >= CARRIED_KEEP {
                n.carried.pop_front();
            }
            n.carried.push_back(hash);
        }
    }
}

/// Broadcast a vote request, feed the replies back; if a pre-vote just won,
/// go again with the real one.
async fn campaign(shared: &Shared, mut req: VoteRequest, timeout: Duration) {
    loop {
        let routes = shared.lock().await.mesh.routes().to_vec();
        let auth = PeerAuth::snapshot(shared).await;
        let mut set = tokio::task::JoinSet::new();
        for r in routes {
            let auth = auth.clone();
            let body = req.clone();
            set.spawn(async move {
                let rep = async {
                    let bytes = serde_json::to_vec(&body).ok()?;
                    let headers = sign_headers(&auth.identity, &bytes);
                    let resp = auth
                        .http
                        .post(format!("{r}/mesh/vote"))
                        .headers(headers)
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(bytes)
                        .timeout(timeout)
                        .send()
                        .await
                        .ok()?;
                    let resp_headers = resp.headers().clone();
                    let rbytes = resp.bytes().await.ok()?;
                    verify_headers(&resp_headers, &rbytes, |a| auth.is_trusted(a)).ok()?;
                    serde_json::from_slice::<VoteReply>(&rbytes).ok()
                }
                .await;
                (r, rep)
            });
        }
        let mut next = None;
        while let Some(Ok((r, rep))) = set.join_next().await {
            let Some(rep) = rep else { continue };
            let mut n = shared.lock().await;
            let (now, head) = (n.now_ms(), n.store.head());
            if let Some(nx) = n.mesh.on_vote_reply(&r, &req, rep, now, head) {
                next = Some(nx);
            }
            n.follow_mesh();
        }
        match next {
            Some(nx) => req = nx,
            None => return,
        }
    }
}

// ---------------------------------------------------------------- replica sync

#[derive(Serialize, Deserialize)]
struct BlockRow {
    height: u64,
    body_hex: String,
}

#[derive(Serialize, Deserialize)]
struct ChainHead {
    head: u64,
    last_checkpoint: u64,
}

#[derive(Deserialize)]
struct BlocksQuery {
    from: u64,
    limit: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct CheckpointRow {
    height: u64,
    state_hex: String,
}

/// A page is bounded so one catch-up request can't come back as one huge
/// response; the next tick asks for the rest.
const SYNC_PAGE: u64 = 256;

/// One replica tick: reconcile first if the primary is new, then tail it.
pub async fn replica_round(shared: &Shared) {
    let (peer, reconcile_first) = {
        let n = shared.lock().await;
        if n.producing {
            return;
        }
        match n.peer.clone() {
            Some(p) => (p, n.needs_reconcile),
            None => return, // election in progress; nobody to follow yet
        }
    };
    if reconcile_first {
        if !reconcile_if_diverged(shared, &peer).await {
            return; // peer unreachable; try again next tick
        }
        let mut n = shared.lock().await;
        if n.peer.as_deref() == Some(peer.as_str()) {
            n.needs_reconcile = false;
        }
    }
    sync_once(shared, &peer).await;
}

async fn peer_head(auth: &PeerAuth, peer: &str) -> Option<ChainHead> {
    let headers = sign_headers(&auth.identity, b"");
    let resp = auth.http.get(format!("{peer}/chain/head")).headers(headers).send().await.ok()?;
    let resp_headers = resp.headers().clone();
    let bytes = resp.bytes().await.ok()?;
    verify_headers(&resp_headers, &bytes, |a| auth.is_trusted(a)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Fetch and fold whatever the primary has beyond our head.
async fn sync_once(shared: &Shared, peer: &str) {
    let auth = PeerAuth::snapshot(shared).await;
    let Some(head) = peer_head(&auth, peer).await else {
        return; // the mesh round notices a dead primary; nothing to say here
    };
    adopt_peer_checkpoint_if_ahead(shared, &auth, peer, head.last_checkpoint).await;
    loop {
        let from = shared.lock().await.store.head() + 1;
        if from > head.head {
            break;
        }
        let rows = fetch_blocks(&auth, peer, from, SYNC_PAGE).await;
        if rows.is_empty() {
            break;
        }
        let mut n = shared.lock().await;
        // The primary may have changed while we were fetching; a page from
        // the old one must not land on top of the new one's log.
        if n.producing || n.peer.as_deref() != Some(peer) {
            return;
        }
        for row in &rows {
            if row.height <= n.store.head() {
                continue; // a push got there first
            }
            let Ok(body) = hex::decode(&row.body_hex) else {
                eprintln!("[node] sync: peer sent bad hex for block {}", row.height);
                return;
            };
            if let Err(e) = n.store.append(row.height, &body) {
                eprintln!("[node] sync: append failed at block {}: {e}", row.height);
                return;
            }
            let (effects, at) = open_body(&body).expect("corrupt block body from peer");
            n.apply_block(row.height, effects, at);
            n.mesh.appended();
        }
        n.save_hard();
    }
}

async fn fetch_blocks(auth: &PeerAuth, peer: &str, from: u64, limit: u64) -> Vec<BlockRow> {
    async {
        let query = format!("from={from}&limit={limit}");
        let headers = sign_headers(&auth.identity, query.as_bytes());
        let resp = auth.http.get(format!("{peer}/chain/blocks?{query}")).headers(headers).send().await.ok()?;
        let resp_headers = resp.headers().clone();
        let bytes = resp.bytes().await.ok()?;
        verify_headers(&resp_headers, &bytes, |a| auth.is_trusted(a)).ok()?;
        serde_json::from_slice::<Vec<BlockRow>>(&bytes).ok()
    }
    .await
    .unwrap_or_default()
}

async fn fetch_checkpoint(auth: &PeerAuth, peer: &str) -> Option<(u64, Vec<u8>)> {
    let headers = sign_headers(&auth.identity, b"");
    let resp = auth.http.get(format!("{peer}/chain/checkpoint")).headers(headers).send().await.ok()?;
    let resp_headers = resp.headers().clone();
    let bytes = resp.bytes().await.ok()?;
    verify_headers(&resp_headers, &bytes, |a| auth.is_trusted(a)).ok()?;
    let row: Option<CheckpointRow> = serde_json::from_slice(&bytes).ok()?;
    let row = row?;
    Some((row.height, hex::decode(&row.state_hex).ok()?))
}

/// If the primary has compacted past us — including a fresh node with no
/// log — the blocks we'd need are gone on its side too, so adopt its
/// checkpoint directly. Runs every tick, since `/clear` can move the
/// primary's checkpoint at any time. Found live: without that, a replica
/// that had already caught up got stuck one block short of where the
/// primary could still serve from.
async fn adopt_peer_checkpoint_if_ahead(shared: &Shared, auth: &PeerAuth, peer: &str, peer_cp: u64) -> bool {
    if peer_cp <= shared.lock().await.store.last_checkpoint() {
        return false;
    }
    let Some((cp_height, cp_state)) = fetch_checkpoint(auth, peer).await else {
        eprintln!("[node] sync: peer reports a checkpoint but didn't serve one");
        return false;
    };
    let mut n = shared.lock().await;
    if n.producing || n.peer.as_deref() != Some(peer) {
        return false;
    }
    n.store.adopt_checkpoint(cp_height, &cp_state).expect("adopt_checkpoint");
    println!("[node] adopted peer's checkpoint at block {cp_height}");
    n.reload_from_store();
    n.mesh.appended();
    n.save_hard();
    true
}

/// Compare our whole range against the primary's, once per new primary.
/// Comparing only the tip isn't enough: a quiet block (no effects, the
/// common case) encodes the same on every chain, so a divergence can sit
/// under a few agreeing quiet blocks. Observed live before this existed.
///
/// Returns false if the peer couldn't be asked, so the caller retries.
async fn reconcile_if_diverged(shared: &Shared, peer: &str) -> bool {
    let (auth, my_head, my_cp) = {
        let n = shared.lock().await;
        (PeerAuth { http: n.http.clone(), identity: n.identity, trusted: n.trusted_set() }, n.store.head(), n.store.last_checkpoint())
    };
    let Some(ph) = peer_head(&auth, peer).await else {
        return false;
    };

    if ph.last_checkpoint < my_cp {
        // We compacted somewhere the primary never did: a `/clear` block we
        // produced (or pulled from an old primary) that this one never got.
        // Everything above our checkpoint is unverifiable against it, and so
        // is the checkpoint, so rebuild from what the primary has.
        println!("[node] sync: our checkpoint {my_cp} is ahead of the primary's {}; rewinding to genesis", ph.last_checkpoint);
        rewind(shared, peer, my_cp.saturating_sub(1)).await;
        return true;
    }
    if adopt_peer_checkpoint_if_ahead(shared, &auth, peer, ph.last_checkpoint).await {
        return true;
    }
    if my_head == my_cp {
        return true; // nothing of our own above the checkpoint to compare
    }

    let mut theirs = Vec::new();
    let mut from = my_cp + 1;
    while from <= my_head {
        let rows = fetch_blocks(&auth, peer, from, SYNC_PAGE).await;
        if rows.is_empty() {
            break;
        }
        from += rows.len() as u64;
        for row in rows {
            match hex::decode(&row.body_hex) {
                Ok(b) => theirs.push(b),
                Err(_) => break,
            }
        }
    }
    let fork = shared.lock().await.store.fork_point(my_cp + 1, &theirs).expect("fork_point");
    if fork < my_head {
        rewind(shared, peer, fork).await;
    }
    true
}

/// Leader wins, back to the last compaction at or below `fork`.
async fn rewind(shared: &Shared, peer: &str, fork: u64) {
    let mut n = shared.lock().await;
    if n.producing || n.peer.as_deref() != Some(peer) {
        return;
    }
    let r = n.store.rewind_for_fork(fork).expect("rewind_for_fork");
    println!(
        "[node] sync: diverged from primary above block {fork}, rewound to {} (dropped {} block(s))",
        r.height, r.dropped
    );
    n.reload_from_store();
}

// ------------------------------------------------------------- leader push
//
// Pull assumes a follower can call its primary. One behind a NAT it doesn't
// control can't — the AWS pair, calling home through a router with no
// forwards (2026-09-24): home could call *them*, but nothing in the protocol
// ever did anything useful with that. So the primary, which sees from its
// own status polls that a peer's head has been stuck off its own for an
// election window (`Mesh::push_targets`), brings that peer's log to its own
// from this side. It runs the follower's reconcile itself — the peer's
// `/chain/head` and `/chain/blocks` are readable from here, and
// `Store::fork_point` is the same byte compare — then sends the result as
// `/chain/push` operations. The receiver applies them through the same
// store calls a pull uses, and only from the leader it follows, in the
// current term (`Mesh::accepts_push_from`).

/// A failed session waits this long before the next one to the same peer.
const PUSH_BACKOFF_MS: u64 = 10_000;

#[derive(Serialize, Deserialize)]
struct Push {
    /// The pusher's own status — who it is, and that it leads, in what term.
    leader: Status,
    #[serde(flatten)]
    op: PushOp,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum PushOp {
    /// Adopt this checkpoint: ours is past yours.
    Checkpoint { height: u64, state_hex: String },
    /// Leader wins: drop your log above `fork`, back to your last
    /// compaction at or below it (`Store::rewind_for_fork`).
    Rewind { fork: u64 },
    /// Append these, from your head + 1.
    Blocks { rows: Vec<BlockRow> },
}

#[derive(Serialize, Deserialize)]
struct PushReply {
    accepted: bool,
    head: u64,
    last_checkpoint: u64,
    #[serde(default)]
    why: Option<String>,
}

async fn push_session(shared: Shared, route: String) {
    let result = push_to(&shared, &route).await;
    let mut n = shared.lock().await;
    let now = n.now_ms();
    match result {
        Ok(()) => {
            n.push_busy_until.insert(route.clone(), now);
            n.push_last_err.remove(&route);
        }
        Err(why) => {
            n.push_busy_until.insert(route.clone(), now + PUSH_BACKOFF_MS);
            // Once per distinct error: a peer on an older build (no
            // `/chain/push`) would otherwise say so every ten seconds.
            if n.push_last_err.get(&route) != Some(&why) {
                eprintln!("[mesh] push to {route}: {why}");
                n.push_last_err.insert(route, why);
            }
        }
    }
}

/// One session: reconcile, then blocks, until the peer is at our head.
async fn push_to(shared: &Shared, route: &str) -> Result<(), String> {
    let auth = PeerAuth::snapshot(shared).await;
    let mut theirs = peer_head(&auth, route).await.ok_or("no answer from /chain/head")?;
    let started = theirs.head;
    let mut reconciled_now = false;
    // Bounded: a checkpoint, a rewind, and then pages — never a loop that
    // outlives a leadership by much.
    for _ in 0..1_000 {
        let (op, term) = {
            let n = shared.lock().await;
            if !n.producing {
                return Ok(()); // no longer primary; nothing of ours to push
            }
            let (my_cp, my_head, term) = (n.store.last_checkpoint(), n.store.head(), n.mesh.term());
            if theirs.last_checkpoint > my_cp {
                // They compacted where we never did. Same rule as a
                // follower's reconcile: rebuild from what we have.
                (Some(PushOp::Rewind { fork: theirs.last_checkpoint.saturating_sub(1) }), term)
            } else if theirs.last_checkpoint < my_cp {
                let state = n.store.checkpoint_state().map_err(|e| e.to_string())?.ok_or("our checkpoint has no state")?;
                (Some(PushOp::Checkpoint { height: my_cp, state_hex: hex::encode(state) }), term)
            } else if !n.push_reconciled.contains(&(route.to_string(), term)) {
                (None, term) // compare logs first, below, without holding the lock
            } else if theirs.head < my_head {
                let mut rows = Vec::new();
                let mut h = theirs.head + 1;
                while h <= my_head && (rows.len() as u64) < SYNC_PAGE {
                    if let Some(body) = n.store.block(h).map_err(|e| e.to_string())? {
                        rows.push(BlockRow { height: h, body_hex: hex::encode(body) });
                    }
                    h += 1;
                }
                (Some(PushOp::Blocks { rows }), term)
            } else {
                // The steady state is a block per session; say so only for
                // the first session of a leadership and for real catch-ups.
                if theirs.head != started && (reconciled_now || theirs.head.abs_diff(started) > 10) {
                    println!("[mesh] pushed {route} to block {} (was {started})", theirs.head);
                }
                return Ok(());
            }
        };
        let op = match op {
            Some(op) => op,
            None => {
                let fork = fork_against(shared, &auth, route, &theirs).await?;
                shared.lock().await.push_reconciled.insert((route.to_string(), term));
                reconciled_now = true;
                if fork >= theirs.head {
                    continue; // their log is a prefix of ours
                }
                println!("[mesh] {route} diverged from us above block {fork}; telling it to rewind");
                PushOp::Rewind { fork }
            }
        };
        let reply = push_op(shared, &auth, route, op).await?;
        if !reply.accepted {
            return Err(format!("refused: {}", reply.why.unwrap_or_default()));
        }
        theirs = ChainHead { head: reply.head, last_checkpoint: reply.last_checkpoint };
    }
    Ok(())
}

/// The last height the peer's log agrees with ours, read page by page from
/// its `/chain/blocks` and compared as `reconcile_if_diverged` does.
async fn fork_against(shared: &Shared, auth: &PeerAuth, route: &str, theirs: &ChainHead) -> Result<u64, String> {
    let mut fork = theirs.last_checkpoint;
    let mut from = theirs.last_checkpoint + 1;
    while from <= theirs.head {
        let rows = fetch_blocks(auth, route, from, SYNC_PAGE).await;
        if rows.is_empty() {
            break;
        }
        let bodies: Vec<Vec<u8>> = rows.iter().map_while(|r| hex::decode(&r.body_hex).ok()).collect();
        let agreed = shared.lock().await.store.fork_point(from, &bodies).map_err(|e| e.to_string())?;
        fork = agreed;
        if agreed < from + bodies.len() as u64 - 1 || bodies.len() < rows.len() {
            break;
        }
        from += bodies.len() as u64;
    }
    Ok(fork)
}

async fn push_op(shared: &Shared, auth: &PeerAuth, route: &str, op: PushOp) -> Result<PushReply, String> {
    let leader = {
        let n = shared.lock().await;
        n.mesh.status(n.store.head(), &miot_keys::to_hex(&n.identity.account()))
    };
    let bytes = serde_json::to_vec(&Push { leader, op }).expect("Push serializes");
    let resp = auth
        .http
        .post(format!("{route}/chain/push"))
        .headers(sign_headers(&auth.identity, &bytes))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(bytes)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("POST /chain/push: {e}"))?;
    if resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::METHOD_NOT_ALLOWED {
        return Err("peer has no /chain/push (an older build)".into());
    }
    let resp_headers = resp.headers().clone();
    let rbytes = resp.bytes().await.map_err(|e| e.to_string())?;
    verify_headers(&resp_headers, &rbytes, |a| auth.is_trusted(a)).map_err(|e| format!("reply: {e}"))?;
    serde_json::from_slice(&rbytes).map_err(|e| format!("reply: {e}"))
}

/// The receiving side of a push.
async fn chain_push(AxState(s): AxState<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let mut n = s.lock().await;
    let signer = match verify_headers(&headers, &body, |a| n.is_trusted_signer(a)) {
        Ok(a) => a,
        Err(why) => return unauthorized(why),
    };
    let push: Push = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "malformed push").into_response(),
    };
    if push.leader.account != miot_keys::to_hex(&signer) {
        return (StatusCode::FORBIDDEN, "a push must be signed by the leader it names").into_response();
    }
    let now = n.now_ms();
    let from = push.leader.name.clone();
    n.mesh.on_inbound(push.leader.clone(), now);
    n.follow_mesh();
    let reply = |n: &Node, why: Option<String>| {
        let r = PushReply { accepted: why.is_none(), head: n.store.head(), last_checkpoint: n.store.last_checkpoint(), why };
        signed_json(&n.identity, StatusCode::OK, &r)
    };
    if n.producing || !n.mesh.accepts_push_from(&push.leader) {
        return reply(&n, Some(format!("not following {from} in term {}", push.leader.term)));
    }
    match push.op {
        PushOp::Checkpoint { height, state_hex } => {
            if height > n.store.last_checkpoint() {
                let Ok(state) = hex::decode(&state_hex) else {
                    return reply(&n, Some("bad checkpoint hex".into()));
                };
                if let Err(e) = n.store.adopt_checkpoint(height, &state) {
                    return reply(&n, Some(format!("adopt_checkpoint: {e}")));
                }
                println!("[node] push: adopted {from}'s checkpoint at block {height}");
                n.reload_from_store();
                n.mesh.appended();
                n.save_hard();
            }
        }
        PushOp::Rewind { fork } => {
            if fork < n.store.head() {
                match n.store.rewind_for_fork(fork) {
                    Ok(r) => println!(
                        "[node] push: diverged from {from} above block {fork}, rewound to {} (dropped {} block(s))",
                        r.height, r.dropped
                    ),
                    Err(e) => return reply(&n, Some(format!("rewind: {e}"))),
                }
                n.reload_from_store();
            }
        }
        PushOp::Blocks { rows } => {
            for row in &rows {
                let head = n.store.head();
                if row.height <= head {
                    continue; // already have it (a pull got there first)
                }
                if row.height != head + 1 {
                    break;
                }
                let Ok(body) = hex::decode(&row.body_hex) else {
                    return reply(&n, Some(format!("bad hex for block {}", row.height)));
                };
                let Ok((effects, at)) = open_body(&body) else {
                    return reply(&n, Some(format!("corrupt body for block {}", row.height)));
                };
                if let Err(e) = n.store.append(row.height, &body) {
                    return reply(&n, Some(format!("append {}: {e}", row.height)));
                }
                n.apply_block(row.height, effects, at);
                n.mesh.appended();
            }
            n.save_hard();
        }
    }
    reply(&n, None)
}

// ------------------------------------------------------------- mesh auth
//
// Signs and verifies HTTP traffic — mesh-internal (election, chain sync)
// *and*, since 2026-09-23, every client-facing read too (`/tasks`,
// `/events`, `/account`, ...). This is a private chain: the account universe
// is closed (genesis `members` plus root and leader — the same set that
// gets `catnip`/provider standing), and adding one is a protocol update,
// not a runtime registration — so an unsigned request, or one signed by a
// stranger, gets nothing back, full stop. `/submit` is the one exception:
// its authority comes from the `UncheckedExtrinsic`'s own signature
// (`CheckNonce` already refuses non-members), so it needs no envelope of
// its own. Full write-up: `docs/MESH_AUTH.md`.
//
// The signature covers exactly the bytes sent — the raw request/response
// body, or the raw query string for a parameterless GET — never a
// re-serialized value, so there's no question of canonical JSON.

const SIG_HEADER_SIGNER: &str = "x-miot-signer";
const SIG_HEADER_SIG: &str = "x-miot-sig";

/// `pub` so `tests/election.rs` can call a mesh-internal endpoint directly,
/// and so `client.rs`/`agent.rs` can sign every read they make — every
/// caller of this node, mesh peer or `kot` client alike, proves who it is
/// the same way.
pub fn sign_headers(identity: &Identity, bytes: &[u8]) -> HeaderMap {
    let sig = identity.sign(bytes);
    let mut h = HeaderMap::new();
    h.insert(SIG_HEADER_SIGNER, HeaderValue::from_str(&miot_keys::to_hex(&identity.account())).expect("hex is ascii"));
    h.insert(SIG_HEADER_SIG, HeaderValue::from_str(&hex::encode(sig.0)).expect("hex is ascii"));
    h
}

/// `Ok(signer)` only if the header signature verifies over `bytes` *and*
/// `is_trusted` accepts the signer — a well-formed signature from a stranger
/// is still a rejection.
fn verify_headers(headers: &HeaderMap, bytes: &[u8], is_trusted: impl Fn(&AccountId) -> bool) -> Result<AccountId, &'static str> {
    let signer_hex = headers.get(SIG_HEADER_SIGNER).and_then(|v| v.to_str().ok()).ok_or("missing signer header")?;
    let sig_hex = headers.get(SIG_HEADER_SIG).and_then(|v| v.to_str().ok()).ok_or("missing sig header")?;
    let account = miot_keys::from_hex(signer_hex).map_err(|_| "signer header is not a valid account")?;
    if !is_trusted(&account) {
        return Err("signer is not a trusted mesh member");
    }
    let sig_bytes = hex::decode(sig_hex).map_err(|_| "sig header is not valid hex")?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| "sig is not 64 bytes")?;
    let sig = ed25519::Signature::from_raw(sig_arr);
    let pub_arr: [u8; 32] = AsRef::<[u8]>::as_ref(&account).try_into().expect("AccountId32 is 32 bytes");
    let public = ed25519::Public::from_raw(pub_arr);
    if !ed25519::Pair::verify(&sig, bytes, &public) {
        return Err("signature does not verify");
    }
    Ok(account)
}

/// A signed JSON response: the body, `Content-Type`, and the signature
/// headers together, so a handler can build it in one line.
fn signed_json<T: Serialize>(identity: &Identity, status: StatusCode, body: &T) -> Response {
    let bytes = serde_json::to_vec(body).expect("serializable");
    let mut headers = sign_headers(identity, &bytes);
    headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    (status, headers, Body::from(bytes)).into_response()
}

fn unauthorized(why: &'static str) -> Response {
    (StatusCode::UNAUTHORIZED, why).into_response()
}

/// The gate every client-facing handler opens with: `bytes` (the raw query
/// string, or `b""` for a parameterless GET) must carry a trusted member's
/// signature — a member's, or a patron's (`is_reader`): every gate this
/// opens is a read.
fn require_client_auth(n: &Node, headers: &HeaderMap, bytes: &[u8]) -> Result<(), Response> {
    verify_headers(headers, bytes, |a| n.is_reader(a)).map(|_| ()).map_err(unauthorized)
}

// ---------------------------------------------------------------- HTTP

pub fn router(shared: Shared) -> Router {
    Router::new()
        .route("/head", get(head))
        .route("/events", get(events))
        .route("/submit", post(submit))
        .route("/mempool/relay", post(mempool_relay))
        .route("/tx/{hash}", get(tx_status))
        .route("/meta", get(meta))
        .route("/roster", get(roster))
        .route("/account/{id}", get(account))
        .route("/artifact/{id}", get(artifact))
        .route("/note/{id}", get(standalone_artifact))
        .route("/notes", get(standalone_artifacts))
        .route("/artifacts", get(all_artifacts))
        .route("/stats", get(all_stats))
        .route("/tasks", get(tasks))
        .route("/chain/head", get(chain_head))
        .route("/chain/blocks", get(chain_blocks))
        .route("/chain/checkpoint", get(chain_checkpoint))
        .route("/chain/push", post(chain_push))
        .route("/mesh/status", get(mesh_status).post(mesh_status_post))
        .route("/mesh/vote", post(mesh_vote))
        .route("/mesh/peers", get(mesh_peers))
        .route("/activity", get(activity_get).post(activity_post))
        .with_state(shared)
}

/// The running node's tasks. Dropping this doesn't stop them; [`abort`]
/// does — that's how the election tests kill a node.
///
/// [`abort`]: Running::abort
pub struct Running {
    pub shared: Shared,
    pub addr: std::net::SocketAddr,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Running {
    pub fn abort(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }
    /// Wait for the HTTP server (and so the node) to end. It doesn't, unless
    /// aborted.
    pub async fn wait(mut self) {
        if let Some(t) = self.tasks.pop() {
            let _ = t.await;
        }
    }
}

/// Open the node and start all four of its loops: HTTP, blocks, replica
/// sync, mesh. Returns once it's listening.
pub async fn start(cfg: NodeConfig) -> Result<Running, String> {
    let node = Node::open(&cfg)?;
    println!(
        "[node] {} v{} on {}:{}  root={}  leader={}  peers={}  block={}ms",
        cfg.name,
        crate::version::VERSION,
        cfg.bind,
        cfg.port,
        miot_keys::short(&cfg.root),
        miot_keys::short(&cfg.leader),
        if cfg.peers.is_empty() { "none (a mesh of one)".to_string() } else { cfg.peers.join(",") },
        cfg.block_ms,
    );
    let shared: Shared = Arc::new(Mutex::new(node));
    let tcp = tokio::net::TcpListener::bind((cfg.bind.as_str(), cfg.port)).await.map_err(|e| format!("bind {}:{}: {e}", cfg.bind, cfg.port))?;
    let listener = tls::TlsListener::new(tcp, tls::server_config(&cfg.identity, reader_accounts(&cfg)));
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let mut tasks = Vec::new();

    // The block loop. Nothing in it waits for a cat — Law I. Only the
    // primary produces; on a replica each tick is a no-op.
    let s = shared.clone();
    let block_ms = cfg.block_ms;
    tasks.push(tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(block_ms));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            let mut n = s.lock().await;
            if n.producing {
                n.advance();
            }
        }
    }));

    let s = shared.clone();
    let sync_ms = cfg.sync_ms;
    tasks.push(tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(sync_ms));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            replica_round(&s).await;
        }
    }));

    let s = shared.clone();
    let poll_ms = cfg.poll_ms;
    tasks.push(tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(poll_ms));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            mesh_round(&s, poll_ms).await;
            mempool_round(&s).await;
        }
    }));

    let app = router(shared.clone());
    tasks.push(tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("[node] http server ended: {e}");
        }
    }));

    Ok(Running { shared, addr, tasks })
}

async fn mesh_status(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let n = n.lock().await;
    if let Err(why) = verify_headers(&headers, b"", |a| n.is_reader(a)) {
        return unauthorized(why);
    }
    signed_json(&n.identity, StatusCode::OK, &n.status_wire())
}

/// `POST /mesh/status`: the same answer as `GET`, but the caller sends its
/// own status too, and we take it in — how a peer that can't call us still
/// hears from us (`Mesh::on_inbound`).
async fn mesh_status_post(AxState(n): AxState<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let mut n = n.lock().await;
    let signer = match verify_headers(&headers, &body, |a| n.is_reader(a)) {
        Ok(a) => a,
        Err(why) => return unauthorized(why),
    };
    // A patron gets our answer — that's how it learns who leads — but
    // what it says about itself is only noted for `kot peers`: it's not a
    // member, so it has no say in the election, even if it claimed to lead.
    if !n.is_trusted_signer(&signer) {
        if let Ok(wire) = serde_json::from_slice::<StatusWire>(&body) {
            let now = n.now_ms();
            let name = n.patrons.iter().find(|(_, a)| *a == signer).map(|(name, _)| name.clone());
            n.patrons_seen.insert(name.unwrap_or_else(|| miot_keys::to_hex(&signer)), (now, wire.status));
        }
        return signed_json(&n.identity, StatusCode::OK, &n.status_wire());
    }
    // A status that doesn't name its own signer is ignored, not refused:
    // the caller still gets our answer, which is all the old GET gave it.
    if let Ok(wire) = serde_json::from_slice::<StatusWire>(&body) {
        if wire.status.account == miot_keys::to_hex(&signer) {
            // What the poller took from our queue on an earlier poll is its
            // to deliver now: stop offering it.
            for h in wire.carried.iter().filter_map(|h| hex::decode(h).ok()).filter(|b| b.len() == 32) {
                if let Some(e) = n.mempool.get_mut(&H256::from_slice(&h)) {
                    e.carried = true;
                }
            }
            n.take_peer_activity(&signer, &wire);
            let now = n.now_ms();
            n.mesh.on_inbound(wire.status, now);
            n.follow_mesh();
        }
    }
    signed_json(&n.identity, StatusCode::OK, &n.status_wire())
}

/// `POST /activity`: this node's own cat's live record. Signed by the cat,
/// and taken only if that's this node's own key — `kot run --as <name>`
/// runs the node and the cat as one identity — so no member can post a
/// record for a cat it isn't.
async fn activity_post(AxState(n): AxState<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let mut n = n.lock().await;
    let mine = n.identity.account();
    match verify_headers(&headers, &body, |a| *a == mine) {
        Ok(_) => {}
        Err(why) => return unauthorized(why),
    }
    match serde_json::from_slice::<Activity>(&body) {
        Ok(a) => {
            n.activity = Some((Instant::now(), a));
            StatusCode::NO_CONTENT.into_response()
        }
        Err(_) => (StatusCode::BAD_REQUEST, "malformed activity").into_response(),
    }
}

/// `GET /activity`: every cat's live record this node has heard — its own
/// first, then each peer's, with how old each is now.
async fn activity_get(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let n = n.lock().await;
    if let Err(r) = require_client_auth(&n, &headers, b"") {
        return r;
    }
    let mut out: Vec<Seen> = Vec::new();
    if let Some((got, a)) = &n.activity {
        out.push(Seen { account: miot_keys::to_hex(&n.identity.account()), age_ms: got.elapsed().as_millis() as u64, activity: a.clone() });
    }
    for (account, (got, age, a)) in &n.peer_activity {
        if got.elapsed() > ACTIVITY_FORGET {
            continue;
        }
        out.push(Seen { account: account.clone(), age_ms: age + got.elapsed().as_millis() as u64, activity: a.clone() });
    }
    Json(out).into_response()
}

async fn mesh_vote(AxState(n): AxState<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let mut n = n.lock().await;
    if let Err(why) = verify_headers(&headers, &body, |a| n.is_trusted_signer(a)) {
        return unauthorized(why);
    }
    let req: VoteRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return (StatusCode::BAD_REQUEST, "malformed vote request").into_response(),
    };
    let (now, head) = (n.now_ms(), n.store.head());
    let reply = n.mesh.on_vote_request(&req, now, head);
    // Persisted before the reply leaves (follow_mesh saves first).
    n.follow_mesh();
    signed_json(&n.identity, StatusCode::OK, &reply)
}

/// `kot peers`: this node, and every peer as last seen from here. Despite
/// the `/mesh` prefix this is client-facing, not mesh-internal — reachable
/// over `/mesh/*` for historical reasons, gated the same as `/tasks` et al.
async fn mesh_peers(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let n = n.lock().await;
    if let Err(r) = require_client_auth(&n, &headers, b"") {
        return r;
    }
    let now = n.now_ms();
    let peers: Vec<_> = n
        .mesh
        .routes()
        .iter()
        .map(|r| match n.mesh.seen().get(r) {
            Some((at, st)) => serde_json::json!({"route": r, "seen_ms_ago": now.saturating_sub(*at), "status": st}),
            None => serde_json::json!({"route": r, "seen_ms_ago": null, "status": null}),
        })
        .collect();
    // Peers that call us but that we have no working route to — known only
    // by what they sent. Without these a push-only node's view is all
    // "never answered" while it's in fact following along.
    let routed: BTreeSet<&str> = n.mesh.seen().values().map(|(_, st)| st.name.as_str()).collect();
    let inbound: Vec<_> = n
        .mesh
        .heard()
        .iter()
        .filter(|(name, _)| !routed.contains(name.as_str()))
        .map(|(_, (at, st))| serde_json::json!({"seen_ms_ago": now.saturating_sub(*at), "status": st}))
        .collect();
    Json(serde_json::json!({
        "inbound": inbound,
        "patrons": n.patrons_seen.iter().map(|(name, (at, st))| serde_json::json!({"name": name, "seen_ms_ago": now.saturating_sub(*at), "head": st.head})).collect::<Vec<_>>(),
        "learner": n.mesh.is_learner(),
        "me": n.mesh.status(n.store.head(), &miot_keys::to_hex(&n.identity.account())),
        "quorum": n.mesh.quorum(),
        "producing": n.producing,
        "following": n.peer,
        "last_checkpoint": n.store.last_checkpoint(),
        "peers": peers,
    }))
    .into_response()
}

async fn chain_head(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let n = n.lock().await;
    if let Err(why) = verify_headers(&headers, b"", |a| n.is_reader(a)) {
        return unauthorized(why);
    }
    let body = ChainHead { head: n.store.head(), last_checkpoint: n.store.last_checkpoint() };
    signed_json(&n.identity, StatusCode::OK, &body)
}

async fn chain_blocks(AxState(n): AxState<Shared>, uri: Uri, headers: HeaderMap, Query(q): Query<BlocksQuery>) -> Response {
    let n = n.lock().await;
    // Signed over the raw query string — exactly what the caller put after
    // `?` — never the parsed `BlocksQuery`, so there's no canonicalization
    // to get subtly wrong between the two ends.
    if let Err(why) = verify_headers(&headers, uri.query().unwrap_or("").as_bytes(), |a| n.is_reader(a)) {
        return unauthorized(why);
    }
    let head = n.store.head();
    let limit = q.limit.unwrap_or(SYNC_PAGE).min(SYNC_PAGE);
    let mut out = Vec::new();
    let mut h = q.from;
    while h <= head && (out.len() as u64) < limit {
        if let Some(body) = n.store.block(h).expect("store read") {
            out.push(BlockRow { height: h, body_hex: hex::encode(body) });
        }
        h += 1;
    }
    signed_json(&n.identity, StatusCode::OK, &out)
}

async fn chain_checkpoint(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let n = n.lock().await;
    if let Err(why) = verify_headers(&headers, b"", |a| n.is_reader(a)) {
        return unauthorized(why);
    }
    let cp = n.store.last_checkpoint();
    if cp == 0 {
        return signed_json(&n.identity, StatusCode::OK, &Option::<CheckpointRow>::None);
    }
    let state = n.store.checkpoint_state().expect("store read").expect("checkpoint recorded, its state must exist");
    signed_json(&n.identity, StatusCode::OK, &Some(CheckpointRow { height: cp, state_hex: hex::encode(state) }))
}

#[derive(Deserialize)]
struct Since {
    #[serde(default)]
    since: u64,
}

async fn head(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let mut n = n.lock().await;
    if let Err(r) = require_client_auth(&n, &headers, b"") {
        return r;
    }
    let (block, seq, last_checkpoint) = (n.block, n.seq, n.store.last_checkpoint());
    let (leader, closed) = n.ext.execute_with(|| {
        let t = Litter::table();
        (t.leader().map(miot_keys::to_hex), Litter::artifact(TaskId::parent(1)).is_some())
    });
    Json(serde_json::json!({"block":block,"seq":seq,"leader":leader,"closed":closed,"last_checkpoint":last_checkpoint})).into_response()
}

async fn events(AxState(n): AxState<Shared>, uri: Uri, headers: HeaderMap, Query(q): Query<Since>) -> Response {
    let n = n.lock().await;
    // Signed over the raw query string, same rule as `/chain/blocks`.
    if let Err(r) = require_client_auth(&n, &headers, uri.query().unwrap_or("").as_bytes()) {
        return r;
    }
    Json(n.log.iter().filter(|e| e.seq > q.since).cloned().collect::<Vec<_>>()).into_response()
}

async fn tasks(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let mut n = n.lock().await;
    if let Err(r) = require_client_auth(&n, &headers, b"") {
        return r;
    }
    let rows = n.ext.execute_with(|| {
        Litter::table()
            .tasks()
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id.to_string(),
                    "status": format!("{:?}", t.status),
                    "assignee": t.assignee.as_ref().map(miot_keys::to_hex),
                    "holder": t.holder.as_ref().map(miot_keys::to_hex),
                    "lease_until": t.lease_until,
                    "opened_by": miot_keys::to_hex(&t.opened_by),
                    "opened_at": t.opened_at,
                    "text": t.text,
                    // A sub-task's submitted result — the leader needs this
                    // to decide clear-vs-reopen; it used to be missing here
                    // entirely, which is why the agent loop's
                    // ClearanceNeeded prompt had nothing to show it.
                    "outcome": t.outcome.as_ref().map(|o| serde_json::json!({
                        "kind": if o.failed() { "failed" } else { "done" },
                        "text": o.text(),
                    })),
                })
            })
            .collect::<Vec<_>>()
    });
    Json(rows).into_response()
}

async fn meta(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let mut n = n.lock().await;
    if let Err(r) = require_client_auth(&n, &headers, b"") {
        return r;
    }
    let genesis_hash = n.ext.execute_with(|| System::block_hash(0u64));
    Json(serde_json::json!({
        "genesis_hash": hex::encode(genesis_hash.as_bytes()),
        "spec_version": VERSION.spec_version,
        "tx_version": VERSION.transaction_version,
        "kot_version": crate::version::VERSION,
    }))
    .into_response()
}

/// The genesis roster, from chain state (`pallet_litter::Roster`): who is
/// in this litter, by name. What a client names accounts with.
async fn roster(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let mut n = n.lock().await;
    if let Err(r) = require_client_auth(&n, &headers, b"") {
        return r;
    }
    let rows = n.ext.execute_with(|| {
        pallet_litter::Pallet::<Runtime>::roster()
            .into_iter()
            .map(|(name, a)| serde_json::json!({ "name": name, "account": miot_keys::to_hex(&a) }))
            .collect::<Vec<_>>()
    });
    Json(rows).into_response()
}

/// Where a replica sends what only the primary can answer. `None` while
/// nobody leads.
enum Route {
    Here,
    /// The primary's route, this node's own `http`, and this node's own
    /// identity — a replica forwarding `/account` re-signs as itself rather
    /// than relaying the caller's headers, since it's a trusted member in
    /// its own right and the caller's signature was already checked (or,
    /// for `/submit`, isn't the gate at all) before we got here.
    Primary(String, reqwest::Client, Identity),
    /// No primary to forward to. `Some(name)` when there is one, but this
    /// node follows it by push and has no route to it.
    Nobody(Option<String>),
}

async fn route(n: &Shared) -> Route {
    let n = n.lock().await;
    if n.producing {
        Route::Here
    } else {
        match &n.peer {
            Some(p) => Route::Primary(p.clone(), n.http.clone(), n.identity),
            None => Route::Nobody(n.mesh.leader().map(str::to_string)),
        }
    }
}

fn no_primary(leader: Option<String>) -> (StatusCode, Json<serde_json::Value>) {
    let error = match leader {
        // A push-only follower (behind a NAT): it has the log, but no way
        // to hand a write to the primary. Not an election; retrying won't help.
        Some(l) => format!("the primary is {l}, but this node has no route to it (it follows by push); submit to another node"),
        None => "no primary right now (election in progress); retry shortly".to_string(),
    };
    (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"ok":false,"error":error})))
}

/// Forward to the primary and hand its answer back verbatim.
async fn forward(http: &reqwest::Client, req: reqwest::RequestBuilder) -> (StatusCode, Json<serde_json::Value>) {
    let _ = http;
    match req.send().await {
        Ok(r) => {
            let code = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            (code, Json(r.json().await.unwrap_or_default()))
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"ok":false,"error":format!("primary unreachable: {e}")}))),
    }
}

/// A replica's nonce lags the primary's by up to a sync interval — enough to
/// get a signed extrinsic refused as stale — so the nonce comes from
/// wherever the extrinsic will land.
async fn account(AxState(n): AxState<Shared>, Path(id): Path<String>, headers: HeaderMap) -> (StatusCode, Json<serde_json::Value>) {
    let Ok(who) = miot_keys::from_hex(&id) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"bad account hex"})));
    };
    {
        let n = n.lock().await;
        if require_client_auth(&n, &headers, b"").is_err() {
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error":"unauthorized"})));
        }
    }
    match route(&n).await {
        // Re-signed as this node's own (trusted) identity, not the
        // caller's headers relayed verbatim — see `Route::Primary`.
        Route::Primary(p, http, identity) => {
            let signed = sign_headers(&identity, b"");
            return forward(&http, http.get(format!("{p}/account/{id}")).headers(signed)).await;
        }
        Route::Nobody(_) | Route::Here => {}
    }
    let mut n = n.lock().await;
    let nonce = n.ext.execute_with(|| frame_system::Pallet::<Runtime>::account_nonce(&who));
    (StatusCode::OK, Json(serde_json::json!({"nonce": nonce})))
}

/// Raw SCALE bytes, not JSON — there's no `who` field to trust; the
/// signature over these exact bytes decides the sender. On a replica, the
/// bytes go to the primary unchanged: any node will do (`docs/CLI.md` §5a).
/// Accept a raw signed extrinsic, whichever door it came in — a direct
/// `/submit` from a client, or a `/mempool/relay` hop from a peer that
/// couldn't reach the primary either. Same routing `/submit` always used
/// (apply here, or forward to the primary), plus the one new case: no
/// route to the primary queues it in `mempool` instead of refusing —
/// `mempool_round` keeps trying it every mesh tick (`HANDOFF.md`, "One-way
/// reachability": a push-only follower otherwise has no way to write at
/// all while the primary is on the side it can't reach).
async fn accept_extrinsic(n: &Shared, body: Bytes) -> (StatusCode, Json<serde_json::Value>) {
    let hash = tx_hash(&body);
    // Only a genesis account can ever land a write (`catnip`), so a
    // signer that isn't one is refused here, with the chain's own answer,
    // rather than forwarded, or queued and relayed around the mempool
    // until some primary says the same thing. Patrons reach this door
    // (TLS lets them in to read), and this is what keeps it read-only.
    if let Ok(UncheckedExtrinsic { preamble: sp_runtime::generic::Preamble::Signed(who, ..), .. }) = UncheckedExtrinsic::decode(&mut &body[..]) {
        if !n.lock().await.is_trusted_signer(&who) {
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"ok":false,"error":"rejected: Invalid(Payment)"})));
        }
    }
    match route(n).await {
        Route::Here => {}
        // `/submit` isn't header-gated — the extrinsic's own signature is
        // the authority — so nothing needs re-signing on the way through.
        Route::Primary(p, http, _identity) => return forward(&http, http.post(format!("{p}/submit")).body(body)).await,
        Route::Nobody(leader) => {
            let mut n = n.lock().await;
            n.mempool_insert(hash, body.to_vec());
            let note = match leader {
                Some(l) => format!("no route to the primary ({l}) yet; queued locally and relaying to peers"),
                None => "no primary right now (election in progress); queued locally".to_string(),
            };
            return (StatusCode::OK, Json(serde_json::json!({"ok":true,"status":"pending","tx_hash":hex::encode(hash),"note":note})));
        }
    }
    let uxt = match UncheckedExtrinsic::decode(&mut &body[..]) {
        Ok(u) => u,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok":false,"error":format!("bad extrinsic: {e}")}))),
    };
    let mut n = n.lock().await;
    if !n.producing {
        return no_primary(None); // demoted between the check above and now
    }
    let height = n.block;
    match n.submit(uxt) {
        Ok(()) => {
            n.mempool.remove(&hash);
            n.set_tx_status(hash, TxState::Applied { height });
            (StatusCode::OK, Json(serde_json::json!({"ok":true,"status":"applied","tx_hash":hex::encode(hash),"height":height})))
        }
        // A refusal is the chain's answer, typed — not an error in the cat.
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"ok":false,"error":e}))),
    }
}

async fn submit(AxState(n): AxState<Shared>, body: Bytes) -> (StatusCode, Json<serde_json::Value>) {
    accept_extrinsic(&n, body).await
}

/// A peer relaying an extrinsic it couldn't forward either — see
/// `mempool_round`. Same acceptance path as `/submit`; the only difference
/// is who called it.
async fn mempool_relay(AxState(n): AxState<Shared>, body: Bytes) -> (StatusCode, Json<serde_json::Value>) {
    accept_extrinsic(&n, body).await
}

/// Whether/when a submitted extrinsic landed, by hash — the polling target
/// for a client that got `"status":"pending"` back from `/submit`. Routed
/// like `/account`: this node's own view if it has one (still in
/// `mempool`, or applied/sealed here as the node that actually ran it),
/// otherwise forwarded to whoever the primary is. `"unknown"` covers both
/// "never seen" and "seen, but the node that resolved it has since been
/// evicted or lost leadership" — `tx_status` isn't persisted or replicated,
/// so either looks the same from here.
async fn tx_status(AxState(n): AxState<Shared>, Path(hash_hex): Path<String>, headers: HeaderMap) -> Response {
    {
        let n = n.lock().await;
        if let Err(r) = require_client_auth(&n, &headers, b"") {
            return r;
        }
    }
    let Ok(bytes) = hex::decode(&hash_hex) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"bad hash hex"}))).into_response();
    };
    if bytes.len() != 32 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"hash must be 32 bytes"}))).into_response();
    }
    let hash = H256::from_slice(&bytes);
    {
        let n = n.lock().await;
        if let Some(st) = n.tx_status.get(&hash) {
            return Json(st.json()).into_response();
        }
    }
    match route(&n).await {
        Route::Primary(p, http, identity) => {
            let signed = sign_headers(&identity, b"");
            forward(&http, http.get(format!("{p}/tx/{hash_hex}")).headers(signed)).await.into_response()
        }
        Route::Nobody(_) | Route::Here => Json(serde_json::json!({"status":"unknown"})).into_response(),
    }
}

/// One mesh tick's worth of mempool upkeep — see [`Node::mempool`]. Expired
/// entries are dropped outright: past `MEMPOOL_TTL`, either it landed via
/// some other relay path already (nothing here would know) or it's stuck
/// for a reason retrying won't fix, and the client's own submit-retry
/// (`Cat::submit`, `Client::try_submit`) will queue it again if it still
/// matters. Everything else is re-routed fresh: forwarded to the primary if
/// a route exists now, applied directly if this node *is* the primary now,
/// or relayed to every peer this node's own config can reach — a peer that
/// already knows the hash just no-ops (`mempool_insert`), so re-relaying a
/// stuck entry every tick costs an HTTP round trip, not correctness.
const MEMPOOL_TTL_MS: u64 = 15 * 60 * 1000;

pub async fn mempool_round(shared: &Shared) {
    let (entries, peers, http, identity) = {
        let mut n = shared.lock().await;
        let now = unix_ms();
        n.mempool.retain(|_, e| now.saturating_sub(e.inserted_at) < MEMPOOL_TTL_MS);
        if n.mempool.is_empty() {
            return;
        }
        let entries: Vec<(H256, Vec<u8>, bool)> = n.mempool.iter().map(|(h, e)| (*h, e.bytes.clone(), e.carried)).collect();
        (entries, n.mesh.routes().to_vec(), n.http.clone(), n.identity)
    };
    for (hash, bytes, carried) in entries {
        let route = route(shared).await;
        // Someone else is delivering it. With a route of our own there is
        // nothing left for us to do (`/tx` routes the question from here);
        // without one we still ask reachable peers whether it sealed, below,
        // but never send it again.
        if carried && !matches!(route, Route::Nobody(_)) {
            shared.lock().await.mempool.remove(&hash);
            continue;
        }
        match route {
            Route::Here => {
                let Ok(uxt) = UncheckedExtrinsic::decode(&mut &bytes[..]) else {
                    shared.lock().await.mempool.remove(&hash); // can't apply what we can't decode
                    continue;
                };
                let mut n = shared.lock().await;
                let height = n.block;
                if n.submit(uxt).is_ok() {
                    n.set_tx_status(hash, TxState::Applied { height });
                }
                n.mempool.remove(&hash);
            }
            Route::Primary(p, http, _identity) => {
                if let Ok(r) = http.post(format!("{p}/submit")).body(bytes).send().await {
                    if r.status().is_success() {
                        shared.lock().await.mempool.remove(&hash);
                    }
                }
            }
            // No route to hand the write to — but sealing is a fact about
            // the *hash*, not about this node, and a reachable peer that
            // itself has (or can forward to) a route can answer that
            // whether or not this node ever gets one. Ask before relaying
            // again: a block carries only effects, not extrinsic hashes
            // (`seal_body`), so nothing about a pushed/pulled block would
            // otherwise tell this node its own queued write already landed
            // by some other path.
            Route::Nobody(_) => {
                let hash_hex = hex::encode(hash.as_bytes());
                let mut sealed = false;
                for peer in &peers {
                    let signed = sign_headers(&identity, b"");
                    let Ok(r) = http.get(format!("{peer}/tx/{hash_hex}")).headers(signed).send().await else { continue };
                    let Ok(v) = r.json::<serde_json::Value>().await else { continue };
                    match v.get("status").and_then(|s| s.as_str()) {
                        Some("sealed") => {
                            let height = v.get("height").and_then(|h| h.as_u64()).unwrap_or(0);
                            let mut n = shared.lock().await;
                            n.set_tx_status(hash, TxState::Sealed { height });
                            n.mempool.remove(&hash);
                            sealed = true;
                            break;
                        }
                        Some("applied") => {
                            let height = v.get("height").and_then(|h| h.as_u64()).unwrap_or(0);
                            shared.lock().await.set_tx_status(hash, TxState::Applied { height });
                        }
                        _ => {}
                    }
                }
                if sealed {
                    continue;
                }
                // Known applied (just not sealed yet) somewhere reachable —
                // no point re-submitting the same bytes again this tick,
                // only re-checking next tick.
                let already_applied = matches!(shared.lock().await.tx_status.get(&hash), Some(TxState::Applied { .. }));
                if !already_applied && !carried {
                    for peer in &peers {
                        let _ = http.post(format!("{peer}/mempool/relay")).body(bytes.clone()).send().await;
                    }
                }
            }
        }
    }
}

/// A `Tally` as the wire JSON `/artifact/{id}` and `/note/{id}` carry:
/// accounts as hex; clients resolve names through the roster.
fn tally_json(v: &pallet_litter::Tally<AccountId>) -> serde_json::Value {
    let hexes = |vs: &Vec<AccountId>| vs.iter().map(miot_keys::to_hex).collect::<Vec<_>>();
    serde_json::json!({"up": hexes(&v.up), "down": hexes(&v.down)})
}

/// An artifact's comment thread, as wire JSON — who/when/epoch/body, names
/// resolved by the client. The whole thread comes back in one read (the
/// epoch is stamped on each comment, not used as a key); `epoch` filters
/// the view to one session, default is everything.
fn comments_json(n: &mut Node, a: miot_primitives::ArtifactId, epoch: Option<u32>) -> serde_json::Value {
    serde_json::Value::Array(
        n.ext
            .execute_with(|| Litter::comments(a))
            .into_iter()
            .filter(|c| epoch.is_none_or(|e| c.epoch == e))
            .map(|c| serde_json::json!({"who": miot_keys::to_hex(&c.who), "at": c.at, "epoch": c.epoch, "body": c.body}))
            .collect(),
    )
}

async fn artifact(AxState(n): AxState<Shared>, Path(id): Path<String>, Query(query): Query<std::collections::HashMap<String, String>>, headers: HeaderMap) -> Response {
    let mut n = n.lock().await;
    if let Err(r) = require_client_auth(&n, &headers, b"") {
        return r;
    }
    let task = parse_task(&id).map(miot_primitives::ArtifactId::Task);
    let epoch: Option<u32> = query.get("epoch").and_then(|e| e.parse().ok());
    let cm = task.as_ref().map(|aid| comments_json(&mut n, *aid, epoch));
    let a = task.and_then(|aid| {
        let miot_primitives::ArtifactId::Task(t) = aid else { unreachable!() };
        n.ext.execute_with(|| {
            let a = Litter::artifact(t)?;
            let v = Litter::tally(aid);
            Some((a, v))
        })
    });
    Json(match (a, cm) {
        (Some((a, v)), Some(cm)) => serde_json::json!({"found":true,"title":a.title,"body":a.body,"author":miot_keys::to_hex(&a.author),"votes":tally_json(&v),"comments":cm}),
        _ => serde_json::json!({"found":false}),
    })
    .into_response()
}

/// A standalone artifact — [`Effect::StandaloneArtifact`], no task behind it.
/// `id` is its own counter, never a `TaskId`, so this is a separate route
/// from `/artifact`.
async fn standalone_artifact(AxState(n): AxState<Shared>, Path(id): Path<String>, Query(query): Query<std::collections::HashMap<String, String>>, headers: HeaderMap) -> Response {
    let mut n = n.lock().await;
    if let Err(r) = require_client_auth(&n, &headers, b"") {
        return r;
    }
    let note = id.parse::<u32>().ok().map(miot_primitives::ArtifactId::Note);
    let epoch: Option<u32> = query.get("epoch").and_then(|e| e.parse().ok());
    let cm = note.as_ref().map(|aid| comments_json(&mut n, *aid, epoch));
    let a = note.and_then(|aid| {
        let miot_primitives::ArtifactId::Note(id) = aid else { unreachable!() };
        n.ext.execute_with(|| {
            let a = Litter::standalone_artifact(id)?;
            let v = Litter::tally(aid);
            Some((a, v))
        })
    });
    Json(match (a, cm) {
        (Some((a, v)), Some(cm)) => serde_json::json!({"found":true,"title":a.title,"body":a.body,"author":miot_keys::to_hex(&a.author),"votes":tally_json(&v),"comments":cm}),
        _ => serde_json::json!({"found":false}),
    })
    .into_response()
}

/// Every standalone artifact, oldest first — title and author only; `GET
/// /note/{id}` has the body.
async fn standalone_artifacts(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let mut n = n.lock().await;
    if let Err(r) = require_client_auth(&n, &headers, b"") {
        return r;
    }
    let rows = n.ext.execute_with(|| {
        Litter::standalone_artifacts()
            .iter()
            .map(|(id, a)| serde_json::json!({"id":id,"title":a.title,"author":miot_keys::to_hex(&a.author),"at":a.at}))
            .collect::<Vec<_>>()
    });
    Json(rows).into_response()
}

/// Every artifact reachable by id, task-closed and standalone alike, merged
/// into one list — asked for live, 2026-09-23: "task artifacts should be
/// accessible all the same by id since they are on chain in session." Task
/// ids are rendered `t<parent>` (already how `/artifact/{id}` addresses
/// them); standalone ids are their own bare counter, so the two spaces never
/// collide as long as callers keep the `t` prefix on the task ones —
/// `ArtifactRead` in `agent.rs` relies on exactly that to route a read to
/// `/artifact/{id}` or `/note/{id}`.
async fn all_artifacts(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let mut n = n.lock().await;
    if let Err(r) = require_client_auth(&n, &headers, b"") {
        return r;
    }
    let rows = n.ext.execute_with(|| {
        let mut rows: Vec<serde_json::Value> = Litter::artifacts()
            .iter()
            .map(|(task, a)| serde_json::json!({"id":task.to_string(),"kind":"task","title":a.title,"author":miot_keys::to_hex(&a.author),"at":a.at}))
            .collect();
        rows.extend(Litter::standalone_artifacts().iter().map(|(id, a)| {
            serde_json::json!({"id":id.to_string(),"kind":"standalone","title":a.title,"author":miot_keys::to_hex(&a.author),"at":a.at})
        }));
        rows
    });
    Json(rows).into_response()
}

/// Every cat's latest self-reported work stats — `report_stats`, no
/// authority check beyond `ensure_signed` (a cat can only overwrite its
/// own row), so this is a straight dump of whatever every account most
/// recently reported about itself.
async fn all_stats(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let mut n = n.lock().await;
    if let Err(r) = require_client_auth(&n, &headers, b"") {
        return r;
    }
    let rows = n.ext.execute_with(|| {
        Litter::all_stats()
            .iter()
            .map(|(who, s)| {
                let mut row = serde_json::json!({"account":miot_keys::to_hex(who),"turns":s.turns,"tool_calls":s.tool_calls,"tokens":s.tokens,"ms":s.ms});
                if let Some(m) = Litter::messages_sent(who) {
                    row["messages"] = m.into();
                }
                row
            })
            .collect::<Vec<_>>()
    });
    Json(rows).into_response()
}

#[cfg(test)]
mod seal_tests {
    use super::*;

    fn said(body: &str) -> Effect<AccountId> {
        let a = AccountId::new([7u8; 32]);
        Effect::Said { from: a, to: None, body: body.into(), from_root: false, no_ack: false, off_record: false }
    }

    #[test]
    fn a_sealed_body_carries_its_time() {
        let fx = vec![said("hi"), said("there")];
        let (back, at) = open_body(&seal_body(&fx, 1_790_000_000_123)).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(at, Some(1_790_000_000_123));
    }

    /// Every block sealed before this change: effects and nothing after.
    #[test]
    fn a_body_from_before_seal_times_reads_as_none() {
        let fx = vec![said("old")];
        let (back, at) = open_body(&fx.encode()).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(at, None);
    }

    /// The compatibility claim itself: a node on an older build reads a
    /// body with plain `Decode::decode` into `Vec<Effect>`, and must still
    /// get exactly the effects out of a new, time-carrying body.
    #[test]
    fn an_old_decoder_still_reads_a_new_body() {
        let fx = vec![said("new"), said("block")];
        let body = seal_body(&fx, 42);
        let old: Vec<Effect<AccountId>> = Decode::decode(&mut &body[..]).unwrap();
        assert_eq!(old, fx);
    }

    #[test]
    fn a_quiet_block_seals_too() {
        let (back, at) = open_body(&seal_body(&Vec::new(), 9)).unwrap();
        assert!(back.is_empty());
        assert_eq!(at, Some(9));
    }
}
