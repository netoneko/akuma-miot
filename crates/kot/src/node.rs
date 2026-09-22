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
//! | | |
//! |---|---|
//! | `POST /submit` | a signed extrinsic; checked, dispatched into the *currently open* block (forwarded to the primary from a replica) |
//! | `GET /meta` | genesis hash + spec/tx version |
//! | `GET /account/{id}` | that account's nonce (the primary's, from a replica) |
//! | `GET /events?since=N` | everything the chain emitted after cursor `N` |
//! | `GET /head` | height, litter leader, closed |
//! | `GET /artifact/{id}` | a closed parent's report |
//! | `GET /tasks` | one row per live task |
//! | `GET /chain/{head,blocks,checkpoint}` | the block log, for replicas |
//! | `GET /mesh/status`, `POST /mesh/vote` | election (`miot-mesh`) |
//! | `GET /mesh/peers` | what this node sees of the mesh — `kot peers` |
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

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State as AxState};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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

use crate::common::parse_task;

/// Everything a node needs to start. Every node in one mesh must agree on
/// `root`, `leader` and `members` — they are genesis.
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
    /// Accounts given [`catnip`] at genesis. Root and leader are always added.
    pub members: Vec<AccountId>,
    /// How long a block takes. See [`BLOCK_MS`].
    pub block_ms: u64,
    /// How often a replica pulls the primary.
    pub sync_ms: u64,
    /// How often every node polls every peer's `/mesh/status`.
    pub poll_ms: u64,
    pub timing: Timing,
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
}

const LOG_CAP: usize = 4096;
/// The mesh election's persisted state, in the store's aux space.
const AUX_MESH: &str = "mesh";

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
    members: Vec<AccountId>,
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
    http: reqwest::Client,
    started: Instant,
}

