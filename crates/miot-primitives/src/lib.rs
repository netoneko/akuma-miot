//! Shared vocabulary for the Akuma Miot task lifecycle.
//!
//! Named by three consumers that must not drift: the pure state machine
//! (`miot-tasks`), the FRAME pallet that wraps it, and every agent. That is
//! the same role `litter-wire` plays for the litter today — "so the two ends
//! of the wire cannot independently drift" — one layer down.
//!
//! Two rules hold everywhere below, and both come from the litter:
//!
//! - **No clock.** Time is a [`BlockNumber`] parameter. A state machine with a
//!   clock in it cannot be tested, and one that is going to run inside a
//!   runtime cannot have one at all.
//! - **Applied-vs-refused is typed.** Three places in the litter re-derived
//!   acceptance by testing whether a note *began with the word "refused"*, and
//!   the "already claimed" refusals said no such thing — so a no-op would have
//!   been replicated to the whole litter as though it had happened. Here that
//!   distinction is [`Error`] versus [`Effect`], and there is no prose to
//!   sniff.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Chain height. `u32` at 6 s blocks is ~800 years, which is longer than this
/// project needs to be interesting.
pub type BlockNumber = u32;

/// A parent task, or one sub-task of one.
///
/// `sub == 0` **is** the parent — not a sentinel for "no sub-task", but the
/// parent's own address, so `t42` and `t42.0` are the same thing and a caller
/// cannot accidentally address a parent as a sub-task or vice versa.
///
/// A pair of integers rather than the litter's `"t1.2"` strings: strings in
/// runtime storage are a cost and a parsing surface, and every comparison the
/// table does is on identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo))]
#[cfg_attr(feature = "codec", scale_info(skip_type_params(A)))]
pub struct TaskId {
    pub parent: u32,
    pub sub: u16,
}

impl TaskId {
    pub const fn parent(n: u32) -> Self {
        TaskId { parent: n, sub: 0 }
    }

    pub const fn sub(parent: u32, sub: u16) -> Self {
        TaskId { parent, sub }
    }

    pub const fn is_parent(self) -> bool {
        self.sub == 0
    }

    /// The parent this id belongs to — itself, if it is already one.
    pub const fn parent_id(self) -> Self {
        TaskId {
            parent: self.parent,
            sub: 0,
        }
    }
}

impl core::fmt::Display for TaskId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_parent() {
            write!(f, "t{}", self.parent)
        } else {
            write!(f, "t{}.{}", self.parent, self.sub)
        }
    }
}

/// Where a task is in its life.
///
/// Parents walk `Open → Planned → Closed`. Sub-tasks walk
/// `Pending → InProgress → AwaitingClearance → Cleared`, with `Reopen` sending
/// one back to `Pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo))]
pub enum TaskStatus {
    /// Parent: opened, not yet split.
    Open,
    /// Parent: split into sub-tasks. Stays here until every one is `Cleared`.
    Planned,
    /// Parent: artifact submitted, done.
    Closed,
    /// Parent: the leader never resolved an outstanding directive within its
    /// nag budget — no automatic retry, no automatic reassignment (there is
    /// no such act), and nobody asked for an extension. Terminal, like
    /// `Closed`, but never carries an artifact: nothing was produced.
    Failed,
    /// Sub-task: offered to its assignee, unclaimed.
    Pending,
    /// Sub-task: claimed, lease running.
    InProgress,
    /// Sub-task: a result is in — successful or failed — awaiting the leader.
    AwaitingClearance,
    /// Sub-task: the leader accepted the result.
    Cleared,
}

/// One act on a task.
///
/// A single enum rather than one call per act, because the litter found a
/// small model picks a *value* more reliably than it picks among
/// near-identical tool names — and a new act then costs a value instead of new
/// surface. `Failed` was the first one added that way, and proved it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo))]
pub enum Act {
    Claim,
    Done,
    Failed,
    Clear,
    Reopen,
    Artifact,
}

