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
//!   tool-call itself into a loop with nobody speaking to it. The last
//!   allowed one tells the model so (report now); past it, results are
//!   *held* — queued, not dropped — and ride along with the next wake.
//! - **Check-in before idling.** A turn that worked on results but started
//!   nothing new (only records — a message, a task update) is about to leave
//!   the loop with nothing to do. If the host wants it
//!   ([`Host::check_before_idle`]), the model gets one [`CHECK_IN`] turn
//!   first: "nothing is running — if you said you'd do something, do it".
//!   Calling nothing there is the normal answer, and it leaves no trace in
//!   history. Found live 2026-09-24: meow, asked to build the kernel,
//!   messaged root "next I'm checking whether a plain `cargo build`
//!   works", called no tool, and was never woken again.
//! - **Long results.** A result is fed as its head and tail (build errors
//!   are at the end, a README's point at its start); `Inspect` with an
//!   `offset` pages through the middle. `Bash` takes a `timeout` up to
//!   [`BASH_MAX_TIMEOUT`] — its result lands whenever it finishes.
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
/// back (queued, fed with the next wake) until something actually wakes
/// us. Was 4 until 2026-09-24 — meow spent five on recon alone before it
/// would have started a kernel build. A check-in turn counts as one.
pub const MAX_FOLLOWUPS: u32 = 8;
/// A single result fed back is cut to this: [`FEED_HEAD`] from the start,
/// the rest from the end. The full text stays in the tool log for `Inspect`.
pub const FEED_CHARS: usize = 3000;
const FEED_HEAD: usize = 1000;
/// One `Inspect` page — under [`FEED_CHARS`] with its header, so the page
/// itself is never cut again.
const INSPECT_CHARS: usize = 2700;
/// The most of one result kept at all (head quarter, tail rest) — a build
/// log must not eat a small box's heap.
const STORE_CHARS: usize = 256 * 1024;
/// `Bash` without a `timeout`, and the most one may ask for, in seconds.
pub const BASH_DEFAULT_TIMEOUT: u64 = 30;
pub const BASH_MAX_TIMEOUT: u64 = 3600;
/// Fed to the model, alone, when it's about to go idle mid-work.
pub const CHECK_IN: &str = "(Check-in: none of your tools are running and nothing else is \
coming back to you. If you said you'd do something next, call its tool now — a message \
saying you will doesn't do it. If you're finished, or waiting on someone, call nothing and \
write nothing.)";

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
    /// Give the model a [`CHECK_IN`] turn before going idle mid-work. On
    /// for an unattended cat; `kot chat` turns it off — its operator is
    /// right there to say "go on".
    fn check_before_idle(&self) -> bool {
        true
    }
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
- Your work stops when you stop calling tools. If you say you'll do something next, call \
its tool in that same response — a message alone doesn't start anything.\n\
- Bash waits 30 seconds unless you pass timeout (seconds, up to 3600). Give anything slow, \
like a build, a big enough timeout, and tell whoever asked that it's running; its output \
comes back when it finishes, however long that takes.\n\
- A long result comes back as its start and its end. Inspect with its id and an offset \
reads the part in between.\n\
- After many tool turns in a row with nobody writing to you, you'll be told it's your \
last one; report your progress then. Results that arrive after that aren't lost — they \
come back with the next message you get.\n\
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
    /// A query's result, tagged with the session it was asked in.
    Result(String, ToolOut, u64),
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
    /// Results that arrived past [`MAX_FOLLOWUPS`], oldest first — fed with
    /// the next wake.
    held: Vec<(usize, String)>,
    /// The last turn worked on results and started nothing: give the model
    /// a [`CHECK_IN`] before going idle.
    check_armed: bool,
    /// Bumped by every [`Inbound::Reset`]. Anything started in an older
    /// session — a turn still thinking, a query still running, a wake
    /// queued ahead of the reset — is dropped rather than carried into
    /// the new one. Found live 2026-09-24: kuro's turn on a pre-`/clear`
    /// message finished after the clear and its reply was posted anyway.
    session: u64,
    /// Inbound traffic read early — while checking for a reset between a
    /// turn's thinking and its acting — kept for the main loop, in order.
    pending: std::collections::VecDeque<Inbound>,
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
        held: Vec::new(),
        check_armed: false,
        session: 0,
        pending: std::collections::VecDeque::new(),
    };
    let mut open = true;
    let mut was_idle = false;

    loop {
        // About to idle mid-work: one check-in first — unless something is
        // already waiting (it gets the turn instead) or the follow-up budget
        // is spent.
        if m.check_armed && m.queries == 0 {
            m.check_armed = false;
            while let Ok(i) = inbox.try_recv() {
                m.pending.push_back(i);
            }
            if m.pending.is_empty() && m.followups < MAX_FOLLOWUPS {
                m.followups += 1;
                was_idle = false;
                m.turn(Vec::new(), Vec::new(), true, &mut inbox).await;
                continue;
            }
        }

        if m.queries == 0 && m.records == 0 {
            if !open {
                break;
            }
            if !was_idle {
                m.host.idle();
                was_idle = true;
            }
        }

        // Wait for anything at all — traffic already read early first.
        let mut wakes: Vec<(String, &'static str, serde_json::Value)> = Vec::new();
        let mut results: Vec<(usize, String)> = Vec::new();
        if m.pending.is_empty() {
            tokio::select! {
                got = inbox.recv(), if open => match got {
                    Some(i) => m.take(i, &mut wakes, &mut results),
                    None => open = false,
                },
                Some(b) = back_rx.recv() => m.back(b, &mut results),
            }
        }

        // Aggregate: everything already queued, then — for results with no
        // wake — the rest of the batch, up to the deadline.
        while let Some(i) = m.pending.pop_front() {
            m.take(i, &mut wakes, &mut results);
        }
        while let Ok(i) = inbox.try_recv() {
            m.take(i, &mut wakes, &mut results);
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
                        Some(i) => { m.take(i, &mut wakes, &mut results); break; }
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
                    "{} result(s) held back — {MAX_FOLLOWUPS} follow-up turns in a row with nobody speaking; fed with the next message",
                    results.len()
                )));
                m.held.extend(results);
                continue;
            }
            m.followups += 1;
        } else {
            m.followups = 0;
            if !m.held.is_empty() {
                // Older than anything in this batch, so first.
                results.splice(0..0, m.held.drain(..));
            }
        }
        was_idle = false;
        m.turn(wakes, results, false, &mut inbox).await;
    }
}

