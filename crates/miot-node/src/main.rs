//! The chain, as a process.
//!
//! One writer, many readers. The node owns the state outright — no cat ever
//! touches it — and cats reach it over HTTP, which is the whole difference
//! between this and `miot`'s in-process modes: there, four cats were four integers inside one
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
//! | `GET /tasks` | one row per live task: id, status, assignee, holder, lease |
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
use codec::{Decode, Encode};
use miot_primitives::{Effect, TaskId};
use miot_runtime::{AccountId, Executive, Header, Litter, Runtime, System, UncheckedExtrinsic, VERSION};
use polkadot_sdk::*;
use serde::{Deserialize, Serialize};
use sp_core::H256;
use sp_runtime::traits::Header as HeaderT;
use sp_runtime::traits::UniqueSaturatedInto;
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

/// A node's role in chain replication — HANDOFF item 5. Deliberately not
/// called "leader"/"follower": `pallet-litter`'s `leader` is already the
/// *litter* leader, an agent role (`mimi`, `set_leader`). This is a
/// different axis entirely — which process's block log is canonical — so it
/// gets different words: **primary** (produces blocks, accepts `/submit`)
/// and **replica** (pulls the primary's log over HTTP, read-only).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Primary,
    Replica,
}

fn role_env() -> Role {
    match std::env::var("MIOT_ROLE").as_deref() {
        Ok("replica") => Role::Replica,
        _ => Role::Primary,
    }
}

