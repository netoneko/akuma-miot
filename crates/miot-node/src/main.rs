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
//! | `POST /call` | submit an act; the node applies it into the next block |
//! | `GET /events?since=N` | everything the chain emitted after cursor `N` |
//! | `GET /head` | height, leader, and whether the parent is closed |
//! | `GET /artifact/:id` | a closed parent's report, out of chain state |
//!
//! # What is still missing, stated plainly
//!
//! Calls arrive as JSON naming an account, not as signed extrinsics — so this
//! node trusts `who`. That is the exact thing the whole project exists to fix,
//! and the fix is `UncheckedExtrinsic` over this same endpoint. The wire moves
//! first; the signature goes on it next.

use std::collections::VecDeque;
use std::sync::Arc;

use axum::extract::{Path, Query, State as AxState};
use axum::routing::{get, post};
use axum::{Json, Router};
use miot_primitives::{Act, Effect, PlanItem, TaskId};
use miot_runtime::{Litter, Runtime, RuntimeOrigin, System};
use polkadot_sdk::*;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use frame_support::traits::OnInitialize;

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

/// One act, as it arrives on the wire.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Call {
    Open { who: u64, text: String },
    Plan { who: u64, task: String, assignments: Vec<Assignment> },
    Update { who: u64, task: String, act: String, text: String },
    Reassign { who: u64, task: String, to: u64 },
    Say { who: u64, to: Option<u64>, body: String },
}

#[derive(Debug, Deserialize)]
struct Assignment {
    who: u64,
    what: String,
}

/// An effect plus the cursor position it sits at, so a cat can resume.
#[derive(Serialize, Clone)]
struct Entry {
    seq: u64,
    block: u64,
    /// Rendered rather than raw: the cat needs to act on this, and a JSON
    /// rendering of `Effect` is what it reads.
    effect: serde_json::Value,
    /// Who must take a turn because of it. `null` means nobody — the waking
    /// rule is decided here, by the protocol, not by each cat.
    wakes: Option<u64>,
}

struct Node {
    ext: sp_io::TestExternalities,
    log: VecDeque<Entry>,
    seq: u64,
    block: u64,
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

fn render(e: &Effect<u64>) -> serde_json::Value {
    use serde_json::json;
    match e {
        Effect::Said { from, to, body, from_root } => {
            json!({"t":"said","from":from,"to":to,"body":body,"root":from_root})
        }
        Effect::Opened { who, task } => json!({"t":"opened","who":who,"task":task.to_string()}),
        Effect::Planned { who, task, count } => {
            json!({"t":"planned","who":who,"task":task.to_string(),"count":count})
        }
        Effect::Assigned { to, task, what, expect } => {
            json!({"t":"assigned","to":to,"task":task.to_string(),"what":what,"expect":expect})
        }
        Effect::Directed { to, task, directive } => {
            json!({"t":"directed","to":to,"task":task.to_string(),"directive":format!("{directive:?}")})
        }
        Effect::Nudge { to, task, remaining, last } => {
            json!({"t":"nudge","to":to,"task":task.to_string(),"remaining":remaining,"last":last})
        }
        Effect::Record { who, task, act } => {
            json!({"t":"record","who":who,"task":task.to_string(),"act":act.as_str()})
        }
        Effect::Requeued { task, from, why } => {
            json!({"t":"requeued","task":task.to_string(),"from":from,"why":format!("{why:?}")})
        }
        Effect::NudgeBudgetSpent { holder, task } => {
            json!({"t":"budget_spent","holder":holder,"task":task.to_string()})
        }
        Effect::Closed { task, title } => {
            json!({"t":"closed","task":task.to_string(),"title":title})
        }
        Effect::Rehomed { task, from, to } => {
            json!({"t":"rehomed","task":task.to_string(),"from":from,"to":to})
        }
    }
}

impl Node {
    fn absorb(&mut self, effects: Vec<Effect<u64>>) {
        for e in effects {
            self.seq += 1;
            // The waking rule lives in the protocol. A cat is told whether to
            // take a turn; it does not re-derive that from an event name.
            let wakes = if e.wakes() { e.to().copied() } else { None };
            let entry = Entry { seq: self.seq, block: self.block, effect: render(&e), wakes };
            if self.log.len() >= LOG_CAP {
                self.log.pop_front();
            }
            self.log.push_back(entry);
        }
    }

    /// One block. Consults nobody.
    fn tick(&mut self) {
        self.block += 1;
        let b = self.block;
        let fx = self.ext.execute_with(|| {
            System::set_block_number(b);
            Litter::on_initialize(b);
            drain()
        });
        self.absorb(fx);
    }

