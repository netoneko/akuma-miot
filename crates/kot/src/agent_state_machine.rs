//! The one agent loop — `kot chat` and a cat under `kot run` both think
//! through this. Only *where input comes from*, *which extra tools exist*
//! and *how things are shown* differ between them, and those live in each
//! [`Host`]; the logic is the same everywhere.
//!
//! This is `docs/MAPPING_REPORT.md` §2.3 ("decoupled, async, aggregating"),
//! which the first agent loop shortcut: it awaited every tool, printed the
//! output, and threw it away — so a model that asked `Peers` or ran `Bash`
//! never saw the answer and asked again on the next wake.
//!
//! - **Inbox.** Two kinds of inbound traffic: [`Inbound::Wake`] (something
//!   worth a turn — an operator's line, a chain wake rendered into a prompt)
//!   and tool results. Both queue while a turn is in flight; a turn is
//!   never cancelled for them.
//! - **Queries run on their own.** A query tool (`Bash`, `ReadFile`,
//!   `Peers`, ...) is spawned, not awaited; its result lands in the inbox
//!   and is fed to the model on a later turn, labelled with an id.
//! - **Records are fire-and-forget.** A write (a chain extrinsic, a reply to
//!   the operator) is spawned and shown, never fed back — confirmation, if
//!   there is any, arrives later as a chain event like anything else.
//! - **Aggregation.** A wake assembles a turn at once, folding in whatever
//!   results are already there. Results alone wait until every outstanding
//!   query is back, or [`RESULTS_DEADLINE`], whichever is first — and a run
//!   of result-only turns is capped ([`MAX_FOLLOWUPS`]) so a model can't
//!   tool-call itself into a loop with nobody speaking to it.
//! - **One conversation.** History accumulates for both hosts, with the same
//!   budget warnings and compaction (`TokenBudget`/`Compact`/`BrowseTools`/
//!   `Inspect`), and the same `AboutMe`.

use crate::ui::{self, ToolOut, TurnCost};
use miot_llm::{Call, Llm, Speaker, Tool};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How long results with no wake wait for the rest of their batch.
pub const RESULTS_DEADLINE: Duration = Duration::from_secs(10);
/// Result-driven turns in a row, with no new wake, before results are held
/// back (kept for `Inspect`, not fed) until something actually wakes us.
pub const MAX_FOLLOWUPS: u32 = 4;
/// A single result fed back is cut to this; the full text stays in the
/// tool log for `Inspect`.
const FEED_CHARS: usize = 3000;
const BASH_TIMEOUT: Duration = Duration::from_secs(30);

pub type Query = Pin<Box<dyn Future<Output = ToolOut> + Send>>;
/// `None`: the host already showed it its own way.
pub type Record = Pin<Box<dyn Future<Output = Option<ToolOut>> + Send>>;

pub enum Dispatch {
    /// Runs on its own; the result comes back into the inbox.
    Query(Query),
    /// Runs on its own; shown, never fed back.
    Record(Record),
    /// Not this host's tool.
    Unknown,
}

