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
pub enum TaskStatus {
    /// Parent: opened, not yet split.
    Open,
    /// Parent: split into sub-tasks. Stays here until every one is `Cleared`.
    Planned,
    /// Parent: artifact submitted, done.
    Closed,
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
pub enum Directive {
    /// A parent is open and unsplit.
    PlanNeeded,
    /// At least one sub-task is awaiting clearance.
    ClearanceNeeded,
    /// Every sub-task is cleared; the parent wants its artifact.
    ArtifactNeeded,
    /// This account just became leader. Promotion is an instruction to act,
    /// not merely a fact to notice.
    LeaderElected,
}

/// Why a sub-task went back into the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requeue {
    /// Nobody claimed the offer inside the claim window.
    Unclaimed,
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
    /// Broadcast, non-waking: a parent task was opened.
    Opened { who: A, task: TaskId },
    /// Broadcast, non-waking: a parent was split, atomically, into `count`
    /// directed sub-tasks.
    Planned { who: A, task: TaskId, count: usize },
    /// Broadcast, non-waking: an accepted act. Refused acts produce no
    /// record — nothing happened, so there is nothing to replicate.
    Record { who: A, task: TaskId, act: Act },
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
    Closed { task: TaskId, title: String },
}

impl<A> Effect<A> {
    /// Whether this effect should assemble an LLM turn for its recipient.
    pub const fn wakes(&self) -> bool {
        matches!(
            self,
            Effect::Assigned { .. } | Effect::Directed { .. } | Effect::Nudge { .. }
        )
    }

    /// Who this is addressed to, if anyone. `None` is a broadcast.
    pub const fn to(&self) -> Option<&A> {
        match self {
            Effect::Assigned { to, .. }
            | Effect::Directed { to, .. }
            | Effect::Nudge { to, .. } => Some(to),
            _ => None,
        }
    }
}

/// Every way an act can be refused.
///
/// Typed, so no caller has to read prose to find out whether state changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

impl Default for Timers {
    fn default() -> Self {
        Timers {
            claim_window: 100,
            lease: 150,
            work_nag: 25,
            directive_nag: 20,
            max_nudges: 3,
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
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_text: 4 * 1024,
            max_result: 16 * 1024,
            max_artifact: 64 * 1024,
            max_title: 128,
            max_subtasks: 8,
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
