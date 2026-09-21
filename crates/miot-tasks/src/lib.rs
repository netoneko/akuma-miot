//! The Akuma Miot task lifecycle: **parent → directed sub-task → claim →
//! submit → clear → artifact**, as a pure state machine.
//!
//! `&mut self`, `now` always a parameter, no clock, no socket, no interior
//! mutability, no I/O. The same record applied to the same state gives the
//! same answer anywhere, which is the only property that makes a log worth
//! replicating — and it is what lets `pallet-litter` be a thin wrapper
//! (`ensure_signed`, read, apply, write, emit) and `miot-coord` be the same
//! machine in one tokio task with no chain at all.
//!
//! The behaviour encoded here was found by running the litter and watching it
//! fail. Each finding has a test below named after it; `docs/MAPPING_REPORT.md`
//! §1.1 is the index.
//!
//! # Two laws it exists to serve
//!
//! **The chain never waits.** [`TaskTable::tick`] runs on block cadence and
//! resolves every deadline without consulting anyone. An agent that goes
//! silent costs nothing: its lease expires and the work requeues.
//!
//! **The agent never blocks.** Nothing here returns anything an agent must
//! wait on — [`Effect`]s are handed back for the caller to fan out, and
//! whether one wakes its recipient is [`Effect::wakes`], decided once here
//! rather than re-derived by every consumer.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub use miot_primitives as primitives;
use miot_primitives::{
    title_from_markdown, Act, Artifact, Authority, BlockNumber, Config, Directive, Effect, Error,
    Outcome, PlanItem, Requeue, TaskId, TaskStatus,
};

/// One task: a parent, or one sub-task of one.
///
/// Both kinds share a struct because they share most of their fields and the
/// pallet reads the whole table as a blob either way. `id.is_parent()` is the
/// discriminator, and the accessors below refuse the wrong kind rather than
/// quietly doing nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, scale_info::TypeInfo))]
#[cfg_attr(feature = "codec", scale_info(skip_type_params(A)))]
pub struct Task<A> {
    pub id: TaskId,
    pub status: TaskStatus,
    pub text: String,
    /// What the leader said "done" looks like. Sub-tasks only; may be empty.
    pub expect: String,
    /// Sub-tasks: the named account this was directed to. It survives a
    /// requeue — the work is still *theirs*, it is only unclaimed again.
    pub assignee: Option<A>,
    /// Sub-tasks: who currently holds the claim.
    pub holder: Option<A>,
    pub outcome: Option<Outcome>,
    pub opened_by: A,
    pub opened_at: BlockNumber,
    /// Sub-tasks, `Pending`: when the current offer was made.
    pub offered_at: Option<BlockNumber>,
    /// Sub-tasks, `InProgress`: when the claim lapses.
    pub lease_until: Option<BlockNumber>,
    /// Sub-tasks, `InProgress`: when the holder is next nudged.
    pub next_nag: Option<BlockNumber>,
    /// Consecutive unanswered nudges. Resets whenever the holder acts.
    pub nudges_used: u8,
    /// Whether "budget spent" has already been said for this holder. Said
    /// once, not once per tick.
    budget_announced: bool,
    /// Parents: when the outstanding directive is next repeated.
    next_directive: BlockNumber,
    /// Parents: when the artifact landed. What [`TaskTable::gc`] ages against.
    pub closed_at: Option<BlockNumber>,
}

impl<A> Task<A> {
    pub fn is_parent(&self) -> bool {
        self.id.is_parent()
    }

    /// Whether this sub-task has reached a state the leader has accepted.
    pub fn is_cleared(&self) -> bool {
        self.status == TaskStatus::Cleared
    }
}

/// The part of the table that belongs in storage.
///
/// [`Config`] is deliberately **not** in here. It comes from the pallet's own
/// `Config` associated constants, so a chain can retune a timer with a runtime
/// upgrade instead of a migration, and every block does not pay to encode and
/// decode a struct of constants that never change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "codec", derive(codec::Encode, codec::Decode, scale_info::TypeInfo))]
#[cfg_attr(feature = "codec", scale_info(skip_type_params(A)))]
pub struct State<A> {
    pub tasks: Vec<Task<A>>,
    pub artifacts: Vec<(TaskId, Artifact<A>)>,
    /// Parent ids are never reused, even after GC drops the rows.
    pub next_parent: u32,
    pub leader: Option<A>,
    pub root: Option<A>,
}

