//! One cat's agent loop.
//!
//! Knows two addresses, a node and a model, and nothing else. It has never
//! heard of the other cats. Everything it learns about them arrives as an
//! event from the chain, which is the litter's oldest rule kept intact: *the
//! chain is the only channel between agents.* Under `kot run` the node is in
//! the same process, and it's still reached over HTTP (`docs/CLI.md` §5a).
//!
//! It signs its own acts. Every act is a real
//! [`miot_runtime::UncheckedExtrinsic`] signed with this cat's
//! [`miot_keys::Identity`]; the node recovers the sender from the signature.
//!
//! It polls, it thinks for as long as thinking takes, and it submits. Nothing
//! it does can make the node late. It acts only on events whose `wakes` names
//! it, because waking on every broadcast turns one record into four LLM
//! turns, which the litter learned the expensive way.

use codec::Encode;
use miot_keys::Identity;
use miot_llm::{task_tools, Llm};
use miot_runtime::{client, AccountId, RuntimeCall};
use polkadot_sdk::*;
use serde::Deserialize;
use sp_core::H256;
use std::collections::{HashMap, HashSet};

use crate::common::{parse_task, Roster};

pub struct AgentConfig {
    pub name: String,
    pub identity: Identity,
    pub node: String,
    pub llm: Llm,
    pub persona: String,
    pub roster: Roster,
}

#[derive(Debug, Deserialize, Clone)]
struct Entry {
    seq: u64,
    block: u64,
    effect: serde_json::Value,
    wakes: Option<String>,
}

struct Cat {
    name: String,
    identity: Identity,
    account: AccountId,
    node: String,
    http: reqwest::Client,
    llm: Llm,
    persona: String,
    roster: Roster,
}

impl Cat {
    async fn head(&self) -> Option<serde_json::Value> {
        self.http.get(format!("{}/head", self.node)).send().await.ok()?.json().await.ok()
    }

