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
use std::sync::Arc;
use miot_runtime::{client, AccountId, RuntimeCall};
use polkadot_sdk::*;
use serde::{Deserialize, Serialize};
use sp_core::H256;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::agent_state_machine::{self, Dispatch, Host, Inbound};
use crate::common::{parse_task, EventCursor, Roster};
use crate::ui::{self, ToolOut};

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
- You may call several tools in the same response. They all run at once, so \
don't make one depend on another's output within a response — call what you \
need, and act on the results when they come back.";

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
    /// `SendMessage` not included — that's `messages`.
    tool_calls: u32,
    messages: u32,
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
    /// Sign a GET with no query params — `b""`, same rule `node.rs`'s
    /// `require_client_auth` checks against. Every read below a node now
    /// gates on this envelope; this is the one place that builds it.
    fn signed_get(&self, path: &str) -> reqwest::RequestBuilder {
        self.signed_get_query(path, "")
    }

    fn signed_get_query(&self, path: &str, query: &str) -> reqwest::RequestBuilder {
        let headers = crate::node::sign_headers(&self.identity, query.as_bytes());
        self.http.get(format!("{}{path}", self.node)).headers(headers)
    }

    async fn head(&self) -> Option<serde_json::Value> {
        self.signed_get("/head").send().await.ok()?.json().await.ok()
    }

    async fn events(&self, since: u64) -> Vec<Entry> {
        let query = format!("since={since}");
        match self.signed_get_query(&format!("/events?{query}"), &query).send().await {
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
            match self.signed_get("/tasks").send().await {
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
        let v: serde_json::Value = self.signed_get("/meta").send().await.ok()?.json().await.ok()?;
        let genesis_hash = H256::from_slice(&hex::decode(v.get("genesis_hash")?.as_str()?).ok()?);
        Some(client::Meta {
            genesis_hash,
            spec_version: v.get("spec_version")?.as_u64()? as u32,
            tx_version: v.get("tx_version")?.as_u64()? as u32,
        })
    }

    async fn fetch_nonce(&self) -> u32 {
        let path = format!("/account/{}", miot_keys::to_hex(&self.account));
        match self.signed_get(&path).send().await {
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
    async fn submit(&self, call: RuntimeCall) -> Result<(), String> {
        const ATTEMPTS: u32 = 4;
        const BACKOFF_MS: [u64; 3] = [1000, 2000, 4000];

        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(BACKOFF_MS[attempt as usize - 1])).await;
                println!("{}", ui::note(&format!("{}: retrying submit (attempt {}/{ATTEMPTS}) — {last}", self.name, attempt + 1)));
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
                        last = "node unreachable (meta)".to_string();
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
                Ok(r) if r.status().is_success() => return Ok(()),
                Ok(r) => {
                    *self.nonce.lock().await = None;
                    let e: serde_json::Value = r.json().await.unwrap_or_default();
                    let msg = e.get("error").unwrap_or(&e).to_string();
                    if !(msg.contains("Stale") || msg.contains("Future")) {
                        return Err(format!("refused: {msg}"));
                    }
                    last = format!("refused: {msg}");
                }
                Err(e) => {
                    *self.nonce.lock().await = None;
                    last = format!("node unreachable: {e}");
                }
            }
        }
        Err(format!("gave up after {ATTEMPTS} attempts — {last}"))
    }

    /// Build the prompt for one woken event. The parent question is carried
    /// into every one: a turn is stateless, so the chain is the only memory.
    async fn prompt(&self, e: &Entry, question: &str) -> Option<(String, &'static str)> {
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
                // No "(use Peers for who's live)" hint any more: found live
                // 2026-09-24, qwen3-4b took it as an instruction and called
                // Peers on every single wake, never replying.
                //
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
                     members, by name: {}.\n\n{ack_note}{otr_note}",
                    others.join(", ")
                )
            }
            _ => return None,
        };
        Some((p, if t == "said" { "said" } else { "task" }))
    }

    /// A node-backed read — a query in `agent_state_machine`'s terms: it runs on its own
    /// and its result is fed back to the model on a later turn. `None`: not
    /// one of these.
    async fn query(&self, c: &miot_llm::Call) -> Option<ToolOut> {
        let out = match c.name.as_str() {
            // Merged: task-closed and standalone artifacts alike — asked for
            // live, 2026-09-23, "task artifacts should be accessible all the
            // same by id since they are on chain in session."
            "ArtifactList" => match self.signed_get("/artifacts").send().await {
                Ok(r) => match r.json::<Vec<serde_json::Value>>().await {
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
                        ToolOut::new("", true).meta(format!("{} artifacts", rows.len())).body(lines.join("\n"))
                    }
                    Err(e) => ToolOut::new("", false).meta("bad response").body(e.to_string()),
                },
                Err(e) => ToolOut::new("", false).meta("node unreachable").body(e.to_string()),
            },
            // A `t`-prefixed id (as `ArtifactList` renders a task's) is a
            // closed parent's report; anything else is a standalone id — the
            // two id spaces never collide as long as that prefix is kept.
            "ArtifactRead" => {
                let id = c.str("id").unwrap_or_default();
                let path = if id.trim_start().starts_with(['t', 'T']) { format!("/artifact/{id}") } else { format!("/note/{id}") };
                match self.signed_get(&path).send().await {
                    Ok(r) => match r.json::<serde_json::Value>().await {
                        Ok(v) if v.get("found").and_then(|f| f.as_bool()) == Some(true) => {
                            let body = v.get("body").and_then(|b| b.as_str()).unwrap_or("");
                            ToolOut::new(id, true).meta(ui::bytes(body.len())).body(body)
                        }
                        Ok(_) => ToolOut::new(id, false).meta("no such artifact"),
                        Err(e) => ToolOut::new(id, false).meta("bad response").body(e.to_string()),
                    },
                    Err(e) => ToolOut::new(id, false).meta("node unreachable").body(e.to_string()),
                }
            }
            "Peers" => match self.signed_get("/mesh/peers").send().await {
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
                                    let addr = route.split_once("://").map(|(_, r)| r).unwrap_or(route);
                                    match p.get("seen_ms_ago").and_then(|s| s.as_u64()) {
                                        Some(ms) => lines.push(format!("{name} — {role}, at {addr}, seen {ms}ms ago")),
                                        None => lines.push(format!("{name} — {role}, at {addr}")),
                                    }
                                }
                                None => lines.push(format!("{route} — never answered")),
                            }
                        }
                        for p in v.get("inbound").and_then(|p| p.as_array()).into_iter().flatten() {
                            let status = &p["status"];
                            let name = status["account"]
                                .as_str()
                                .and_then(|h| miot_keys::from_hex(h).ok())
                                .map(|a| self.roster.name_of(&a))
                                .unwrap_or_else(|| status["name"].as_str().unwrap_or("?").to_string());
                            let role = status["role"].as_str().unwrap_or("?");
                            let ms = p["seen_ms_ago"].as_u64().unwrap_or(0);
                            lines.push(format!("{name} — {role}, calls us but we can't reach it, heard {ms}ms ago"));
                        }
                        lines.push(format!("roster (configured, not all necessarily live): {}", self.roster.names().collect::<Vec<_>>().join(", ")));
                        ToolOut::new("", true).meta(format!("{} live", lines.len() - 1)).body(lines.join("\n"))
                    }
                    Err(e) => ToolOut::new("", false).meta("bad response").body(e.to_string()),
                },
                Err(e) => ToolOut::new("", false).meta("node unreachable").body(e.to_string()),
            },
            "Stats" => match self.signed_get("/stats").send().await {
                Ok(r) => match r.json::<Vec<serde_json::Value>>().await {
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
                                format!("{name} — {}", ui::stats_phrase(r))
                            })
                            .collect();
                        ToolOut::new("", true).meta(format!("{} cats", rows.len())).body(lines.join("\n"))
                    }
                    Err(e) => ToolOut::new("", false).meta("bad response").body(e.to_string()),
                },
                Err(e) => ToolOut::new("", false).meta("node unreachable").body(e.to_string()),
            },
            _ => return None,
        };
        Some(out)
    }

    /// A chain write — a record in `agent_state_machine`'s terms: signed, submitted, shown,
    /// never fed back (what it did arrives later as a chain event). The
    /// `ToolOut` says what was asked and whether the node took it.
    async fn record(&self, c: &miot_llm::Call) -> ToolOut {
        let task = c.str("task").unwrap_or_default();
        let refuse = |arg: String, why: &str| ToolOut::new(arg, false).meta(why.to_string());
        let (call, arg) = match c.name.as_str() {
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
                let Some(parent) = parse_task(&task) else { return refuse(task, "bad task id") };
                let arg = format!("{task} → {} sub-tasks", assignments.len());
                (RuntimeCall::Litter(pallet_litter::Call::plan { parent, assignments }), arg)
            }
            "TaskUpdate" => {
                let status = c.str("status").unwrap_or_default();
                let text = c.str("text").unwrap_or_default();
                let arg = format!("{task} {status}  {text}");
                let Some(id) = parse_task(&task) else { return refuse(arg, "bad task id") };
                let act = match status.as_str() {
                    "claim" => miot_primitives::Act::Claim,
                    "done" => miot_primitives::Act::Done,
                    "failed" => miot_primitives::Act::Failed,
                    "clear" => miot_primitives::Act::Clear,
                    "reopen" => miot_primitives::Act::Reopen,
                    "artifact" => miot_primitives::Act::Artifact,
                    _ => return refuse(arg, "unknown status"),
                };
                (RuntimeCall::Litter(pallet_litter::Call::update { task: id, act, text }), arg)
            }
            "TaskReassign" => {
                let to_name = c.str("to").unwrap_or_default();
                let arg = format!("{task} → {to_name}");
                let Some(to) = self.roster.account(&to_name) else { return refuse(arg, "no such cat") };
                let Some(id) = parse_task(&task) else { return refuse(arg, "bad task id") };
                (RuntimeCall::Litter(pallet_litter::Call::reassign { task: id, to }), arg)
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
                let body = c.str("body").unwrap_or_default();
                let raw_to = c.str("to").map(|s| s.trim().to_string()).unwrap_or_default();
                let to = match raw_to.as_str() {
                    "" => None,
                    n if matches!(n.trim_start_matches('@').to_ascii_lowercase().as_str(), "all" | "cats" | "litter") => None,
                    n => match self.roster.account(n) {
                        Some(a) => Some(a),
                        None => return refuse(format!("→ {n}  {body}"), "no such cat — not sent"),
                    },
                };
                let no_ack = c.args.get("no_ack").and_then(|v| v.as_bool()).unwrap_or(false);
                let off_record = c.args.get("off_record").and_then(|v| v.as_bool()).unwrap_or(false);
                let who = if to.is_some() { raw_to.trim_start_matches('@').to_string() } else { "litter".to_string() };
                let mut flags = Vec::new();
                if no_ack {
                    flags.push("no_ack");
                }
                if off_record {
                    flags.push("off record");
                }
                let flags = if flags.is_empty() { String::new() } else { format!("  ({})", flags.join(", ")) };
                (RuntimeCall::Litter(pallet_litter::Call::say { to, body: body.clone(), no_ack, off_record }), format!("→ {who}  {body}{flags}"))
            }
            "Artifact" => {
                let text = c.str("text").unwrap_or_default();
                let title = text.lines().next().unwrap_or("").to_string();
                let bytes = text.len();
                (RuntimeCall::Litter(pallet_litter::Call::publish_standalone_artifact { text }), format!("{title}  ({})", ui::bytes(bytes)))
            }
            "RequestCompaction" => (RuntimeCall::Litter(pallet_litter::Call::request_compaction {}), String::new()),
            other => return refuse(String::new(), &format!("{other}: not a chain write")),
        };
        match self.submit(call).await {
            Ok(()) => ToolOut::new(arg, true).meta("submitted"),
            Err(why) => ToolOut::new(arg, false).meta(why),
        }
    }
}

