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

use miot_llm::{task_tools, Llm, Turn};
use miot_primitives::{Act, Directive, Effect, PlanItem, TaskId, TaskStatus};
use miot_runtime::{Litter, RuntimeOrigin, System};
use polkadot_sdk::*;

use frame_support::traits::OnInitialize;

use crate::{colour, drain, name, new_ext, DIM, OFF};

pub const ROOT: u64 = 1;
const MIMI: u64 = 2;
const TAMA: u64 = 3;
const KURO: u64 = 4;
const SORA: u64 = 5;

pub fn account(n: &str) -> Option<u64> {
    match n.trim().trim_start_matches('@').to_ascii_lowercase().as_str() {
        "mimi" => Some(MIMI),
        "tama" => Some(TAMA),
        "kuro" => Some(KURO),
        "sora" => Some(SORA),
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

/// Distinct reasoning styles, not decoration.
///
/// Lifted from `meow/litter/personas/`, where the litter ran Sherlock, Zenigata,
/// Tiger and friends against each other. The point is that a litter of four
/// identical cats produces four identical answers and the leader learns nothing
/// from having asked twice — heterogeneity is what makes a second opinion an
/// opinion.
fn character(who: u64) -> &'static str {
    match who {
        MIMI => include_str!("../personas/mimi.md"),
        TAMA => include_str!("../personas/tama.md"),
        KURO => include_str!("../personas/kuro.md"),
        _ => include_str!("../personas/sora.md"),
    }
}

fn persona(who: u64, is_leader: bool, brief: &str) -> String {
    let protocol = format!(
        "{}\nThe litter coordinates over a blockchain: every act you take is an \
         extrinsic, and the chain decides what happens next.\n\n\
         How this works:\n\
         - You will be told exactly which verb to use. Use it.\n\
         - Task ids look like t1 (a parent) or t1.2 (a sub-task). Never invent one.\n\
         - Messages reach you automatically. There is no inbox to read, so never \
           try to read one.\n\
         - A sub-task stays open until you say otherwise, so never leave one \
           unanswered.\n\
         - Results are size-capped: say what happened rather than pasting output.\n\
         - Answer the task you were given. Nothing else.\n",
        ""
    );
    let role = if is_leader {
        "\nYou are the LEADER. The other cats are: tama, kuro, sora. \
         Never assign work to root — it has no agent behind it."
    } else {
        "\nYou are a WORKER. You claim what you are given and report back."
    };
    // The brief is "mounted": whatever the operator pointed us at, verbatim, in
    // every cat's context. Cheaper and more honest than a read tool for a
    // question about a document — the cats are debating the same text.
    let material = if brief.trim().is_empty() {
        // No brief: the only thing a cat can honestly report on is its own host.
        format!("\n\nWhat you can observe about the machine you run on:\n{}\n", host_facts())
    } else {
        format!(
            "\n\n--- MATERIAL ---\nThis is the document you are reasoning about. \
             Every claim you make must come from it.\n\n{brief}\n--- END MATERIAL ---\n"
        )
    };
    // Material goes BEFORE the protocol rules: it is the subject, and burying
    // it under housekeeping is how a litter ends up answering the wrong
    // question — which is exactly what happened when host facts sat here.
    format!("{}{material}{protocol}{role}", character(who))
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

/// One model per cat.
///
/// `--models mimi=gemma4-yolo-4b:latest,tama=gemma3:4b,...`, or one `--model`
/// for all of them. A heterogeneous litter is the interesting case: the cats
/// disagree for reasons other than sampling noise.
pub const CATS: [u64; 4] = [MIMI, TAMA, KURO, SORA];

pub fn cat_name(a: u64) -> &'static str { name(a) }

pub fn personas(who: u64, is_leader: bool, brief: &str) -> String { persona(who, is_leader, brief) }

pub struct Bench {
    by_cat: Vec<(u64, Llm)>,
    fallback: Llm,
}

impl Bench {
    /// `spec` is `name=endpoint,...` where an endpoint is either a model name
    /// (served by the default host) or a full base URL, optionally
    /// `url#model`.
    ///
    /// One endpoint per cat is the point: four cats against one server
    /// **serialize**, and the whole design rests on turns being concurrent and
    /// the chain ticking through them. A swarm that queues is not a swarm.
    pub fn new(host: &str, default_model: &str, spec: &str) -> Self {
        let mut by_cat = Vec::new();
        for part in spec.split(',').filter(|p| !p.trim().is_empty()) {
            if let Some((n, target)) = part.split_once('=') {
                if let Some(a) = account(n) {
                    let target = target.trim();
                    let (h, m) = match target.split_once('#') {
                        Some((u, m)) => (u.to_string(), m.to_string()),
                        None if target.starts_with("http") => {
                            (target.to_string(), default_model.to_string())
                        }
                        None => (host.to_string(), target.to_string()),
                    };
                    by_cat.push((a, Llm::local(&h, &m)));
                }
            }
        }
        Bench { by_cat, fallback: Llm::local(host, default_model) }
    }

    pub fn for_cat(&self, who: u64) -> &Llm {
        self.by_cat
            .iter()
            .find(|(a, _)| *a == who)
            .map(|(_, l)| l)
            .unwrap_or(&self.fallback)
    }
}

pub async fn run(host: &str, model: &str, models: &str, task: &str, brief: &str) {
    let bench = Bench::new(host, model, models);
    println!("  {DIM}live via {host}{OFF}");
    for who in [MIMI, TAMA, KURO, SORA] {
        println!("    {}{:>5}{OFF} {DIM}{}{OFF}", colour(who), name(who), bench.for_cat(who).label());
    }
    println!("{DIM}block  who    what{OFF}");
    println!("{DIM}──────────────────────────────────────────────────────────────{OFF}");

    let mut ext = new_ext();
    let parent = TaskId::parent(1);

    let opened = ext.execute_with(|| {
        Litter::open(RuntimeOrigin::signed(ROOT), task.to_string())
            .expect("root may open");
        drain()
    });
    drop(opened);
    let head: String = task.chars().take(64).collect();
    println!(
        "{DIM}   1{OFF}  {}{:>5}{OFF}  opened t1 — \"{head}…\"",
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
            // A turn is stateless, so the chain is the only memory there is.
            // Every prompt carries the parent question: without it a worker
            // answers its sub-task in isolation and the leader synthesises
            // from results it can no longer relate to a question.
            let question = ext.execute_with(|| {
                Litter::task(parent).map(|t| t.text).unwrap_or_default()
            });
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
                                "[clearance-needed: {task}]\nThe question: {question}\n\n\
                                 Results awaiting your verdict:\n{}\n\
                                 For EACH one call TaskUpdate with status=clear if it helps answer \
                                 the question, or status=reopen with text saying why if it does not.",
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
                                "[artifact-needed: {task}]\n\
                                 THE QUESTION YOU MUST ANSWER:\n{question}\n\n\
                                 What the litter found:\n{}\n\n\
                                 Call TaskUpdate with task={task}, status=artifact, and text set \
                                 to the final report in markdown. The '# ' heading must restate \
                                 THE QUESTION ABOVE, and the report must answer it directly. Do \
                                 not report on some other topic the material happens to mention.",
                                results.join("\n")
                            )
                        }
                        // The chain has given up re-offering and is asking
                        // the leader to move the work. Dropping this is what
                        // stalls a parent forever — the protocol surfaced the
                        // problem correctly and the agent has to answer it.
                        Directive::ReassignNeeded => {
                            let stuck = ext.execute_with(|| {
                                Litter::table()
                                    .subtasks(*task)
                                    .filter(|t| t.status == TaskStatus::Pending)
                                    .map(|t| {
                                        format!(
                                            "{} is stuck with {} (offered {} times, never claimed)",
                                            t.id,
                                            t.assignee.map(name).unwrap_or("?"),
                                            t.reoffers
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            });
                            if stuck.is_empty() {
                                continue;
                            }
                            format!(
                                "[reassign-needed: {task}]\n{}\nCall TaskReassign for EACH \
                                 stuck sub-task, moving it to a cat that is not the one \
                                 already stuck with it.",
                                stuck.join("\n")
                            )
                        }
                        Directive::LeaderElected => continue,
                    };
                    (*to, p)
                }
                Effect::Assigned { to, task, what, .. } => (
                    *to,
                    format!(
                        "The litter is working on this question:\n{question}\n\n\
                         [assigned: {task}] Your part: {what}\n\
                         Call TaskUpdate with task={task} and status=claim to take it."
                    ),
                ),
                Effect::Nudge { to, task, .. } => {
                    let mine = ext.execute_with(|| {
                        Litter::task(*task).map(|t| t.text).unwrap_or_default()
                    });
                    (
                        *to,
                        format!(
                            "The litter is working on this question:\n{question}\n\n\
                             [work: {task}] You claimed this part: {mine}\n\
                             Do it now from the MATERIAL in your context. Call TaskUpdate with \
                             task={task}, status=done, and text set to your findings — concrete, \
                             citing what in the material supports them. If you cannot, use \
                             status=failed."
                        ),
                    )
                }
                Effect::Closed { task, title } => {
                    println!("{DIM}{block:>4}         {task} closed — \"{title}\"{OFF}");
                    closed = true;
                    continue;
                }
                _ => continue,
            };

            let turn = match bench
                .for_cat(who)
                .turn(&persona(who, who == MIMI, brief), &prompt, task_tools())
                .await
            {
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
        "TaskReassign" => {
            let Some(task) = c.str("task").and_then(|t| parse_task(&t)) else { return };
            let Some(to) = c.str("to").and_then(|n| account(&n)) else {
                banner(block, who, turn, "TaskReassign: unknown cat");
                return;
            };
            let r = ext.execute_with(|| Litter::reassign(RuntimeOrigin::signed(who), task, to));
            match r {
                Ok(()) => banner(block, who, turn, &format!("re-homed {task} → {}", name(to))),
                Err(e) => banner(block, who, turn, &format!("reassign {task} REFUSED: {e:?}")),
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
