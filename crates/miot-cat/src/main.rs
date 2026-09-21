//! One cat.
//!
//! A process that knows two addresses — a node and a model — and nothing else.
//! It has never heard of the other cats. Everything it learns about them
//! arrives as an event from the chain, which is the litter's oldest rule kept
//! intact: *the chain is the only channel between agents.*
//!
//! # It signs its own acts now
//!
//! There used to be a `who` field in a JSON body the node trusted. There
//! isn't one any more: every act this cat takes is a real
//! [`miot_runtime::UncheckedExtrinsic`], signed with its own
//! [`miot_keys::Identity`], and the node recovers the sender from the
//! signature instead of being told it.
//!
//! # The agent loop is the loose one
//!
//! It polls, it thinks for as long as thinking takes, and it submits. Nothing
//! it does can make the node late — the block loop is in another process on
//! another container, ticking on its own clock. A turn here spanning twenty
//! blocks is normal and costs the chain nothing.
//!
//! # It is told when to think
//!
//! The node marks each event with `wakes`. A cat acts on events addressed to
//! it and ignores the rest, because waking on every broadcast turns one record
//! into four LLM turns — which the litter learned the expensive way.
//!
//! ```text
//!   MIOT_NAME    which cat this is        MIOT_NODE     http://node:9944
//!   MIOT_SEED    its identity              MIOT_LLM      http://host:8081
//!   MIOT_MODEL   model name                MIOT_PERSONA  path to a persona file
//!   MIOT_ROSTER  name=seed,name=seed,...   (seed: a small int, or 64 hex chars)
//! ```

use codec::Encode;
use miot_keys::Identity;
use miot_llm::{task_tools, Llm};
use miot_runtime::{client, AccountId, RuntimeCall};
use polkadot_sdk::*;
use serde::Deserialize;
use sp_core::H256;
use std::collections::HashSet;

#[derive(Debug, Deserialize, Clone)]
struct Entry {
    seq: u64,
    block: u64,
    effect: serde_json::Value,
    wakes: Option<String>,
}

/// `MIOT_SEED`/a roster entry is either a small int (a deterministic seed
/// byte, `[n; 32]` — fine for one operator's own trusted litter, same
/// reasoning `miot-keys`'s `Identity::from_seed` doc already gives) or 64
/// hex characters, a real 32-byte seed.
fn parse_seed(spec: &str) -> [u8; 32] {
    if let Ok(n) = spec.parse::<u8>() {
        return [n; 32];
    }
    let bytes = hex::decode(spec.trim_start_matches("0x"))
        .unwrap_or_else(|_| panic!("seed {spec:?} is neither a small int nor 64 hex chars"));
    bytes.try_into().unwrap_or_else(|_| panic!("seed {spec:?} is not 32 bytes"))
}

struct Cat {
    name: String,
    identity: Identity,
    account: AccountId,
    node: String,
    http: reqwest::Client,
    llm: Llm,
    persona: String,
    roster: Vec<(String, AccountId)>,
}

