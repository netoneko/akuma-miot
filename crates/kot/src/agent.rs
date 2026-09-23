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
use serde::{Deserialize, Serialize};
use sp_core::H256;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::common::{parse_task, Roster};

/// This cat's local memory across a process restart — `docs/
/// AGENT_SESSION_EPOCH.md`. A turn is stateless and the chain is the only
/// channel between agents, but `question`/`cursor` are still plain Rust
/// locals today: a restart zeroes them and the cat re-derives both from a
/// full `/events?since=0` replay, which works only because the `opened`
/// event it needs usually hasn't aged out of the log yet. This makes that
/// explicit and correct instead of incidental.
///
/// Keyed by `epoch` — `last_checkpoint`, the chain's own name for "how far
/// back a rewind or compaction can reach." Any change to it, whether from a
/// routine `/clear` compaction or an actual fork rewind, is treated
/// uniformly as a session boundary (same rule `clear_all`'s own doc comment
/// already uses: "a new session, same chain") rather than trying to tell the
/// two apart — simpler, and always safe, since starting fresh is exactly
/// what happens today regardless. `seen` is deliberately not part of this:
/// re-waking on an already-resolved event is a cheap no-op turn, not a redo
/// worth persisting against.
#[derive(Debug, Serialize, Deserialize)]
struct Session {
    epoch: u64,
    cursor: u64,
    question: String,
}

impl Session {
    fn path(name: &str) -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::Path::new(&home).join(".akuma/kot").join(format!("{name}.session.json"))
    }

    /// Load this cat's saved session if it matches `epoch` — a stale one (the
    /// chain moved on while this cat was down) is exactly the case that
    /// should start fresh, not resume against events the log may no longer
    /// hold.
    fn load(name: &str, epoch: u64) -> Self {
        std::fs::read_to_string(Self::path(name))
            .ok()
            .and_then(|s| serde_json::from_str::<Session>(&s).ok())
            .filter(|s| s.epoch == epoch)
            .unwrap_or(Session { epoch, cursor: 0, question: String::new() })
    }

    fn save(&self, name: &str) {
        let path = Self::path(name);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string(self) {
            let _ = std::fs::write(path, s);
        }
    }
}

/// Appended to every persona. Found live, 2026-09-23: GLM's tool calls were
/// landing as `WrongKind`/`SubtasksOutstanding`/no-call-at-all often enough
/// to break a run. Two of the three causes are id confusion (a small model
/// substituting a parent id for a sub-task id or back) and simply not
/// calling a tool — both are cheap to head off with an explicit rule rather
/// than left implicit in each one-off prompt.
const AGENT_RULES: &str = "\n\nRules:\n\
- A task id like \"t1\" names a whole task; \"t1.2\" names one sub-task of \
it. Use exactly the id you were given for the action you are taking — never \
shorten a sub-task id to its parent, and never use a parent id where a \
sub-task id is asked for.\n\
- Always respond by calling at least one of the tools offered. Never reply \
with plain text alone. The one exception: a message that tells you it \
doesn't need a reply (no_ack) — there, calling nothing is the correct \
response, not a rule violation. Don't manufacture a reply just to have said \
something.\n\
- You may call several tools in the same response — for example Bash then \
SendMessage, or Artifact then TaskUpdate. Each one runs independently and \
asynchronously: none of them feed their result back to you, so make every \
call self-contained rather than depending on what an earlier one in the \
same response will return.";

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
    // `meta` never changes for this chain's life (no forkless upgrade path
    // here — see HANDOFF's "FRAME, executed natively... we do not
    // upgrade"), so one fetch, ever, is correct, not just cheaper.
    // `nonce` is this cat's own count of its own signed extrinsics — the
    // one thing here it never needed the node's help to know. Tracking it
    // locally instead of re-reading `/account` before every submit is what
    // actually fixes the concurrent-submit race found live 2026-09-23
    // (two calls in one turn both reading the same current nonce and both
    // signing it — reordering at the node can't rescue a genuinely
    // duplicate nonce, only a distinct one that arrived out of order).
    meta: tokio::sync::OnceCell<client::Meta>,
    nonce: tokio::sync::Mutex<Option<u32>>,
    /// This cat's own cumulative work stats — updated after every turn,
    /// reported on chain (`report_stats`) the same way, so any cat's
    /// `Stats` tool call and the operator's `GET /stats` both see it.
    stats: tokio::sync::Mutex<CatStats>,
}