    fn apply(&mut self, c: Call) -> Result<(), String> {
        let b = self.block;
        let r = self.ext.execute_with(|| {
            System::set_block_number(b);
            let out = match c {
                Call::Open { who, text } => Litter::open(RuntimeOrigin::signed(who), text),
                Call::Plan { who, task, assignments } => {
                    let id = parse_task(&task).ok_or("bad task id")?;
                    Litter::plan(
                        RuntimeOrigin::signed(who),
                        id,
                        assignments
                            .into_iter()
                            .map(|a| PlanItem { who: a.who, what: a.what, expect: String::new() })
                            .collect(),
                    )
                }
                Call::Update { who, task, act, text } => {
                    let id = parse_task(&task).ok_or("bad task id")?;
                    let a = match act.as_str() {
                        "claim" => Act::Claim,
                        "done" => Act::Done,
                        "failed" => Act::Failed,
                        "clear" => Act::Clear,
                        "reopen" => Act::Reopen,
                        "artifact" => Act::Artifact,
                        _ => return Err("bad act".to_string()),
                    };
                    Litter::update(RuntimeOrigin::signed(who), id, a, text)
                }
                Call::Reassign { who, task, to } => {
                    let id = parse_task(&task).ok_or("bad task id")?;
                    Litter::reassign(RuntimeOrigin::signed(who), id, to)
                }
                Call::Say { who, to, body } => Litter::say(RuntimeOrigin::signed(who), to, body),
            };
            match out {
                Ok(()) => Ok(drain()),
                Err(e) => Err(format!("{e:?}")),
            }
        })?;
        self.absorb(r);
        Ok(())
    }
}

fn drain() -> Vec<Effect<u64>> {
    let out: Vec<_> = System::events()
        .into_iter()
        .filter_map(|r| match r.event {
            miot_runtime::RuntimeEvent::Litter(pallet_litter::Event::Happened(e)) => Some(e),
            _ => None,
        })
        .collect();
    System::reset_events();
    out
}

fn genesis(root: u64, leader: u64) -> sp_io::TestExternalities {
    use sp_runtime::BuildStorage;
    let mut t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
    pallet_litter::GenesisConfig::<Runtime> { root: Some(root), leader: Some(leader) }
        .assimilate_storage(&mut t)
        .unwrap();
    let mut ext: sp_io::TestExternalities = t.into();
    ext.execute_with(|| System::set_block_number(1));
    ext
}

type Shared = Arc<Mutex<Node>>;

#[derive(Deserialize)]
struct Since {
    #[serde(default)]
    since: u64,
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("MIOT_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(9944);
    let root: u64 = std::env::var("MIOT_ROOT").ok().and_then(|p| p.parse().ok()).unwrap_or(1);
    let leader: u64 = std::env::var("MIOT_LEADER").ok().and_then(|p| p.parse().ok()).unwrap_or(2);

    let node: Shared =
        Arc::new(Mutex::new(Node { ext: genesis(root, leader), log: VecDeque::new(), seq: 0, block: 1 }));

    // The block loop. Its own task, its own clock, and nothing in it waits for
    // a cat — that is the whole of Law I.
    {
        let node = node.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_millis(BLOCK_MS));
            loop {
                iv.tick().await;
                node.lock().await.tick();
            }
        });
    }

    let app = Router::new()
        .route("/head", get(head))
        .route("/events", get(events))
        .route("/call", post(call))
        .route("/artifact/{id}", get(artifact))
        .with_state(node);

    let addr = format!("0.0.0.0:{port}");
    println!("[node] chain on {addr}  root={root} leader={leader}  block={BLOCK_MS}ms");
    let l = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(l, app).await.unwrap();
}

async fn head(AxState(n): AxState<Shared>) -> Json<serde_json::Value> {
    let mut n = n.lock().await;
    let block = n.block;
    let seq = n.seq;
    let (leader, closed) = n.ext.execute_with(|| {
        let t = Litter::table();
        (t.leader().copied(), Litter::artifact(TaskId::parent(1)).is_some())
    });
    Json(serde_json::json!({"block":block,"seq":seq,"leader":leader,"closed":closed}))
}

async fn events(AxState(n): AxState<Shared>, Query(q): Query<Since>) -> Json<Vec<Entry>> {
    let n = n.lock().await;
    Json(n.log.iter().filter(|e| e.seq > q.since).cloned().collect())
}

async fn call(
    AxState(n): AxState<Shared>,
    Json(c): Json<Call>,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let mut n = n.lock().await;
    match n.apply(c) {
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
        Some(a) => serde_json::json!({"found":true,"title":a.title,"body":a.body,"author":a.author}),
        None => serde_json::json!({"found":false}),
    })
}