fn render(e: &Effect<AccountId>) -> serde_json::Value {
    use miot_keys::to_hex;
    use serde_json::json;
    match e {
        Effect::Said { from, to, body, from_root } => {
            json!({"t":"said","from":to_hex(from),"to":to.as_ref().map(to_hex),"body":body,"root":from_root})
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
fn genesis(root: &AccountId, leader: &AccountId, members: &[AccountId], replaying: bool) -> (sp_io::TestExternalities, H256) {
    use sp_runtime::BuildStorage;
    let mut t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
    pallet_litter::GenesisConfig::<Runtime> { root: Some(root.clone()), leader: Some(leader.clone()) }
        .assimilate_storage(&mut t)
        .unwrap();
    let mut ext: sp_io::TestExternalities = t.into();
    // Block 1's parent is "genesis" by definition — what `CheckGenesis`
    // binds a signature to, and what `/meta` reports.
    let genesis_hash = H256::zero();
    let first = Header::new(1, Default::default(), Default::default(), genesis_hash, Default::default());
    let mut all = members.to_vec();
    for a in [root, leader] {
        if !all.contains(a) {
            all.push(a.clone());
        }
    }
    ext.execute_with(|| {
        pallet_litter::Pallet::<Runtime>::set_replaying(replaying);
        Executive::initialize_block(&first);
        for m in &all {
            catnip(m);
        }
    });
    (ext, genesis_hash)
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
        let store = miot_store::Store::open(&cfg.db).map_err(|e| format!("open store at {:?}: {e}", cfg.db))?;
        let hard: Hard = match store.aux(AUX_MESH).map_err(|e| e.to_string())? {
            Some(b) => serde_json::from_slice(&b).map_err(|e| format!("corrupt mesh state: {e}"))?,
            None => Hard::default(),
        };
        let seed = cfg.name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
        let mesh = Mesh::new(cfg.name.clone(), cfg.peers.clone(), cfg.timing, hard, 0, seed);
        let (ext, genesis_hash) = genesis(&cfg.root, &cfg.leader, &cfg.members, true);
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
            members: cfg.members.clone(),
            identity: cfg.identity,
            mesh,
            producing: false,
            peer: None,
            needs_reconcile: false,
            // Prior knowledge, not negotiated: these are plain `http://`
            // routes (no TLS/ALPN to negotiate over), and mesh-internal
            // traffic is a tight poll loop between the same peers over and
            // over, so one multiplexed connection beats a fresh handshake
            // per call. axum's server side matches via `http2` feature on
            // `hyper-util`'s `auto::Builder` (sniffs the h2c preface).
            http: reqwest::Client::builder().timeout(Duration::from_secs(10)).http2_prior_knowledge().build().unwrap(),
            started: Instant::now(),
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

    pub fn head(&self) -> u64 {
        self.store.head()
    }

    pub fn mesh(&self) -> &Mesh {
        &self.mesh
    }

    pub fn is_producing(&self) -> bool {
        self.producing
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
        let mut v = self.members.clone();
        for a in [&self.genesis_root, &self.genesis_leader] {
            if !v.contains(a) {
                v.push(a.clone());
            }
        }
        v
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
            let wakes = if e.wakes() { e.to().map(miot_keys::to_hex) } else { None };
            let entry = Entry { seq: self.seq, block: self.block, effect: render(&e), wakes };
            if self.log.len() >= LOG_CAP {
                self.log.pop_front();
            }
            self.log.push_back(entry);
            self.pending.push(e);
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

    /// Close the open block and open the next — the primary's block loop.
    fn advance(&mut self) {
        let closing = self.block;
        let header = self.ext.execute_with(Executive::finalize_block);
        self.persist(closing);
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
        let body = self.pending.encode();
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
    fn apply_block(&mut self, height: u64, effects: Vec<Effect<AccountId>>) {
        self.ext.execute_with(|| {
            let now: miot_primitives::BlockNumber = frame_system::Pallet::<Runtime>::block_number().unique_saturated_into();
            for e in &effects {
                pallet_litter::Pallet::<Runtime>::replay_effect(e, now);
            }
        });
        let header = self.ext.execute_with(Executive::finalize_block);
        self.parent_hash = header.hash();
        self.absorb(effects);
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
            let effects: Vec<Effect<AccountId>> = Decode::decode(&mut &body[..]).expect("corrupt block body in store");
            self.apply_block(h, effects);
        }
    }

    /// Throw away in-memory state and rebuild it from the store alone — on
    /// demotion (the open block's effects were never persisted) and after
    /// any change the store made underneath us (rewind, adopted checkpoint).
    fn reload_from_store(&mut self) {
        let (ext, genesis_hash) = genesis(&self.genesis_root, &self.genesis_leader, &self.members, !self.producing);
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
        let is_clear = matches!(uxt.function, miot_runtime::RuntimeCall::Litter(pallet_litter::Call::clear_all {}));
        let r = self.ext.execute_with(|| Executive::apply_extrinsic(uxt));
        let fx = self.drain();
        self.absorb(fx);
        match r {
            Ok(Ok(())) => {
                if is_clear {
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
        let route = self.mesh.leader_route().map(str::to_string);
        if route != self.peer {
            if let Some(r) = &route {
                println!(
                    "[mesh] following {} at {r} (term {})",
                    self.mesh.leader().unwrap_or("?"),
                    self.mesh.term()
                );
                self.needs_reconcile = true;
            }
            self.peer = route;
        }
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
    let routes = shared.lock().await.mesh.routes().to_vec();
    let auth = PeerAuth::snapshot(shared).await;
    // Longer than the poll interval on purpose: a slow answer is still an
    // answer. Only a peer that misses every poll for a whole election
    // window counts as gone.
    let timeout = Duration::from_millis((poll_ms * 2).max(2_000));
    let mut set = tokio::task::JoinSet::new();
    for r in routes {
        let auth = auth.clone();
        set.spawn(async move {
            let st = async {
                let headers = sign_headers(&auth.identity, b"");
                let resp = auth.http.get(format!("{r}/mesh/status")).headers(headers).timeout(timeout).send().await.ok()?;
                let resp_headers = resp.headers().clone();
                let bytes = resp.bytes().await.ok()?;
                verify_headers(&resp_headers, &bytes, |a| auth.is_trusted(a)).ok()?;
                serde_json::from_slice::<Status>(&bytes).ok()
            }
            .await;
            (r, st)
        });
    }
    let mut got = Vec::new();
    while let Some(Ok(x)) = set.join_next().await {
        got.push(x);
    }

    let req = {
        let mut n = shared.lock().await;
        let now = n.now_ms();
        for (r, st) in got {
            if let Some(st) = st {
                n.mesh.on_status(&r, st, now);
            }
        }
        let head = n.store.head();
        let req = n.mesh.tick(now, head);
        n.follow_mesh();
        req
    };
    if let Some(req) = req {
        campaign(shared, req, timeout).await;
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
            let Ok(body) = hex::decode(&row.body_hex) else {
                eprintln!("[node] sync: peer sent bad hex for block {}", row.height);
                return;
            };
            if let Err(e) = n.store.append(row.height, &body) {
                eprintln!("[node] sync: append failed at block {}: {e}", row.height);
                return;
            }
            let effects: Vec<Effect<AccountId>> = Decode::decode(&mut &body[..]).expect("corrupt block body from peer");
            n.apply_block(row.height, effects);
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

// ------------------------------------------------------------- mesh auth
//
// Signs and verifies mesh-internal HTTP traffic (election, chain sync) —
// not client-facing endpoints like `/tasks` or `/account`, which stay open
// to any `kot` client per `CLAUDE.md`. Distinct from `/submit`'s signed
// `UncheckedExtrinsic`: that authorizes a *state change*; this authenticates
// *who a mesh peer is* on the wire, which nothing checked before. Full
// write-up: `docs/MESH_AUTH.md`.
//
// The signature covers exactly the bytes sent — the raw request/response
// body, or the raw query string for a parameterless GET — never a
// re-serialized value, so there's no question of canonical JSON.

const SIG_HEADER_SIGNER: &str = "x-miot-signer";
const SIG_HEADER_SIG: &str = "x-miot-sig";

/// `pub` only so `tests/election.rs` can call a mesh-internal endpoint
/// directly to inspect a node's raw log, the same way a real peer would.
/// Not part of the client surface: `kot`'s own client never touches
/// `/mesh/*` or `/chain/*`.
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

// ---------------------------------------------------------------- HTTP

pub fn router(shared: Shared) -> Router {
    Router::new()
        .route("/head", get(head))
        .route("/events", get(events))
        .route("/submit", post(submit))
        .route("/meta", get(meta))
        .route("/account/{id}", get(account))
        .route("/artifact/{id}", get(artifact))
        .route("/tasks", get(tasks))
        .route("/chain/head", get(chain_head))
        .route("/chain/blocks", get(chain_blocks))
        .route("/chain/checkpoint", get(chain_checkpoint))
        .route("/mesh/status", get(mesh_status))
        .route("/mesh/vote", post(mesh_vote))
        .route("/mesh/peers", get(mesh_peers))
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
        "[node] {} on {}:{}  root={}  leader={}  peers={}  block={}ms",
        cfg.name,
        cfg.bind,
        cfg.port,
        miot_keys::short(&cfg.root),
        miot_keys::short(&cfg.leader),
        if cfg.peers.is_empty() { "none (a mesh of one)".to_string() } else { cfg.peers.join(",") },
        cfg.block_ms,
    );
    let shared: Shared = Arc::new(Mutex::new(node));
    let listener = tokio::net::TcpListener::bind((cfg.bind.as_str(), cfg.port)).await.map_err(|e| format!("bind {}:{}: {e}", cfg.bind, cfg.port))?;
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
    if let Err(why) = verify_headers(&headers, b"", |a| n.is_trusted_signer(a)) {
        return unauthorized(why);
    }
    let status = n.mesh.status(n.store.head());
    signed_json(&n.identity, StatusCode::OK, &status)
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

/// `kot peers`: this node, and every peer as last seen from here.
async fn mesh_peers(AxState(n): AxState<Shared>) -> Json<serde_json::Value> {
    let n = n.lock().await;
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
    Json(serde_json::json!({
        "me": n.mesh.status(n.store.head()),
        "quorum": n.mesh.quorum(),
        "producing": n.producing,
        "following": n.peer,
        "last_checkpoint": n.store.last_checkpoint(),
        "peers": peers,
    }))
}

async fn chain_head(AxState(n): AxState<Shared>, headers: HeaderMap) -> Response {
    let n = n.lock().await;
    if let Err(why) = verify_headers(&headers, b"", |a| n.is_trusted_signer(a)) {
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
    if let Err(why) = verify_headers(&headers, uri.query().unwrap_or("").as_bytes(), |a| n.is_trusted_signer(a)) {
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
    if let Err(why) = verify_headers(&headers, b"", |a| n.is_trusted_signer(a)) {
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

async fn head(AxState(n): AxState<Shared>) -> Json<serde_json::Value> {
    let mut n = n.lock().await;
    let (block, seq) = (n.block, n.seq);
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

async fn tasks(AxState(n): AxState<Shared>) -> Json<Vec<serde_json::Value>> {
    let mut n = n.lock().await;
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
                })
            })
            .collect::<Vec<_>>()
    });
    Json(rows)
}

async fn meta(AxState(n): AxState<Shared>) -> Json<serde_json::Value> {
    let mut n = n.lock().await;
    let genesis_hash = n.ext.execute_with(|| System::block_hash(0u64));
    Json(serde_json::json!({
        "genesis_hash": hex::encode(genesis_hash.as_bytes()),
        "spec_version": VERSION.spec_version,
        "tx_version": VERSION.transaction_version,
    }))
}

/// Where a replica sends what only the primary can answer. `None` while
/// nobody leads.
enum Route {
    Here,
    Primary(String, reqwest::Client),
    Nobody,
}

async fn route(n: &Shared) -> Route {
    let n = n.lock().await;
    if n.producing {
        Route::Here
    } else {
        match &n.peer {
            Some(p) => Route::Primary(p.clone(), n.http.clone()),
            None => Route::Nobody,
        }
    }
}

fn no_primary() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"ok":false,"error":"no primary right now (election in progress); retry shortly"})))
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
async fn account(AxState(n): AxState<Shared>, Path(id): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    let Ok(who) = miot_keys::from_hex(&id) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"bad account hex"})));
    };
    match route(&n).await {
        Route::Primary(p, http) => return forward(&http, http.get(format!("{p}/account/{id}"))).await,
        Route::Nobody | Route::Here => {}
    }
    let mut n = n.lock().await;
    let nonce = n.ext.execute_with(|| frame_system::Pallet::<Runtime>::account_nonce(&who));
    (StatusCode::OK, Json(serde_json::json!({"nonce": nonce})))
}