impl<A> Default for State<A> {
    fn default() -> Self {
        State {
            tasks: Vec::new(),
            artifacts: Vec::new(),
            // Ids start at 1 so `TaskId::default()` — all zeroes — is never a
            // real task. `set_leader` uses it as the "no task" address.
            next_parent: 1,
            leader: None,
            root: None,
        }
    }
}

/// The table. Owns every task and every committed artifact.
#[derive(Debug, Clone)]
pub struct TaskTable<A> {
    tasks: Vec<Task<A>>,
    artifacts: Vec<(TaskId, Artifact<A>)>,
    next_parent: u32,
    leader: Option<A>,
    /// The operator account. In the roster so it can send, but there is no
    /// agent loop behind it, so it is never assignable.
    root: Option<A>,
    cfg: Config,
}

impl<A: Clone + Eq> TaskTable<A> {
    pub fn new(cfg: Config, root: Option<A>) -> Self {
        TaskTable {
            tasks: Vec::new(),
            artifacts: Vec::new(),
            next_parent: 1,
            leader: None,
            root,
            cfg,
        }
    }

    /// Rebuild a table from stored state plus the caller's config.
    ///
    /// This is the whole of `pallet-litter`'s read half: load, apply, store.
    /// Nothing is validated on the way in — the state was written by this same
    /// machine, and re-checking it every block would be paying for a
    /// corruption that can only come from a bad migration.
    pub fn from_state(state: State<A>, cfg: Config) -> Self {
        TaskTable {
            tasks: state.tasks,
            artifacts: state.artifacts,
            next_parent: state.next_parent,
            leader: state.leader,
            root: state.root,
            cfg,
        }
    }