/// A cat as `agent_state_machine`'s host. The cat itself stays behind an `Arc` so a
/// spawned query or record can hold it past the call that started it.
struct CatHost(Arc<Cat>);

impl Host for CatHost {
    fn name(&self) -> &str {
        &self.0.name
    }
    fn tools(&self, kind: &'static str) -> Vec<miot_llm::Tool> {
        if kind == "said" { miot_llm::chat_tools() } else { task_tools() }
    }
    fn rules(&self) -> &'static str {
        AGENT_RULES
    }
    fn dispatch(&self, c: &miot_llm::Call) -> Dispatch {
        let cat = self.0.clone();
        let c = c.clone();
        match c.name.as_str() {
            "ArtifactList" | "ArtifactRead" | "Peers" | "Stats" => {
                Dispatch::Query(Box::pin(async move { cat.query(&c).await.unwrap_or_else(|| ToolOut::new("", false)) }))
            }
            "TaskPlan" | "TaskUpdate" | "TaskReassign" | "SendMessage" | "Artifact" | "RequestCompaction" => {
                Dispatch::Record(Box::pin(async move { Some(cat.record(&c).await) }))
            }
            _ => Dispatch::Unknown,
        }
    }
    fn about(&self) -> String {
        format!(
            "Where: a cat in the Akuma Miot litter, account {}, talking to its node at {}",
            miot_keys::short(&self.0.account),
            self.0.node
        )
    }

    /// Spoken to, a cat must always answer — `AGENT_RULES` tells it to use
    /// SendMessage, but a small model's plain-text reply would otherwise
    /// reach nobody. Found live testing `kot chat`'s tool calls, then
    /// confirmed here. Not for a `no_ack` wake: there, silence was asked
    /// for, and auto-replying would resurrect the ping-pong `no_ack` stops.
    fn spoke(&self, text: &str, ctx: &serde_json::Value) {
        let said = ctx.get("t").and_then(|v| v.as_str()) == Some("said");
        let no_ack = ctx.get("no_ack").and_then(|v| v.as_bool()).unwrap_or(false);
        if !said || no_ack {
            self.show(ui::note(&format!("plain text, no tool call — not sent: {}", text.lines().next().unwrap_or(""))));
            return;
        }
        // Back to whoever spoke, marked `no_ack` (the model never chose to
        // say this — it's a stand-in for a reply it skipped) and mirroring
        // the wake's `off_record` (a fallback for a message that was never
        // committed must not itself commit one).
        let to = ctx.get("from").and_then(|v| v.as_str()).and_then(|h| miot_keys::from_hex(h).ok());
        let who = to.as_ref().map(|a| self.0.roster.name_of(a)).unwrap_or_else(|| "litter".into());
        let off_record = ctx.get("off_record").and_then(|v| v.as_bool()).unwrap_or(false);
        let body = text.to_string();
        let cat = self.0.clone();
        tokio::spawn(async move {
            // A message all the same — the next stats report counts it.
            cat.stats.lock().await.messages += 1;
            let arg = format!("→ {who}  {body}  (auto, no_ack)");
            let out = match cat.submit(RuntimeCall::Litter(pallet_litter::Call::say { to, body, no_ack: true, off_record })).await {
                Ok(()) => ToolOut::new(arg, true).meta("submitted"),
                Err(why) => ToolOut::new(arg, false).meta(why),
            };
            println!("{}", ui::tool(&cat.name, "SendMessage", &out));
        });
    }

    /// Whole cumulative totals each time, not a delta, so a dropped report
    /// just gets corrected by the next one instead of drifting. One per
    /// turn: turns are minutes apart, finer than any timer would need.
    fn after_turn(&self, cost: &ui::TurnCost) {
        let cat = self.0.clone();
        let (tools, messages, tokens, ms) = (cost.tools as u32, cost.messages as u32, cost.total as u64, cost.ms);
        tokio::spawn(async move {
            let snapshot = {
                let mut s = cat.stats.lock().await;
                s.turns += 1;
                s.tool_calls += tools;
                s.messages += messages;
                s.tokens += tokens;
                s.ms += ms;
                *s
            };
            let _ = cat
                .submit(RuntimeCall::Litter(pallet_litter::Call::report_stats2 {
                    turns: snapshot.turns,
                    tool_calls: snapshot.tool_calls,
                    messages: snapshot.messages,
                    tokens: snapshot.tokens,
                    ms: snapshot.ms,
                }))
                .await;
        });
    }
}

