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
//! - **Calls in flight are visible to the model.** Each has an `r`-id and a
//!   live output buffer (`Bash` streams into it). `Running` shows them and
//!   their output so far, `Cancel` stops one; every turn taken while some
//!   are out lists them ("still running"); and a call silent for
//!   [`Host::stall_after`] gets the model one notice turn, once per silence
//!   — counted as a follow-up, so it can't loop.
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
//! - **Local-task nudge.** An ignored check-in still leaves the loop with no
//!   further wake queued — found live again 2026-09-25, same meow, a later
//!   kernel-build task this time. If [`Host::reminder`] says there's an open
//!   `LocalTask`, idling past [`Host::local_nag_after`] queues a real wake
//!   ([`AgentStateMachine::maybe_nag`]) — the same shape as a chain task's
//!   `nudge`, bounded by [`MAX_LOCAL_TASK_NUDGES`] and reset once a turn
//!   starts a query again.
//! - **Long results.** A result is fed as its head and tail (build errors
//!   are at the end, a README's point at its start); `Inspect` with an
//!   `offset` pages through the middle. `Bash` takes a `timeout` up to
//!   [`BASH_MAX_TIMEOUT`] — its result lands whenever it finishes.
//! - **One conversation.** History accumulates for both hosts, with the same
//!   budget warnings and compaction (`TokenBudget`/`Compact`/`BrowseTools`/
//!   `Inspect`), and the same `AboutMe`.
//! - **Watchable.** Every step rewrites one live [`Activity`] record — which
//!   part of the loop this is, what's in flight, how finished calls went,
//!   the tail of the last reasoning — handed to [`Host::activity`] (a cat
//!   sends it to its node; `crate::activity`). The model's reasoning and any
//!   text it wrote alongside its tool calls are shown, and every turn, result
//!   and record goes to a JSONL transcript if the host names one
//!   ([`Host::transcript`]).