    /// Hand the storable part back.
    pub fn into_state(self) -> State<A> {
        State {
            tasks: self.tasks,
            artifacts: self.artifacts,
            next_parent: self.next_parent,
            leader: self.leader,
            root: self.root,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn leader(&self) -> Option<&A> {
        self.leader.as_ref()
    }

    /// Install a new leader.
    ///
    /// Taking the role queues a [`Directive::LeaderElected`] for the new
    /// holder, because promotion is an instruction to act and not merely a
    /// fact to notice — the litter found that a leader which is never told it
    /// is the leader simply never plans anything. Outstanding directives are
    /// re-armed to fire on the next tick, so a leadership change does not
    /// leave a parent waiting out a full nag interval.
    pub fn set_leader(&mut self, who: A, now: BlockNumber) -> Vec<Effect<A>> {
        if self.leader.as_ref() == Some(&who) {
            return Vec::new();
        }
        self.leader = Some(who.clone());
        for t in self.tasks.iter_mut().filter(|t| t.is_parent()) {
            t.next_directive = now;
        }
        alloc::vec![Effect::Directed {
            to: who,
            task: TaskId::default(),
            directive: Directive::LeaderElected,
        }]
    }

    pub fn get(&self, id: TaskId) -> Option<&Task<A>> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Every sub-task of `parent`, in creation order.
    pub fn subtasks(&self, parent: TaskId) -> impl Iterator<Item = &Task<A>> {
        let p = parent.parent_id().parent;
        self.tasks
            .iter()
            .filter(move |t| t.id.parent == p && !t.is_parent())
    }

    pub fn artifact(&self, parent: TaskId) -> Option<&Artifact<A>> {
        self.artifacts
            .iter()
            .find(|(id, _)| *id == parent.parent_id())
            .map(|(_, a)| a)
    }

    pub fn tasks(&self) -> &[Task<A>] {
        &self.tasks
    }

    // ---- acts -------------------------------------------------------------

    /// Open a parent task. Operator or leader only.
    pub fn open(
        &mut self,
        who: &A,
        auth: Authority,
        text: &str,
        now: BlockNumber,
    ) -> Result<(TaskId, Vec<Effect<A>>), Error> {
        if !matches!(auth, Authority::Root | Authority::Leader) {
            return Err(Error::NotAuthorized);
        }
        if text.len() > self.cfg.limits.max_text {
            return Err(Error::TooLong);
        }
        // One parent plus its eventual sub-tasks has to fit.
        if self.tasks.len() + 1 + self.cfg.limits.max_subtasks > self.cfg.limits.max_tasks {
            return Err(Error::TooManyTasks);
        }
        let id = TaskId::parent(self.next_parent);
        self.next_parent += 1;
        self.tasks.push(Task {
            id,
            status: TaskStatus::Open,
            text: text.to_string(),
            expect: String::new(),
            assignee: None,
            holder: None,
            outcome: None,
            opened_by: who.clone(),
            opened_at: now,
            offered_at: None,
            lease_until: None,
            next_nag: None,
            nudges_used: 0,
            budget_announced: false,
            // Due immediately: the leader should be asked to plan on the very
            // next tick, not one nag interval from now.
            next_directive: now,
            closed_at: None,
        });
        Ok((
            id,
            alloc::vec![Effect::Opened {
                who: who.clone(),
                task: id
            }],
        ))
    }

    /// Split a parent into directed sub-tasks. Leader only, one call.
    ///
    /// **Every** sub-task arrives together, matching the workflow's single
    /// `CreateSubTasks` step. Without that the table could never know planning
    /// had finished, so "all sub-tasks cleared" — the trigger for the final
    /// artifact — would never be decidable.
    pub fn plan(
        &mut self,
        who: &A,
        auth: Authority,
        parent: TaskId,
        items: &[PlanItem<A>],
        now: BlockNumber,
    ) -> Result<Vec<Effect<A>>, Error> {
        if auth != Authority::Leader {
            return Err(Error::NotAuthorized);
        }
        if !parent.is_parent() {
            return Err(Error::WrongKind);
        }
        if items.is_empty() {
            return Err(Error::EmptyPlan);
        }
        if items.len() > self.cfg.limits.max_subtasks {
            return Err(Error::TooManySubtasks);
        }
        for it in items {
            if self.root.as_ref() == Some(&it.who) {
                return Err(Error::RootNotAssignable);
            }
            if it.what.len() > self.cfg.limits.max_text
                || it.expect.len() > self.cfg.limits.max_text
            {
                return Err(Error::TooLong);
            }
        }
        match self.tasks.iter().find(|t| t.id == parent) {
            None => return Err(Error::NoSuchTask),
            Some(t) if t.status == TaskStatus::Open => {}
            Some(t) if t.status == TaskStatus::Planned => return Err(Error::AlreadyPlanned),
            Some(_) => return Err(Error::WrongStatus),
        }

        let mut effects = alloc::vec![Effect::Planned {
            who: who.clone(),
            task: parent,
            count: items.len() as u32,
        }];
        for (i, it) in items.iter().enumerate() {
            let id = TaskId::sub(parent.parent, (i + 1) as u16);
            self.tasks.push(Task {
                id,
                status: TaskStatus::Pending,
                text: it.what.clone(),
                expect: it.expect.clone(),
                assignee: Some(it.who.clone()),
                holder: None,
                outcome: None,
                opened_by: who.clone(),
                opened_at: now,
                offered_at: Some(now),
                lease_until: None,
                next_nag: None,
                nudges_used: 0,
                budget_announced: false,
                next_directive: now,
                closed_at: None,
            });
            effects.push(Effect::Assigned {
                to: it.who.clone(),
                task: id,
                what: it.what.clone(),
                expect: it.expect.clone(),
            });
        }
        let nag = self.cfg.timers.directive_nag;
        let p = self.task_mut(parent)?;
        p.status = TaskStatus::Planned;
        p.next_directive = now.saturating_add(nag);
        Ok(effects)
    }

    /// Every per-task act: claim, done, failed, clear, reopen, artifact.
    ///
    /// One entry point with an enum rather than six near-identical ones,
    /// mirroring the tool surface a small model actually copes with.
    pub fn update(
        &mut self,
        who: &A,
        auth: Authority,
        id: TaskId,
        act: Act,
        text: &str,
        now: BlockNumber,
    ) -> Result<Vec<Effect<A>>, Error> {
        match act {
            Act::Claim => self.claim(who, id, now),
            Act::Done => self.submit(who, id, Outcome::Done(text.to_string()), now),
            Act::Failed => self.submit(who, id, Outcome::Failed(text.to_string()), now),
            Act::Clear => self.clear(who, auth, id, now),
            Act::Reopen => self.reopen(who, auth, id, text, now),
            Act::Artifact => self.commit_artifact(who, auth, id, text, now),
        }
    }

    fn claim(&mut self, who: &A, id: TaskId, now: BlockNumber) -> Result<Vec<Effect<A>>, Error> {
        let (lease, work_nag) = (self.cfg.timers.lease, self.cfg.timers.work_nag);
        let t = self.task_mut(id)?;
        if t.is_parent() {
            return Err(Error::WrongKind);
        }
        match t.status {
            TaskStatus::Pending => {}
            TaskStatus::InProgress => return Err(Error::AlreadyClaimed),
            _ => return Err(Error::WrongStatus),
        }
        if t.assignee.as_ref() != Some(who) {
            return Err(Error::NotYours);
        }
        t.status = TaskStatus::InProgress;
        t.holder = Some(who.clone());
        t.lease_until = Some(now.saturating_add(lease));
        t.next_nag = Some(now.saturating_add(work_nag));
        t.nudges_used = 0;
        t.budget_announced = false;
        t.offered_at = None;
        // Nudged once immediately, then on the nag interval. Claiming ends a
        // turn, and nothing in the protocol addresses the holder again — the
        // replicated record is non-waking by design — so without this the
        // sub-task rides its lease out untouched. Observed live: two agents
        // claimed, both turns ended cleanly, neither ever reported.
        Ok(alloc::vec![
            Effect::Record {
                who: who.clone(),
                task: id,
                act: Act::Claim
            },
            Effect::Nudge {
                to: who.clone(),
                task: id,
                remaining: self.cfg.timers.max_nudges,
                last: false,
            },
        ])
    }

    fn submit(
        &mut self,
        who: &A,
        id: TaskId,
        outcome: Outcome,
        now: BlockNumber,
    ) -> Result<Vec<Effect<A>>, Error> {
        if outcome.text().len() > self.cfg.limits.max_result {
            return Err(Error::TooLong);
        }
        let act = if outcome.failed() {
            Act::Failed
        } else {
            Act::Done
        };
        let t = self.task_mut(id)?;
        if t.is_parent() {
            return Err(Error::WrongKind);
        }
        match t.status {
            // The handshake, taken normally.
            TaskStatus::InProgress if t.holder.as_ref() == Some(who) => {}
            TaskStatus::InProgress => return Err(Error::NotYours),
            // A submit without a claim is accepted as claim-then-submit. The
            // handshake is in the protocol and workers are told to use it, but
            // a small model that skips straight to the result should not have
            // that work thrown away: losing the ceremony is cheaper than
            // losing the answer.
            //
            // This is also the **late submit** path. A lease that expired
            // while its holder was still thinking sends the sub-task back to
            // `Pending` with the assignee intact, so the holder's eventual
            // answer lands here and is accepted rather than discarded.
            TaskStatus::Pending if t.assignee.as_ref() == Some(who) => {}
            TaskStatus::Pending => return Err(Error::NotYours),
            TaskStatus::AwaitingClearance => return Err(Error::AlreadySubmitted),
            _ => return Err(Error::WrongStatus),
        }
        t.status = TaskStatus::AwaitingClearance;
        t.outcome = Some(outcome);
        t.holder = Some(who.clone());
        t.lease_until = None;
        t.next_nag = None;
        t.offered_at = None;
        t.nudges_used = 0;
        let parent = id.parent_id();
        // The leader is wanted now, not one interval from now.
        if let Ok(p) = self.task_mut(parent) {
            p.next_directive = now;
        }
        Ok(alloc::vec![Effect::Record {
            who: who.clone(),
            task: id,
            act
        }])
    }

    fn clear(
        &mut self,
        who: &A,
        auth: Authority,
        id: TaskId,
        now: BlockNumber,
    ) -> Result<Vec<Effect<A>>, Error> {
        if auth != Authority::Leader {
            return Err(Error::NotAuthorized);
        }
        let t = self.task_mut(id)?;
        if t.is_parent() {
            return Err(Error::WrongKind);
        }
        if t.status != TaskStatus::AwaitingClearance {
            return Err(Error::WrongStatus);
        }
        t.status = TaskStatus::Cleared;
        let parent = id.parent_id();
        if let Ok(p) = self.task_mut(parent) {
            p.next_directive = now;
        }
        Ok(alloc::vec![Effect::Record {
            who: who.clone(),
            task: id,
            act: Act::Clear
        }])
    }

    fn reopen(
        &mut self,
        who: &A,
        auth: Authority,
        id: TaskId,
        _why: &str,
        now: BlockNumber,
    ) -> Result<Vec<Effect<A>>, Error> {
        if auth != Authority::Leader {
            return Err(Error::NotAuthorized);
        }
        let t = self.task_mut(id)?;
        if t.is_parent() {
            return Err(Error::WrongKind);
        }
        if t.status != TaskStatus::AwaitingClearance {
            return Err(Error::WrongStatus);
        }
        t.status = TaskStatus::Pending;
        t.outcome = None;
        t.holder = None;
        t.offered_at = Some(now);
        t.lease_until = None;
        t.next_nag = None;
        // A requeue hands the next holder a fresh budget.
        t.nudges_used = 0;
        t.budget_announced = false;
        let (assignee, what, expect) = (t.assignee.clone(), t.text.clone(), t.expect.clone());
        let mut effects = alloc::vec![
            Effect::Record {
                who: who.clone(),
                task: id,
                act: Act::Reopen
            },
            Effect::Requeued {
                task: id,
                from: None,
                why: Requeue::Reopened
            },
        ];
        if let Some(to) = assignee {
            effects.push(Effect::Assigned {
                to,
                task: id,
                what,
                expect,
            });
        }
        Ok(effects)
    }

    fn commit_artifact(
        &mut self,
        who: &A,
        auth: Authority,
        id: TaskId,
        body: &str,
        now: BlockNumber,
    ) -> Result<Vec<Effect<A>>, Error> {
        if auth != Authority::Leader {
            return Err(Error::NotAuthorized);
        }
        if !id.is_parent() {
            return Err(Error::WrongKind);
        }
        if body.len() > self.cfg.limits.max_artifact {
            return Err(Error::TooLong);
        }
        match self.tasks.iter().find(|t| t.id == id) {
            None => return Err(Error::NoSuchTask),
            Some(t) if t.status == TaskStatus::Planned => {}
            Some(_) => return Err(Error::WrongStatus),
        }
        // An artifact requires EVERY sub-task cleared. This is the invariant
        // that made "root is not a worker" matter: a sub-task nobody can claim
        // is a parent that can never close.
        if self.subtasks(id).any(|t| !t.is_cleared()) {
            return Err(Error::SubtasksOutstanding);
        }
        let title = title_from_markdown(body, self.cfg.limits.max_title);
        self.artifacts.push((
            id,
            Artifact {
                title: title.clone(),
                body: body.to_string(),
                author: who.clone(),
                at: now,
            },
        ));
        let p = self.task_mut(id)?;
        p.status = TaskStatus::Closed;
        p.closed_at = Some(now);
        Ok(alloc::vec![
            Effect::Record {
                who: who.clone(),
                task: id,
                act: Act::Artifact
            },
            Effect::Closed { task: id, title },
        ])
    }

    // ---- the tick ---------------------------------------------------------

    /// Advance every deadline to `now`.
    ///
    /// **This never consults an agent and never waits for one.** It is the
    /// whole of Law I: the state machine is bounded and eternal, so an agent
    /// that goes silent costs it nothing.
    ///
    /// Order is load-bearing — lease expiry is resolved before nudges, so a
    /// sub-task whose lease just ran out is requeued rather than nudged at
    /// someone who no longer holds it.
    pub fn tick(&mut self, now: BlockNumber) -> Vec<Effect<A>> {
        let cfg = self.cfg;
        let mut effects = Vec::new();

        for t in self.tasks.iter_mut().filter(|t| !t.is_parent()) {
            match t.status {
                TaskStatus::InProgress => {
                    let expired = t.lease_until.is_some_and(|d| now >= d);
                    if expired {
                        let from = t.holder.take();
                        t.status = TaskStatus::Pending;
                        t.offered_at = Some(now);
                        t.lease_until = None;
                        t.next_nag = None;
                        t.nudges_used = 0;
                        t.budget_announced = false;
                        effects.push(Effect::Requeued {
                            task: t.id,
                            from,
                            why: Requeue::LeaseExpired,
                        });
                        if let Some(to) = t.assignee.clone() {
                            effects.push(Effect::Assigned {
                                to,
                                task: t.id,
                                what: t.text.clone(),
                                expect: t.expect.clone(),
                            });
                        }
                        continue;
                    }
                    if t.next_nag.is_some_and(|d| now >= d) {
                        let holder = match t.holder.clone() {
                            Some(h) => h,
                            None => continue,
                        };
                        if t.nudges_used < cfg.timers.max_nudges {
                            t.nudges_used += 1;
                            t.next_nag = Some(now.saturating_add(cfg.timers.work_nag));
                            let remaining = cfg.timers.max_nudges - t.nudges_used;
                            effects.push(Effect::Nudge {
                                to: holder,
                                task: t.id,
                                remaining,
                                last: remaining == 0,
                            });
                        } else if !t.budget_announced {
                            // Bounded, and that is the whole design of it.
                            // After the budget the table stops asking, says so
                            // once, and lets the lease requeue the work to
                            // somebody else.
                            t.budget_announced = true;
                            t.next_nag = None;
                            effects.push(Effect::NudgeBudgetSpent { holder, task: t.id });
                        }
                    }
                }
                TaskStatus::Pending
                    if t.offered_at
                        .is_some_and(|o| now >= o.saturating_add(cfg.timers.claim_window)) =>
                {
                    t.offered_at = Some(now);
                    effects.push(Effect::Requeued {
                        task: t.id,
                        from: None,
                        why: Requeue::Unclaimed,
                    });
                    if let Some(to) = t.assignee.clone() {
                        effects.push(Effect::Assigned {
                            to,
                            task: t.id,
                            what: t.text.clone(),
                            expect: t.expect.clone(),
                        });
                    }
                }
                _ => {}
            }
        }

        // Leader directives. Re-sent on an interval rather than once, because
        // a dropped directive would otherwise stall a parent permanently.
        let leader = match self.leader.clone() {
            Some(l) => l,
            None => return effects,
        };
        let due: Vec<(TaskId, Directive)> = self
            .tasks
            .iter()
            .filter(|t| t.is_parent() && now >= t.next_directive)
            .filter_map(|t| self.directive_for(t).map(|d| (t.id, d)))
            .collect();
        for (id, directive) in due {
            if let Ok(p) = self.task_mut(id) {
                p.next_directive = now.saturating_add(cfg.timers.directive_nag);
            }
            effects.push(Effect::Directed {
                to: leader.clone(),
                task: id,
                directive,
            });
        }
        effects
    }

    /// What, if anything, this parent needs the leader to type right now.
    fn directive_for(&self, parent: &Task<A>) -> Option<Directive> {
        match parent.status {
            TaskStatus::Open => Some(Directive::PlanNeeded),
            TaskStatus::Planned => {
                let mut subs = self.subtasks(parent.id).peekable();
                subs.peek()?;
                if self
                    .subtasks(parent.id)
                    .any(|t| t.status == TaskStatus::AwaitingClearance)
                {
                    Some(Directive::ClearanceNeeded)
                } else if self.subtasks(parent.id).all(Task::is_cleared) {
                    Some(Directive::ArtifactNeeded)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Drop the rows of parents closed at least `keep_for` blocks ago.
    ///
    /// This is the **only** kind of compaction that is consensus business
    /// (`docs/MAPPING_REPORT.md` §2.7). It is deterministic, it is cheap, and
    /// it is what keeps the table bounded over a long-lived chain.
    ///
    /// **Artifacts are never dropped.** They are the durable output the whole
    /// lifecycle exists to produce; it is the bookkeeping around them that is
    /// disposable. A closed parent's rows are recoverable from history if
    /// anyone ever needs them, and its artifact is right here if they do not.
    ///
    /// Returns how many rows went. Idempotent.
    pub fn gc(&mut self, now: BlockNumber, keep_for: BlockNumber) -> usize {
        let doomed: Vec<u32> = self
            .tasks
            .iter()
            .filter(|t| t.is_parent() && t.status == TaskStatus::Closed)
            .filter(|t| now.saturating_sub(t.closed_at.unwrap_or(now)) >= keep_for)
            .map(|t| t.id.parent)
            .collect();
        if doomed.is_empty() {
            return 0;
        }
        let before = self.tasks.len();
        self.tasks.retain(|t| !doomed.contains(&t.id.parent));
        before - self.tasks.len()
    }

    /// How many rows are live. The thing [`Limits::max_tasks`] caps.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn task_mut(&mut self, id: TaskId) -> Result<&mut Task<A>, Error> {
        self.tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or(Error::NoSuchTask)
    }
}

#[cfg(test)]
mod tests;