impl<H: Host> AgentStateMachine<H> {
    /// One inbound item into the batch being assembled. A reset empties the
    /// batch too: wakes and results gathered ahead of it belong to the
    /// session it just ended.
    fn take(&mut self, i: Inbound, wakes: &mut Vec<(String, &'static str, serde_json::Value)>, results: &mut Vec<(usize, String)>) {
        match i {
            Inbound::Wake { text, kind, ctx } => wakes.push((text, kind, ctx)),
            Inbound::Reset(why) => {
                self.host.show(ui::note(&format!("new session — {why}")));
                self.session += 1;
                self.history.clear();
                self.warned_tier = 0;
                self.pending_warning = None;
                self.followups = 0;
                self.held.clear();
                self.check_armed = false;
                wakes.clear();
                results.clear();
            }
        }
    }

    /// A query result: shown now (the truthful moment it finished), queued
    /// for the next turn by id.
    fn back(&mut self, b: Back, results: &mut Vec<(usize, String)>) {
        match b {
            Back::RecordDone => self.records = self.records.saturating_sub(1),
            Back::Result(name, out, session) => {
                self.queries = self.queries.saturating_sub(1);
                self.host.show(ui::tool(self.host.name(), &name, &out));
                if session != self.session {
                    self.host.show(ui::note(&format!("{name}'s result is from before the session reset — not fed back")));
                    return;
                }
                let id = self.tool_log.len();
                self.tool_log.push((name, keep(&out.text())));
                results.push((id, feed(id, &self.tool_log[id].1)));
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

    /// One turn. `check`: a [`CHECK_IN`] — no wakes, no results, and if the
    /// model calls nothing it leaves no trace in history.
    async fn turn(&mut self, wakes: Vec<(String, &'static str, serde_json::Value)>, results: Vec<(usize, String)>, check: bool, inbox: &mut mpsc::UnboundedReceiver<Inbound>) {
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
        if check {
            msg.push(CHECK_IN.to_string());
        }
        if wakes.is_empty() && self.followups >= MAX_FOLLOWUPS {
            msg.push(format!(
                "(This is your last turn without someone writing to you: {MAX_FOLLOWUPS} in a row. \
                 After it, tool results are held until a message arrives, then fed with it. If \
                 you're in the middle of something, SendMessage whoever asked now: what you've \
                 done, what's still running, what's next.)"
            ));
        }
        let why = if check {
            "check-in — nothing running".to_string()
        } else if wakes.is_empty() {
            format!("{} result(s) back", results.len())
        } else {
            summary(&wakes[wakes.len() - 1].0)
        };
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
        let messages = turn.calls.iter().filter(|c| c.name == "SendMessage").count();
        let cost = TurnCost {
            prompt: turn.prompt_tokens,
            out: turn.tokens,
            total: turn.total_tokens,
            window: self.window,
            ms: turn.ms,
            tools: turn.calls.len() - messages,
            messages,
        };
        self.host.show(ui::turn(&name, &cost));

        // Thinking takes minutes; a reset may have landed meanwhile. If so,
        // this turn answered a session that no longer exists — act on none
        // of it. (Whatever was read here stays queued, in order, for the
        // main loop, which applies the reset itself.)
        while let Ok(i) = inbox.try_recv() {
            self.pending.push_back(i);
        }
        if self.pending.iter().any(|i| matches!(i, Inbound::Reset(_))) {
            let n = turn.calls.len();
            self.host.show(ui::note(&format!("session reset while thinking — this turn's {n} call(s) dropped, nothing sent")));
            self.host.after_turn(&cost);
            return;
        }

        // The model's own side of the conversation: only what it actually
        // said. Its calls are *not* written in here as text — found live,
        // 2026-09-24: with `[called: Bash{...}]` in its own past turns,
        // qwen3-4b started typing `[called: SendMessage{...}]` as its reply
        // instead of calling the tool. What it asked for is recoverable
        // anyway: every result comes back labelled with its tool and
        // argument (`[#3 Bash] $ uname -sm`). A tool-only turn leaves no
        // assistant message, and the results follow as the next user one.
        // A check-in answered with nothing: the normal "I'm done". Leave no
        // trace, so a long session isn't a stack of check-ins.
        if check && turn.calls.is_empty() {
            self.history.pop();
            self.host.show(ui::note("check-in: nothing more to do"));
            self.host.after_turn(&cost);
            return;
        }

        let own = turn.text.trim();
        if !own.is_empty() {
            self.history.push((Speaker::Assistant, own.to_string()));
        }

        let queries_before = self.queries;
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
                    let _ = self.back_tx.send(Back::Result(c.name.clone(), out, self.session));
                }
                _ => self.dispatch(c),
            }
        }
        // Worked on results, then only wrote things (a message, a task
        // update): the shape of "I'll do X next" with no X started. A
        // check-in never arms another.
        let started_nothing = self.queries == queries_before;
        let wrote = !turn.calls.is_empty();
        self.check_armed = !check && !results.is_empty() && wrote && started_nothing && self.host.check_before_idle();
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
                let session = self.session;
                tokio::spawn(async move {
                    let out = timed(q.await);
                    let _ = tx.send(Back::Result(name, out, session));
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
                let _ = self.back_tx.send(Back::Result(c.name.clone(), ToolOut::new("", false).meta("no such tool here"), self.session));
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
                let offset = c.args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                match id.and_then(|i| self.tool_log.get(i).map(|r| (i, r))) {
                    Some((i, (name, out))) => {
                        let total = out.chars().count();
                        let from = offset.min(total);
                        let to = (from + INSPECT_CHARS).min(total);
                        let mut page: String = out.chars().skip(from).take(to - from).collect();
                        if to < total {
                            page.push_str(&format!("\n… [Inspect {{\"id\": {i}, \"offset\": {to}}} for the next part]"));
                        }
                        ToolOut::new(format!("#{i} {name}"), true).meta(format!("chars {from}–{to} of {total}")).body(page)
                    }
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
            let secs = c.args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(BASH_DEFAULT_TIMEOUT).clamp(1, BASH_MAX_TIMEOUT);
            Some(Box::pin(async move {
                // Timed out means killed, not left running unseen. (Only the
                // shell itself — a child it forked may outlive it.)
                let run = tokio::process::Command::new("/bin/sh").arg("-c").arg(&command).kill_on_drop(true).output();
                match tokio::time::timeout(Duration::from_secs(secs), run).await {
                    Ok(Ok(out)) => {
                        let code = out.status.code();
                        let mut body = String::from_utf8_lossy(&out.stdout).into_owned();
                        body.push_str(&String::from_utf8_lossy(&out.stderr));
                        ToolOut::new(format!("$ {command}"), code == Some(0))
                            .meta(code.map(|c| format!("exit {c}")).unwrap_or_else(|| "killed".into()))
                            .body(body)
                    }
                    Ok(Err(e)) => ToolOut::new(format!("$ {command}"), false).meta("failed to spawn").body(e.to_string()),
                    Err(_) => ToolOut::new(format!("$ {command}"), false)
                        .meta(format!("timed out after {secs}s, killed"))
                        .body(format!("Pass a larger timeout (seconds, up to {BASH_MAX_TIMEOUT}) if this needs longer.")),
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

/// What the model is fed of result `id`: all of it if short, else its head
/// and its tail with a pointer to the middle. Head-only (the old way) fed
/// meow the first 3000 chars of a 43 KB README and would have fed it the
/// first screen of a build log, never the error at the end.
fn feed(id: usize, s: &str) -> String {
    let n = s.chars().count();
    if n <= FEED_CHARS {
        return s.to_string();
    }
    let tail = FEED_CHARS - FEED_HEAD;
    let head: String = s.chars().take(FEED_HEAD).collect();
    let end: String = s.chars().skip(n - tail).collect();
    format!(
        "{head}\n… [{} chars cut here — Inspect {{\"id\": {id}, \"offset\": {FEED_HEAD}}} reads on from this point] …\n{end}",
        n - FEED_CHARS
    )
}

/// What the tool log keeps of one result: at most [`STORE_CHARS`], a
/// quarter from the start and the rest from the end.
fn keep(s: &str) -> String {
    let n = s.chars().count();
    if n <= STORE_CHARS {
        return s.to_string();
    }
    let head_n = STORE_CHARS / 4;
    let head: String = s.chars().take(head_n).collect();
    let end: String = s.chars().skip(n - (STORE_CHARS - head_n)).collect();
    format!("{head}\n… [{} chars not kept] …\n{end}", n - STORE_CHARS)
}

/// The first line of a wake, for the `thinking ·` row.
fn summary(s: &str) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let short: String = line.chars().take(70).collect();
    if line.chars().count() > 70 { format!("{short}…") } else { short }
}