impl Act {
    pub const fn as_str(self) -> &'static str {
        match self {
            Act::Claim => "claim",
            Act::Done => "done",
            Act::Failed => "failed",
            Act::Clear => "clear",
            Act::Reopen => "reopen",
            Act::Artifact => "artifact",
        }
    }
}

/// What the caller is allowed to be, decided by the *origin* and never read
/// off a field the sender filled in.
///
/// This is the whole point of the move to a chain. The litter stamps authority
/// hub-side from a `from` string the sender picks, and its own documentation
/// calls that out: *"root is the key to the cat house"* — anyone who can reach
/// the socket can claim to be the operator. Under FRAME this comes from
/// `ensure_signed`/`ensure_root`, so there is no name to forge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo))]
pub enum Authority {
    /// The operator. Outranks the leader. **Never a worker** — see
    /// `Error::RootNotAssignable`.
    Root,
    /// Whoever currently holds the leader role.
    Leader,
    /// An ordinary member of the litter.
    Peer,
}

/// One line of a leader's plan: who does what, and what "done" looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo))]
#[cfg_attr(feature = "codec", scale_info(skip_type_params(A)))]
pub struct PlanItem<A> {
    pub who: A,
    pub what: String,
    /// What the leader expects back. Optional; empty is fine.
    pub expect: String,
}

/// A finished sub-task's result. `Failed` is a **sibling** of `Done`, not a
/// flavour of it: both land in `AwaitingClearance` and differ only in what is
/// stored, because whether to retry, reassign or accept a failure is the
/// leader's decision and not the table's.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo))]
pub enum Outcome {
    Done(String),
    Failed(String),
}

impl Outcome {
    pub fn text(&self) -> &str {
        match self {
            Outcome::Done(t) | Outcome::Failed(t) => t,
        }
    }

    pub const fn failed(&self) -> bool {
        matches!(self, Outcome::Failed(_))
    }
}

/// A leader decision the table is waiting on.
///
/// These exist because *a 0.8B model will not infer `[artifact: t1]` from a
/// design document*. Every point where the workflow needs the leader to act,
/// the table names the exact verb and delivers it, the same way assignments
/// are delivered to workers — and re-sends on a nag interval, because a
/// dropped directive stalls a parent permanently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo))]
pub enum Directive {
    /// A parent is open and unsplit.
    PlanNeeded,
    /// At least one sub-task is awaiting clearance.
    ClearanceNeeded,
    /// Every sub-task is cleared; the parent wants its artifact.
    ArtifactNeeded,
    /// A sub-task has been re-offered to its assignee as many times as the
    /// budget allows and is still unclaimed. The assignee is dead, wedged, or
    /// unable — and the table stops guessing which. Only the leader can decide
    /// where the work goes instead, so it is asked.
    ReassignNeeded,
    /// This account just became leader. Promotion is an instruction to act,
    /// not merely a fact to notice.
    LeaderElected,
}

/// Why a sub-task went back into the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo))]
pub enum Requeue {
    /// Nobody claimed the offer inside the claim window.
    Unclaimed,
    /// The leader re-homed it to somebody else.
    Rehomed,
    /// The holder's lease ran out.
    LeaseExpired,
    /// The leader rejected the result.
    Reopened,
}

