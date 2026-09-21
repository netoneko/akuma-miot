//! The same litter, with real cats.
//!
//! Every state transition is still the real `pallet-litter`. What changes is
//! that the scripted policies are replaced by actual model turns, and that
//! makes the two-loop separation load-bearing rather than decorative:
//!
//! - the **chain loop** advances a block, ticks, and hands back effects. It
//!   runs inside externalities and never waits for anybody.
//! - the **agent loop** takes an effect addressed to a cat, thinks about it for
//!   however long it takes, and submits the result as a call.
//!
//! The model is never asked to invent a task id, spell a bracket syntax, or
//! infer which verb the protocol wants next. The chain says which verb; the cat
//! decides the content. That division is the whole reason a small model can
//! drive this at all.

use miot_llm::{task_tools, Msg, Ollama, Turn};
use miot_primitives::{Act, Directive, Effect, PlanItem, TaskId, TaskStatus};
use miot_runtime::{Litter, RuntimeOrigin, System};
use polkadot_sdk::*;

use frame_support::traits::OnInitialize;

use crate::{colour, drain, name, new_ext, DIM, OFF};

const ROOT: u64 = 1;
const MIMI: u64 = 2;
const TAMA: u64 = 3;
const KURO: u64 = 4;

fn account(n: &str) -> Option<u64> {
    match n.trim().trim_start_matches('@').to_ascii_lowercase().as_str() {
        "mimi" => Some(MIMI),
        "tama" => Some(TAMA),
        "kuro" => Some(KURO),
        _ => None,
    }
}