/// Raw SCALE bytes, not JSON — there's no `who` field to trust; the
/// signature over these exact bytes decides the sender. On a replica, the
/// bytes go to the primary unchanged: any node will do (`docs/CLI.md` §5a).
async fn submit(AxState(n): AxState<Shared>, body: Bytes) -> (StatusCode, Json<serde_json::Value>) {
    match route(&n).await {
        Route::Here => {}
        Route::Primary(p, http) => return forward(&http, http.post(format!("{p}/submit")).body(body)).await,
        Route::Nobody => return no_primary(),
    }
    let uxt = match UncheckedExtrinsic::decode(&mut &body[..]) {
        Ok(u) => u,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok":false,"error":format!("bad extrinsic: {e}")}))),
    };
    let mut n = n.lock().await;
    if !n.producing {
        return no_primary(); // demoted between the check above and now
    }
    match n.submit(uxt) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok":true}))),
        // A refusal is the chain's answer, typed — not an error in the cat.
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"ok":false,"error":e}))),
    }
}

async fn artifact(AxState(n): AxState<Shared>, Path(id): Path<String>) -> Json<serde_json::Value> {
    let mut n = n.lock().await;
    let a = parse_task(&id).and_then(|t| n.ext.execute_with(|| Litter::artifact(t)));
    Json(match a {
        Some(a) => serde_json::json!({"found":true,"title":a.title,"body":a.body,"author":miot_keys::to_hex(&a.author)}),
        None => serde_json::json!({"found":false}),
    })
}