/// Something that happened, for the caller to fan out.
///
/// The waking/non-waking split is load-bearing and comes straight from the
/// litter: a **record is not an instruction to anyone**, and waking four
/// agents per record turns one task into sixteen LLM turns. So
/// [`Effect::wakes`] is what an agent's aggregator consults, and it is a
/// property of the effect rather than a decision each consumer re-derives.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo))]
#[cfg_attr(feature = "codec", scale_info(skip_type_params(A)))]
pub enum Effect<A> {
    /// Targeted, waking: a sub-task is offered to its assignee.
    Assigned {
        to: A,
        task: TaskId,
        what: String,
        expect: String,
    },
    /// Targeted, waking: the table tells the leader which verb to type.
    Directed {
        to: A,
        task: TaskId,
        directive: Directive,
    },
    /// Targeted, waking: get on with the work you claimed.
    ///
    /// `remaining` counts nudges left after this one; `last` says this is the
    /// final one. **Bounded on purpose** — every nudge costs an LLM turn, so
    /// an unbounded reminder is a loop that pays forever for an agent that was
    /// never going to answer, while the litter's other work queues behind it.
    Nudge {
        to: A,
        task: TaskId,
        remaining: u8,
        last: bool,
    },
    /// Somebody said something.
    ///
    /// `to` is `None` for the whole litter. Waking is decided by
    /// [`Effect::wakes`]: a message addressed to one cat wakes it, and so does
    /// anything from the operator — root speaking is an instruction, not
    /// chatter. A peer talking to the litter at large is **not** waking, for
    /// the same reason a record is not: waking four cats per broadcast turns
    /// one remark into four LLM turns.
    /// `no_ack`: the sender's own signal that this doesn't need a reply —
    /// a closing remark, a pure acknowledgment — so the recipient's prompt
    /// (`agent.rs`'s `Cat::prompt`) can say so instead of demanding
    /// `SendMessage` every time regardless of content. Added 2026-09-23
    /// after two cats ping-ponged "thanks!"/"sounds good!" for a dozen
    /// turns straight: the old prompt's "Reply with SendMessage" applied
    /// uniformly whether the message asked something or just closed one
    /// out, so every closing remark generated another one.
    ///
    /// `off_record`: the sender's own signal that this message must never
    /// land in the block log — the host (`kot::node::Node::absorb`) still
    /// fans it out live (wakes, `/events`) but leaves it out of the block
    /// body it hands `Store::append`, so it never survives a replay,
    /// rewind, or a peer that wasn't already tailing live. Safe to do
    /// unconditionally because `TaskTable::apply(Said)` is already a
    /// no-op — chat touches no task state, so skipping persistence can
    /// never desync a replica from the primary.
    Said { from: A, to: Option<A>, body: String, from_root: bool, no_ack: bool, off_record: bool },
    /// Broadcast, non-waking: a parent task was opened.
    ///
    /// Carries `text` — the task's own description — because this effect is
    /// also the sole record `TaskTable::apply` has to reconstruct the row
    /// from: state is rebuilt by folding the effect log, not by re-running
    /// the original transaction, so anything the table needs that isn't
    /// already derivable from existing state has to live on the effect. See
    /// `docs/PROTOCOL.md`'s tx/state/event section.
    Opened { who: A, task: TaskId, text: String },
    /// Broadcast, non-waking: a parent was split, atomically, into `count`
    /// directed sub-tasks.
    Planned { who: A, task: TaskId, count: u32 },
    /// Broadcast, non-waking: an accepted act. Refused acts produce no
    /// record — nothing happened, so there is nothing to replicate.
    ///
    /// `text` carries `Act::Done`/`Act::Failed`'s result body — the same
    /// text that becomes `Task::outcome` — empty for every other `Act`. Same
    /// reason as `Opened::text`: the effect is what `apply` rebuilds state
    /// from, so the result body has to ride along rather than live only in
    /// the original (unpersisted) transaction.
    Record { who: A, task: TaskId, act: Act, text: String },
    /// Broadcast, non-waking: a sub-task returned to the queue.
    Requeued {
        task: TaskId,
        from: Option<A>,
        why: Requeue,
    },
    /// Broadcast, non-waking: the nudge budget for a holder is spent. Said
    /// once, then the table stops asking and lets the lease do its work.
    NudgeBudgetSpent { holder: A, task: TaskId },
    /// Broadcast, non-waking: a parent closed with its artifact.
    ///
    /// `body` and `author` ride along for the same reason `Opened::text`
    /// does: `Artifact::body`/`::author` live only here and in `State`, so
    /// without them `apply` could set the parent `Closed` but could never
    /// reconstruct the artifact itself on replay.
    Closed { task: TaskId, title: String, body: String, author: A },
    /// Broadcast, non-waking: a parent's directive-nag budget ran out and
    /// the table closed it as [`TaskStatus::Failed`] rather than nag
    /// forever. Distinct from [`Effect::NudgeBudgetSpent`] (that one leaves
    /// the sub-task open, riding its lease out) because there is no lease
    /// and no reassignment act for a *leader* — going quiet would just mean
    /// nobody ever hears about it. This is the operator's signal instead.
    Failed { task: TaskId },
    /// Broadcast, non-waking: the leader moved a sub-task to a different cat.
    Rehomed { task: TaskId, from: Option<A>, to: A },
    /// Broadcast, non-waking: a standalone artifact published with no task
    /// behind it — the usual artifact lifecycle needs a parent and every
    /// sub-task cleared, which is real ceremony for "I looked at something,
    /// here's what I found" with nobody coordinating it. `id` is a running
    /// counter, its own namespace, never a [`TaskId`] — this was never part
    /// of a plan.
    StandaloneArtifact { author: A, id: u32, title: String, body: String },
    /// Broadcast, non-waking: a cat's own cumulative work stats, self-
    /// reported (`pallet_litter::Call::report_stats`). Not part of
    /// `miot_tasks::State` — `who`'s `Stats` storage row lives in
    /// `pallet-litter` alone — but it still has to ride as an `Effect`
    /// like everything else here: a replica only ever folds state by
    /// replaying the effect log (`Node::apply_block`), never by
    /// re-executing the original extrinsic, so a state change with no
    /// effect is invisible to every replica forever. Found live
    /// 2026-09-23: the first version of this call wrote straight to
    /// storage with no effect at all, and `GET /stats` came back correct
    /// on the primary and permanently empty on every replica.
    StatsReported { who: A, turns: u32, tool_calls: u32, tokens: u64, ms: u64 },
}