#[derive(Default, Clone, Copy)]
struct CatStats {
    turns: u32,
    tool_calls: u32,
    tokens: u64,
    ms: u64,
}

/// A one-shot read worth retrying: unlike `head`/`events` (below), nothing
/// else re-issues these before the turn that needed them uses whatever
/// they returned, so a transient blip here isn't self-healing the way the
/// main poll loop's next tick is.
const READ_ATTEMPTS: u32 = 3;
const READ_RETRY_MS: u64 = 500;

impl Cat {
    // `head`/`events` are polled every ~700ms by `run`'s own loop
    // regardless of outcome — that loop *is* their retry, on a shorter
    // cadence than anything added here would be, so failing fast and
    // letting the next tick paper over a blip is correct, not an
    // oversight. Retrying inside these would only add latency nothing
    // downstream is waiting on.
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

    /// Used once per turn to build a `ClearanceNeeded` prompt — no outer
    /// loop re-issues this before that prompt goes out, so a blip here
    /// silently produces "(none outstanding)" instead of the real list.
    async fn tasks(&self) -> Vec<serde_json::Value> {
        for attempt in 0..READ_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(READ_RETRY_MS)).await;
            }
            match self.http.get(format!("{}/tasks", self.node)).send().await {
                Ok(r) => match r.json().await {
                    Ok(v) => return v,
                    Err(_) if attempt + 1 < READ_ATTEMPTS => continue,
                    Err(_) => return Vec::new(),
                },
                Err(_) if attempt + 1 < READ_ATTEMPTS => continue,
                Err(_) => return Vec::new(),
            }
        }
        Vec::new()
    }

    async fn fetch_meta(&self) -> Option<client::Meta> {
        let v: serde_json::Value = self.http.get(format!("{}/meta", self.node)).send().await.ok()?.json().await.ok()?;
        let genesis_hash = H256::from_slice(&hex::decode(v.get("genesis_hash")?.as_str()?).ok()?);
        Some(client::Meta {
            genesis_hash,
            spec_version: v.get("spec_version")?.as_u64()? as u32,
            tx_version: v.get("tx_version")?.as_u64()? as u32,
        })
    }

    async fn fetch_nonce(&self) -> u32 {
        let url = format!("{}/account/{}", self.node, miot_keys::to_hex(&self.account));
        match self.http.get(url).send().await {
            Ok(r) => r.json::<serde_json::Value>().await.ok().and_then(|v| v.get("nonce")?.as_u64()).unwrap_or(0) as u32,
            Err(_) => 0,
        }
    }

    /// Sign `call` and submit it, retrying a transient failure instead of
    /// dropping it — found live 2026-09-23 (`docs/LOCAL_SIM.md`): the old
    /// version made exactly one attempt, and the wake that produced `call`
    /// was already marked seen before that attempt happened, so a single
    /// dropped connection meant the action was gone for good, silently.
    ///
    /// `meta` is fetched once, ever, and cached — nothing on this chain
    /// ever changes it. `nonce` is read from the node once, then tracked
    /// locally and incremented under a lock before the next call can read
    /// it: this cat is the only signer for its own account, so it is
    /// already the authority on what its next nonce is. On any failure the
    /// cached nonce is dropped so the *next* attempt (retry or otherwise)
    /// resyncs from the chain — cheap, and correct after a rewind, a
    /// restart, or a nonce race this cat didn't cause.
    ///
    /// Not every failure is worth retrying: a node-unreachable error or a
    /// `Stale`/`Future` nonce might succeed next time (the node came back,
    /// or the nonce cache just resynced), but a business-logic refusal
    /// (`NotAuthorized`, `WrongKind`, `NoSuchTask`, ...) will fail
    /// identically every time — retrying it would just spend wall clock
    /// confirming what the first attempt already proved.
    async fn submit(&self, call: RuntimeCall) -> bool {
        const ATTEMPTS: u32 = 4;
        const BACKOFF_MS: [u64; 3] = [1000, 2000, 4000];

        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(BACKOFF_MS[attempt as usize - 1])).await;
                println!("  [{}] retrying submit (attempt {}/{ATTEMPTS})", self.name, attempt + 1);
            }

            let meta = match self.meta.get() {
                Some(m) => *m,
                None => match self.fetch_meta().await {
                    Some(m) => {
                        // A concurrent call may have already set it — same
                        // chain, same value either way, so losing the race
                        // here is fine.
                        let _ = self.meta.set(m);
                        m
                    }
                    None => {
                        println!("  [{}] node unreachable (meta)", self.name);
                        continue;
                    }
                },
            };
            let mut guard = self.nonce.lock().await;
            let nonce = match *guard {
                Some(n) => n,
                None => self.fetch_nonce().await,
            };
            *guard = Some(nonce + 1);
            drop(guard);
            let uxt = client::sign(&self.identity, call.clone(), nonce, &meta);
            match self.http.post(format!("{}/submit", self.node)).body(uxt.encode()).send().await {
                Ok(r) if r.status().is_success() => return true,
                Ok(r) => {
                    *self.nonce.lock().await = None;
                    let e: serde_json::Value = r.json().await.unwrap_or_default();
                    let msg = e.get("error").unwrap_or(&e).to_string();
                    println!("  [{}] refused: {msg}", self.name);
                    if !(msg.contains("Stale") || msg.contains("Future")) {
                        return false;
                    }
                }
                Err(e) => {
                    *self.nonce.lock().await = None;
                    println!("  [{}] node unreachable: {e}", self.name);
                }
            }
        }
        println!("  [{}] submit gave up after {ATTEMPTS} attempts", self.name);
        false
    }

    /// Build the prompt for one woken event. The parent question is carried
    /// into every one: a turn is stateless, so the chain is the only memory.
    async fn prompt(&self, e: &Entry, question: &str) -> Option<(String, Vec<miot_llm::Tool>)> {
        let t = e.effect.get("t")?.as_str()?;
        let task = e.effect.get("task").and_then(|v| v.as_str()).unwrap_or("t1");
        // Root only — the leader is a valid assignee, itself included
        // (`miot-tasks::plan` never barred it, only `RootNotAssignable`).
        let workers = || {
            self.roster
                .0
                .iter()
                .filter(|(n, _)| n != "root")
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
                "ClearanceNeeded" => {
                    // Used to tell the leader "results are in" without ever
                    // saying which sub-tasks, what they returned, or what id
                    // to call TaskUpdate with — the model had nothing to act
                    // on but the parent id in this header, which is exactly
                    // the id `clear`/`reopen` refuse (`Error::WrongKind`).
                    // Observed live, 2026-09-23: WrongKind and
                    // SubtasksOutstanding refusals, and turns with no tool
                    // call at all, tracing back to this gap.
                    let rows = self.tasks().await;
                    let prefix = format!("{task}.");
                    let lines: Vec<String> = rows
                        .iter()
                        .filter(|r| r.get("id").and_then(|v| v.as_str()).is_some_and(|id| id.starts_with(&prefix)))
                        .filter(|r| r.get("status").and_then(|v| v.as_str()) == Some("AwaitingClearance"))
                        .map(|r| {
                            let id = r.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                            let holder = r
                                .get("holder")
                                .and_then(|v| v.as_str())
                                .and_then(|h| miot_keys::from_hex(h).ok())
                                .map(|a| self.roster.name_of(&a))
                                .unwrap_or_else(|| "someone".into());
                            let (kind, text) = r
                                .get("outcome")
                                .and_then(|o| Some((o.get("kind")?.as_str()?, o.get("text")?.as_str()?)))
                                .unwrap_or(("done", ""));
                            let text: String = if text.chars().count() > 600 {
                                text.chars().take(600).chain(['…']).collect()
                            } else {
                                text.to_string()
                            };
                            format!("- {id} ({holder}, {kind}): {text}")
                        })
                        .collect();
                    let list = if lines.is_empty() {
                        "(none outstanding right now — check /tasks before acting)".to_string()
                    } else {
                        lines.join("\n")
                    };
                    format!(
                        "[clearance-needed: {task}]\nThe question: {question}\n\n\
                         Sub-tasks awaiting your decision:\n{list}\n\n\
                         For EACH one listed above, call TaskUpdate using ITS OWN id shown above \
                         (never {task}, the parent) with status=clear if it helps answer the \
                         question, or status=reopen (with text saying why not) if it doesn't."
                    )
                }
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
                // Found live 2026-09-23: asked to "discuss this with your
                // littermate" with no name given, two cats each invented a
                // plausible-sounding one and tried to SendMessage it —
                // refused, the round silently dropped. The roster is
                // already loaded locally; there was never a reason to make
                // the model guess it.
                let others: Vec<&str> = self.roster.names().filter(|n| *n != self.name.as_str()).collect();
                // A broadcast (no `to`) now wakes every live cat, not just
                // the one addressed (`Effect::wakes`, `Node::absorb`'s `"*"`
                // sentinel) — say so, so a cat woken by something meant for
                // the group doesn't feel obliged to manufacture a full
                // reply. Two sentences was always a ceiling, not a quota;
                // this makes the floor explicit too.
                let addressed = e.effect.get("to").map(|v| !v.is_null()).unwrap_or(false);
                let framing = if addressed {
                    "This was sent to you directly."
                } else {
                    "This was sent to the whole litter, not just you — reply if you have \
                     something to add, or a short acknowledgment is a complete answer too."
                };
                // Found live 2026-09-23: two cats, told a broadcast reply
                // was fine to just acknowledge, still ping-ponged
                // "thanks!"/"sounds good!" for a dozen turns — every
                // acknowledgment was itself something the *other* one felt
                // obliged to acknowledge. `no_ack` (`Effect::Said`) is the
                // sender's own signal that a message is a closing remark,
                // not something needing a reply at all; SendMessage now
                // takes it too, so the fix is symmetric instead of only
                // ever telling the *reader* not to bother.
                let ack_note = if e.effect.get("no_ack").and_then(|v| v.as_bool()).unwrap_or(false) {
                    "This is a closing remark, not a question — it does not need a reply. Doing \
                     nothing is the correct response unless you actually have something new."
                } else {
                    "Reply with SendMessage. Two sentences at most. If your reply is itself just \
                     a closing remark or acknowledgment rather than something that needs an \
                     answer, set SendMessage's no_ack to true so it doesn't bounce back to you."
                };
                // A message flagged `off_record` was never written into the
                // block log (`Effect::Said`'s doc comment; `Node::absorb`
                // leaves it out of the block body) — it only reached this
                // cat because it's live right now. A reply sent the normal
                // way *does* get committed, which would leak the substance
                // of an off-the-record exchange onto the chain even though
                // the original message never landed there — so the reply
                // has to carry the same flag for the "off the record" to
                // mean anything.
                let otr_note = if e.effect.get("off_record").and_then(|v| v.as_bool()).unwrap_or(false) {
                    " This was sent off the record — it was never written to the chain. If you \
                      reply, set SendMessage's off_record to true as well, or your reply will be \
                      committed even though this message wasn't."
                } else {
                    ""
                };
                format!(
                    "{who} said to the litter:\n\"{body}\"\n\n{framing} The litter's other \
                     members, by name (use Peers for who's actually live right now): {}.\n\n{ack_note}{otr_note}",
                    others.join(", ")
                )
            }
            _ => return None,
        };
        let tools = if t == "said" { miot_llm::chat_tools() } else { task_tools() };
        Some((p, tools))
    }

    /// Stubs: run locally, log the result, done. No sandbox, no output fed
    /// back to the model — a turn is one LLM call in, tool calls out, with no
    /// loop that would let it see what came back and react.
    async fn act_local(&self, c: &miot_llm::Call) -> bool {
        match c.name.as_str() {
            "Bash" => {
                let command = c.str("command").unwrap_or_default();
                let run = tokio::process::Command::new("/bin/sh").arg("-c").arg(&command).output();
                match tokio::time::timeout(std::time::Duration::from_secs(30), run).await {
                    Ok(Ok(out)) => println!(
                        "  [{}] bash `{command}` exit={:?}\n{}{}",
                        self.name,
                        out.status.code(),
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr),
                    ),
                    Ok(Err(e)) => println!("  [{}] bash `{command}` failed to spawn: {e}", self.name),
                    Err(_) => println!("  [{}] bash `{command}` timed out after 30s", self.name),
                }
            }
            "ReadFile" => {
                let path = c.str("path").unwrap_or_default();
                match tokio::fs::read_to_string(&path).await {
                    Ok(s) => println!("  [{}] read {path} ({} bytes):\n{s}", self.name, s.len()),
                    Err(e) => println!("  [{}] read {path} failed: {e}", self.name),
                }
            }
            "WriteFile" => {
                let path = c.str("path").unwrap_or_default();
                let content = c.str("content").unwrap_or_default();
                match tokio::fs::write(&path, &content).await {
                    Ok(()) => println!("  [{}] wrote {path} ({} bytes)", self.name, content.len()),
                    Err(e) => println!("  [{}] write {path} failed: {e}", self.name),
                }
            }
            // Merged: task-closed and standalone artifacts alike — asked for
            // live, 2026-09-23, "task artifacts should be accessible all the
            // same by id since they are on chain in session."
            "ArtifactList" => match self.http.get(format!("{}/artifacts", self.node)).send().await {
                Ok(r) => match r.json::<Vec<serde_json::Value>>().await {
                    Ok(rows) if rows.is_empty() => println!("  [{}] no artifacts yet", self.name),
                    Ok(rows) => {
                        let lines: Vec<String> = rows
                            .iter()
                            .map(|r| {
                                let id = r.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                                let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
                                let author = r
                                    .get("author")
                                    .and_then(|v| v.as_str())
                                    .and_then(|h| miot_keys::from_hex(h).ok())
                                    .map(|a| self.roster.name_of(&a))
                                    .unwrap_or_else(|| "someone".into());
                                format!("{id}: {title} ({author})")
                            })
                            .collect();
                        println!("  [{}] artifacts:\n{}", self.name, lines.join("\n"));
                    }
                    Err(e) => println!("  [{}] ArtifactList: bad response: {e}", self.name),
                },
                Err(e) => println!("  [{}] ArtifactList: node unreachable: {e}", self.name),
            },
            // A `t`-prefixed id (as `ArtifactList` renders a task's) is a
            // closed parent's report; anything else is a standalone id — the
            // two id spaces never collide as long as that prefix is kept.
            "ArtifactRead" => {
                let id = c.str("id").unwrap_or_default();
                let path = if id.trim_start().starts_with(['t', 'T']) { format!("/artifact/{id}") } else { format!("/note/{id}") };
                match self.http.get(format!("{}{path}", self.node)).send().await {
                    Ok(r) => match r.json::<serde_json::Value>().await {
                        Ok(v) if v.get("found").and_then(|f| f.as_bool()) == Some(true) => {
                            println!("  [{}] artifact {id}:\n{}", self.name, v.get("body").and_then(|b| b.as_str()).unwrap_or(""));
                        }
                        Ok(_) => println!("  [{}] no artifact {id}", self.name),
                        Err(e) => println!("  [{}] ArtifactRead {id}: bad response: {e}", self.name),
                    },
                    Err(e) => println!("  [{}] ArtifactRead {id}: node unreachable: {e}", self.name),
                }
            }
            "Peers" => match self.http.get(format!("{}/mesh/peers", self.node)).send().await {
                Ok(r) => match r.json::<serde_json::Value>().await {
                    Ok(v) => {
                        let me_role = v.get("me").and_then(|m| m.get("role")).and_then(|r| r.as_str()).unwrap_or("?");
                        let mut lines = vec![format!("{} — me, {me_role}", self.name)];
                        for p in v.get("peers").and_then(|p| p.as_array()).into_iter().flatten() {
                            let route = p.get("route").and_then(|r| r.as_str()).unwrap_or("?");
                            match p.get("status").filter(|s| !s.is_null()) {
                                Some(status) => {
                                    let name = status
                                        .get("account")
                                        .and_then(|a| a.as_str())
                                        .and_then(|h| miot_keys::from_hex(h).ok())
                                        .map(|a| self.roster.name_of(&a))
                                        .unwrap_or_else(|| route.to_string());
                                    let role = status.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                                    match p.get("seen_ms_ago").and_then(|s| s.as_u64()) {
                                        Some(ms) => lines.push(format!("{name} — {role}, seen {ms}ms ago")),
                                        None => lines.push(format!("{name} — {role}")),
                                    }
                                }
                                None => lines.push(format!("{route} — never answered")),
                            }
                        }
                        println!("  [{}] peers (live):\n  {}\n  [{}] roster (configured, not all necessarily live): {}", self.name, lines.join("\n  "), self.name, self.roster.names().collect::<Vec<_>>().join(", "));
                    }
                    Err(e) => println!("  [{}] Peers: bad response: {e}", self.name),
                },
                Err(e) => println!("  [{}] Peers: node unreachable: {e}", self.name),
            },
            "Stats" => match self.http.get(format!("{}/stats", self.node)).send().await {
                Ok(r) => match r.json::<Vec<serde_json::Value>>().await {
                    Ok(rows) if rows.is_empty() => println!("  [{}] no stats reported yet", self.name),
                    Ok(rows) => {
                        let lines: Vec<String> = rows
                            .iter()
                            .map(|r| {
                                let name = r
                                    .get("account")
                                    .and_then(|v| v.as_str())
                                    .and_then(|h| miot_keys::from_hex(h).ok())
                                    .map(|a| self.roster.name_of(&a))
                                    .unwrap_or_else(|| "someone".into());
                                let turns = r.get("turns").and_then(|v| v.as_u64()).unwrap_or(0);
                                let tool_calls = r.get("tool_calls").and_then(|v| v.as_u64()).unwrap_or(0);
                                let tokens = r.get("tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                let ms = r.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
                                format!("{name} — {turns} turns, {tool_calls} tool calls, {tokens} tok, {:.1}s thinking", ms as f64 / 1000.0)
                            })
                            .collect();
                        println!("  [{}] stats:\n  {}", self.name, lines.join("\n  "));
                    }
                    Err(e) => println!("  [{}] Stats: bad response: {e}", self.name),
                },
                Err(e) => println!("  [{}] Stats: node unreachable: {e}", self.name),
            },
            _ => return false,
        }
        true
    }

    async fn act(&self, c: &miot_llm::Call) {
        if self.act_local(c).await {
            return;
        }
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
            // `to` was silently dropped here until found live running a
            // two-cat debate: every reply went out as a broadcast
            // (`to: None`) no matter what the model asked for, so a cat
            // could never actually address another cat by name — only the
            // operator's own `kot say --to` (a different code path,
            // `client.rs`) ever worked. `@all`/`@cats`/`@litter` stay a
            // broadcast (`docs/CLI.md`'s synonyms); anything else must
            // resolve in the roster, or the message doesn't go out at all —
            // silently broadcasting a misspelled name would put words in
            // front of the wrong audience.
            "SendMessage" => {
                let to = match c.str("to").as_deref().map(str::trim) {
                    None | Some("") => None,
                    Some(n) if matches!(n.trim_start_matches('@').to_ascii_lowercase().as_str(), "all" | "cats" | "litter") => None,
                    Some(n) => match self.roster.account(n) {
                        Some(a) => Some(a),
                        None => {
                            println!("  [{}] SendMessage: no such cat {n:?} in the roster — not sent", self.name);
                            return;
                        }
                    },
                };
                let no_ack = c.args.get("no_ack").and_then(|v| v.as_bool()).unwrap_or(false);
                let off_record = c.args.get("off_record").and_then(|v| v.as_bool()).unwrap_or(false);
                RuntimeCall::Litter(pallet_litter::Call::say { to, body: c.str("body").unwrap_or_default(), no_ack, off_record })
            }
            "Artifact" => RuntimeCall::Litter(pallet_litter::Call::publish_standalone_artifact { text: c.str("text").unwrap_or_default() }),
            "RequestCompaction" => RuntimeCall::Litter(pallet_litter::Call::request_compaction {}),
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
        persona: format!("{}{AGENT_RULES}", cfg.persona),
        roster: cfg.roster,
        meta: tokio::sync::OnceCell::new(),
        nonce: tokio::sync::Mutex::new(None),
        stats: tokio::sync::Mutex::new(CatStats::default()),
    };
    let name = cat.name.clone();
    println!("[{name}] id={} node={} llm={}", miot_keys::short(&account), cat.node, cat.llm.label());

    let mut head = loop {
        match cat.head().await {
            Some(h) => break h,
            None => {
                println!("[{name}] waiting for the node...");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    };
    println!("[{name}] connected.");

    let epoch = head["last_checkpoint"].as_u64().unwrap_or(0);
    let mut session = Session::load(&name, epoch);
    let mut cursor = session.cursor;
    let mut question = std::mem::take(&mut session.question);
    let mut seen: HashSet<u64> = HashSet::new();
    let my_hex = miot_keys::to_hex(&account);

    loop {
        head = cat.head().await.unwrap_or(head);

        // A node that rebuilt its log (demoted, rewound, adopted a
        // checkpoint) restarts `seq`. A cursor past the new end would go
        // deaf until the log grew back past it, so jump to the new end.
        // Skipping what's there is right: those wakes are old news, and the
        // chain re-issues anything still outstanding on its own.
        if let Some(seq) = head["seq"].as_u64() {
            if seq < cursor {
                println!("[{name}] node's log restarted (seq {seq} < cursor {cursor}); resuming from its end");
                cursor = seq;
                seen.clear();
            }
        }
        // Any change to the checkpoint — a routine compaction or an actual
        // fork rewind, treated alike (`docs/AGENT_SESSION_EPOCH.md`) — ends
        // this session: `question` may name a task the chain no longer
        // remembers opening.
        if let Some(ep) = head["last_checkpoint"].as_u64() {
            if ep != session.epoch {
                println!("[{name}] chain checkpoint moved ({} -> {ep}); starting a new session", session.epoch);
                session.epoch = ep;
                question.clear();
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
        for e in batch.into_iter().filter(|e| {
            match e.wakes.as_deref() {
                Some(w) if w == my_hex => true,
                // The broadcast sentinel (`Node::absorb`) wakes every live
                // cat but the one that sent it — otherwise a cat's own
                // broadcast would wake itself and it'd start replying to
                // its own message.
                Some("*") => e.effect.get("from").and_then(|v| v.as_str()) != Some(my_hex.as_str()),
                _ => false,
            }
        }) {
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
            let Some((prompt, tools)) = cat.prompt(&e, &question).await else { continue };
            let t = e.effect.get("t").and_then(|v| v.as_str()).unwrap_or("");
            println!("[{name}] block {} {t} — thinking", e.block);
            match cat.llm.turn(&cat.persona, &prompt, tools).await {
                Ok(turn) => {
                    if turn.calls.is_empty() {
                        println!("[{name}]   no tool call ({} tok) — turn wasted", turn.tokens);
                    }
                    // Several calls in one turn run concurrently, not one
                    // after another — none of them feeds a result back for
                    // the next to react to (`AGENT_RULES`), so there is
                    // nothing sequencing them.
                    for c in &turn.calls {
                        println!("[{name}]   {} ({} tok, {:.0}s)", c.name, turn.tokens, turn.ms as f64 / 1000.0);
                    }
                    futures_util::future::join_all(turn.calls.iter().map(|c| cat.act(c))).await;

                    // Spoken to, a cat must always answer — `AGENT_RULES`
                    // tells it to call SendMessage every time, but a small
                    // model reaching for Bash/ReadFile instead (or a plain-
                    // text reply the tool-only path above has no way to
                    // send) left the asker hearing nothing back. Found live
                    // testing `kot chat`'s tool calls, then confirmed here:
                    // same gap, same fix — don't require the model to pick
                    // SendMessage on purpose.
                    //
                    // Except when the wake itself was `no_ack`: there, a
                    // turn that produced neither a reply nor a tool call is
                    // the *correct* outcome (silence, as asked), not the
                    // dropped-reply bug this fallback exists to paper over
                    // — auto-replying anyway would just resurrect the
                    // ping-pong loop `no_ack` exists to stop.
                    let was_no_ack = e.effect.get("no_ack").and_then(|v| v.as_bool()).unwrap_or(false);
                    if t == "said" && !was_no_ack && !turn.calls.iter().any(|c| c.name == "SendMessage") {
                        let body = if !turn.text.trim().is_empty() {
                            turn.text.trim().to_string()
                        } else if !turn.calls.is_empty() {
                            format!("(ran {} — no further reply)", turn.calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", "))
                        } else {
                            String::new()
                        };
                        if !body.is_empty() {
                            // Back to whoever actually spoke, not a
                            // broadcast further — a fallback reply is
                            // naturally a reply *to* someone, addressed
                            // the same way the direct-reply path already
                            // is. (Before 2026-09-23 this also mattered
                            // because a non-root broadcast woke nobody at
                            // all; broadcasts wake everyone now, but
                            // targeting the original speaker is still the
                            // right shape for "here's what I did.")
                            let to = e.effect.get("from").and_then(|v| v.as_str()).and_then(|h| miot_keys::from_hex(h).ok());
                            println!("[{name}]   (auto) SendMessage: {body}");
                            // The model never chose to say this — it's a
                            // synthesized status note standing in for a
                            // reply it skipped, not a question. Marking it
                            // `no_ack` keeps a fallback from starting (or
                            // extending) a ping-pong nobody actually meant.
                            // Mirrors the wake's own `off_record`: a
                            // fallback for a message that was never
                            // committed must not itself commit one — see
                            // `otr_note` above for why that matters.
                            let off_record = e.effect.get("off_record").and_then(|v| v.as_bool()).unwrap_or(false);
                            cat.submit(RuntimeCall::Litter(pallet_litter::Call::say { to, body, no_ack: true, off_record })).await;
                        }
                    }

                    // Periodic, one report per turn rather than a timer:
                    // turns here are already minutes apart, so "after every
                    // turn" is a finer cadence than a wall-clock interval
                    // would need to be, with no extra bookkeeping. Whole
                    // cumulative totals each time, not a delta, so a
                    // dropped report (same no-retry-queue gap `submit`
                    // itself now retries around, but a report is fire-and-
                    // forget by design — worth knowing, not worth an
                    // agent's own turn budget defending) just gets
                    // corrected by the next one instead of drifting.
                    let snapshot = {
                        let mut s = cat.stats.lock().await;
                        s.turns += 1;
                        s.tool_calls += turn.calls.len() as u32;
                        s.tokens += turn.total_tokens as u64;
                        s.ms += turn.ms;
                        *s
                    };
                    cat.submit(RuntimeCall::Litter(pallet_litter::Call::report_stats {
                        turns: snapshot.turns,
                        tool_calls: snapshot.tool_calls,
                        tokens: snapshot.tokens,
                        ms: snapshot.ms,
                    }))
                    .await;
                }
                Err(e) => println!("[{name}]   llm error: {e}"),
            }
        }

        if cursor != session.cursor || question != session.question {
            session.cursor = cursor;
            session.question = question.clone();
            session.save(&name);
        }

        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    }
}