use crate::activity::{self, Activity, Finished, Flight};
use crate::ui::{self, ToolOut, TurnCost};
use miot_llm::{Call, Llm, Speaker, Tool};
use std::future::Future;
use std::io::Write as _;
use std::path::PathBuf;
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
/// A transcript past this is moved aside to `<name>.1` (one generation
/// kept) and started again.
const TRANSCRIPT_MAX: u64 = 64 * 1024 * 1024;
/// A running call's buffer, before it's cut to its head and tail — a build
/// log must not grow without bound while it runs either.
const LIVE_BYTES: usize = 512 * 1024;
const LIVE_HEAD: usize = 64 * 1024;
/// A running call quiet this long gets the model a notice (`Host::stall_after`).
pub const STALL_AFTER: Duration = Duration::from_secs(120);
/// Idle this long with open local tasks gets the model a nudge — the same
/// idea as a chain task's `work_nag` (`Timers::work_nag`, 150 s default):
/// "you claimed this, do it now." Added 2026-09-25 after meow answered a
/// check-in with nothing, went idle with an open `LocalTask`, and was never
/// woken again (`docs/AGENT_STATE_MACHINE.md`, "A model that ignores the
/// check-in" — the one case the check-in itself doesn't cover, since a
/// check-in that gets nothing back leaves no trace and arms no other wake).
pub const LOCAL_TASK_NAG_AFTER: Duration = Duration::from_secs(150);
/// Consecutive unanswered local-task nudges before we stop — same bound and
/// reason as a chain task's `max_nudges` (default 3): nagging a model that
/// will never answer burns turns forever otherwise. Resets the moment the
/// model starts a query again (mirrors `max_nudges`: "resets whenever the
/// holder acts").
pub const MAX_LOCAL_TASK_NUDGES: u32 = 3;
/// How much of a running call's output `Running <id>` shows — its tail.
const RUNNING_TAIL: usize = 2400;
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
    /// A call quiet this long gets the model a notice. A test shortens it.
    fn stall_after(&self) -> Duration {
        STALL_AFTER
    }
    /// Idle this long with open local tasks gets the model a nudge. A test
    /// shortens it.
    fn local_nag_after(&self) -> Duration {
        LOCAL_TASK_NAG_AFTER
    }
    /// The live record changed — a cat sends it to its node. Called often
    /// (every step); coalescing is the host's business.
    fn activity(&self, _a: &Activity) {}
    /// Where to append the JSONL transcript — every turn's prompt,
    /// reasoning, text and calls, every result and record in full. `None`:
    /// no transcript.
    fn transcript(&self) -> Option<PathBuf> {
        None
    }
    /// A line added to every turn that has a wake in it — a cat's open
    /// local tasks, so it knows where it was even after a restart.
    fn reminder(&self) -> Option<String> {
        None
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
- Running shows your tool calls still in flight (ids like r3) and what they've printed so \
far; Cancel stops one. You'll be told when one has gone quiet for a while — check it, \
cancel it, or leave it.\n\
- TokenBudget tells you how much context you have left. BrowseTools lists past tool \
results (id, name, preview); Inspect pulls one back by id. Compact replaces the \
conversation so far with a summary you write, to free room — past tool results survive \
it.";

/// Tools every host gets and this module runs itself.
pub fn shared_tools() -> Vec<Tool> {
    let mut t = vec![miot_llm::about_me_tool()];
    t.extend(miot_llm::local_tools());
    t.extend(miot_llm::budget_tools());
    t.extend(miot_llm::flight_tools());
    t
}

/// A call in flight, as `Running`/`Cancel` see it: what it has printed so
/// far (only `Bash` prints as it goes), when it last did, and the switch
/// that stops it.
pub struct Live {
    out: std::sync::Mutex<Vec<u8>>,
    bytes: std::sync::atomic::AtomicU64,
    started: Instant,
    last_output: std::sync::Mutex<Option<Instant>>,
    cancel: tokio::sync::Notify,
    /// A stall notice went out for the current silence; new output re-arms.
    stall_noted: std::sync::atomic::AtomicBool,
}

impl Live {
    fn new() -> Self {
        Live {
            out: Default::default(),
            bytes: Default::default(),
            started: Instant::now(),
            last_output: Default::default(),
            cancel: tokio::sync::Notify::new(),
            stall_noted: Default::default(),
        }
    }

    fn push(&self, chunk: &[u8]) {
        let mut out = self.out.lock().unwrap();
        out.extend_from_slice(chunk);
        if out.len() > LIVE_BYTES {
            // Keep the head (what it set out to do) and the newest tail.
            let tail_from = out.len() - (LIVE_BYTES / 2);
            let mut kept = out[..LIVE_HEAD].to_vec();
            kept.extend_from_slice(format!("\n… [{} bytes not kept] …\n", tail_from - LIVE_HEAD).as_bytes());
            kept.extend_from_slice(&out[tail_from..]);
            *out = kept;
        }
        self.bytes.fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
        *self.last_output.lock().unwrap() = Some(Instant::now());
        self.stall_noted.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Everything kept so far.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    fn tail(&self, n: usize) -> String {
        let t = self.text();
        let len = t.chars().count();
        if len <= n { t } else { format!("…{}", t.chars().skip(len - n).collect::<String>()) }
    }

    fn bytes(&self) -> u64 {
        self.bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How long since it last printed — or since it started, if never.
    fn quiet(&self) -> Duration {
        self.last_output.lock().unwrap().unwrap_or(self.started).elapsed()
    }

    fn last_output(&self) -> Option<Instant> {
        *self.last_output.lock().unwrap()
    }
}

enum Back {
    /// A query's result, tagged with the session it was asked in and its
    /// flight (`None`: an instant one, never in flight).
    Result(String, ToolOut, u64, Option<u64>),
    /// A record finished; `None` if the host showed it its own way.
    RecordDone(u64, Option<ToolOut>),
}

/// The JSONL transcript ([`Host::transcript`]). Best effort: a write that
/// fails is dropped, never allowed to stop the loop.
struct Transcript {
    path: PathBuf,
    file: Option<std::fs::File>,
    written: u64,
}

impl Transcript {
    fn open(path: PathBuf) -> Self {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
        let written = file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(0);
        Transcript { path, file, written }
    }

    fn write(&mut self, mut v: serde_json::Value) {
        if self.written > TRANSCRIPT_MAX {
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            let _ = std::fs::rename(&self.path, old);
            *self = Transcript::open(self.path.clone());
        }
        v["at"] = activity::unix_ms().into();
        let mut line = v.to_string();
        line.push('\n');
        if let Some(f) = &mut self.file {
            if f.write_all(line.as_bytes()).is_ok() {
                self.written += line.len() as u64;
            }
        }
    }
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
    /// The live record ([`Host::activity`]).
    act: Activity,
    /// Each query in flight's buffer and cancel switch, by flight id.
    live: std::collections::HashMap<u64, Arc<Live>>,
    /// Stall notices waiting for a turn.
    notices: Vec<String>,
    /// The next flight id.
    flights: u64,
    transcript: Option<Transcript>,
    /// Since when we've been continuously idle (no query, no record in
    /// flight) — `None` while working. Drives the local-task nudge.
    idle_since: Option<Instant>,
    /// Consecutive local-task nudges sent with no query started in between —
    /// bounded by [`MAX_LOCAL_TASK_NUDGES`], reset by [`AgentStateMachine::turn`]
    /// the moment a turn actually starts one.
    local_nudges: u32,
}

/// Think until `inbox` closes and nothing is left in flight.
pub async fn run<H: Host>(host: Arc<H>, llm: Llm, persona: String, mut inbox: mpsc::UnboundedReceiver<Inbound>) {
    let window = llm.context_window().await;
    let (back_tx, mut back_rx) = mpsc::unbounded_channel();
    let system = format!("{persona}{RULES}{}", host.rules());
    let mut transcript = host.transcript().map(Transcript::open);
    if let Some(t) = &mut transcript {
        if t.file.is_none() {
            host.show(ui::note(&format!("transcript: can't open {} — not writing one", t.path.display())));
        } else {
            host.show(ui::note(&format!("transcript: {}", t.path.display())));
        }
        t.write(serde_json::json!({"t": "start", "name": host.name(), "model": llm.label(), "window": window, "system": system}));
    }
    let act = Activity {
        name: host.name().to_string(),
        model: llm.label().to_string(),
        phase: "idle".into(),
        since: activity::unix_ms(),
        window,
        ..Default::default()
    };
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
        act,
        flights: 0,
        transcript,
        live: std::collections::HashMap::new(),
        notices: Vec::new(),
        idle_since: None,
        local_nudges: 0,
    };
    // Checks running calls for silence, and refreshes their output figures
    // in the live record — and, the same way, catches an idle cat with open
    // local tasks within a quarter of its nag threshold. Never more than
    // every 5 s.
    let every = (m.host.stall_after().min(m.host.local_nag_after()) / 4).clamp(Duration::from_millis(50), Duration::from_secs(5));
    let mut watchdog = tokio::time::interval(every);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    m.publish();
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
                m.idle_since = None;
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
                m.idle_since = Some(Instant::now());
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
                _ = watchdog.tick() => m.watch(),
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

        if wakes.is_empty() && results.is_empty() && m.notices.is_empty() {
            continue;
        }
        if wakes.is_empty() {
            if m.followups >= MAX_FOLLOWUPS {
                // Advisory only: past the cap a notice is dropped, not held.
                if !m.notices.is_empty() {
                    m.host.show(ui::note(&format!("{} stall notice(s) not given — follow-up turns used up", m.notices.len())));
                    m.notices.clear();
                }
                if results.is_empty() {
                    continue;
                }
                m.host.show(ui::note(&format!(
                    "{} result(s) held back — {MAX_FOLLOWUPS} follow-up turns in a row with nobody speaking; fed with the next message",
                    results.len()
                )));
                m.log(serde_json::json!({"t": "held", "n": results.len()}));
                m.held.extend(results);
                m.publish();
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
        m.idle_since = None;
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
                self.log(serde_json::json!({"t": "reset", "why": why}));
                self.session += 1;
                self.history.clear();
                self.warned_tier = 0;
                self.pending_warning = None;
                self.followups = 0;
                self.held.clear();
                self.notices.clear();
                self.check_armed = false;
                self.local_nudges = 0;
                self.idle_since = None;
                wakes.clear();
                results.clear();
            }
        }
    }

    /// A query result: shown now (the truthful moment it finished), queued
    /// for the next turn by id.
    fn back(&mut self, b: Back, results: &mut Vec<(usize, String)>) {
        match b {
            Back::RecordDone(flight, out) => {
                self.records = self.records.saturating_sub(1);
                let (ok, meta) = out.as_ref().map(|o| (o.ok, o.meta.join(" · "))).unwrap_or((true, String::new()));
                let (tool, arg) = self.land(Some(flight), "", "", ok, meta.clone());
                self.log(serde_json::json!({"t": "record", "tool": tool, "arg": out.as_ref().map(|o| o.arg.clone()).unwrap_or(arg), "ok": ok, "meta": meta}));
            }
            Back::Result(name, out, session, flight) => {
                self.queries = self.queries.saturating_sub(1);
                self.host.show(ui::tool(self.host.name(), &name, &out));
                self.land(flight, &name, &out.arg, out.ok, out.meta.join(" · "));
                if session != self.session {
                    self.host.show(ui::note(&format!("{name}'s result is from before the session reset — not fed back")));
                    self.log(serde_json::json!({"t": "result", "id": null, "tool": name, "ok": out.ok, "stale": true, "text": out.text()}));
                    return;
                }
                let id = self.tool_log.len();
                self.tool_log.push((name, keep(&out.text())));
                self.log(serde_json::json!({"t": "result", "id": id, "tool": self.tool_log[id].0, "ok": out.ok, "text": self.tool_log[id].1}));
                results.push((id, feed(id, &self.tool_log[id].1)));
            }
        }
    }

    /// On the watchdog's tick: refresh each running call's output figures,
    /// queue a notice for any that has gone quiet for [`Host::stall_after`]
    /// — once per silence — and nudge a cat that's gone idle with open
    /// local tasks.
    fn watch(&mut self) {
        self.maybe_nag();
        if self.act.running.is_empty() {
            return;
        }
        let stall = self.host.stall_after();
        let now = activity::unix_ms();
        for f in &mut self.act.running {
            let Some(live) = self.live.get(&f.id) else { continue };
            f.output = live.bytes();
            f.last_output = live.last_output().map(|t| now.saturating_sub(t.elapsed().as_millis() as u64)).unwrap_or(0);
            let quiet = live.quiet();
            if quiet >= stall && !live.stall_noted.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let ran = ui::human(live.started.elapsed().as_secs());
                let heard = if live.bytes() == 0 { format!("no output at all in {ran}") } else { format!("no new output for {}", ui::human(quiet.as_secs())) };
                let notice = format!(
                    "[r{} {} still running] {} — {ran} so far, {heard}. It may be stalled: Running r{} shows its \
                     output so far, Cancel r{} stops it. Or leave it — its result comes back when it finishes.",
                    f.id, f.tool, f.arg, f.id, f.id
                );
                self.host.show(ui::note(&format!("r{} {} quiet for {} — telling the model", f.id, f.tool, ui::human(quiet.as_secs()))));
                self.notices.push(notice);
            }
        }
        self.publish();
    }

    /// Idle (no query, no record in flight) with open local tasks, for
    /// longer than [`LOCAL_TASK_NAG_AFTER`]: queue a wake reminding the
    /// model to get on with it — the same shape as a chain task's `nudge`
    /// (`agent.rs`: `"[work: {task}] You claimed this. Do it now."`), just
    /// for the to-do list nothing on chain knows about. This is the case
    /// `docs/AGENT_STATE_MACHINE.md` names as uncovered: a model that
    /// answers the check-in with nothing leaves no trace and arms no other
    /// wake, so an open `LocalTask` sat forever with nobody nudging it.
    /// Bounded by [`MAX_LOCAL_TASK_NUDGES`], same reason a chain nudge is
    /// bounded — nagging a model that will never answer burns turns
    /// forever otherwise. `Host::check_before_idle` off (`kot chat`) means
    /// no unattended cat to answer for, so it's skipped there too.
    fn maybe_nag(&mut self) {
        if self.queries != 0 || self.records != 0 || !self.host.check_before_idle() {
            return;
        }
        if self.local_nudges >= MAX_LOCAL_TASK_NUDGES {
            return;
        }
        let Some(since) = self.idle_since else { return };
        if since.elapsed() < self.host.local_nag_after() {
            return;
        }
        let Some(reminder) = self.host.reminder() else { return };
        self.local_nudges += 1;
        let last = self.local_nudges >= MAX_LOCAL_TASK_NUDGES;
        let tail = if last {
            " This is the last reminder on these — if you're stuck or waiting on someone, say so; nothing more will nudge you about them."
        } else {
            ""
        };
        let text = format!(
            "(Idle {} with open local tasks.{tail}\n{reminder}\nDo the next step now, or call LocalTask to update the list.)",
            ui::human(since.elapsed().as_secs())
        );
        self.host.show(ui::note(&format!("nudging {} on its open local tasks — idle {}", self.host.name(), ui::human(since.elapsed().as_secs()))));
        let kind = self.kinds.last().copied().unwrap_or("said");
        self.pending.push_back(Inbound::Wake { text, kind, ctx: self.ctx.clone() });
        // Restart the clock: if this nudge also gets nothing back, the next
        // one waits a full interval again rather than firing right away.
        self.idle_since = Some(Instant::now());
    }

    /// The queries still out — what the model can look at or cancel.
    /// Records (chain writes) aren't among them: as far as the model is
    /// told, a write just happens (`RULES`), and listing one that's merely
    /// waiting to be tallied had GLM deliberating whether to resend it
    /// (found live 2026-09-25). The live record still shows them.
    fn running_queries(&self) -> impl Iterator<Item = (&Flight, &Arc<Live>)> {
        self.act.running.iter().filter_map(|f| self.live.get(&f.id).map(|l| (f, l)))
    }

    /// What's still out, for the top of a turn — so the model sees a slow
    /// call's progress whenever it's woken, without asking.
    fn still_running(&self) -> Option<String> {
        let rows: Vec<String> = self
            .running_queries()
            .map(|(f, l)| {
                let ran = activity::unix_ms().saturating_sub(f.since) / 1000;
                let out = if l.bytes() == 0 {
                    "no output yet".to_string()
                } else {
                    format!("{} of output, last {} ago", ui::bytes(l.bytes() as usize), ui::human(l.quiet().as_secs()))
                };
                format!("r{} {} {} — {}, {out}", f.id, f.tool, f.arg, ui::human(ran))
            })
            .collect();
        if rows.is_empty() {
            return None;
        }
        Some(format!("Still running (Running <id> for output so far, Cancel <id> to stop one):\n{}", rows.join("\n")))
    }

    /// A call came back: off the running list (if it was ever on it), into
    /// the tally and the recent list. The waiting phase ends with the last
    /// one. Returns the call's tool and argument as they were dispatched.
    fn land(&mut self, flight: Option<u64>, tool: &str, arg: &str, ok: bool, meta: String) -> (String, String) {
        let now = activity::unix_ms();
        if let Some(id) = flight {
            self.live.remove(&id);
        }
        let f = flight.and_then(|id| self.act.running.iter().position(|f| f.id == id)).map(|i| self.act.running.remove(i));
        let (tool, arg, ms) = match f {
            Some(f) => (f.tool, f.arg, now.saturating_sub(f.since)),
            None => (tool.to_string(), activity::short(arg, activity::ARG_CHARS), 0),
        };
        if ok {
            self.act.ok += 1;
        } else {
            self.act.failed += 1;
        }
        self.act.recent.push(Finished { tool: tool.clone(), arg: arg.clone(), ok, ms, meta, at: now });
        if self.act.recent.len() > activity::RECENT {
            self.act.recent.remove(0);
        }
        if self.act.phase == "waiting" && self.act.running.is_empty() {
            self.phase("idle");
        }
        self.publish();
        (tool, arg)
    }

    /// Enter `phase` — its clock restarts only if it's actually new.
    fn phase(&mut self, phase: &str) {
        if self.act.phase != phase {
            self.act.phase = phase.to_string();
            self.act.since = activity::unix_ms();
        }
    }

    /// Where the model is after acting: tools still out, or nothing.
    fn settle(&mut self) {
        let p = if self.act.running.is_empty() { "idle" } else { "waiting" };
        self.phase(p);
        self.publish();
    }

    fn publish(&mut self) {
        self.act.followups = self.followups;
        self.act.held = self.held.len() as u32;
        self.act.at = activity::unix_ms();
        self.host.activity(&self.act);
    }

    fn log(&mut self, mut v: serde_json::Value) {
        if let Some(t) = &mut self.transcript {
            v["session"] = self.session.into();
            t.write(v);
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
        if !self.notices.is_empty() {
            msg.push(std::mem::take(&mut self.notices).join("\n"));
        }
        msg.extend(self.still_running());
        if check {
            msg.push(CHECK_IN.to_string());
        }
        if !wakes.is_empty() {
            msg.extend(self.host.reminder());
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
        } else if wakes.is_empty() && results.is_empty() {
            "a call went quiet".to_string()
        } else if wakes.is_empty() {
            format!("{} result(s) back", results.len())
        } else {
            summary(&wakes[wakes.len() - 1].0)
        };
        self.host.show(ui::thinking(&name, &why));
        let prompt = msg.join("\n\n");
        self.history.push((Speaker::User, prompt.clone()));
        self.act.turns += 1;
        self.act.why = why.clone();
        self.act.since = activity::unix_ms();
        self.act.phase = "thinking".into();
        self.publish();

        let system_now = match self.pending_warning.take() {
            Some(w) => format!("{}\n\n{w}", self.system),
            None => self.system.clone(),
        };
        let turn = match self.llm.converse(&system_now, &self.history, self.tools()).await {
            Ok(t) => t,
            Err(e) => {
                self.host.show(ui::note(&format!("llm error: {e}")));
                self.log(serde_json::json!({"t": "turn", "why": why, "check": check, "prompt": prompt, "error": e}));
                // A failed turn never happened, as far as history goes.
                self.history.pop();
                self.settle();
                return;
            }
        };
        if let Some(r) = &turn.reasoning {
            self.host.show(ui::musing(&name, "reasoning", r));
            self.act.thought = activity::tail(r, activity::THOUGHT_CHARS);
        }
        self.act.tokens = turn.total_tokens;
        self.log(serde_json::json!({
            "t": "turn",
            "why": why,
            "check": check,
            "prompt": prompt,
            "reasoning": turn.reasoning,
            "text": turn.text,
            "calls": turn.calls.iter().map(|c| serde_json::json!({"name": c.name, "args": c.args})).collect::<Vec<_>>(),
            "prompt_tokens": turn.prompt_tokens,
            "out_tokens": turn.tokens,
            "total_tokens": turn.total_tokens,
            "ms": turn.ms,
        }));
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
            self.log(serde_json::json!({"t": "dropped", "calls": n}));
            self.host.after_turn(&cost);
            self.settle();
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
            self.settle();
            return;
        }

        let own = turn.text.trim();
        if !own.is_empty() {
            self.history.push((Speaker::Assistant, own.to_string()));
            // Alone, it's a reply and the host shows it as one (below);
            // beside tool calls it used to go unseen.
            if !turn.calls.is_empty() {
                self.host.show(ui::musing(&name, "wrote, beside its calls", own));
            }
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
                    self.log(serde_json::json!({"t": "compact", "forced": false, "summary": summary}));
                    self.history = vec![(Speaker::Assistant, summary)];
                    self.warned_tier = 0;
                    compacted = true;
                }
                // Instant, but still a result: it comes back like any other.
                "TokenBudget" | "BrowseTools" | "Inspect" | "AboutMe" | "Running" | "Cancel" => {
                    let out = self.session_tool(c, turn.total_tokens);
                    self.queries += 1;
                    let _ = self.back_tx.send(Back::Result(c.name.clone(), out, self.session, None));
                }
                _ => self.dispatch(c),
            }
        }
        // Worked on results, then only wrote things (a message, a task
        // update): the shape of "I'll do X next" with no X started. A
        // check-in never arms another.
        let started_nothing = self.queries == queries_before;
        if !started_nothing {
            // The holder acted — same rule a chain task's nudge budget
            // follows (`Timers::max_nudges`: "resets whenever the holder
            // acts"). Otherwise three real turns of work would still leave
            // a nag due the moment it next goes idle.
            self.local_nudges = 0;
        }
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
        self.settle();

        // Against what this turn actually cost.
        if let Some(pct) = pct_used(turn.total_tokens, self.window) {
            if pct >= miot_llm::FORCE_COMPACT_PCT && !compacted {
                self.host.show(ui::note(&format!("{pct}% of the context window used — force-compacting")));
                self.act.phase = "compacting".into();
                self.act.since = activity::unix_ms();
                self.publish();
                let summary = summarize(&self.llm, &self.system, &self.history).await;
                self.log(serde_json::json!({"t": "compact", "forced": true, "summary": summary}));
                self.history = vec![(Speaker::Assistant, summary)];
                self.warned_tier = 0;
                self.settle();
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
        let live = Arc::new(Live::new());
        let d = match local_tool(c, live.clone()) {
            Some(q) => Dispatch::Query(q),
            None => self.host.dispatch(c),
        };
        if matches!(d, Dispatch::Unknown) {
            // Fed back, so the model learns it rather than retrying blind.
            self.queries += 1;
            let _ = self.back_tx.send(Back::Result(c.name.clone(), ToolOut::new(gist(c), false).meta("no such tool here"), self.session, None));
            return;
        }
        // In flight from here until it lands (`land`).
        let id = self.flights;
        self.flights += 1;
        let arg = activity::short(&gist(c), activity::ARG_CHARS);
        self.act.running.push(Flight { id, tool: c.name.clone(), arg: arg.clone(), since: activity::unix_ms(), ..Default::default() });
        self.host.show(ui::started(self.host.name(), &c.name, &arg, self.act.running.len()));
        self.publish();
        match d {
            Dispatch::Query(q) => {
                self.queries += 1;
                self.live.insert(id, live.clone());
                let tx = self.back_tx.clone();
                let name = c.name.clone();
                let session = self.session;
                let gist = gist(c);
                tokio::spawn(async move {
                    // `Cancel` drops the call — a `Bash` child is killed with
                    // it (`kill_on_drop`) — and answers with what it printed.
                    let out = tokio::select! {
                        out = q => out,
                        _ = live.cancel.notified() => {
                            let so_far = live.text();
                            ToolOut::new(gist, false).meta("cancelled").body(so_far)
                        }
                    };
                    let _ = tx.send(Back::Result(name, timed(out), session, Some(id)));
                });
            }
            Dispatch::Record(r) => {
                self.records += 1;
                let tx = self.back_tx.clone();
                let host = self.host.clone();
                let name = c.name.clone();
                tokio::spawn(async move {
                    let out = r.await.map(timed);
                    if let Some(out) = &out {
                        host.show(ui::tool(host.name(), &name, out));
                    }
                    let _ = tx.send(Back::RecordDone(id, out));
                });
            }
            Dispatch::Unknown => unreachable!("handled above"),
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
            "Running" => {
                let want = c.str("id").map(|s| s.trim().trim_start_matches(['r', 'R', '#']).to_string()).filter(|s| !s.is_empty());
                let n = self.running_queries().count();
                if n == 0 {
                    return ToolOut::new("", true).body("Nothing is running.");
                }
                match want {
                    None => {
                        let mut out = vec![self.still_running().unwrap_or_default()];
                        for (f, l) in self.running_queries().filter(|(_, l)| l.bytes() > 0) {
                            out.push(format!("--- r{} latest output:\n{}", f.id, l.tail(400)));
                        }
                        ToolOut::new("", true).meta(format!("{n} running")).body(out.join("\n"))
                    }
                    Some(w) => match self.running_queries().find(|(f, _)| f.id.to_string() == w) {
                        None => ToolOut::new(format!("r{w}"), false).meta("not running").body(self.still_running().unwrap_or_default()),
                        Some((f, l)) => {
                            let ran = ui::human(l.started.elapsed().as_secs());
                            let head = format!("r{} {} {} — running {ran}, {} of output, last {} ago", f.id, f.tool, f.arg, ui::bytes(l.bytes() as usize), ui::human(l.quiet().as_secs()));
                            let body = if l.bytes() == 0 { "(no output yet)".to_string() } else { l.tail(RUNNING_TAIL) };
                            ToolOut::new(format!("r{w}"), true).body(format!("{head}\n{body}"))
                        }
                    },
                }
            }
            "Cancel" => {
                let w = c.str("id").unwrap_or_default().trim().trim_start_matches(['r', 'R', '#']).to_string();
                match self.running_queries().find(|(f, _)| f.id.to_string() == w) {
                    None => ToolOut::new(format!("r{w}"), false).meta("not running").body(self.still_running().unwrap_or_else(|| "Nothing is running.".into())),
                    Some((f, l)) => {
                        l.cancel.notify_one();
                        ToolOut::new(format!("r{w}"), true).body(format!("Cancelling r{} ({} {}). Its result comes back marked cancelled.", f.id, f.tool, f.arg))
                    }
                }
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
fn local_tool(c: &Call, live: Arc<Live>) -> Option<Query> {
    match c.name.as_str() {
        "Bash" => {
            let command = c.str("command").unwrap_or_default();
            let secs = c.args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(BASH_DEFAULT_TIMEOUT).clamp(1, BASH_MAX_TIMEOUT);
            Some(Box::pin(async move {
                use std::process::Stdio;
                use tokio::io::AsyncReadExt;
                // Timed out (or cancelled) means killed, not left running
                // unseen. (Only the shell itself — a child it forked may
                // outlive it.)
                let spawned = tokio::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(&command)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn();
                let mut child = match spawned {
                    Ok(c) => c,
                    Err(e) => return ToolOut::new(format!("$ {command}"), false).meta("failed to spawn").body(e.to_string()),
                };
                // Both streams into the live buffer as they come, in arrival
                // order — what `Running` shows while it runs, and the result.
                let pump = |mut r: Box<dyn tokio::io::AsyncRead + Unpin + Send>, live: Arc<Live>| {
                    tokio::spawn(async move {
                        let mut buf = [0u8; 8192];
                        while let Ok(n) = r.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                            live.push(&buf[..n]);
                        }
                    })
                };
                let pumps = [
                    pump(Box::new(child.stdout.take().expect("piped")), live.clone()),
                    pump(Box::new(child.stderr.take().expect("piped")), live.clone()),
                ];
                match tokio::time::timeout(Duration::from_secs(secs), child.wait()).await {
                    Ok(Ok(status)) => {
                        // The last of the output; a background child still
                        // holding the pipes doesn't get to hold this up.
                        let _ = tokio::time::timeout(Duration::from_secs(2), futures_util::future::join_all(pumps)).await;
                        let code = status.code();
                        ToolOut::new(format!("$ {command}"), code == Some(0))
                            .meta(code.map(|c| format!("exit {c}")).unwrap_or_else(|| "killed".into()))
                            .body(live.text())
                    }
                    Ok(Err(e)) => ToolOut::new(format!("$ {command}"), false).meta("failed to wait").body(e.to_string()),
                    Err(_) => {
                        let _ = child.kill().await;
                        let so_far = live.text();
                        let hint = format!("Pass a larger timeout (seconds, up to {BASH_MAX_TIMEOUT}) if this needs longer.");
                        let body = if so_far.trim().is_empty() { hint } else { format!("{so_far}\n[{hint}]") };
                        ToolOut::new(format!("$ {command}"), false).meta(format!("timed out after {secs}s, killed")).body(body)
                    }
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

/// A call's gist, one line, for the running list and the log: the command,
/// the path, the recipient and message — whatever says what it's doing.
fn gist(c: &Call) -> String {
    let s = |k: &str| c.str(k).unwrap_or_default();
    match c.name.as_str() {
        "Bash" => format!("$ {}", s("command")),
        "ReadFile" | "WriteFile" => s("path"),
        "SendMessage" => {
            let to = s("to");
            let to = if to.is_empty() { "litter".to_string() } else { to.trim_start_matches('@').to_string() };
            format!("→ {to}  {}", s("body"))
        }
        "TaskUpdate" => format!("{} {}  {}", s("task"), s("status"), s("text")),
        "TaskPlan" | "TaskReassign" => format!("{} {}", s("task"), s("to")),
        "ArtifactRead" | "Inspect" | "Running" | "Cancel" => s("id"),
        "LocalTask" => [s("action"), s("id"), s("text")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" "),
        "Artifact" => s("text"),
        _ if c.args.as_object().is_some_and(|o| o.is_empty()) || c.args.is_null() => String::new(),
        _ => c.args.to_string(),
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