impl<A> Effect<A> {
    /// Whether this effect should assemble an LLM turn for its recipient(s).
    ///
    /// A broadcast `Said` (`to: None`) always wakes now — every live cat,
    /// not just root's own broadcasts. Until 2026-09-23 this was
    /// `to.is_some() || from_root`, so a non-root broadcast woke nobody at
    /// all despite looking delivered to a human watching the raw log
    /// (rendering never consulted `wakes()`). Fan-out to "everyone" is a
    /// wire concern (who the *other* live accounts even are), not
    /// something this `no_std` type can decide on its own — see
    /// `Node::absorb` for where a broadcast actually turns into "wakes
    /// every member but the sender."
    pub const fn wakes(&self) -> bool {
        matches!(self, Effect::Assigned { .. } | Effect::Directed { .. } | Effect::Nudge { .. } | Effect::Said { .. })
    }

    /// Who this is addressed to, if anyone specific. `None` is a broadcast
    /// — [`Effect::wakes`] is still `true` for it, it just has no *single*
    /// target for [`Self::to`] to name.
    pub const fn to(&self) -> Option<&A> {
        match self {
            Effect::Assigned { to, .. }
            | Effect::Directed { to, .. }
            | Effect::Nudge { to, .. } => Some(to),
            Effect::Said { to, .. } => to.as_ref(),
            _ => None,
        }
    }
}

/// Every way an act can be refused.
///
/// Typed, so no caller has to read prose to find out whether state changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo))]
pub enum Error {
    /// The origin may not do this.
    NotAuthorized,
    NoSuchTask,
    /// The act is not legal from the task's current status.
    WrongStatus,
    /// Addressed a parent as a sub-task, or the reverse.
    WrongKind,
    /// Someone other than the assignee or holder tried to work it.
    NotYours,
    /// `root` is in the roster so it can send, but there is no agent loop
    /// behind it. Found the only way it could be: a leader canvassing the
    /// litter split its task four ways and gave the fourth to the operator.
    /// That sub-task could never be claimed, and since an artifact needs every
    /// sub-task cleared, the parent could never close.
    RootNotAssignable,
    /// A plan must carry every sub-task in one call — otherwise the table can
    /// never know planning finished, so "all sub-tasks cleared" is never
    /// decidable and the artifact never fires.
    EmptyPlan,
    TooManySubtasks,
    /// This parent was already split. Planning is not repeatable.
    AlreadyPlanned,
    /// An artifact needs every sub-task `Cleared` first.
    SubtasksOutstanding,
    /// Past `Limits`.
    TooLong,
    /// A result is already in and the leader has not ruled on it.
    AlreadySubmitted,
    /// Two agents cannot hold one sub-task.
    AlreadyClaimed,
    /// The table is full. Close or GC something first.
    TooManyTasks,
}