impl Cat {
    fn who(&self, id: &AccountId) -> String {
        self.roster
            .iter()
            .find(|(_, i)| i == id)
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| miot_keys::short(id))
    }

    fn account_of(&self, name: &str) -> Option<AccountId> {
        let n = name.trim().trim_start_matches('@').to_ascii_lowercase();
        self.roster.iter().find(|(r, _)| *r == n).map(|(_, i)| i.clone())
    }

    async fn head(&self) -> Option<serde_json::Value> {
        self.http.get(format!("{}/head", self.node)).send().await.ok()?.json().await.ok()
    }

    async fn events(&self, since: u64) -> Vec<Entry> {
        match self.http.get(format!("{}/events?since={since}", self.node)).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            // A node that is not answering is not an error to hang on. Fail
            // fast, keep polling — the litter's WAYWARD rule, which exists
            // because a stalled tool call wastes a whole turn.
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

    /// Sign `call` and submit it. Fetches `/meta` and its own nonce fresh
    /// every time — an extra two requests per act, against a turn that costs
    /// tens of seconds of LLM time, is not the bottleneck here.
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
    /// into every one of them: a turn is stateless, so the chain is the only
    /// memory there is.
    async fn prompt(&self, e: &Entry, question: &str) -> Option<(String, Vec<miot_llm::Tool>)> {
        let t = e.effect.get("t")?.as_str()?;
        let task = e.effect.get("task").and_then(|v| v.as_str()).unwrap_or("t1");
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
            "directed" => {
                let d = e.effect.get("directive")?.as_str().unwrap_or("");
                match d {
                    "PlanNeeded" => format!(
                        "[plan-needed: {task}]\nThe operator asked:\n{question}\n\n\
                         Call TaskPlan on {task} now. One assignment each to: {}. All in ONE call.",
                        self.roster
                            .iter()
                            .filter(|(n, i)| *i != self.account && n != "root")
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
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
                         heading must restate the question, and the report must answer it."
                    ),
                    "ReassignNeeded" => format!(
                        "[reassign-needed: {task}]\nA sub-task has been offered repeatedly and \
                         never claimed — that cat cannot do it. Call TaskReassign to move it to \
                         another cat. The litter is: {}",
                        self.roster.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                    _ => return None,
                }
            }
            "said" => {
                let from = e.effect.get("from")?.as_str()?;
                let body = e.effect.get("body")?.as_str().unwrap_or("");
                format!(
                    "{} said to the litter:\n\"{body}\"\n\nReply with SendMessage. \
                     Two sentences at most.",
                    self.who(&miot_keys::from_hex(from).unwrap_or_else(|_| self.account.clone()))
                )
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
                                let who = self.account_of(it.get("who")?.as_str()?)?;
                                Some(miot_primitives::PlanItem {
                                    who,
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
                let text = c.str("text").unwrap_or_default();
                RuntimeCall::Litter(pallet_litter::Call::update { task: id, act, text })
            }
            "TaskReassign" => {
                let Some(to) = c.str("to").and_then(|n| self.account_of(&n)) else { return };
                let Some(id) = parse_task(&task) else { return };
                RuntimeCall::Litter(pallet_litter::Call::reassign { task: id, to })
            }
            "SendMessage" => RuntimeCall::Litter(pallet_litter::Call::say {
                to: None,
                body: c.str("body").unwrap_or_default(),
            }),
            _ => return,
        };
        self.submit(call).await;
    }
}

fn parse_task(s: &str) -> Option<miot_primitives::TaskId> {
    let s = s.trim().trim_start_matches('t');
    let mut it = s.split('.');
    let p: u32 = it.next()?.parse().ok()?;
    match it.next() {
        None => Some(miot_primitives::TaskId::parent(p)),
        Some(sub) => Some(miot_primitives::TaskId::sub(p, sub.parse().ok()?)),
    }
}

#[tokio::main]
async fn main() {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let name = env("MIOT_NAME", "tama");
    let identity = Identity::from_seed(&parse_seed(&env("MIOT_SEED", "3")));
    let account = identity.account();
    let node = env("MIOT_NODE", "http://node:9944");
    let model = env("MIOT_MODEL", "qwen3:4b");
    let llm_url = env("MIOT_LLM", "http://host.docker.internal:8081");
    let persona = std::fs::read_to_string(env("MIOT_PERSONA", "/personas/tama.md"))
        .unwrap_or_else(|_| format!("You are {name}, a cat in the Akuma Miot litter."));

    let roster: Vec<(String, AccountId)> =
        env("MIOT_ROSTER", "root=1,mimi=2,tama=3,kuro=4,sora=5")
            .split(',')
            .filter_map(|p| {
                let (n, seed) = p.split_once('=')?;
                Some((n.trim().to_string(), Identity::from_seed(&parse_seed(seed.trim())).account()))
            })
            .collect();

    let cat = Cat {
        name: name.clone(),
        identity,
        account: account.clone(),
        node: node.clone(),
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(900))
            .build()
            .unwrap(),
        llm: Llm::local(&llm_url, &model),
        persona,
        roster,
    };

    println!("[{name}] id={} node={node} llm={} model={model}", miot_keys::short(&account), llm_url);

    // Wait for the node. A cat that starts first is normal in a compose file.
    let mut cursor = 0u64;
    loop {
        if cat.head().await.is_some() {
            break;
        }
        println!("[{name}] waiting for the node...");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    println!("[{name}] connected.");

    let mut question = String::new();
    let mut seen: HashSet<u64> = HashSet::new();

    loop {
        let batch = cat.events(cursor).await;
        for e in &batch {
            cursor = cursor.max(e.seq);
            if e.effect.get("t").and_then(|v| v.as_str()) == Some("said")
                && e.effect.get("root").and_then(|v| v.as_bool()) == Some(true)
            {
                // Root speaking sets the subject if nothing else has.
                if question.is_empty() {
                    question = e.effect.get("body").and_then(|v| v.as_str()).unwrap_or("").into();
                }
            }
        }

        // COALESCE. A turn takes minutes; the chain ticks in seconds. By the
        // time a cat finishes thinking, several more wakes for the same task
        // are waiting, and every one of them is superseded by the newest.
        // Acting on each in turn is how a cat spends two minutes submitting a
        // result the chain already has — observed live, as a string of
        // `AlreadySubmitted` refusals.
        //
        // This is the `Coalesce` aggregation policy from docs/CLI.md: fold
        // repeats of one thing into the latest one. Keep the newest wake per
        // (task, kind) and drop the rest unread.
        let my_hex = miot_keys::to_hex(&account);
        let mut mine: Vec<Entry> = batch.into_iter().filter(|e| e.wakes.as_deref() == Some(my_hex.as_str())).collect();
        let mut latest: std::collections::HashMap<(String, String), Entry> =
            std::collections::HashMap::new();
        for e in mine.drain(..) {
            let k = (
                e.effect.get("task").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                e.effect.get("t").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            );
            latest
                .entry(k)
                .and_modify(|cur| {
                    if e.seq > cur.seq {
                        *cur = e.clone();
                    }
                })
                .or_insert(e);
        }
        let mut mine: Vec<Entry> = latest.into_values().collect();
        mine.sort_by_key(|e| e.seq);

        for e in mine {
            if !seen.insert(e.seq) {
                continue;
            }
            let Some((prompt, tools)) = cat.prompt(&e, &question).await else { continue };
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
