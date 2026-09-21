//! A litter, end to end, against the real runtime.
//!
//! Every state transition here is the **actual** `pallet-litter` running inside
//! `frame_executive`-style externalities — the same code a node would run, with
//! no wasm blob and no `sc-executor` anywhere. Blocks advance, `on_initialize`
//! ticks, and the chain issues directives to whoever is leader.
//!
//! **What is simulated:** the cats. Each one is a scripted policy standing in
//! for an LLM turn — it reacts to what the chain addressed to it. That is the
//! honest boundary: the protocol is real, the intelligence is a stub.
//!
//! **What it demonstrates**, in order:
//!   1. the chain telling the leader which verb to type, unprompted;
//!   2. directed sub-tasks claimed and submitted;
//!   3. a cat that never answers — its offer budget draining, the chain asking
//!      for a re-home by name, and the work changing hands;
//!   4. clearance, and a markdown artifact committed on chain.

use miot_primitives::{Act, Directive, Effect, PlanItem, TaskId};
use miot_runtime::{AccountId, Litter, Runtime, RuntimeOrigin, System};
use polkadot_sdk::*;

use frame_support::traits::OnInitialize;

/// Fixed, reproducible byte patterns — not derived from a real
/// `miot_keys::Identity` seed, because nothing in this file signs anything:
/// every call here goes straight into `RuntimeOrigin::signed` in-process, the
/// same way it always did. Only `--rpc` (a real node, which actually verifies
/// a signature) needs a real keypair — see `rpc.rs`.
const ROOT: AccountId = AccountId::new([1u8; 32]);
const MIMI: AccountId = AccountId::new([2u8; 32]); // leader
const TAMA: AccountId = AccountId::new([3u8; 32]); // works
const KURO: AccountId = AccountId::new([4u8; 32]); // never answers — the cat we have to recover from
const SORA: AccountId = AccountId::new([5u8; 32]); // picks up what kuro dropped

pub fn name(a: AccountId) -> &'static str {
    match a {
        ROOT => "root",
        MIMI => "mimi",
        TAMA => "tama",
        KURO => "kuro",
        SORA => "sora",
        _ => "?",
    }
}

/// One ANSI colour per sender, reused every time that cat acts — the thing that
/// makes a scroll of the whole litter's back-and-forth readable at a glance.
pub fn colour(a: AccountId) -> &'static str {
    match a {
        ROOT => "\x1b[97m",
        MIMI => "\x1b[95m",
        TAMA => "\x1b[96m",
        KURO => "\x1b[91m",
        SORA => "\x1b[92m",
        _ => "\x1b[0m",
    }
}

pub const DIM: &str = "\x1b[2m";
pub const OFF: &str = "\x1b[0m";

fn who(a: AccountId) -> String {
    format!("{}{:>5}{}", colour(a.clone()), name(a), OFF)
}

fn say(block: u64, actor: AccountId, what: String) {
    println!("{DIM}{block:>4}{OFF}  {}  {what}", who(actor));
}

fn note(block: u64, what: &str) {
    println!("{DIM}{block:>4}         {what}{OFF}");
}

pub fn new_ext() -> sp_io::TestExternalities {
    use sp_runtime::BuildStorage;
    let mut t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
    pallet_litter::GenesisConfig::<Runtime> { root: Some(ROOT), leader: Some(MIMI) }
        .assimilate_storage(&mut t)
        .unwrap();
    let mut ext: sp_io::TestExternalities = t.into();
    ext.execute_with(|| System::set_block_number(1));
    ext
}