/// Deadlines, in **blocks**.
///
/// Every one of these is longer than it looks like it should be, because the
/// unit that matters is an LLM turn and a turn on a 4B model measured
/// 120–200 s. The litter's `CLAIM_WINDOW` was 180 s at first — *shorter than a
/// single turn* — so an offer lapsed and was re-made while its assignee was
/// still thinking about the first copy.
///
/// Defaults below assume **6 s blocks**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timers {
    /// An offer nobody claimed is re-offered. 600 s.
    pub claim_window: BlockNumber,
    /// A claimed sub-task whose worker died is requeued. 900 s.
    pub lease: BlockNumber,
    /// How often a holder is told to get on with it. 150 s.
    pub work_nag: BlockNumber,
    /// How often an outstanding leader directive is repeated. 120 s.
    pub directive_nag: BlockNumber,
    /// How many consecutive unanswered reminders a holder gets.
    pub max_nudges: u8,
    /// How many times an unclaimed offer is re-made before the table gives up
    /// and asks the leader to re-home it.
    ///
    /// Bounded for the same reason nudges are: re-offering forever to an
    /// assignee that will never answer is a loop that pays forever, and the
    /// parent can never close while one sub-task is stuck in it.
    pub max_reoffers: u8,
    /// How many consecutive unanswered directive nags a leader gets, before
    /// the table stops asking.
    ///
    /// Directives were once sent once and dropped a parent permanently on a
    /// missed turn (`directives_are_repeated_not_sent_once`) — nagging fixed
    /// that. But nagging *forever* trades one failure for another: a leader
    /// that is wedged, dead, or simply wrong about the verb every time
    /// (observed live: `WrongStatus`/`WrongKind` refusals, repeating) burns
    /// an LLM turn per nag with nothing to show for it, same as an unbounded
    /// worker nudge would. There is no automatic "reassign the leader" —
    /// see [`Effect::DirectiveBudgetSpent`] — so exhausting this budget means
    /// the parent goes quiet until the operator notices, not that it recovers
    /// on its own. Bounded anyway, because a quiet parent is cheaper than a
    /// litter spending its whole token budget on a leader that will never
    /// get it right.
    pub max_directive_nudges: u8,
}

impl Default for Timers {
    fn default() -> Self {
        Timers {
            claim_window: 100,
            lease: 150,
            work_nag: 25,
            directive_nag: 20,
            max_nudges: 3,
            max_reoffers: 3,
            max_directive_nudges: 3,
        }
    }
}

/// Size bounds. Enforced by the state machine itself, so the pallet's
/// `BoundedVec` limits and the machine's limits cannot disagree.
///
/// `max_result` is a forcing function, not just a safety rail: an agent that
/// cannot publish a 40 KB build log has to say what happened instead of
/// pasting what scrolled by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_text: usize,
    pub max_result: usize,
    pub max_artifact: usize,
    pub max_title: usize,
    pub max_subtasks: usize,
    /// Cap on live task rows — parents plus sub-tasks.
    ///
    /// Without this the table is *unbounded*: every other limit caps the size
    /// of one row, and none of them stops rows accumulating forever. A state
    /// machine destined for runtime storage has to bound its own growth, and
    /// [`Error::TooManyTasks`] plus GC of closed parents is how.
    pub max_tasks: usize,
    /// Cap on one message. Same forcing function as `max_result`: say it, do
    /// not paste it.
    pub max_message: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_text: 4 * 1024,
            max_result: 16 * 1024,
            max_artifact: 64 * 1024,
            max_title: 128,
            max_subtasks: 8,
            max_tasks: 512,
            max_message: 2048,
        }
    }
}