fn host_facts() -> String {
    let out = |c: &str, a: &[&str]| {
        std::process::Command::new(c)
            .args(a)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    format!(
        "uname: {}\nkernel: {}\narch: {}\ncontainer: {}",
        out("uname", &["-s"]),
        out("uname", &["-r"]),
        out("uname", &["-m"]),
        if std::path::Path::new("/.dockerenv").exists() { "yes (docker)" } else { "no" },
    )
}

fn persona(who: u64, is_leader: bool) -> String {
    let base = format!(
        "You are {}, one cat in a litter of AI agents called Akuma Miot. \
         The litter coordinates over a blockchain: every act you take is an \
         extrinsic, and the chain decides what happens next.\n\n\
         What you can actually observe about the machine you run on:\n{}\n\n\
         Rules:\n\
         - You will be told exactly which verb to use. Use it.\n\
         - Task ids look like t1 (a parent) or t1.2 (a sub-task). Never invent one.\n\
         - Be concise and concrete. Results are size-capped, so say what happened \
           rather than pasting output.\n",
        name(who),
        host_facts()
    );
    if is_leader {
        format!(
            "{base}\nYou are the LEADER. You split parent tasks into directed \
             sub-tasks with TaskPlan (all sub-tasks in ONE call), you clear \
             results you accept, and you write the final report. \
             The other cats are: tama, kuro. Never assign work to root."
        )
    } else {
        format!("{base}\nYou are a WORKER. You claim what you are given and report back.")
    }
}

fn banner(block: u64, who: u64, t: &Turn, what: &str) {
    println!(
        "{DIM}{block:>4}{OFF}  {}{:>5}{OFF}  {what} {DIM}({} tok, {:.1}s){OFF}",
        colour(who),
        name(who),
        t.tokens,
        t.ms as f64 / 1000.0
    );
    if !t.text.trim().is_empty() {
        for l in t.text.trim().lines().take(3) {
            println!("{DIM}            {l}{OFF}");
        }
    }
}

pub async fn run(host: &str, model: &str) {
    let llm = Ollama::new(host, model);
    println!("  {DIM}live: {model} via {host}{OFF}");
    println!("{DIM}block  who    what{OFF}");
    println!("{DIM}──────────────────────────────────────────────────────────────{OFF}");

    let mut ext = new_ext();
    let parent = TaskId::parent(1);

    let opened = ext.execute_with(|| {
        Litter::open(
            RuntimeOrigin::signed(ROOT),
            "Where are you running? Each cat reports what it can determine about \
             its host. Then produce a combined report."
                .into(),
        )
        .expect("root may open");
        drain()
    });
    drop(opened);
    println!(
        "{DIM}   1{OFF}  {}{:>5}{OFF}  opened t1 — \"where are you running?\"",
        colour(ROOT),
        name(ROOT)
    );

    let mut block = 1u64;
    let mut closed = false;

    while block < 4000 && !closed {
        block += 1;
        // ---- chain loop: never waits for anyone ----
        let effects = ext.execute_with(|| {
            System::set_block_number(block);
            Litter::on_initialize(block);
            drain()
        });

        for e in effects {
            // ---- agent loop: takes as long as it takes ----
            let (who, prompt) = match &e {
                Effect::Directed { to, task, directive } => {
                    let p = match directive {
                        Directive::PlanNeeded => {
                            let text = ext.execute_with(|| {
                                Litter::task(*task).map(|t| t.text).unwrap_or_default()
                            });
                            format!(
                                "[plan-needed: {task}]\nThe operator opened {task}: \"{text}\"\n\
                                 Call TaskPlan on {task} now. Give one assignment to tama and \
                                 one to kuro, in a single call."
                            )
                        }
                        Directive::ClearanceNeeded => {
                            let results = ext.execute_with(|| {
                                Litter::table()
                                    .subtasks(*task)
                                    .filter(|t| t.status == TaskStatus::AwaitingClearance)
                                    .map(|t| {
                                        format!(
                                            "{} from {}: {}",
                                            t.id,
                                            t.assignee.map(name).unwrap_or("?"),
                                            t.outcome.as_ref().map(|o| o.text()).unwrap_or("")
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            });
                            if results.is_empty() {
                                continue;
                            }
                            format!(
                                "[clearance-needed: {task}]\nResults awaiting your verdict:\n{}\n\
                                 For EACH one call TaskUpdate with status=clear if you accept it.",
                                results.join("\n")
                            )
                        }
                        Directive::ArtifactNeeded => {
                            let results = ext.execute_with(|| {
                                Litter::table()
                                    .subtasks(*task)
                                    .map(|t| {
                                        format!(
                                            "{} ({}): {}",
                                            t.id,
                                            t.assignee.map(name).unwrap_or("?"),
                                            t.outcome.as_ref().map(|o| o.text()).unwrap_or("")
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            });
                            format!(
                                "[artifact-needed: {task}]\nEvery sub-task is cleared:\n{}\n\
                                 Call TaskUpdate with task={task}, status=artifact, and text set \
                                 to the final report in markdown. Start it with a '# ' heading \
                                 that names the question. Answer: where is this litter running?",
                                results.join("\n")
                            )
                        }
                        Directive::ReassignNeeded | Directive::LeaderElected => continue,
                    };
                    (*to, p)
                }
                Effect::Assigned { to, task, what, .. } => (
                    *to,
                    format!(
                        "[assigned: {task}] {what}\nCall TaskUpdate with task={task} and \
                         status=claim to take it."
                    ),
                ),
                Effect::Nudge { to, task, .. } => (
                    *to,
                    format!(
                        "[work: {task}] You claimed this. Do it now and report: call TaskUpdate \
                         with task={task}, status=done, and text set to what you found about the \
                         host you run on. If you cannot, use status=failed."
                    ),
                ),
                Effect::Closed { task, title } => {
                    println!("{DIM}{block:>4}         {task} closed — \"{title}\"{OFF}");
                    closed = true;
                    continue;
                }
                _ => continue,
            };

            let msgs = [Msg::system(persona(who, who == MIMI)), Msg::user(prompt)];
            let turn = match llm.turn(&msgs, &task_tools()).await {
                Ok(t) => t,
                Err(err) => {
                    println!("{DIM}{block:>4}         llm error: {err}{OFF}");
                    continue;
                }
            };

            if turn.calls.is_empty() {
                banner(block, who, &turn, "(no tool call — turn wasted)");
                continue;
            }
            for c in &turn.calls {
                apply(&mut ext, block, who, c, &turn);
            }
        }
    }

    println!("{DIM}──────────────────────────────────────────────────────────────{OFF}");
    let artifact = ext.execute_with(|| Litter::artifact(parent));
    match artifact {
        Some(a) => {
            println!(
                "\n  {DIM}artifact for t1, read back out of chain state ({} bytes, by {}){OFF}\n",
                a.body.len(),
                name(a.author)
            );
            for l in a.body.lines() {
                println!("  │ {l}");
            }
            println!("\n  {DIM}blocks elapsed: {block}{OFF}");
        }
        None => println!("\n  {DIM}no artifact — the litter did not finish in {block} blocks{OFF}"),
    }
}

/// Turn one tool call into an extrinsic. A refused call is reported and
/// dropped: the chain is the authority on what was allowed, not the model.
fn apply(
    ext: &mut sp_io::TestExternalities,
    block: u64,
    who: u64,
    c: &miot_llm::Call,
    turn: &Turn,
) {
    let parse_task = |s: &str| -> Option<TaskId> {
        let s = s.trim().trim_start_matches('t');
        let mut it = s.split('.');
        let p: u32 = it.next()?.parse().ok()?;
        match it.next() {
            None => Some(TaskId::parent(p)),
            Some(sub) => Some(TaskId::sub(p, sub.parse().ok()?)),
        }
    };

    match c.name.as_str() {
        "TaskPlan" => {
            let Some(task) = c.str("task").and_then(|t| parse_task(&t)) else { return };
            let items: Vec<PlanItem<u64>> = c
                .args
                .get("assignments")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|it| {
                            Some(PlanItem {
                                who: account(it.get("who")?.as_str()?)?,
                                what: it.get("what")?.as_str()?.to_string(),
                                expect: String::new(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let n = items.len();
            let r = ext.execute_with(|| Litter::plan(RuntimeOrigin::signed(who), task, items));
            match r {
                Ok(()) => banner(block, who, turn, &format!("TaskPlan {task} → {n} sub-tasks")),
                Err(e) => banner(block, who, turn, &format!("TaskPlan {task} REFUSED: {e:?}")),
            }
        }
        "TaskUpdate" => {
            let Some(task) = c.str("task").and_then(|t| parse_task(&t)) else { return };
            let Some(status) = c.str("status") else { return };
            let act = match status.trim().to_ascii_lowercase().as_str() {
                "claim" => Act::Claim,
                "done" => Act::Done,
                "failed" => Act::Failed,
                "clear" => Act::Clear,
                "reopen" => Act::Reopen,
                "artifact" => Act::Artifact,
                _ => return,
            };
            let text = c.str("text").unwrap_or_default();
            let r = ext.execute_with(|| {
                Litter::update(RuntimeOrigin::signed(who), task, act, text)
            });
            match r {
                Ok(()) => banner(block, who, turn, &format!("{} {task}", act.as_str())),
                Err(e) => {
                    banner(block, who, turn, &format!("{} {task} REFUSED: {e:?}", act.as_str()))
                }
            }
        }
        other => banner(block, who, turn, &format!("unknown tool {other}")),
    }
}