pub enum Inbound {
    /// Worth a turn. `kind` picks the host's tool set for it; `ctx` is
    /// whatever the host wants back in [`Host::spoke`] (a chain effect, or
    /// nothing).
    Wake { text: String, kind: &'static str, ctx: serde_json::Value },
    /// Start over: the chain moved to a new checkpoint, the conversation
    /// may name things it no longer remembers.
    Reset(String),
}

/// What differs between `kot chat` and a cat.
pub trait Host: Send + Sync + 'static {
    /// Who is calling the tools — leads every line shown.
    fn name(&self) -> &str;
    /// The host's own tools for a wake of `kind` — the shared ones
    /// ([`shared_tools`]) are added, and win on a name clash.
    fn tools(&self, kind: &'static str) -> Vec<Tool>;
    /// The host's own rules, appended to the persona after [`RULES`].
    fn rules(&self) -> &'static str;
    fn dispatch(&self, c: &Call) -> Dispatch;
    /// Extra facts for `AboutMe` beyond model/platform/build/persona.
    fn about(&self) -> String {
        String::new()
    }
    /// The model answered in plain text and called nothing. `ctx` is the
    /// newest wake's.
    fn spoke(&self, text: &str, ctx: &serde_json::Value);
    /// After every turn — a cat reports its stats on chain from here.
    fn after_turn(&self, _cost: &TurnCost) {}
    /// Nothing in flight, nothing queued — `kot chat` shows its prompt.
    fn idle(&self) {}
    fn show(&self, s: String) {
        println!("{s}");
    }
}

/// Appended to every persona, both hosts. The model has to know results
/// come back, and how, or it keeps behaving as if they don't.
pub const RULES: &str = "\n\nHow tools work here:\n\
- Tools like Bash, ReadFile, Peers, ArtifactRead, AboutMe run in the background. You \
don't get their output in the same response — it comes back to you in a later message, \
each one labelled [#id ToolName]. So call what you need, then act on the results when \
they arrive; you don't have to reply in the same response you call a tool.\n\
- Writes (SendMessage, TaskUpdate, Artifact, ...) are final: they just happen, and \
nothing comes back from them.\n\
- AboutMe tells you who you are: your persona, model, and what you're running on.\n\
- TokenBudget tells you how much context you have left. BrowseTools lists past tool \
results (id, name, preview); Inspect pulls one back by id. Compact replaces the \
conversation so far with a summary you write, to free room — past tool results survive \
it.";

/// Tools every host gets and this module runs itself.
pub fn shared_tools() -> Vec<Tool> {
    let mut t = vec![miot_llm::about_me_tool()];
    t.extend(miot_llm::local_tools());
    t.extend(miot_llm::budget_tools());
    t
}

enum Back {
    Result(String, ToolOut),
    RecordDone,
}

struct AgentStateMachine<H: Host> {
    host: Arc<H>,
    llm: Arc<Llm>,
    persona: String,
    system: String,
    window: Option<u32>,
    history: Vec<(Speaker, String)>,
    /// Every query result, full text, by id — survives compaction.
    tool_log: Vec<(String, String)>,
    warned_tier: u32,
    pending_warning: Option<String>,
    kinds: Vec<&'static str>,
    ctx: serde_json::Value,
    back_tx: mpsc::UnboundedSender<Back>,
    queries: usize,
    records: usize,
    followups: u32,
}

/// Think until `inbox` closes and nothing is left in flight.
pub async fn run<H: Host>(host: Arc<H>, llm: Llm, persona: String, mut inbox: mpsc::UnboundedReceiver<Inbound>) {
    let window = llm.context_window().await;
    let (back_tx, mut back_rx) = mpsc::unbounded_channel();
    let system = format!("{persona}{RULES}{}", host.rules());
    let mut m = AgentStateMachine {
        host,
        llm: Arc::new(llm),
        persona,
        system,
        window,
        history: Vec::new(),
        tool_log: Vec::new(),
        warned_tier: 0,
        pending_warning: None,
        kinds: Vec::new(),
        ctx: serde_json::Value::Null,
        back_tx,
        queries: 0,
        records: 0,
        followups: 0,
    };
    let mut open = true;
    let mut was_idle = false;

    loop {
        if m.queries == 0 && m.records == 0 {
            if !open {
                break;
            }
            if !was_idle {
                m.host.idle();
                was_idle = true;
            }
        }

        // Wait for anything at all.
        let mut wakes: Vec<(String, &'static str, serde_json::Value)> = Vec::new();
        let mut results: Vec<(usize, String)> = Vec::new();
        tokio::select! {
            got = inbox.recv(), if open => match got {
                Some(i) => m.take(i, &mut wakes),
                None => open = false,
            },
            Some(b) = back_rx.recv() => m.back(b, &mut results),
        }

        // Aggregate: everything already queued, then — for results with no
        // wake — the rest of the batch, up to the deadline.
        while let Ok(i) = inbox.try_recv() {
            m.take(i, &mut wakes);
        }
        while let Ok(b) = back_rx.try_recv() {
            m.back(b, &mut results);
        }
        if wakes.is_empty() && !results.is_empty() && m.queries > 0 {
            let until = tokio::time::Instant::now() + RESULTS_DEADLINE;
            while m.queries > 0 {
                tokio::select! {
                    Some(b) = back_rx.recv() => m.back(b, &mut results),
                    got = inbox.recv(), if open => match got {
                        Some(i) => { m.take(i, &mut wakes); break; }
                        None => open = false,
                    },
                    _ = tokio::time::sleep_until(until) => break,
                }
            }
        }

        if wakes.is_empty() && results.is_empty() {
            continue;
        }
        if wakes.is_empty() {
            if m.followups >= MAX_FOLLOWUPS {
                m.host.show(ui::note(&format!(
                    "{} result(s) held back — {MAX_FOLLOWUPS} follow-up turns in a row with nobody speaking; Inspect still has them",
                    results.len()
                )));
                continue;
            }
            m.followups += 1;
        } else {
            m.followups = 0;
        }
        was_idle = false;
        m.turn(wakes, results).await;
    }
}

impl<H: Host> AgentStateMachine<H> {
    fn take(&mut self, i: Inbound, wakes: &mut Vec<(String, &'static str, serde_json::Value)>) {
        match i {
            Inbound::Wake { text, kind, ctx } => wakes.push((text, kind, ctx)),
            Inbound::Reset(why) => {
                self.host.show(ui::note(&format!("new session — {why}")));
                self.history.clear();
                self.warned_tier = 0;
                self.pending_warning = None;
            }
        }
    }

    /// A query result: shown now (the truthful moment it finished), queued
    /// for the next turn by id.
    fn back(&mut self, b: Back, results: &mut Vec<(usize, String)>) {
        match b {
            Back::RecordDone => self.records = self.records.saturating_sub(1),
            Back::Result(name, out) => {
                self.queries = self.queries.saturating_sub(1);
                self.host.show(ui::tool(self.host.name(), &name, &out));
                let id = self.tool_log.len();
                self.tool_log.push((name, out.text()));
                results.push((id, clip(&self.tool_log[id].1, FEED_CHARS)));
            }
        }
    }

    fn tools(&self) -> Vec<Tool> {
        let mut t = shared_tools();
        for k in &self.kinds {
            for tool in self.host.tools(k) {
                if !t.iter().any(|x| x.name == tool.name) {
                    t.push(tool);
                }
            }
        }
        t
    }

    async fn turn(&mut self, wakes: Vec<(String, &'static str, serde_json::Value)>, results: Vec<(usize, String)>) {
        let name = self.host.name().to_string();
        if !wakes.is_empty() {
            self.kinds = wakes.iter().map(|w| w.1).collect();
            self.kinds.dedup();
            self.ctx = wakes.last().map(|w| w.2.clone()).unwrap_or_default();
        }

        let mut msg: Vec<String> = wakes.iter().map(|w| w.0.clone()).collect();
        if !results.is_empty() {
            let rows: Vec<String> = results.iter().map(|(id, text)| format!("[#{id} {}] {text}", self.tool_log[*id].0)).collect();
            msg.push(format!("Results of tools you called:\n{}", rows.join("\n\n")));
        }
        let why = if wakes.is_empty() { format!("{} result(s) back", results.len()) } else { summary(&wakes[wakes.len() - 1].0) };
        self.host.show(ui::thinking(&name, &why));
        self.history.push((Speaker::User, msg.join("\n\n")));

        let system_now = match self.pending_warning.take() {
            Some(w) => format!("{}\n\n{w}", self.system),
            None => self.system.clone(),
        };
        let turn = match self.llm.converse(&system_now, &self.history, self.tools()).await {
            Ok(t) => t,
            Err(e) => {
                self.host.show(ui::note(&format!("llm error: {e}")));
                // A failed turn never happened, as far as history goes.
                self.history.pop();
                return;
            }
        };
        let cost = TurnCost {
            prompt: turn.prompt_tokens,
            out: turn.tokens,
            total: turn.total_tokens,
            window: self.window,
            ms: turn.ms,
            tools: turn.calls.len(),
        };
        self.host.show(ui::turn(&name, &cost));

        // The model's own side of the conversation: only what it actually
        // said. Its calls are *not* written in here as text — found live,
        // 2026-09-24: with `[called: Bash{...}]` in its own past turns,
        // qwen3-4b started typing `[called: SendMessage{...}]` as its reply
        // instead of calling the tool. What it asked for is recoverable
        // anyway: every result comes back labelled with its tool and
        // argument (`[#3 Bash] $ uname -sm`). A tool-only turn leaves no
        // assistant message, and the results follow as the next user one.
        let own = turn.text.trim();
        if !own.is_empty() {
            self.history.push((Speaker::Assistant, own.to_string()));
        }

        let mut compacted = false;
        for c in &turn.calls {
            match c.name.as_str() {
                "Compact" => {
                    let summary = c.str("summary").unwrap_or_default();
                    self.host.show(ui::note(&format!(
                        "compacted: history replaced with a {}-char summary the model wrote; {} tool results kept",
                        summary.len(),
                        self.tool_log.len()
                    )));
                    self.history = vec![(Speaker::Assistant, summary)];
                    self.warned_tier = 0;
                    compacted = true;
                }
                // Instant, but still a result: it comes back like any other.
                "TokenBudget" | "BrowseTools" | "Inspect" | "AboutMe" => {
                    let out = self.session_tool(c, turn.total_tokens);
                    self.queries += 1;
                    let _ = self.back_tx.send(Back::Result(c.name.clone(), out));
                }
                _ => self.dispatch(c),
            }
        }
        if turn.calls.is_empty() {
            let text = turn.text.trim();
            if text.is_empty() {
                self.host.show(ui::note("no tool call, no text — turn wasted"));
            } else {
                self.host.spoke(text, &self.ctx);
            }
        }
        self.host.after_turn(&cost);

        // Against what this turn actually cost.
        if let Some(pct) = pct_used(turn.total_tokens, self.window) {
            if pct >= miot_llm::FORCE_COMPACT_PCT && !compacted {
                self.host.show(ui::note(&format!("{pct}% of the context window used — force-compacting")));
                let summary = summarize(&self.llm, &self.system, &self.history).await;
                self.history = vec![(Speaker::Assistant, summary)];
                self.warned_tier = 0;
            } else if let Some(tier) = miot_llm::budget_checkpoint(pct, self.warned_tier) {
                self.warned_tier = tier;
                self.pending_warning = Some(format!(
                    "⚠ context budget: {pct}% of your window used. Consider calling Compact soon \
                     (past tool results survive it) — this session force-compacts at {}%.",
                    miot_llm::FORCE_COMPACT_PCT
                ));
            }
        }
    }

    fn dispatch(&mut self, c: &Call) {
        let started = Instant::now();
        let timed = move |out: ToolOut| out.meta(ui::millis(started.elapsed().as_millis() as u64));
        let d = match local_tool(c) {
            Some(q) => Dispatch::Query(q),
            None => self.host.dispatch(c),
        };
        match d {
            Dispatch::Query(q) => {
                self.queries += 1;
                let tx = self.back_tx.clone();
                let name = c.name.clone();
                tokio::spawn(async move {
                    let out = timed(q.await);
                    let _ = tx.send(Back::Result(name, out));
                });
            }
            Dispatch::Record(r) => {
                self.records += 1;
                let tx = self.back_tx.clone();
                let host = self.host.clone();
                let name = c.name.clone();
                tokio::spawn(async move {
                    if let Some(out) = r.await {
                        host.show(ui::tool(host.name(), &name, &timed(out)));
                    }
                    let _ = tx.send(Back::RecordDone);
                });
            }
            // Fed back, so the model learns it rather than retrying blind.
            Dispatch::Unknown => {
                self.queries += 1;
                let _ = self.back_tx.send(Back::Result(c.name.clone(), ToolOut::new("", false).meta("no such tool here")));
            }
        }
    }

    fn session_tool(&self, c: &Call, total_tokens: u32) -> ToolOut {
        match c.name.as_str() {
            "TokenBudget" => {
                let usage = match (pct_used(total_tokens, self.window), self.window) {
                    (Some(p), Some(w)) => format!("{total_tokens}/{w} tokens ({p}%) used last turn"),
                    _ => format!("{total_tokens} tokens used last turn (context window unknown for this model)"),
                };
                ToolOut::new("", true).body(format!("{usage}. {} tool result(s) stored — BrowseTools to list them.", self.tool_log.len()))
            }
            "BrowseTools" => {
                if self.tool_log.is_empty() {
                    return ToolOut::new("", true).body("No tool results stored yet.");
                }
                let lines: Vec<String> = self
                    .tool_log
                    .iter()
                    .enumerate()
                    .map(|(id, (name, out))| format!("{id}: {name} — {}", out.lines().next().unwrap_or("").chars().take(60).collect::<String>()))
                    .collect();
                ToolOut::new("", true).meta(format!("{} stored", lines.len())).body(lines.join("\n"))
            }
            "Inspect" => {
                let id = c.args.get("id").and_then(|v| v.as_u64()).map(|n| n as usize);
                match id.and_then(|i| self.tool_log.get(i).map(|r| (i, r))) {
                    Some((i, (name, out))) => ToolOut::new(format!("#{i} {name}"), true).body(out.clone()),
                    None => ToolOut::new(format!("{id:?}"), false).meta(format!("no such id ({} stored)", self.tool_log.len())),
                }
            }
            _ => {
                let extra = self.host.about();
                let extra = if extra.is_empty() { String::new() } else { format!("{extra}\n") };
                ToolOut::new("", true).body(format!(
                    "Name: {}\n{extra}Model: {}\nPlatform: {} {}\nBuild: kot {}\nPersona:\n{}",
                    self.host.name(),
                    self.llm.label(),
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    crate::version::VERSION,
                    self.persona
                ))
            }
        }
    }
}

/// `Bash`/`ReadFile`/`WriteFile` on this host — the same for every cat and
/// for `kot chat`. No sandbox.
fn local_tool(c: &Call) -> Option<Query> {
    match c.name.as_str() {
        "Bash" => {
            let command = c.str("command").unwrap_or_default();
            Some(Box::pin(async move {
                let run = tokio::process::Command::new("/bin/sh").arg("-c").arg(&command).output();
                match tokio::time::timeout(BASH_TIMEOUT, run).await {
                    Ok(Ok(out)) => {
                        let code = out.status.code();
                        let mut body = String::from_utf8_lossy(&out.stdout).into_owned();
                        body.push_str(&String::from_utf8_lossy(&out.stderr));
                        ToolOut::new(format!("$ {command}"), code == Some(0))
                            .meta(code.map(|c| format!("exit {c}")).unwrap_or_else(|| "killed".into()))
                            .body(body)
                    }
                    Ok(Err(e)) => ToolOut::new(format!("$ {command}"), false).meta("failed to spawn").body(e.to_string()),
                    Err(_) => ToolOut::new(format!("$ {command}"), false).meta(format!("timed out after {}s", BASH_TIMEOUT.as_secs())),
                }
            }))
        }
        "ReadFile" => {
            let path = c.str("path").unwrap_or_default();
            Some(Box::pin(async move {
                match tokio::fs::read_to_string(&path).await {
                    Ok(s) => ToolOut::new(path, true).meta(ui::bytes(s.len())).body(s),
                    Err(e) => ToolOut::new(path, false).body(e.to_string()),
                }
            }))
        }
        "WriteFile" => {
            let path = c.str("path").unwrap_or_default();
            let content = c.str("content").unwrap_or_default();
            Some(Box::pin(async move {
                match tokio::fs::write(&path, &content).await {
                    Ok(()) => ToolOut::new(path, true).meta(format!("wrote {}", ui::bytes(content.len()))),
                    Err(e) => ToolOut::new(path, false).body(e.to_string()),
                }
            }))
        }
        _ => None,
    }
}

/// `used_pct` against `window`, if known — `None` means "can't tell", not zero.
fn pct_used(total_tokens: u32, window: Option<u32>) -> Option<u32> {
    let w = window.filter(|&w| w > 0)?;
    Some(((total_tokens as u64 * 100) / w as u64) as u32)
}

/// One extra call, no tools, asking the model to summarize itself — for
/// force-compaction.
async fn summarize(llm: &Llm, system: &str, history: &[(Speaker, String)]) -> String {
    let ask = "Summarize this conversation so far for your own future reference — what was asked, \
               what you found or did, what's still open. Plain text, no tools, as concise as it can \
               be while staying useful.";
    let mut h = history.to_vec();
    h.push((Speaker::User, ask.to_string()));
    match llm.converse(system, &h, Vec::new()).await {
        Ok(turn) if !turn.text.trim().is_empty() => turn.text.trim().to_string(),
        _ => "(compaction summary unavailable — history cleared anyway)".to_string(),
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let kept: String = s.chars().take(n).collect();
    format!("{kept}\n… ({} more chars — Inspect for the rest)", s.chars().count() - n)
}

/// The first line of a wake, for the `thinking ·` row.
fn summary(s: &str) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let short: String = line.chars().take(70).collect();
    if line.chars().count() > 70 { format!("{short}…") } else { short }
}