/// Everything the table needs that is not state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Config {
    pub timers: Timers,
    pub limits: Limits,
}

/// A committed artifact: the markdown report a closed parent produced.
///
/// Stored as bytes with a separately typed `title` so a listing never has to
/// parse markdown. The runtime validates length and UTF-8 and **nothing
/// else** — markdown structure is a convention the agent renders, never a
/// consensus rule a model could violate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo))]
#[cfg_attr(feature = "codec", scale_info(skip_type_params(A)))]
pub struct Artifact<A> {
    pub title: String,
    pub body: String,
    pub author: A,
    pub at: BlockNumber,
}

/// Pull a title out of rendered markdown: the first `# ` heading, trimmed.
///
/// Deterministic and total — a body with no heading gets a stable fallback
/// rather than a refusal, because losing an artifact over a missing `#` is the
/// same mistake as refusing a submit that skipped its claim.
pub fn title_from_markdown(body: &str, max: usize) -> String {
    let found = body
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let mut out: String = match found {
        Some(t) => t.into(),
        None => "untitled".into(),
    };
    if out.len() > max {
        // Truncate on a char boundary; `max` is bytes.
        let cut = (0..=max)
            .rev()
            .find(|&i| out.is_char_boundary(i))
            .unwrap_or(0);
        out.truncate(cut);
    }
    out
}

/// Split effects into the ones that wake their recipient and the ones that do
/// not. A convenience for consumers, so the waking rule is applied in one
/// place rather than re-derived.
pub fn partition_waking<A>(effects: Vec<Effect<A>>) -> (Vec<Effect<A>>, Vec<Effect<A>>) {
    let mut waking = Vec::new();
    let mut quiet = Vec::new();
    for e in effects {
        if e.wakes() {
            waking.push(e);
        } else {
            quiet.push(e);
        }
    }
    (waking, quiet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_and_sub_ids_render_and_relate() {
        let p = TaskId::parent(42);
        let s = TaskId::sub(42, 1);
        assert!(p.is_parent() && !s.is_parent());
        assert_eq!(s.parent_id(), p);
        assert_eq!(alloc::format!("{p}"), "t42");
        assert_eq!(alloc::format!("{s}"), "t42.1");
    }

    #[test]
    fn only_targeted_effects_wake() {
        let a = Effect::Assigned {
            to: "tama",
            task: TaskId::sub(1, 1),
            what: "build it".into(),
            expect: String::new(),
        };
        let r = Effect::Record {
            who: "tama",
            task: TaskId::sub(1, 1),
            act: Act::Done,
            text: "done".into(),
        };
        assert!(a.wakes(), "an assignment must wake its assignee");
        assert!(!r.wakes(), "a broadcast record must not wake anyone");
        assert_eq!(a.to(), Some(&"tama"));
        assert_eq!(r.to(), None);
    }

    #[test]
    fn title_comes_from_the_first_heading() {
        let md = "# Does this codebase work?\n\nbody\n\n# later heading\n";
        assert_eq!(title_from_markdown(md, 128), "Does this codebase work?");
    }

    #[test]
    fn title_falls_back_rather_than_failing() {
        assert_eq!(title_from_markdown("no heading here", 128), "untitled");
        assert_eq!(title_from_markdown("#not a heading", 128), "untitled");
    }

    #[test]
    fn title_truncation_respects_char_boundaries() {
        // 'é' is two bytes: a byte-wise truncate at 9 would split it.
        let t = title_from_markdown("# aaaaaaaaéb", 9);
        assert!(t.len() <= 9);
        assert_eq!(t, "aaaaaaaa");
    }
}