struct Node {
    ext: sp_io::TestExternalities,
    log: VecDeque<Entry>,
    seq: u64,
    block: u64,
    /// The hash the block *currently open* for extrinsics will chain to when
    /// it closes. Updated only in [`Node::advance`] (primary) or
    /// [`Node::apply_block`] (replica), never both for the same node.
    parent_hash: H256,
    /// The chain's persisted block log — HANDOFF item 2. `None` means
    /// running without persistence (state lost on restart, as this always
    /// did before); `Some` means every block's effects are written to disk
    /// as they close and replayed on the next start. A replica requires
    /// `Some` — see `main`.
    store: Option<miot_store::Store>,
    /// Effects absorbed since the currently-open block began — what
    /// [`Node::advance`] persists as that block's body when it closes.
    /// State is a fold over effects (`docs/PROTOCOL.md`), so this is
    /// literally the same log `apply` already knows how to replay.
    pending: Vec<Effect<AccountId>>,
    /// Primary or replica — HANDOFF item 5. See [`Role`].
    role: Role,
    /// The primary's base URL. `Some` only for a replica.
    peer: Option<String>,
    /// Used for the replica's sync loop; unused (but harmless) on a primary.
    http: reqwest::Client,
    /// Kept so a replica (or `Node::replay`, or `Node::restore_from_snapshot`
    /// via [`genesis`]) can rebuild state from scratch when there's no
    /// compaction checkpoint to restore from instead.
    genesis_root: AccountId,
    genesis_leader: AccountId,
    /// Set when a submitted `clear_all` dispatches successfully; consumed by
    /// the *next* [`Node::advance`] once that block actually closes and
    /// persists — `clear_all`'s effects land in the block currently open,
    /// which `Store::compact` can't target until it closes (`compact`
    /// requires `height <= store.head()`).
    pending_compaction: bool,
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
        Effect::Opened { who, task, text } => {
            json!({"t":"opened","who":to_hex(who),"task":task.to_string(),"text":text})
        }
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
        Effect::Record { who, task, act, text } => {
            json!({"t":"record","who":to_hex(who),"task":task.to_string(),"act":act.as_str(),"text":text})
        }
        Effect::Requeued { task, from, why } => {
            json!({"t":"requeued","task":task.to_string(),"from":from.as_ref().map(to_hex),"why":format!("{why:?}")})
        }
        Effect::NudgeBudgetSpent { holder, task } => {
            json!({"t":"budget_spent","holder":to_hex(holder),"task":task.to_string()})
        }
        Effect::Closed { task, title, body, author } => {
            json!({"t":"closed","task":task.to_string(),"title":title,"body":body,"author":to_hex(author)})
        }
        Effect::Failed { task } => {
            json!({"t":"failed","task":task.to_string()})
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
            self.pending.push(e);
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
        let closing = self.block;
        let header = self.ext.execute_with(Executive::finalize_block);
        self.persist(closing);
        if self.pending_compaction {
            self.compact_at(closing);
            self.pending_compaction = false;
        }
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

    /// Write `height`'s accumulated effects to the store as that block's
    /// body, then clear the buffer. A no-op without persistence configured.
    /// Effects, not raw extrinsics — `TaskTable::apply` folds this straight
    /// back into state on replay, no re-validation needed (see
    /// `docs/PROTOCOL.md`).
    fn persist(&mut self, height: u64) {
        let Some(store) = self.store.as_mut() else { return };
        // Every block gets a row, even a quiet one with no effects at all —
        // the store is append-only and gap-free (`Error::NotContiguous`),
        // so skipping empty blocks isn't an option.
        let body = self.pending.encode();
        if let Err(e) = store.append(height, &body) {
            eprintln!("[node] store append failed at block {height}: {e}");
        }
        self.pending.clear();
    }

    /// Fold one already-decided block's effects into `self.ext`. Assumes
    /// block `height` is the one currently open — either from [`genesis`]
    /// (height 1) or from the previous call's own trailing
    /// `initialize_block` (every height after). Finalizes it, absorbs the
    /// effects into `self.log`/`/events`, clears `self.pending` (these
    /// effects are already decided and, for a replay, already on disk — the
    /// live block loop must not persist them a second time under some later
    /// height), then opens the next block.
    ///
    /// The one place a stored or peer-fetched block gets folded back into
    /// state — [`Node::replay`] (local store, on start) and the replica sync
    /// loop (`sync_once`, over the network) both call this rather than each
    /// having their own copy of the sequence, the same "one place, not two
    /// paths to drift apart" principle `docs/PROTOCOL.md` already applies to
    /// `TaskTable::apply`.
    ///
    /// Folds through `pallet_litter::Pallet::replay_effect` rather than
    /// re-applying the original extrinsics — no signatures, nonces or
    /// mortality to re-check, because none of that touches state; only the
    /// effect does (`docs/PROTOCOL.md`).
    fn apply_block(&mut self, height: u64, effects: Vec<Effect<AccountId>>) {
        self.ext.execute_with(|| {
            let now: miot_primitives::BlockNumber =
                frame_system::Pallet::<Runtime>::block_number().unique_saturated_into();
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
    }

    /// Rebuild state from the store on start. If a compaction checkpoint
    /// exists, restore from it directly (`restore_from_snapshot`) rather
    /// than replaying from genesis — required, not optional, once
    /// `compact_at` ever actually runs: `store.block(h)` returns `None` for
    /// every `h <= last_checkpoint` (compaction deletes them), so looping
    /// from 1 unconditionally would panic on the first restart after any
    /// real compaction. With no checkpoint yet (`last_checkpoint() == 0`,
    /// still true until `/clear` runs at least once), this is exactly
    /// today's behavior: start at block 1, whose `initialize_block` was
    /// already run by [`genesis`].
    fn replay(&mut self, store: &miot_store::Store) {
        let head = store.head();
        let cp = store.last_checkpoint();
        if cp > 0 {
            let blob = store.checkpoint_state().expect("store read").expect("checkpoint recorded, its state must exist");
            self.restore_from_snapshot(cp, &blob);
        }
        let from = cp + 1;
        if from > head {
            return;
        }
        for h in from..=head {
            self.block = h;
            let body = store.block(h).expect("store read").expect("contiguous store above the checkpoint");
            let effects: Vec<Effect<AccountId>> =
                Decode::decode(&mut &body[..]).expect("corrupt block body in store");
            self.apply_block(h, effects);
        }
    }

    /// Take a snapshot of `self.ext` as of the just-finalized block `height`
    /// and record it as a new compaction checkpoint (`Store::compact`) —
    /// the one trigger this project wires up: root's own `/clear`
    /// (`Node::submit` sets `pending_compaction`), a deliberate session
    /// boundary that already sweeps every closed parent immediately
    /// (`clear_all`'s own `gc(now, keep_for: 0)`), so the state this
    /// snapshots is already the "nothing worth keeping below here" state
    /// that boundary is for.
    ///
    /// `into_raw_snapshot`/`from_raw_snapshot` (`sp_io::TestExternalities`)
    /// dump and restore the *entire* storage trie — every pallet, not just
    /// `pallet-litter`'s own value — as raw key/value bytes; this project's
    /// `Header`s never carry a real state root to stay consistent with
    /// (`Header::new` always passes `Default::default()` for it), so the
    /// round-trip only needs to be internally self-consistent, which those
    /// two calls already guarantee. `into_raw_snapshot` *drains* the
    /// externalities it's called on, so a fresh one is rebuilt from the
    /// same raw data immediately, to keep serving live traffic.
    fn compact_at(&mut self, height: u64) {
        let Some(store) = self.store.as_mut() else { return };
        let mut ext = std::mem::replace(&mut self.ext, sp_io::TestExternalities::new_empty());
        // `into_raw_snapshot` drains the *backend* only, not the pending
        // overlay `execute_with` accumulates — without this, the snapshot
        // silently reflects whatever was last committed (genesis, since
        // nothing here ever called this before), not current state. Found
        // live: block production crashed on the very next block with
        // frame_system's own "block number must be strictly increasing"
        // assertion, because the restored ext still thought it was at
        // block 1.
        ext.commit_all().expect("no open storage transactions to conflict with a plain commit");
        let (raw, root) = ext.into_raw_snapshot();
        let version = sp_storage::StateVersion::default();
        self.ext = sp_io::TestExternalities::from_raw_snapshot(raw.clone(), root, version);
        let blob = Snapshot { raw, root, version }.encode();
        match store.compact(height, &blob) {
            Ok(pruned) => println!("[node] compacted at block {height} ({pruned} block(s) pruned)"),
            Err(e) => eprintln!("[node] compact failed at block {height}: {e}"),
        }
    }

    /// Restore `self.ext` directly from a compaction checkpoint instead of
    /// replaying from genesis. Leaves block `height + 1` open, the same
    /// convention [`Node::apply_block`] leaves every block in — a caller
    /// (`replay`, or `reconcile` once a rewind lands above genesis) picks
    /// up from there exactly as if `apply_block(height, ...)` had just run.
    fn restore_from_snapshot(&mut self, height: u64, blob: &[u8]) {
        let Snapshot { raw, root, version } = Decode::decode(&mut &blob[..]).expect("corrupt checkpoint state");
        self.ext = sp_io::TestExternalities::from_raw_snapshot(raw, root, version);
        self.parent_hash = self.ext.execute_with(|| System::block_hash(height));
        self.log.clear();
        self.seq = 0;
        self.pending.clear();
        self.block = height + 1;
        let next = Header::new(self.block, Default::default(), Default::default(), self.parent_hash, Default::default());
        self.ext.execute_with(|| Executive::initialize_block(&next));
    }

    /// Check and dispatch one signed extrinsic into the block that is
    /// currently open. Synchronous, because a cat's own refusal-handling
    /// (`AlreadySubmitted`, `NotYours`, …) depends on an immediate answer —
    /// the same guarantee `/call` used to give, now backed by a real check.
    fn submit(&mut self, uxt: UncheckedExtrinsic) -> Result<(), String> {
        // Checked before `uxt` moves into `apply_extrinsic` below — a
        // successful `clear_all` is the one trigger `compact_at` fires on
        // (`Node::advance`, once this block actually closes).
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

/// One page of raw block bytes, as `/chain/blocks` renders them. Hex, not
/// raw bytes, for the same reason `/meta`'s `genesis_hash` is — JSON has no
/// native byte string.
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

/// The full storage trie, as `Node::compact_at`/`Node::restore_from_snapshot`
/// round-trip it through `sp_io::TestExternalities::into_raw_snapshot`/
/// `from_raw_snapshot`. This *is* `Store::compact`'s opaque `state` blob —
/// `miot-store` never looks inside it, so its shape only has to make sense
/// to `miot-node`.
#[derive(Encode, Decode)]
struct Snapshot {
    raw: Vec<(Vec<u8>, (Vec<u8>, i32))>,
    root: H256,
    version: sp_storage::StateVersion,
}

/// What `GET /chain/checkpoint` serves — the compaction state a replica
/// adopts (`reconcile_if_diverged`/`reconcile`) when its own log doesn't
/// reach as far back as the peer's latest checkpoint. Hex for the same
/// reason `BlockRow.body_hex` is.
#[derive(Serialize, Deserialize)]
struct CheckpointRow {
    height: u64,
    state_hex: String,
}

/// A page is bounded so one replica's catch-up request can't come back as
/// one huge response; `sync_once` just asks again next tick for the rest.
const SYNC_PAGE: u64 = 256;

/// A replica's periodic pull from its peer (HANDOFF item 5): fetches
/// whatever new blocks exist since our own head and folds them in through
/// [`Node::apply_block`], the same step a local-store replay already uses.
///
/// Assumes we're not currently diverged from `peer` — [`reconcile_if_diverged`]
/// is what checks and fixes that, and it only needs to run once, not on
/// every tick (see its doc comment for why).
async fn sync_once(shared: &Shared) {
    let (http, peer) = {
        let n = shared.lock().await;
        (n.http.clone(), n.peer.clone().expect("sync_once only runs for a replica"))
    };

    let head = match http.get(format!("{peer}/chain/head")).send().await {
        Ok(r) => match r.json::<ChainHead>().await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[node] sync: peer sent a bad /chain/head: {e}");
                return;
            }
        },
        Err(e) => {
            eprintln!("[node] sync: peer unreachable: {e}");
            return;
        }
    };

    // Catch up / tail — the common case, every tick once caught up.
    loop {
        let from = {
            let n = shared.lock().await;
            n.store.as_ref().expect("replica always has a store").head() + 1
        };
        if from > head.head {
            break;
        }
        let rows = fetch_blocks(&http, &peer, from, SYNC_PAGE).await;
        if rows.is_empty() {
            break;
        }
        let mut n = shared.lock().await;
        for row in &rows {
            let Ok(body) = hex::decode(&row.body_hex) else {
                eprintln!("[node] sync: peer sent bad hex for block {}", row.height);
                return;
            };
            if let Some(store) = n.store.as_mut() {
                if let Err(e) = store.append(row.height, &body) {
                    eprintln!("[node] sync: append failed at block {}: {e}", row.height);
                    return;
                }
            }
            let effects: Vec<Effect<AccountId>> =
                Decode::decode(&mut &body[..]).expect("corrupt block body from peer");
            n.apply_block(row.height, effects);
        }
    }
}

async fn fetch_blocks(http: &reqwest::Client, peer: &str, from: u64, limit: u64) -> Vec<BlockRow> {
    match http.get(format!("{peer}/chain/blocks?from={from}&limit={limit}")).send().await {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn fetch_checkpoint(http: &reqwest::Client, peer: &str) -> Option<(u64, Vec<u8>)> {
    let row: Option<CheckpointRow> = http.get(format!("{peer}/chain/checkpoint")).send().await.ok()?.json().await.ok()?;
    let row = row?;
    let state = hex::decode(&row.state_hex).ok()?;
    Some((row.height, state))
}

/// Compare our local block range against the peer's, once — at startup,
/// before the periodic [`sync_once`] tail loop begins. A replica only ever
/// appends blocks it received from this peer, so once this has run it
/// cannot diverge from the peer again on its own before the next restart;
/// there is no need to repeat it per tick.
///
/// Two cases, handled separately:
///
/// 1. **The peer has compacted further than we have** (`peer.last_checkpoint
///    > our own`) — including a fresh node with no log at all. There is no
///    way to reach that point by replaying blocks the peer already dropped,
///    so adopt its checkpoint directly (`Store::adopt_checkpoint`) rather
///    than trying to compare byte ranges we can't get.
/// 2. **Otherwise**, compare the range above our own checkpoint (or from
///    block 1, if we have none yet) against the peer's. A "quiet" block —
///    no effects that tick, the common case — encodes as the exact same
///    bytes (an empty `Vec<Effect>`) no matter which chain produced it, so
///    comparing only the tip can find a false "agreement" while a real
///    divergence sits at an earlier, non-quiet height beneath it (observed
///    live standing this up, before compaction existed to bound the range).
async fn reconcile_if_diverged(shared: &Shared) {
    let (http, peer, my_head, my_cp) = {
        let n = shared.lock().await;
        let peer = n.peer.clone().expect("reconcile_if_diverged only runs for a replica");
        let store = n.store.as_ref().expect("replica always has a store");
        (n.http.clone(), peer, store.head(), store.last_checkpoint())
    };

    let peer_head: Option<ChainHead> = match http.get(format!("{peer}/chain/head")).send().await {
        Ok(r) => r.json().await.ok(),
        Err(_) => None,
    };
    let Some(peer_head) = peer_head else {
        eprintln!("[node] reconcile: peer unreachable, will retry next tick");
        return;
    };

    if peer_head.last_checkpoint > my_cp {
        let Some((cp_height, cp_state)) = fetch_checkpoint(&http, &peer).await else {
            eprintln!("[node] reconcile: peer reports a checkpoint but didn't serve one");
            return;
        };
        let mut n = shared.lock().await;
        let store = n.store.as_mut().expect("replica always has a store");
        store.adopt_checkpoint(cp_height, &cp_state).expect("adopt_checkpoint");
        println!("[node] adopted peer's checkpoint at block {cp_height}");
        n.restore_from_snapshot(cp_height, &cp_state);
        return; // sync_once's tail loop catches up from here next.
    }

    if my_head == 0 {
        return; // fresh, and the peer has no checkpoint either — nothing to compare yet.
    }

    let mut theirs = Vec::new();
    let mut from = my_cp + 1;
    while from <= my_head {
        let rows = fetch_blocks(&http, &peer, from, SYNC_PAGE).await;
        if rows.is_empty() {
            break;
        }
        let got = rows.len() as u64;
        for row in &rows {
            match hex::decode(&row.body_hex) {
                Ok(body) => theirs.push(body),
                Err(_) => break,
            }
        }
        from += got;
    }

    let fork = {
        let mut n = shared.lock().await;
        let store = n.store.as_mut().expect("replica always has a store");
        store.fork_point(my_cp + 1, &theirs).expect("fork_point")
    };
    if fork < my_head {
        reconcile(shared, fork).await;
    }
}

/// We diverged from the peer above `fork` (the true last-agreeing height,
/// from [`reconcile_if_diverged`]). Reconcile the honest way: rewind to the
/// latest compaction at or below it (`miot_store::Store::rewind_for_fork`),
/// then restore `self.ext` from that checkpoint (or, if it landed at
/// genesis — no checkpoint at or below the fork — rebuild from `genesis()`
/// instead) and let `sync_once`'s catch-up loop replay everything the peer
/// holds from there.
async fn reconcile(shared: &Shared, fork: u64) {
    let mut n = shared.lock().await;
    let store = n.store.as_mut().expect("replica always has a store");
    let rewind = store.rewind_for_fork(fork).expect("rewind_for_fork");
    println!(
        "[node] sync: diverged from peer above block {fork}, rewound to {} (dropped {} block(s))",
        rewind.height, rewind.dropped
    );

    if rewind.height > 0 {
        let state = rewind.state.clone().expect("a rewind above genesis always carries checkpoint state");
        n.restore_from_snapshot(rewind.height, &state);
    } else {
        let (genesis_root, genesis_leader) = (n.genesis_root.clone(), n.genesis_leader.clone());
        let (ext, genesis_hash) = genesis(genesis_root, genesis_leader);
        n.ext = ext;
        n.parent_hash = genesis_hash;
        n.block = 1;
        n.log.clear();
        n.seq = 0;
        n.pending.clear();
    }
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

    // HANDOFF item 5: primary (default, today's only behavior) or replica.
    // A replica pulls its peer's block log over HTTP instead of ticking its
    // own clock — see `sync_once`.
    let role = role_env();
    let peer = std::env::var("MIOT_PEER").ok();
    if role == Role::Replica && peer.is_none() {
        eprintln!("[node] MIOT_ROLE=replica requires MIOT_PEER");
        std::process::exit(1);
    }

    // `MIOT_DB` unset or unopenable → run exactly as this always did, state
    // in memory only. Set it to persist across restarts — HANDOFF item 2.
    // A replica has no in-memory-only mode: without a durable log it has
    // nothing to compare against a peer's blocks, so it always rebuilds from
    // genesis on every restart instead of resuming — defeating the point.
    let db_path = std::env::var("MIOT_DB").unwrap_or_else(|_| "miot-node.db".to_string());
    let store = match miot_store::Store::open(&db_path) {
        Ok(s) => Some(s),
        Err(e) if role == Role::Replica => {
            eprintln!("[node] a replica requires persistence — could not open store at {db_path:?}: {e}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("[node] persistence disabled — could not open store at {db_path:?}: {e}");
            None
        }
    };

    let (ext, genesis_hash) = genesis(root.clone(), leader.clone());
    let mut node = Node {
        ext,
        log: VecDeque::new(),
        seq: 0,
        block: 1,
        parent_hash: genesis_hash,
        store: None,
        pending: Vec::new(),
        role,
        peer: peer.clone(),
        http: reqwest::Client::new(),
        genesis_root: root.clone(),
        genesis_leader: leader.clone(),
        pending_compaction: false,
    };
    if let Some(store) = store {
        if !store.is_empty() {
            let cp = store.last_checkpoint();
            if cp > 0 {
                println!(
                    "[node] restoring from checkpoint at block {cp}, then replaying {} block(s) from {db_path}",
                    store.head() - cp
                );
            } else {
                println!("[node] replaying {} block(s) from {db_path}", store.head());
            }
            node.replay(&store);
        }
        node.store = Some(store);
    }
    let node: Shared = Arc::new(Mutex::new(node));

    // A replica reconciles against its peer, then catches up, before
    // serving its first request — the same way a primary's local-store
    // replay above runs synchronously before this point — so `/tasks` etc.
    // are never seen empty (or, worse, diverged) just because the sync loop
    // hasn't ticked yet.
    if role == Role::Replica {
        reconcile_if_diverged(&node).await;
        sync_once(&node).await;
    }

    // Primary: the block loop. Its own task, its own clock, and nothing in
    // it waits for a cat — that is the whole of Law I.
    // Replica: the sync loop instead — pulls the peer, never produces a
    // block of its own.
    match role {
        Role::Primary => {
            let node = node.clone();
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(std::time::Duration::from_millis(BLOCK_MS));
                loop {
                    iv.tick().await;
                    node.lock().await.advance();
                }
            });
        }
        Role::Replica => {
            let sync_ms: u64 = std::env::var("MIOT_SYNC_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(BLOCK_MS);
            let node = node.clone();
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(std::time::Duration::from_millis(sync_ms));
                loop {
                    iv.tick().await;
                    sync_once(&node).await;
                }
            });
        }
    }

    let app = Router::new()
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
        .with_state(node);

    let addr = format!("0.0.0.0:{port}");
    let role_str = match role {
        Role::Primary => "primary",
        Role::Replica => "replica",
    };
    println!(
        "[node] chain on {addr}  root={}  leader={}  role={role_str}{}  block={BLOCK_MS}ms",
        miot_keys::short(&root),
        miot_keys::short(&leader),
        peer.map(|p| format!("  peer={p}")).unwrap_or_default(),
    );
    let l = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(l, app).await.unwrap();
}

/// Peer-facing: what a replica's sync loop reads (`sync_once`). Deliberately
/// under `/chain/*`, never `/head` — `/head`'s `leader` field is the
/// *litter* leader, an unrelated concept, and conflating the two names would
/// be exactly the trap `Role`'s doc comment warns about.
async fn chain_head(AxState(n): AxState<Shared>) -> Json<ChainHead> {
    let n = n.lock().await;
    let (head, last_checkpoint) = match &n.store {
        Some(s) => (s.head(), s.last_checkpoint()),
        None => (0, 0),
    };
    Json(ChainHead { head, last_checkpoint })
}

/// Raw stored block bytes, by height — what a replica folds through
/// [`Node::apply_block`] and what `fork_point`/`rewind_for_fork` compare
/// against. Reads straight off `self.store`; a node with no store (never a
/// replica — see `main` — only a primary run without `MIOT_DB`) has nothing
/// to serve here.
async fn chain_blocks(AxState(n): AxState<Shared>, Query(q): Query<BlocksQuery>) -> Json<Vec<BlockRow>> {
    let n = n.lock().await;
    let Some(store) = n.store.as_ref() else {
        return Json(Vec::new());
    };
    let head = store.head();
    let limit = q.limit.unwrap_or(SYNC_PAGE).min(SYNC_PAGE);
    let mut out = Vec::new();
    let mut h = q.from;
    while h <= head && (out.len() as u64) < limit {
        if let Some(body) = store.block(h).expect("store read") {
            out.push(BlockRow { height: h, body_hex: hex::encode(body) });
        }
        h += 1;
    }
    Json(out)
}

/// The peer's current compaction checkpoint, if it has one — what a
/// replica adopts when its own log doesn't reach back as far as the peer's
/// `last_checkpoint` (see `reconcile_if_diverged`).
async fn chain_checkpoint(AxState(n): AxState<Shared>) -> Json<Option<CheckpointRow>> {
    let n = n.lock().await;
    let Some(store) = n.store.as_ref() else {
        return Json(None);
    };
    let cp = store.last_checkpoint();
    if cp == 0 {
        return Json(None);
    }
    let state = store.checkpoint_state().expect("store read").expect("checkpoint recorded, its state must exist");
    Json(Some(CheckpointRow { height: cp, state_hex: hex::encode(state) }))
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

/// One row per live task — `docs/CLI.md` §5's `/tasks`: "id, status,
/// assignee, lease." Reads `Litter::table()` fresh off storage rather than
/// reconstructing state from the event log, since the pallet already is the
/// authoritative view and a client has no business re-deriving it.
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
    if n.role == Role::Replica {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok":false,"error":"read-only replica; submit to the primary"})),
        );
    }
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
