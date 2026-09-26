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
//! - **One lane for the host's files.** `Bash`, `ReadFile`, `WriteFile`,
//!   `Edit`, `MultiEdit`, `LS`, `Glob` and `Grep` run one at a time, in the
//!   order they were called, across turns
//!   ([`AgentStateMachine::lane`]). Found live 2026-09-25: they all ran at
//!   once, so meow's `sed -i` on `hda.rs` started while its previous turn's
//!   edit-and-build script was still rewriting it, a note's `WriteFile`
//!   raced the `reboot` beside it, and the files came out shredded — 12
//!   overlaps in one evening's transcript. A queued call is shown as queued,
//!   and its timeout and stall clock start only when it runs.
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
//! - **Old results age out.** A result stays in the conversation, as fed,
//!   for [`RESULT_TURNS`] turns; after that its row is replaced by a one-line
//!   stub (what ran, how it went, how long it was, `Inspect` to reread).
//!   Found 2026-09-25: every result ever fed stayed in history for good, and
//!   meow — a GLM cat, so no window, so never compacted — was re-sending
//!   ~75k tokens a turn, 181 KB of its 235 KB history old tool output.
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
use crate::langfuse_log::LangfuseLog;
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
/// Turns a fed result stays in the conversation in full before its row is
/// replaced by a stub ([`AgentStateMachine::age`]). The full text stays in the
/// tool log for `Inspect`.
pub const RESULT_TURNS: u64 = 6;
/// A fed row this short is left alone: its stub would save next to nothing.
const AGE_MIN_CHARS: usize = 400;
/// How much of a result's first line (its call and outcome) a stub keeps.
const STUB_HEAD: usize = 160;
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
    /// Where to keep the conversation across a restart of this process.
    /// `None`: it starts empty every time (`kot chat`). A cat's path carries
    /// the checkpoint epoch, so a conversation from before the chain moved on
    /// is never picked back up. Found live 2026-09-25: meow restarted with no
    /// memory at all, found its own open "reboot -f" task, and rebooted
    /// again — dozens of times, re-exploring the repo in GLM tokens on each.
    fn history_path(&self) -> Option<PathBuf> {
        None
    }
    /// Told to the model once, in the first turn after its conversation was
    /// restored from [`Host::history_path`]: that it was restarted, and
    /// anything the host knows about why (how long the machine has been up).
    fn restart_note(&self) -> Option<String> {
        None
    }
    /// Told once, in the first turn, when there was *no* conversation to
    /// restore: what the host knows about the machine it woke up on. For the
    /// first start after an upgrade, a wiped history, or a new session — a
    /// cat's local task list survives those, and an open "reboot" in it must
    /// not read as still to do.
    fn start_note(&self) -> Option<String> {
        None
    }
    /// Where to append the JSONL transcript — every turn's prompt,
    /// reasoning, text and calls, every result and record in full. `None`:
    /// no transcript.
    fn transcript(&self) -> Option<PathBuf> {
        None
    }
    /// Whether this cat gets the `Reboot` tool (compact, then actually
    /// reboot the host) — off by default. `tools()` doesn't offer the tool
    /// at all unless this says yes; `docs/TOOLING.md` has the reasoning and
    /// which cat, if any, has it turned on.
    fn reboot_tool(&self) -> bool {
        false
    }
    /// The actual OS-level side effect of `Reboot`, once history is already
    /// compacted — split out from [`reboot_tool`] on purpose: this crate's
    /// test harness turns `reboot_tool` on to exercise the compaction and
    /// gating around the call, and must never risk running a real `reboot
    /// -f` on whatever machine happens to run `cargo test`. The default
    /// does nothing — safe for any host that never sets `reboot_tool`.
    fn reboot(&self) {}
    /// Where to append the Langfuse-ingestion-shaped log
    /// ([`crate::langfuse_log`]) — a `trace-create`/`generation-create`/
    /// `span-create` per line, disk-only, nothing sent anywhere. `None`:
    /// no such log.
    fn langfuse_log(&self) -> Option<PathBuf> {
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
- Bash, ReadFile, WriteFile, Edit, MultiEdit, LS, Glob and Grep run one at a time, in the \
order you call them — across responses too: one waits for the one before it to finish, so a \
later edit never races an earlier script. Running shows a waiting one as queued. Put a long \
build last, or it holds up everything after it.\n\
- Edit changes one exact stretch of text in a file — old_string must match the file exactly \
once (whitespace and all) unless you pass replace_all. It fails, saying why, rather than \
guess: ReadFile first to get the text exact, or use WriteFile for a full rewrite. MultiEdit is \
the same rule, several edits at once, all-or-nothing.\n\
- Grep and Glob search rather than guessing a path: Grep for content (output_mode picks \
content/files_with_matches/count), Glob for a filename pattern. LS lists one directory, not \
recursively.\n\
- Bash waits 30 seconds unless you pass timeout (seconds, up to 3600). Give anything slow, \
like a build, a big enough timeout, and tell whoever asked that it's running; its output \
comes back when it finishes, however long that takes.\n\
- A long result comes back as its start and its end. Inspect with its id and an offset \
reads the part in between.\n\
- A result stays in the conversation for a few turns, then shrinks to a one-line stub. If \
you need it again, Inspect it by id rather than rerunning it.\n\
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

/// Only offered when [`Host::reboot_tool`] says so — not a shared tool.
/// Not `miot_llm::local_tools()` either: unlike `Bash`/`ReadFile`/..., which
/// every host gets, this one is a real system action (reboots the box this
/// process runs on) that must stay opt-in per cat, so it lives next to the
/// gate it's dispatched behind (`AgentStateMachine::turn`) rather than in
/// the shared tool crate.
fn reboot_tool() -> Tool {
    Tool::new("Reboot")
        .with_description(
            "Compact your conversation to a summary you write, then reboot this host \
             (busybox reboot -f, or a plain reboot -f). Only offered when explicitly enabled for \
             this cat. Write down what you were doing and what's next before you call this — \
             that summary, and your LocalTask list, are all that survive; the reboot itself does \
             not wait for anything.",
        )
        .with_schema(serde_json::json!({
            "type": "object",
            "properties": {"summary": {"type": "string", "description": "everything about the conversation so far worth remembering"}},
            "required": ["summary"]
        }))
}

/// A call in flight, as `Running`/`Cancel` see it: what it has printed so
/// far (only `Bash` prints as it goes), when it last did, and the switch
/// that stops it.
pub struct Live {
    out: std::sync::Mutex<Vec<u8>>,
    bytes: std::sync::atomic::AtomicU64,
    /// When it actually began: for a lane call, when its turn came.
    started: std::sync::Mutex<Instant>,
    /// Still waiting in the lane for the calls before it.
    queued: std::sync::atomic::AtomicBool,
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
            started: std::sync::Mutex::new(Instant::now()),
            queued: Default::default(),
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

    /// Its turn in the lane came: the clocks start now.
    fn begin(&self) {
        *self.started.lock().unwrap() = Instant::now();
        self.queued.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn is_queued(&self) -> bool {
        self.queued.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn ran(&self) -> Duration {
        self.started.lock().unwrap().elapsed()
    }

    /// How long since it last printed — or since it started, if never. Never
    /// quiet while queued: waiting its turn isn't a stall.
    fn quiet(&self) -> Duration {
        if self.is_queued() {
            return Duration::ZERO;
        }
        self.last_output.lock().unwrap().unwrap_or(*self.started.lock().unwrap()).elapsed()
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
    /// Every query result, full text — survives compaction. Result `id` is
    /// at `id - tool_base`.
    tool_log: Vec<(String, String)>,
    /// The id of `tool_log[0]`. Ids carry on across a restart (the history
    /// file keeps the next one) so an old `[#3 Bash]` in a restored
    /// conversation never names a new, different result; one below this is
    /// from before the restart and no longer kept.
    tool_base: usize,
    /// Rows still in the conversation in full, oldest first — aged into
    /// stubs by [`AgentStateMachine::age`].
    fresh: Vec<Fresh>,
    /// Turns taken, over this conversation's whole life (restarts included) —
    /// the clock [`RESULT_TURNS`] counts on.
    turns_fed: u64,
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
    /// Held by whichever lane tool (`Bash`/`ReadFile`/`WriteFile`/`Edit`/
    /// `MultiEdit`/`LS`/`Glob`/`Grep`) is running. Tokio's
    /// mutex is fair, so calls take it in the order they were dispatched.
    lane: Arc<tokio::sync::Mutex<()>>,
    /// Stall notices waiting for a turn.
    notices: Vec<String>,
    /// The next flight id.
    flights: u64,
    transcript: Option<Transcript>,
    langfuse: Option<LangfuseLog>,
    /// Stable per session — every `trace-create`/`generation-create`/
    /// `span-create` for this session's [`Self::langfuse`] shares it.
    /// Regenerated on every [`Inbound::Reset`], same as `session` itself.
    trace_id: String,
    /// Since when we've been continuously idle (no query, no record in
    /// flight) — `None` while working. Drives the local-task nudge.
    idle_since: Option<Instant>,
    /// Said once, in the next turn: the conversation was restored after a
    /// restart ([`Host::restart_note`]).
    restart_note: Option<String>,
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
    let mut langfuse = host.langfuse_log().map(LangfuseLog::open);
    let trace_id = format!("trace-{}-{}", host.name(), activity::unix_ms());
    if let Some(lf) = &mut langfuse {
        if lf.is_open() {
            host.show(ui::note(&format!("langfuse log: {}", trace_id)));
            lf.trace_create(&trace_id, &format!("{} session", host.name()), llm.label(), window);
        } else {
            host.show(ui::note("langfuse log: can't open — not writing one"));
        }
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
        tool_base: 0,
        fresh: Vec::new(),
        turns_fed: 0,
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
        langfuse,
        trace_id,
        live: std::collections::HashMap::new(),
        lane: Arc::new(tokio::sync::Mutex::new(())),
        notices: Vec::new(),
        idle_since: None,
        local_nudges: 0,
        restart_note: None,
    };
    if let Some(path) = m.host.history_path() {
        let restored = load_history(&path);
        if !restored.history.is_empty() {
            m.host.show(ui::note(&format!("restored {} message(s) of conversation from {}", restored.history.len(), path.display())));
            if restored.trimmed > 0 {
                m.host.show(ui::note(&format!("{} old-format message(s) had their tool results cut down to one line each", restored.trimmed)));
            }
            m.log(serde_json::json!({"t": "restored", "messages": restored.history.len(), "trimmed": restored.trimmed}));
            m.history = restored.history;
            m.fresh = restored.fresh;
            m.turns_fed = restored.turns;
            m.tool_base = restored.next_id;
            m.restart_note = Some(m.host.restart_note().unwrap_or_else(|| RESTARTED.to_string()));
        } else {
            m.restart_note = m.host.start_note();
        }
    }
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
                m.persist();
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
        m.persist();
    }
}

impl<H: Host> AgentStateMachine<H> {
    /// The conversation to [`Host::history_path`], if the host keeps one.
    /// Best effort: a write that fails costs the next restart its memory,
    /// never this turn.
    fn persist(&self) {
        if let Some(path) = self.host.history_path() {
            save_history(&path, &Saved { history: self.history.clone(), fresh: self.fresh.clone(), turns: self.turns_fed, next_id: self.next_id(), trimmed: 0 });
        }
    }

    fn next_id(&self) -> usize {
        self.tool_base + self.tool_log.len()
    }

    /// Result `id`'s tool and full text, if it's still kept.
    fn logged(&self, id: usize) -> Option<&(String, String)> {
        id.checked_sub(self.tool_base).and_then(|i| self.tool_log.get(i))
    }

    /// Replace every row fed [`RESULT_TURNS`] or more turns ago with its
    /// stub, in the message it went out in. A row whose message is gone (a
    /// failed turn's prompt, popped) is just forgotten.
    fn age(&mut self) {
        let now = self.turns_fed;
        let (old, keep): (Vec<Fresh>, Vec<Fresh>) = std::mem::take(&mut self.fresh).into_iter().partition(|f| now.saturating_sub(f.turn) >= RESULT_TURNS);
        self.fresh = keep;
        for f in old {
            let stub = if f.id >= self.tool_base {
                format!("[#{} {}] {} — {} chars, out of the conversation now; Inspect {{\"id\": {}}} reads it again.", f.id, f.name, f.head, f.chars, f.id)
            } else {
                format!("[#{} {}] {} — {} chars, from before a restart and no longer kept; run it again if you need it.", f.id, f.name, f.head, f.chars)
            };
            if let Some(m) = self.history.iter_mut().rev().find(|(who, said)| *who == Speaker::User && said.contains(&f.full)) {
                m.1 = m.1.replacen(&f.full, &stub, 1);
            }
        }
    }

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
                self.trace_id = format!("trace-{}-{}", self.host.name(), activity::unix_ms());
                if let Some(lf) = &mut self.langfuse {
                    lf.trace_create(&self.trace_id, &format!("{} session", self.host.name()), self.llm.label(), self.window);
                }
                self.history.clear();
                self.fresh.clear();
                self.restart_note = None;
                self.persist();
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
                let (tool, arg, ms) = self.land(Some(flight), "", "", ok, meta.clone());
                let arg = out.as_ref().map(|o| o.arg.clone()).unwrap_or(arg);
                self.log(serde_json::json!({"t": "record", "tool": tool, "arg": arg, "ok": ok, "meta": meta}));
                if let Some(lf) = &mut self.langfuse {
                    lf.span_create(&format!("span-{}-{flight}", self.session), &self.trace_id, &tool, ms, &arg, &meta, ok);
                }
            }
            Back::Result(name, out, session, flight) => {
                self.queries = self.queries.saturating_sub(1);
                self.host.show(ui::tool(self.host.name(), &name, &out));
                let (_, _, ms) = self.land(flight, &name, &out.arg, out.ok, out.meta.join(" · "));
                if let Some(lf) = &mut self.langfuse {
                    let span_flight = flight.map(|f| f.to_string()).unwrap_or_else(|| "instant".into());
                    lf.span_create(&format!("span-{}-{span_flight}", self.session), &self.trace_id, &name, ms, &out.arg, &out.text(), out.ok);
                }
                if session != self.session {
                    self.host.show(ui::note(&format!("{name}'s result is from before the session reset — not fed back")));
                    self.log(serde_json::json!({"t": "result", "id": null, "tool": name, "ok": out.ok, "stale": true, "text": out.text()}));
                    return;
                }
                let id = self.next_id();
                self.tool_log.push((name, keep(&out.text())));
                let (tool, text) = self.tool_log.last().expect("just pushed").clone();
                let fed = feed(id, &text);
                self.log(serde_json::json!({"t": "result", "id": id, "tool": tool, "ok": out.ok, "text": text}));
                results.push((id, fed));
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
                let ran = ui::human(live.ran().as_secs());
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
                if l.is_queued() {
                    return format!("r{} {} {} — queued behind the calls before it", f.id, f.tool, f.arg);
                }
                let ran = l.ran().as_secs();
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
    /// one. Returns the call's tool, argument and how long it ran.
    fn land(&mut self, flight: Option<u64>, tool: &str, arg: &str, ok: bool, meta: String) -> (String, String, u64) {
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
        (tool, arg, ms)
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
        if self.host.reboot_tool() {
            t.push(reboot_tool());
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

        self.turns_fed += 1;
        let mut msg: Vec<String> = wakes.iter().map(|w| w.0.clone()).collect();
        if !results.is_empty() {
            let mut rows = Vec::new();
            for (id, text) in &results {
                let (name, full_text) = self.logged(*id).cloned().unwrap_or_default();
                let row = format!("[#{id} {name}] {text}");
                if row.chars().count() > AGE_MIN_CHARS {
                    let head = full_text.lines().next().unwrap_or("").chars().take(STUB_HEAD).collect();
                    self.fresh.push(Fresh { id: *id, name, head, chars: full_text.chars().count(), turn: self.turns_fed, full: row.clone() });
                }
                rows.push(row);
            }
            msg.push(format!("Results of tools you called:\n{}", rows.join("\n\n")));
        }
        if !self.notices.is_empty() {
            msg.push(std::mem::take(&mut self.notices).join("\n"));
        }
        msg.extend(self.restart_note.take());
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
        self.age();
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
            "cached_tokens": turn.cached_tokens,
            "ms": turn.ms,
        }));
        if let Some(lf) = &mut self.langfuse {
            let gen_id = format!("gen-{}-{}", self.session, self.turns_fed);
            lf.generation_create(&gen_id, &self.trace_id, self.llm.label(), turn.ms, &prompt, &turn.text, turn.prompt_tokens, turn.cached_tokens, turn.tokens, turn.total_tokens);
        }
        let messages = turn.calls.iter().filter(|c| c.name == "SendMessage").count();
        let cost = TurnCost {
            prompt: turn.prompt_tokens,
            out: turn.tokens,
            total: turn.total_tokens,
            cached: turn.cached_tokens,
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
                    self.fresh.clear();
                    self.warned_tier = 0;
                    compacted = true;
                }
                // Gated by `Host::reboot_tool` — off by default, `tools()`
                // doesn't even offer it unless the host says so (meow only,
                // config: `docs/TOOLING.md`). Compacts first, same as
                // `Compact` above, then actually reboots: 0 `compact`
                // events across 96 of meow's real restarts, most of them
                // its own `reboot -f`, is the finding this exists to fix —
                // the conversation now leaves itself a note before the box
                // goes down, instead of just losing whatever wasn't
                // written to a `LocalTask`.
                "Reboot" if self.host.reboot_tool() => {
                    let summary = c.str("summary").unwrap_or_default();
                    self.host.show(ui::note(&format!(
                        "compacting before reboot: history replaced with a {}-char summary the model wrote; rebooting now",
                        summary.len()
                    )));
                    self.log(serde_json::json!({"t": "compact", "forced": false, "reboot": true, "summary": summary}));
                    self.history = vec![(Speaker::Assistant, summary)];
                    self.fresh.clear();
                    self.warned_tier = 0;
                    compacted = true;
                    self.persist();
                    self.host.reboot();
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
                self.fresh.clear();
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
            Some(q) => {
                // Into the lane: the call doesn't start (no child spawned, no
                // file touched, no timeout ticking) until the one before it
                // has finished. Its place is taken *here*, in dispatch order:
                // a tokio lock joins the mutex's fair queue when first polled,
                // and spawned tasks are first polled in no particular order —
                // so poll it once now. Cancelling a queued call drops it out of
                // the queue; whatever is running keeps the lane.
                use futures_util::FutureExt as _;
                let mut ticket = Box::pin(self.lane.clone().lock_owned());
                let got = (&mut ticket).now_or_never();
                live.queued.store(got.is_none(), std::sync::atomic::Ordering::Relaxed);
                let live = live.clone();
                Dispatch::Query(Box::pin(async move {
                    let _turn = match got {
                        Some(g) => g,
                        None => ticket.await,
                    };
                    live.begin();
                    q.await
                }))
            }
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
                    .map(|(i, (name, out))| format!("{}: {name} — {}", self.tool_base + i, out.lines().next().unwrap_or("").chars().take(60).collect::<String>()))
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
                            if l.is_queued() {
                                return ToolOut::new(format!("r{w}"), true).body(format!("r{} {} {} — queued: it runs when the calls before it finish.", f.id, f.tool, f.arg));
                            }
                            let ran = ui::human(l.ran().as_secs());
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
                if let Some(i) = id.filter(|&i| i < self.tool_base) {
                    return ToolOut::new(format!("#{i}"), false).meta("from before a restart — not kept; run it again if you need it");
                }
                match id.and_then(|i| self.logged(i).map(|r| (i, r))) {
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
                    None => ToolOut::new(format!("{id:?}"), false).meta(format!("no such id (#{}–#{} stored)", self.tool_base, self.next_id().saturating_sub(1))),
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

/// `Bash`/`ReadFile`/`WriteFile`/`Edit`/`MultiEdit`/`LS`/`Glob`/`Grep` on
/// this host — the same for every cat and for `kot chat`. No sandbox.
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
        // Schema in `miot_llm::edit_tool` — see its header for where the
        // shape (and the "must match exactly once" rule) comes from.
        "Edit" => {
            let path = c.str("file_path").unwrap_or_default();
            let old = c.str("old_string").unwrap_or_default();
            let new = c.str("new_string").unwrap_or_default();
            let replace_all = c.args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
            Some(Box::pin(async move {
                let content = match tokio::fs::read_to_string(&path).await {
                    Ok(s) => s,
                    Err(e) => return ToolOut::new(path, false).body(e.to_string()),
                };
                match apply_edit(&content, &old, &new, replace_all) {
                    Ok(updated) => match tokio::fs::write(&path, &updated).await {
                        Ok(()) => ToolOut::new(path, true).meta(edit_meta(&content, &old, replace_all)),
                        Err(e) => ToolOut::new(path, false).body(e.to_string()),
                    },
                    Err(msg) => ToolOut::new(path, false).meta(msg).body("Nothing written.".to_string()),
                }
            }))
        }
        // Schema in `miot_llm::fs_tools` — see its header for the
        // `file_path`/`path` split and what's not reproduced from real
        // Grep/LS (`multiline`, `type`).
        "MultiEdit" => {
            let path = c.str("file_path").unwrap_or_default();
            let edits: Vec<(String, String, bool)> = c
                .args
                .get("edits")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|e| {
                            (
                                e.get("old_string").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                                e.get("new_string").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                                e.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Box::pin(async move {
                let mut content = match tokio::fs::read_to_string(&path).await {
                    Ok(s) => s,
                    Err(e) => return ToolOut::new(path, false).body(e.to_string()),
                };
                let n = edits.len();
                for (i, (old, new, replace_all)) in edits.iter().enumerate() {
                    match apply_edit(&content, old, new, *replace_all) {
                        Ok(updated) => content = updated,
                        Err(msg) => {
                            return ToolOut::new(path, false)
                                .meta(format!("edit {} of {n}: {msg}", i + 1))
                                .body("Nothing written — every edit must succeed before any of them are applied.".to_string());
                        }
                    }
                }
                match tokio::fs::write(&path, &content).await {
                    Ok(()) => ToolOut::new(path, true).meta(format!("{n} edit(s) applied")),
                    Err(e) => ToolOut::new(path, false).body(e.to_string()),
                }
            }))
        }
        "LS" => {
            // `path` is schema-required, but a small model doesn't always
            // honor that (found live against qwen3:4b, 2026-09-26: it
            // omitted `path` outright, expecting the same cwd-default
            // Glob/Grep already have, and got "No such file or directory"
            // from an empty one). Same fallback as those two, for the same
            // reason.
            let path = c.str("path").filter(|s| !s.is_empty()).unwrap_or_else(|| ".".to_string());
            let ignore: Vec<String> = c.args.get("ignore").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            Some(Box::pin(async move {
                let mut dir = match tokio::fs::read_dir(&path).await {
                    Ok(d) => d,
                    Err(e) => return ToolOut::new(path, false).body(e.to_string()),
                };
                let mut names = Vec::new();
                loop {
                    match dir.next_entry().await {
                        Ok(Some(e)) => {
                            let name = e.file_name().to_string_lossy().into_owned();
                            if ignore.iter().any(|pat| glob_match(pat, &name)) {
                                continue;
                            }
                            let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                            names.push(if is_dir { format!("{name}/") } else { name });
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
                names.sort();
                let n = names.len();
                ToolOut::new(path, true).meta(format!("{n} entrie(s)")).body(names.join("\n"))
            }))
        }
        "Glob" => {
            let pattern = c.str("pattern").unwrap_or_default();
            let base = c.str("path").filter(|s| !s.is_empty()).unwrap_or_else(|| ".".to_string());
            Some(Box::pin(async move {
                let name_pat = pattern.strip_prefix("**/").unwrap_or(&pattern);
                let mut cmd = tokio::process::Command::new("find");
                cmd.arg(&base).arg("-type").arg("f");
                if pattern.contains('/') && !pattern.starts_with("**/") {
                    cmd.arg("-path").arg(format!("*/{pattern}"));
                } else {
                    cmd.arg("-name").arg(name_pat);
                }
                let out = cmd.output().await;
                let arg = format!("{pattern} under {base}");
                match out {
                    Ok(o) => {
                        let paths: Vec<String> = String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect();
                        if !o.status.success() && paths.is_empty() {
                            return ToolOut::new(arg, false).body(String::from_utf8_lossy(&o.stderr).to_string());
                        }
                        let mut with_mtime = Vec::with_capacity(paths.len());
                        for p in paths {
                            let mtime = tokio::fs::metadata(&p).await.ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                            with_mtime.push((mtime, p));
                        }
                        with_mtime.sort_by(|a, b| b.0.cmp(&a.0));
                        let n = with_mtime.len();
                        let body = with_mtime.into_iter().take(200).map(|(_, p)| p).collect::<Vec<_>>().join("\n");
                        ToolOut::new(arg, true).meta(format!("{n} match(es)")).body(body)
                    }
                    Err(e) => ToolOut::new(arg, false).body(e.to_string()),
                }
            }))
        }
        "Grep" => {
            let pattern = c.str("pattern").unwrap_or_default();
            let path = c.str("path").filter(|s| !s.is_empty()).unwrap_or_else(|| ".".to_string());
            let glob = c.str("glob");
            let content_mode = c.str("output_mode").as_deref() == Some("content");
            let count_mode = c.str("output_mode").as_deref() == Some("count");
            let ci = c.args.get("-i").and_then(|v| v.as_bool()).unwrap_or(false);
            let line_numbers = c.args.get("-n").and_then(|v| v.as_bool()).unwrap_or(false);
            let before = c.args.get("-B").and_then(|v| v.as_u64());
            let after = c.args.get("-A").and_then(|v| v.as_u64());
            let around = c.args.get("-C").and_then(|v| v.as_u64());
            let head_limit = c.args.get("head_limit").and_then(|v| v.as_u64()).map(|n| n as usize);
            Some(Box::pin(async move {
                let mut gcmd = tokio::process::Command::new("grep");
                gcmd.arg("-r");
                if count_mode {
                    gcmd.arg("-c");
                } else if !content_mode {
                    gcmd.arg("-l");
                } else if line_numbers {
                    gcmd.arg("-n");
                }
                if ci {
                    gcmd.arg("-i");
                }
                if content_mode {
                    if let Some(n) = around {
                        gcmd.arg(format!("-C{n}"));
                    } else {
                        if let Some(n) = before {
                            gcmd.arg(format!("-B{n}"));
                        }
                        if let Some(n) = after {
                            gcmd.arg(format!("-A{n}"));
                        }
                    }
                }
                if let Some(g) = &glob {
                    gcmd.arg(format!("--include={g}"));
                }
                gcmd.arg("-E").arg(&pattern).arg(&path);
                let arg = format!("{pattern} in {path}");
                match gcmd.output().await {
                    Ok(o) => {
                        // grep: 0 matches found, 1 no matches (still a
                        // successful search), >=2 a real error.
                        if o.status.code().is_none_or(|c| c > 1) {
                            return ToolOut::new(arg, false).body(String::from_utf8_lossy(&o.stderr).to_string());
                        }
                        let mut lines: Vec<String> = String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect();
                        if let Some(n) = head_limit {
                            lines.truncate(n);
                        }
                        ToolOut::new(arg, true).meta(format!("{} line(s)", lines.len())).body(lines.join("\n"))
                    }
                    Err(e) => ToolOut::new(arg, false).body(e.to_string()),
                }
            }))
        }
        _ => None,
    }
}

/// `old` must appear in `content` exactly once unless `replace_all` — the
/// rule kept from Anthropic's `str_replace` (`miot_llm::edit_tool`'s
/// header). `Err` names why, for the model to retry against.
fn apply_edit(content: &str, old: &str, new: &str, replace_all: bool) -> Result<String, String> {
    let n = content.matches(old).count();
    if n == 0 {
        return Err("old_string not found".to_string());
    }
    if n > 1 && !replace_all {
        return Err(format!("old_string matches {n} times"));
    }
    Ok(if replace_all { content.replace(old, new) } else { content.replacen(old, new, 1) })
}

fn edit_meta(content: &str, old: &str, replace_all: bool) -> String {
    let n = content.matches(old).count();
    if replace_all { format!("{n} replaced") } else { "1 replaced".to_string() }
}

/// `*` and `?` only — enough for `LS`'s `ignore` list (`node_modules`,
/// `*.lock`, ...). Not a full glob: no `**`, no character classes.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => (0..=s.len()).any(|i| go(&p[1..], &s[i..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &s[1..]),
            (Some(&pc), Some(&sc)) if pc == sc => go(&p[1..], &s[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

/// A call's gist, one line, for the running list and the log: the command,
/// the path, the recipient and message — whatever says what it's doing.
fn gist(c: &Call) -> String {
    let s = |k: &str| c.str(k).unwrap_or_default();
    match c.name.as_str() {
        "Bash" => format!("$ {}", s("command")),
        "ReadFile" | "WriteFile" => s("path"),
        "Edit" | "MultiEdit" => s("file_path"),
        "LS" => s("path"),
        "Grep" | "Glob" => {
            let path = s("path");
            format!("{} in {}", s("pattern"), if path.is_empty() { ".".to_string() } else { path })
        }
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
/// The note a restored conversation gets when the host has nothing better to
/// say ([`Host::restart_note`]).
const RESTARTED: &str = "(You were restarted. The conversation above is from before it. \
    Anything you started then that was still running is gone, and anything that \
    needed a restart — a reboot, a reinstall — has happened. Check before redoing it.)";

/// A row still in the conversation in full, and what its stub needs.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Fresh {
    id: usize,
    name: String,
    /// The result's first line — its call and how it went.
    head: String,
    /// The full result's length, as kept in the tool log.
    chars: usize,
    /// [`AgentStateMachine::turns_fed`] when it was fed.
    turn: u64,
    /// The row exactly as it went into the message, to find and replace.
    full: String,
}

/// What [`Host::history_path`] holds: the conversation, the rows in it not
/// yet aged, and the two counters that must carry on across a restart.
#[derive(Default)]
pub struct Saved {
    pub history: Vec<(Speaker, String)>,
    fresh: Vec<Fresh>,
    pub turns: u64,
    /// The id the next result gets.
    pub next_id: usize,
    /// Old-format messages whose results were cut down on load.
    pub trimmed: usize,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedFile {
    history: Vec<(String, String)>,
    #[serde(default)]
    fresh: Vec<Fresh>,
    #[serde(default)]
    turns: u64,
    #[serde(default)]
    next_id: usize,
}

const RESULTS_HEADER: &str = "Results of tools you called:\n";

/// `{"history": [["user", "…"], ["assistant", "…"]], "fresh": […], …}`. An
/// unreadable or malformed file is an empty conversation: starting fresh is
/// what a cat did before this existed, so it's always a safe fallback.
///
/// The format before aging was the bare `history` array. Loading one cuts
/// every results block in it down to each row's first line
/// ([`trim_legacy`]): nothing records which rows are whose, and those
/// results are from before the restart anyway. meow's was 235 KB, 181 KB of
/// it tool output (2026-09-25).
pub fn load_history(path: &std::path::Path) -> Saved {
    let Ok(text) = std::fs::read_to_string(path) else { return Saved::default() };
    let parse = |rows: Vec<(String, String)>| -> Vec<(Speaker, String)> {
        rows.into_iter()
            .filter_map(|(who, said)| match who.as_str() {
                "user" => Some((Speaker::User, said)),
                "assistant" => Some((Speaker::Assistant, said)),
                _ => None,
            })
            .collect()
    };
    if let Ok(f) = serde_json::from_str::<SavedFile>(&text) {
        return Saved { history: parse(f.history), fresh: f.fresh, turns: f.turns, next_id: f.next_id, trimmed: 0 };
    }
    let Ok(rows) = serde_json::from_str::<Vec<(String, String)>>(&text) else { return Saved::default() };
    let mut history = parse(rows);
    let (trimmed, max_id) = trim_legacy(&mut history);
    Saved { history, fresh: Vec::new(), turns: 0, next_id: max_id.map_or(0, |m| m + 1), trimmed }
}

/// Cut each old-format results block to one line per row (`[#3 Bash] $ make
/// (exit 0, 2s)`), dropping whatever followed it in that message (a
/// still-running list, a reminder — stale either way). Returns how many
/// messages changed and the highest result id seen, so new ids start above it.
fn trim_legacy(history: &mut [(Speaker, String)]) -> (usize, Option<usize>) {
    let mut trimmed = 0;
    let mut max_id = None;
    for (who, said) in history.iter_mut() {
        if *who != Speaker::User {
            continue;
        }
        let Some(at) = said.find(RESULTS_HEADER) else { continue };
        let heads: Vec<String> = said[at..]
            .lines()
            .filter(|l| row_id(l).is_some())
            .inspect(|l| max_id = max_id.max(row_id(l)))
            .map(|l| l.chars().take(STUB_HEAD).collect())
            .collect();
        let short = format!(
            "{}{RESULTS_HEADER}{}\n(Only each result's first line is kept: they're from before a restart and can't be read again.)",
            &said[..at],
            heads.join("\n")
        );
        if short.len() < said.len() {
            *said = short;
            trimmed += 1;
        }
    }
    (trimmed, max_id)
}

/// `Some(3)` for a row's first line, `[#3 Bash] …`.
fn row_id(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("[#")?;
    let (n, rest) = rest.split_once(' ')?;
    rest.split_once("] ")?;
    n.parse().ok()
}

/// [`load_history`]'s inverse, written to a temp file and renamed, so a crash
/// mid-write leaves the previous conversation rather than half of one.
pub fn save_history(path: &std::path::Path, saved: &Saved) {
    let file = SavedFile {
        history: saved.history.iter().map(|(who, said)| ((if *who == Speaker::User { "user" } else { "assistant" }).to_string(), said.clone())).collect(),
        fresh: saved.fresh.clone(),
        turns: saved.turns,
        next_id: saved.next_id,
    };
    let Ok(text) = serde_json::to_string(&file) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.new");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

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