pub async fn run(cfg: AgentConfig) {
    let account = cfg.identity.account();
    // mTLS pinned to the roster's accounts (`crate::tls`) — same trust
    // boundary as `client.rs`'s `Client`, since a cat's node connection is
    // just another caller of a node, not a special case.
    let trusted = cfg.roster.0.iter().map(|(_, a)| a.clone()).collect();
    let cat = Arc::new(Cat {
        name: cfg.name.clone(),
        identity: cfg.identity,
        account: account.clone(),
        node: cfg.node,
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(900))
            .use_preconfigured_tls(crate::tls::client_config(&cfg.identity, trusted))
            .build()
            .unwrap(),
        roster: cfg.roster,
        meta: tokio::sync::OnceCell::new(),
        nonce: tokio::sync::Mutex::new(None),
        stats: tokio::sync::Mutex::new(CatStats::default()),
    });
    let name = cat.name.clone();
    println!("{}", ui::note(&format!("{name} id={} node={} llm={}", miot_keys::short(&account), cat.node, cfg.llm.label())));

    let head = loop {
        match cat.head().await {
            Some(h) => break h,
            None => {
                println!("{}", ui::note(&format!("{name} waiting for the node...")));
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    };
    println!("{}", ui::note(&format!("{name} connected")));

    // The chain is one source of the inbox; the state machine's own tool
    // results are the other. This task only turns chain events into wakes.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(watch_chain(cat.clone(), head, tx));
    agent_state_machine::run(Arc::new(CatHost(cat)), cfg.llm, cfg.persona, rx).await;
}

/// Poll the node's `/events`, and send every event that wakes this cat into
/// the inbox as a prompt — newest per (task, kind), since a turn takes
/// minutes and the chain ticks in seconds. Also owns the session cursor:
/// `question`, what the litter is working on, and the checkpoint epoch.
async fn watch_chain(cat: Arc<Cat>, mut head: serde_json::Value, tx: tokio::sync::mpsc::UnboundedSender<Inbound>) {
    let name = cat.name.clone();
    let epoch = head["last_checkpoint"].as_u64().unwrap_or(0);
    let mut session = Session::load(&name, epoch);
    let mut cursor = EventCursor::new(session.cursor);
    let mut question = std::mem::take(&mut session.question);
    let mut seen: HashSet<u64> = HashSet::new();
    let my_hex = miot_keys::to_hex(&cat.account);

    loop {
        head = cat.head().await.unwrap_or(head);

        // A node that rebuilt its log (demoted, rewound, adopted a
        // checkpoint) restarts `seq`. `EventCursor` re-reads the new log
        // but skips everything up to what this cat already handled — it
        // used to jump to "the end" while the log was still empty, then
        // re-wake on every replayed message (found live 2026-09-24: kuro
        // answering a long-finished exchange after a rewind).
        if let Some(seq) = head["seq"].as_u64() {
            if cursor.check_head(seq) {
                println!("{}", ui::note(&format!("{name}: node's log rebuilt (seq {seq}); skipping what was already handled")));
                seen.clear();
            }
        }
        // Any change to the checkpoint — a routine compaction or an actual
        // fork rewind, treated alike (`docs/AGENT_SESSION_EPOCH.md`) — ends
        // this session: `question`, and the conversation itself, may name a
        // task the chain no longer remembers opening.
        if let Some(ep) = head["last_checkpoint"].as_u64() {
            if ep != session.epoch {
                let _ = tx.send(Inbound::Reset(format!("chain checkpoint moved ({} -> {ep})", session.epoch)));
                session.epoch = ep;
                question.clear();
            }
        }

        let batch: Vec<Entry> = cat.events(cursor.seq).await.into_iter().filter(|e| cursor.accept_at(e.seq, e.block)).collect();
        for e in &batch {
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

        // COALESCE — keep the newest per (task, kind), the `Coalesce`
        // policy from docs/CLI.md. The state machine folds whatever is
        // still queued when its current turn ends into the next one.
        let mut latest: HashMap<(String, String), Entry> = HashMap::new();
        for e in batch.into_iter().filter(|e| {
            // `no_ack` is the sender saying "this needs no reply" — a
            // closing remark. It used to wake everyone anyway, with only a
            // prompt line asking the model to stay quiet; found live
            // 2026-09-24, the GLM/OpenRouter cats answered it every time
            // and "Understood" / "Acknowledged" ran on for rounds. Now it
            // simply wakes no one.
            if e.effect.get("t").and_then(|v| v.as_str()) == Some("said") && e.effect.get("no_ack").and_then(|v| v.as_bool()) == Some(true) {
                return false;
            }
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
            let Some((text, kind)) = cat.prompt(&e, &question).await else { continue };
            let text = format!("[block {}] {text}", e.block);
            if tx.send(Inbound::Wake { text, kind, ctx: e.effect.clone() }).is_err() {
                return;
            }
        }

        if cursor.seq != session.cursor || question != session.question {
            session.cursor = cursor.seq;
            session.question = question.clone();
            session.save(&name);
        }

        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    }
}