/// Everything the chain emitted since the last call.
pub fn drain() -> Vec<Effect<AccountId>> {
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

mod chat;
mod live;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let is_live = args.iter().any(|a| a == "--live");
    let is_chat = args.iter().any(|a| a == "--chat");
    println!("{}", include_str!("../../../assets/akuma_40.txt"));
    println!("  {DIM}akuma miot — a litter, against the real runtime, no wasm{OFF}\n");
    if is_chat || is_live {
        let host = std::env::var("OLLAMA_HOST")
            .unwrap_or_else(|_| "http://localhost:11434".into());
        let model = args
            .iter()
            .position(|a| a == "--model")
            .and_then(|i| args.get(i + 1).cloned())
            .unwrap_or_else(|| "gemma4-yolo-4b:latest".into());
        let models = args
            .iter()
            .position(|a| a == "--models")
            .and_then(|i| args.get(i + 1).cloned())
            .unwrap_or_default();
        let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1).cloned());
        let task = arg("--task").unwrap_or_else(|| {
            "Where are you running? Each cat reports what it can determine about its host. \
             Then produce a combined report."
                .into()
        });
        // "Mount" a document into every cat's context.
        let brief = arg("--brief")
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .unwrap_or_default();
        if !brief.is_empty() {
            println!("  {DIM}brief: {} chars mounted{OFF}", brief.len());
        }
        if is_chat {
            chat::run(&host, &model, &models).await;
        } else {
            live::run(&host, &model, &models, &task, &brief).await;
        }
        return;
    }
    println!("{DIM}block  who    what{OFF}");
    println!("{DIM}─────────────────────────────────────────────────────────────{OFF}");

    let mut ext = new_ext();
    ext.execute_with(|| {
        let parent = TaskId::parent(1);
        Litter::open(
            RuntimeOrigin::signed(ROOT),
            "Debate whether this codebase works and produce a report.".into(),
        )
        .expect("root may open");
        say(1, ROOT, "opened t1 — \"debate whether this codebase works\"".into());

        let mut planned = false;
        let mut rehomed = false;
        let mut done = false;

        for block in 2..=1200u64 {
            System::set_block_number(block);
            Litter::on_initialize(block);

            for e in drain() {
                match e {
                    // --- the chain tells the leader which verb to type ---
                    Effect::Directed { to, task, directive } => match directive {
                        Directive::PlanNeeded if !planned => {
                            planned = true;
                            note(block, "chain → mimi: [plan-needed: t1]");
                            Litter::plan(
                                RuntimeOrigin::signed(MIMI),
                                task,
                                vec![
                                    PlanItem {
                                        who: TAMA,
                                        what: "run the build and tests".into(),
                                        expect: "pass/fail".into(),
                                    },
                                    PlanItem {
                                        who: KURO,
                                        what: "audit locking".into(),
                                        expect: "bugs".into(),
                                    },
                                ],
                            )
                            .expect("leader may plan");
                            say(block, MIMI, "split t1 → tama, kuro".into());
                        }
                        Directive::ReassignNeeded if !rehomed => {
                            rehomed = true;
                            note(block, "chain → mimi: [reassign-needed] — kuro never answered");
                            // Find whatever is still stuck and move it.
                            let stuck: Vec<TaskId> = Litter::table()
                                .subtasks(parent)
                                .filter(|t| t.assignee == Some(KURO))
                                .map(|t| t.id)
                                .collect();
                            for s in stuck {
                                Litter::reassign(RuntimeOrigin::signed(MIMI), s, SORA)
                                    .expect("leader may re-home");
                                say(block, MIMI, format!("re-homed {s} → sora"));
                            }
                        }
                        Directive::ClearanceNeeded => {
                            let awaiting: Vec<TaskId> = Litter::table()
                                .subtasks(parent)
                                .filter(|t| {
                                    t.status == miot_primitives::TaskStatus::AwaitingClearance
                                })
                                .map(|t| t.id)
                                .collect();
                            for s in awaiting {
                                Litter::update(
                                    RuntimeOrigin::signed(MIMI),
                                    s,
                                    Act::Clear,
                                    String::new(),
                                )
                                .expect("leader may clear");
                                say(block, MIMI, format!("cleared {s}"));
                            }
                        }
                        Directive::ArtifactNeeded if !done => {
                            done = true;
                            note(block, "chain → mimi: [artifact-needed: t1]");
                            let report = render_report(parent);
                            Litter::update(
                                RuntimeOrigin::signed(MIMI),
                                task,
                                Act::Artifact,
                                report,
                            )
                            .expect("leader may close");
                            say(block, MIMI, "committed the artifact for t1".into());
                        }
                        _ => {}
                        }

                    // --- a cat is handed work ---
                    Effect::Assigned { to, task, .. } => {
                        if to == KURO {
                            // The whole point: kuro is dead. It never claims.
                            note(block, &format!("offer of {task} → kuro (no answer)"));
                            continue;
                        }
                        Litter::update(RuntimeOrigin::signed(to.clone()), task, Act::Claim, String::new())
                            .expect("assignee may claim");
                        say(block, to, format!("claimed {task}"));
                    }

                    // --- nudged: do the work ---
                    Effect::Nudge { to, task, .. } => {
                        let result = match to {
                            TAMA => "214 tests green, 38 s cold",
                            SORA => "one lock in memory.rs is unheld on the error path",
                            _ => "done",
                        };
                        if Litter::update(
                            RuntimeOrigin::signed(to.clone()),
                            task,
                            Act::Done,
                            result.into(),
                        )
                        .is_ok()
                        {
                            say(block, to, format!("{task}: {result}"));
                        }
                    }

                    Effect::Requeued { task, why, .. } => {
                        note(block, &format!("{task} requeued ({why:?})"));
                    }
                    Effect::Closed { task, title } => {
                        say(block, MIMI, format!("{task} closed — \"{title}\""));
                    }
                    _ => {}
                }
            }

            if done {
                println!("{DIM}─────────────────────────────────────────────────────────────{OFF}");
                let a = Litter::artifact(parent).expect("artifact is on chain");
                println!(
                    "\n  {DIM}artifact for t1, read back out of chain state \
                     ({} bytes, by {}){OFF}\n",
                    a.body.len(),
                    name(a.author.clone())
                );
                for line in a.body.lines() {
                    println!("  │ {line}");
                }
                println!("\n  {DIM}blocks elapsed: {block}{OFF}");
                return;
            }
        }
        panic!("the litter never closed its parent task");
    });
}

/// The header is rendered from chain state; a real cat would write only the
/// findings and the answer. A 0.8B model is never asked to spell a field it
/// cannot see.
fn render_report(parent: TaskId) -> String {
    let table = Litter::table();
    let mut s = String::new();
    s.push_str("# Does this codebase actually work?\n\n");
    s.push_str(&format!("- **task**: {parent}\n"));
    s.push_str("- **closed by**: mimi (leader)\n\n## Sub-tasks\n\n");
    for t in table.subtasks(parent) {
        s.push_str(&format!(
            "### {} — {}\n{}\n\n",
            t.id,
            t.assignee.clone().map(name).unwrap_or("?"),
            t.outcome.as_ref().map(|o| o.text()).unwrap_or("(none)")
        ));
    }
    s.push_str("## Answer\n\nCompiles and passes its tests, but races under concurrent load.\n");
    s
}