    async fn events(&self, since: u64) -> Vec<Entry> {
        match self.http.get(format!("{}/events?since={since}", self.node)).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            // A node that isn't answering isn't an error to hang on. Fail
            // fast, keep polling — the litter's WAYWARD rule.
            Err(_) => Vec::new(),
        }
    }

    async fn meta(&self) -> Option<client::Meta> {
        let v: serde_json::Value = self.http.get(format!("{}/meta", self.node)).send().await.ok()?.json().await.ok()?;
        let genesis_hash = H256::from_slice(&hex::decode(v.get("genesis_hash")?.as_str()?).ok()?);
        Some(client::Meta {
            genesis_hash,
            spec_version: v.get("spec_version")?.as_u64()? as u32,
            tx_version: v.get("tx_version")?.as_u64()? as u32,
        })
    }

    async fn nonce(&self) -> u32 {
        let url = format!("{}/account/{}", self.node, miot_keys::to_hex(&self.account));
        match self.http.get(url).send().await {
            Ok(r) => r.json::<serde_json::Value>().await.ok().and_then(|v| v.get("nonce")?.as_u64()).unwrap_or(0) as u32,
            Err(_) => 0,
        }
    }

    /// Sign `call` and submit it. `/meta` and the nonce are fetched fresh
    /// every time. Two extra requests against a turn that costs tens of
    /// seconds isn't the bottleneck. On a replica, both calls reach the
    /// primary, so the nonce is never stale.
    async fn submit(&self, call: RuntimeCall) -> bool {
        let Some(meta) = self.meta().await else {
            println!("  [{}] node unreachable (meta)", self.name);
            return false;
        };
        let nonce = self.nonce().await;
        let uxt = client::sign(&self.identity, call, nonce, &meta);
        match self.http.post(format!("{}/submit", self.node)).body(uxt.encode()).send().await {
            Ok(r) if r.status().is_success() => true,
            Ok(r) => {
                let e: serde_json::Value = r.json().await.unwrap_or_default();
                println!("  [{}] refused: {}", self.name, e.get("error").unwrap_or(&e));
                false
            }
            Err(e) => {
                println!("  [{}] node unreachable: {e}", self.name);
                false
            }
        }
    }

    /// Build the prompt for one woken event. The parent question is carried
    /// into every one: a turn is stateless, so the chain is the only memory.
    fn prompt(&self, e: &Entry, question: &str) -> Option<(String, Vec<miot_llm::Tool>)> {
        let t = e.effect.get("t")?.as_str()?;
        let task = e.effect.get("task").and_then(|v| v.as_str()).unwrap_or("t1");
        let workers = || {
            self.roster
                .0
                .iter()
                .filter(|(n, i)| *i != self.account && n != "root")
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let p = match t {
            "assigned" => {
                let what = e.effect.get("what")?.as_str().unwrap_or("");
                format!(
                    "The litter is working on:\n{question}\n\n[assigned: {task}] Your part: {what}\n\
                     Call TaskUpdate with task={task} and status=claim to take it."
                )
            }
            "nudge" => format!(
                "The litter is working on:\n{question}\n\n[work: {task}] You claimed this.\n\
                 Do it now. Call TaskUpdate with task={task}, status=done, and text set to your \
                 findings — concrete and specific. If you cannot, use status=failed."
            ),
            "directed" => match e.effect.get("directive")?.as_str().unwrap_or("") {
                "PlanNeeded" => format!(
                    "[plan-needed: {task}]\nThe operator asked:\n{question}\n\n\
                     Call TaskPlan on {task} now. One assignment each to: {}. All in ONE call.",
                    workers()
                ),
                "ClearanceNeeded" => format!(
                    "[clearance-needed: {task}]\nThe question: {question}\n\n\
                     Results are in. For EACH sub-task call TaskUpdate with status=clear if it \
                     helps answer the question, or status=reopen with text saying why not."
                ),
                "ArtifactNeeded" => format!(
                    "[artifact-needed: {task}]\nTHE QUESTION YOU MUST ANSWER:\n{question}\n\n\
                     Every sub-task is cleared. Call TaskUpdate with task={task}, \
                     status=artifact, and text set to the final report in markdown. The '# ' \
                     heading must restate the question, and the report must answer it. \
                     Keep it under {} words — about {} pages; anything longer is refused.",
                    miot_runtime::MAX_ARTIFACT_BYTES / 6,
                    miot_runtime::ARTIFACT_PAGES,
                ),
                "ReassignNeeded" => format!(
                    "[reassign-needed: {task}]\nA sub-task has been offered repeatedly and \
                     never claimed — that cat cannot do it. Call TaskReassign to move it to \
                     another cat. The litter is: {}",
                    workers()
                ),
                _ => return None,
            },
            "said" => {
                let from = e.effect.get("from")?.as_str()?;
                let body = e.effect.get("body")?.as_str().unwrap_or("");
                let who = miot_keys::from_hex(from).map(|a| self.roster.name_of(&a)).unwrap_or_else(|_| "someone".into());
                format!("{who} said to the litter:\n\"{body}\"\n\nReply with SendMessage. Two sentences at most.")
            }
            _ => return None,
        };
        let tools = if t == "said" { miot_llm::chat_tools() } else { task_tools() };
        Some((p, tools))
    }

    async fn act(&self, c: &miot_llm::Call) {
        let task = c.str("task").unwrap_or_default();
        let call = match c.name.as_str() {
            "TaskPlan" => {
                let assignments: Vec<miot_primitives::PlanItem<AccountId>> = c
                    .args
                    .get("assignments")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|it| {
                                Some(miot_primitives::PlanItem {
                                    who: self.roster.account(it.get("who")?.as_str()?)?,
                                    what: it.get("what")?.as_str()?.to_string(),
                                    expect: String::new(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let Some(parent) = parse_task(&task) else { return };
                RuntimeCall::Litter(pallet_litter::Call::plan { parent, assignments })
            }
            "TaskUpdate" => {
                let Some(id) = parse_task(&task) else { return };
                let act = match c.str("status").unwrap_or_default().as_str() {
                    "claim" => miot_primitives::Act::Claim,
                    "done" => miot_primitives::Act::Done,
                    "failed" => miot_primitives::Act::Failed,
                    "clear" => miot_primitives::Act::Clear,
                    "reopen" => miot_primitives::Act::Reopen,
                    "artifact" => miot_primitives::Act::Artifact,
                    _ => return,
                };
                RuntimeCall::Litter(pallet_litter::Call::update { task: id, act, text: c.str("text").unwrap_or_default() })
            }
            "TaskReassign" => {
                let Some(to) = c.str("to").and_then(|n| self.roster.account(&n)) else { return };
                let Some(id) = parse_task(&task) else { return };
                RuntimeCall::Litter(pallet_litter::Call::reassign { task: id, to })
            }
            "SendMessage" => RuntimeCall::Litter(pallet_litter::Call::say { to: None, body: c.str("body").unwrap_or_default() }),
            _ => return,
        };
        self.submit(call).await;
    }
}

pub async fn run(cfg: AgentConfig) {
    let account = cfg.identity.account();
    let cat = Cat {
        name: cfg.name.clone(),
        identity: cfg.identity,
        account: account.clone(),
        node: cfg.node,
        http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(900)).build().unwrap(),
        llm: cfg.llm,
        persona: cfg.persona,
        roster: cfg.roster,
    };
    let name = cat.name.clone();
    println!("[{name}] id={} node={} llm={}", miot_keys::short(&account), cat.node, cat.llm.label());

    while cat.head().await.is_none() {
        println!("[{name}] waiting for the node...");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    println!("[{name}] connected.");

    let mut cursor = 0u64;
    let mut question = String::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let my_hex = miot_keys::to_hex(&account);

    loop {
        // A node that rebuilt its log (demoted, rewound, adopted a
        // checkpoint) restarts `seq`. A cursor past the new end would go
        // deaf until the log grew back past it, so jump to the new end.
        // Skipping what's there is right: those wakes are old news, and the
        // chain re-issues anything still outstanding on its own.
        if let Some(seq) = cat.head().await.and_then(|h| h["seq"].as_u64()) {
            if seq < cursor {
                println!("[{name}] node's log restarted (seq {seq} < cursor {cursor}); resuming from its end");
                cursor = seq;
                seen.clear();
            }
        }

        let batch = cat.events(cursor).await;
        for e in &batch {
            cursor = cursor.max(e.seq);
            if e.effect.get("t").and_then(|v| v.as_str()) == Some("said")
                && e.effect.get("root").and_then(|v| v.as_bool()) == Some(true)
                && question.is_empty()
            {
                // Root speaking sets the subject if nothing else has.
                question = e.effect.get("body").and_then(|v| v.as_str()).unwrap_or("").into();
            }
            if e.effect.get("t").and_then(|v| v.as_str()) == Some("opened") {
                question = e.effect.get("text").and_then(|v| v.as_str()).unwrap_or("").into();
            }
        }

        // COALESCE. A turn takes minutes; the chain ticks in seconds. By
        // the time a cat finishes thinking, several more wakes for the same
        // task are waiting, and the newest supersedes them all. Keep the
        // newest per (task, kind) — the `Coalesce` policy from docs/CLI.md.
        let mut latest: HashMap<(String, String), Entry> = HashMap::new();
        for e in batch.into_iter().filter(|e| e.wakes.as_deref() == Some(my_hex.as_str())) {
            let k = (
                e.effect.get("task").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                e.effect.get("t").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            );
            match latest.get(&k) {
                Some(cur) if cur.seq >= e.seq => {}
                _ => {
                    latest.insert(k, e);
                }
            }
        }
        let mut mine: Vec<Entry> = latest.into_values().collect();
        mine.sort_by_key(|e| e.seq);

        for e in mine {
            if !seen.insert(e.seq) {
                continue;
            }
            let Some((prompt, tools)) = cat.prompt(&e, &question) else { continue };
            let t = e.effect.get("t").and_then(|v| v.as_str()).unwrap_or("");
            println!("[{name}] block {} {t} — thinking", e.block);
            match cat.llm.turn(&cat.persona, &prompt, tools).await {
                Ok(turn) => {
                    if turn.calls.is_empty() {
                        println!("[{name}]   no tool call ({} tok) — turn wasted", turn.tokens);
                    }
                    for c in &turn.calls {
                        println!("[{name}]   {} ({} tok, {:.0}s)", c.name, turn.tokens, turn.ms as f64 / 1000.0);
                        cat.act(c).await;
                    }
                }
                Err(e) => println!("[{name}]   llm error: {e}"),
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    }
}
