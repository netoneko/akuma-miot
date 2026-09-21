//! One test per finding in `docs/MAPPING_REPORT.md` §1.1.
//!
//! Named after the finding rather than the method, because the findings are
//! what survived the rewrite — the code is new, the behaviour is not. A test
//! that fails here means a thing the litter learned the expensive way has been
//! quietly un-learned.

use super::*;
use miot_primitives::{Limits, Timers};

type A = &'static str;

const ROOT: A = "root";
const LEAD: A = "mimi";
const TAMA: A = "tama";
const KURO: A = "kuro";

/// Small, readable timers. The *defaults* are sized to an LLM turn
/// (see `timers_are_sized_to_an_llm_turn`); these are sized to a test.
fn cfg() -> Config {
    Config {
        timers: Timers {
            claim_window: 10,
            lease: 20,
            work_nag: 5,
            directive_nag: 8,
            max_nudges: 3,
        },
        limits: Limits::default(),
    }
}

fn table() -> TaskTable<A> {
    TaskTable::new(cfg(), Some(ROOT))
}

/// A table with a leader installed and one parent opened by the operator.
fn opened() -> (TaskTable<A>, TaskId) {
    let mut t = table();
    t.set_leader(LEAD, 0);
    let (id, _) = t
        .open(&ROOT, Authority::Root, "debate if this works", 0)
        .unwrap();
    (t, id)
}

/// …and planned two ways: t.1 → tama, t.2 → kuro.
fn planned() -> (TaskTable<A>, TaskId) {
    let (mut t, p) = opened();
    t.plan(
        &LEAD,
        Authority::Leader,
        p,
        &[
            PlanItem {
                who: TAMA,
                what: "run the build".into(),
                expect: "pass/fail".into(),
            },
            PlanItem {
                who: KURO,
                what: "audit locking".into(),
                expect: String::new(),
            },
        ],
        0,
    )
    .unwrap();
    (t, p)
}

fn assigned_to(fx: &[Effect<A>], who: A) -> Option<TaskId> {
    fx.iter().find_map(|e| match e {
        Effect::Assigned { to, task, .. } if *to == who => Some(*task),
        _ => None,
    })
}

fn directives(fx: &[Effect<A>]) -> Vec<Directive> {
    fx.iter()
        .filter_map(|e| match e {
            Effect::Directed { directive, .. } => Some(*directive),
            _ => None,
        })
        .collect()
}

// ---- authority ------------------------------------------------------------

#[test]
fn opening_a_parent_is_the_one_privileged_act() {
    let mut t = table();
    assert_eq!(
        t.open(&TAMA, Authority::Peer, "do a thing", 0).unwrap_err(),
        Error::NotAuthorized
    );
    assert!(t.open(&ROOT, Authority::Root, "do a thing", 0).is_ok());
    assert!(t.open(&LEAD, Authority::Leader, "another", 0).is_ok());
}

#[test]
fn only_the_leader_may_plan_clear_reopen_or_close() {
    let (mut t, p) = opened();
    let items = [PlanItem {
        who: TAMA,
        what: "x".into(),
        expect: String::new(),
    }];
    assert_eq!(
        t.plan(&TAMA, Authority::Peer, p, &items, 0).unwrap_err(),
        Error::NotAuthorized
    );
    t.plan(&LEAD, Authority::Leader, p, &items, 0).unwrap();
    let s = TaskId::sub(p.parent, 1);
    t.update(&TAMA, Authority::Peer, s, Act::Done, "done", 1)
        .unwrap();
    assert_eq!(
        t.update(&TAMA, Authority::Peer, s, Act::Clear, "", 2)
            .unwrap_err(),
        Error::NotAuthorized
    );
    assert_eq!(
        t.update(&KURO, Authority::Peer, p, Act::Artifact, "# r", 2)
            .unwrap_err(),
        Error::NotAuthorized
    );
}

/// `root` is in the roster so it can send, but there is no agent loop behind
/// it. Found the only way it could be: a leader canvassing the litter gave a
/// fourth sub-task to the operator, that sub-task could never be claimed, and
/// since an artifact needs every sub-task cleared the parent could never close.
#[test]
fn root_is_not_a_worker() {
    let (mut t, p) = opened();
    let err = t
        .plan(
            &LEAD,
            Authority::Leader,
            p,
            &[
                PlanItem {
                    who: TAMA,
                    what: "real work".into(),
                    expect: String::new(),
                },
                PlanItem {
                    who: ROOT,
                    what: "canvass yourself".into(),
                    expect: String::new(),
                },
            ],
            0,
        )
        .unwrap_err();
    assert_eq!(err, Error::RootNotAssignable);
    // Refused means nothing happened: no half-applied plan.
    assert_eq!(t.subtasks(p).count(), 0);
    assert_eq!(t.get(p).unwrap().status, TaskStatus::Open);
}

// ---- planning -------------------------------------------------------------

/// A plan carries every sub-task in one call. Without that the table could
/// never know planning had finished, so "all sub-tasks cleared" — the trigger
/// for the artifact — would never be decidable.
#[test]
fn a_plan_is_atomic_and_never_empty() {
    let (mut t, p) = opened();
    assert_eq!(
        t.plan(&LEAD, Authority::Leader, p, &[], 0).unwrap_err(),
        Error::EmptyPlan
    );
    let (t2, p2) = planned();
    assert_eq!(t2.subtasks(p2).count(), 2);
    assert_eq!(t2.get(p2).unwrap().status, TaskStatus::Planned);
}

#[test]
fn planning_is_not_repeatable() {
    let (mut t, p) = planned();
    let again = [PlanItem {
        who: TAMA,
        what: "again".into(),
        expect: String::new(),
    }];
    assert_eq!(
        t.plan(&LEAD, Authority::Leader, p, &again, 1).unwrap_err(),
        Error::AlreadyPlanned
    );
    assert_eq!(t.subtasks(p).count(), 2);
}

#[test]
fn a_plan_larger_than_the_limit_is_refused_whole() {
    let (mut t, p) = opened();
    let items: Vec<PlanItem<A>> = (0..99)
        .map(|_| PlanItem {
            who: TAMA,
            what: "x".into(),
            expect: String::new(),
        })
        .collect();
    assert_eq!(
        t.plan(&LEAD, Authority::Leader, p, &items, 0).unwrap_err(),
        Error::TooManySubtasks
    );
    assert_eq!(t.subtasks(p).count(), 0);
}

// ---- the waking rule ------------------------------------------------------

/// A record is not an instruction to anyone, and waking four agents per record
/// turns one task into sixteen LLM turns. Targeted traffic wakes; broadcast
/// does not.
#[test]
fn assignments_wake_and_records_do_not() {
    let (mut t, p) = planned();
    let fx = t
        .update(
            &TAMA,
            Authority::Peer,
            TaskId::sub(p.parent, 1),
            Act::Claim,
            "",
            1,
        )
        .unwrap();
    let record = fx
        .iter()
        .find(|e| matches!(e, Effect::Record { .. }))
        .unwrap();
    let nudge = fx
        .iter()
        .find(|e| matches!(e, Effect::Nudge { .. }))
        .unwrap();
    assert!(!record.wakes(), "a broadcast record must never wake anyone");
    assert!(nudge.wakes(), "a targeted nudge must wake its holder");
    assert_eq!(record.to(), None);
    assert_eq!(nudge.to(), Some(&TAMA));
}

// ---- claiming -------------------------------------------------------------

/// Claiming ends a turn, and nothing then addresses the holder again — the
/// replicated record is non-waking by design — so a claimed sub-task would
/// ride its lease out untouched. Observed live 2026-09-20: two agents claimed,
/// both turns ended cleanly, neither ever reported.
#[test]
fn a_claim_nudges_immediately_because_claiming_ends_a_turn() {
    let (mut t, p) = planned();
    let s = TaskId::sub(p.parent, 1);
    let fx = t
        .update(&TAMA, Authority::Peer, s, Act::Claim, "", 1)
        .unwrap();
    assert!(
        fx.iter()
            .any(|e| matches!(e, Effect::Nudge { to, task, .. } if *to == TAMA && *task == s)),
        "the claim itself must hand the holder something to act on"
    );
    assert_eq!(t.get(s).unwrap().status, TaskStatus::InProgress);
    assert_eq!(t.get(s).unwrap().lease_until, Some(1 + cfg().timers.lease));
}

#[test]
fn a_subtask_is_claimable_only_by_the_agent_it_was_directed_to() {
    let (mut t, p) = planned();
    let s = TaskId::sub(p.parent, 1); // tama's
    assert_eq!(
        t.update(&KURO, Authority::Peer, s, Act::Claim, "", 1)
            .unwrap_err(),
        Error::NotYours
    );
    t.update(&TAMA, Authority::Peer, s, Act::Claim, "", 1)
        .unwrap();
    assert_eq!(
        t.update(&TAMA, Authority::Peer, s, Act::Claim, "", 2)
            .unwrap_err(),
        Error::AlreadyClaimed
    );
}

// ---- submitting -----------------------------------------------------------

/// The handshake is in the protocol and workers are told to use it, but a
/// small model that skips straight to the result should not have that work
/// thrown away: losing the ceremony is cheaper than losing the answer.
#[test]
fn a_submit_without_a_claim_is_accepted() {
    let (mut t, p) = planned();
    let s = TaskId::sub(p.parent, 2); // kuro's, never claimed
    let fx = t
        .update(
            &KURO,
            Authority::Peer,
            s,
            Act::Done,
            "one lock is unheld",
            3,
        )
        .unwrap();
    assert!(fx
        .iter()
        .any(|e| matches!(e, Effect::Record { act: Act::Done, .. })));
    assert_eq!(t.get(s).unwrap().status, TaskStatus::AwaitingClearance);
}

/// Law I says the chain requeues on lease expiry knowing nothing about a
/// holder four minutes into a turn. With async tools a turn can outlive a
/// lease, so the answer must still be accepted — the holder loses a race, not
/// its work.
#[test]
fn a_late_submit_after_the_lease_expired_is_still_accepted() {
    let (mut t, p) = planned();
    let s = TaskId::sub(p.parent, 1);
    t.update(&TAMA, Authority::Peer, s, Act::Claim, "", 1)
        .unwrap();
    let fx = t.tick(1 + cfg().timers.lease);
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Requeued { why: Requeue::LeaseExpired, from: Some(h), .. } if *h == TAMA
    )));
    assert_eq!(t.get(s).unwrap().status, TaskStatus::Pending);
    // tama finally finishes thinking.
    t.update(&TAMA, Authority::Peer, s, Act::Done, "build passes", 99)
        .unwrap();
    assert_eq!(t.get(s).unwrap().status, TaskStatus::AwaitingClearance);
}

#[test]
fn a_second_submit_is_refused_rather_than_overwriting() {
    let (mut t, p) = planned();
    let s = TaskId::sub(p.parent, 1);
    t.update(&TAMA, Authority::Peer, s, Act::Done, "first", 1)
        .unwrap();
    assert_eq!(
        t.update(&TAMA, Authority::Peer, s, Act::Done, "second", 2)
            .unwrap_err(),
        Error::AlreadySubmitted
    );
    assert_eq!(
        t.get(s).unwrap().outcome,
        Some(Outcome::Done("first".into()))
    );
}

/// `failed` is a sibling of `done`, not a flavour of it: both land in
/// `AwaitingClearance` and differ only in what is stored, because whether to
/// retry, reassign or accept a failure is the leader's decision, not the
/// table's.
#[test]
fn failed_is_a_sibling_of_done_not_a_flavour_of_it() {
    let (mut t, p) = planned();
    let s = TaskId::sub(p.parent, 1);
    t.update(&TAMA, Authority::Peer, s, Act::Failed, "no toolchain", 1)
        .unwrap();
    let task = t.get(s).unwrap();
    assert_eq!(
        task.status,
        TaskStatus::AwaitingClearance,
        "same state as done"
    );
    assert!(
        task.outcome.as_ref().unwrap().failed(),
        "differing only in the stored result"
    );
}

// ---- the tick: bounded and eternal ---------------------------------------

/// An offer nobody claimed is re-offered — the assignee was busy or gone.
#[test]
fn an_unclaimed_offer_is_re_offered_after_the_claim_window() {
    let (mut t, p) = planned();
    assert!(t.tick(cfg().timers.claim_window - 1).is_empty(), "not yet");
    let fx = t.tick(cfg().timers.claim_window);
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Requeued {
            why: Requeue::Unclaimed,
            ..
        }
    )));
    assert_eq!(assigned_to(&fx, TAMA), Some(TaskId::sub(p.parent, 1)));
    assert_eq!(assigned_to(&fx, KURO), Some(TaskId::sub(p.parent, 2)));
}

/// Every nudge wakes the holder and every wake costs an LLM turn, so an
/// unbounded reminder is a loop that pays forever for an agent that was never
/// going to answer. After the budget the table says so once and stops asking.
#[test]
fn the_nudge_budget_is_bounded_and_spent_exactly_once() {
    let mut t = TaskTable::new(
        Config {
            timers: Timers {
                claim_window: 10,
                lease: 1000,
                work_nag: 5,
                directive_nag: 8,
                max_nudges: 3,
            },
            limits: Limits::default(),
        },
        Some(ROOT),
    );
    t.set_leader(LEAD, 0);
    let (p, _) = t.open(&ROOT, Authority::Root, "x", 0).unwrap();
    t.plan(
        &LEAD,
        Authority::Leader,
        p,
        &[PlanItem {
            who: TAMA,
            what: "work".into(),
            expect: String::new(),
        }],
        0,
    )
    .unwrap();
    let s = TaskId::sub(p.parent, 1);
    t.update(&TAMA, Authority::Peer, s, Act::Claim, "", 0)
        .unwrap();

    let mut remaining_seen = Vec::new();
    for tick in [5u32, 10, 15] {
        let fx = t.tick(tick);
        for e in &fx {
            if let Effect::Nudge {
                remaining, last, ..
            } = e
            {
                remaining_seen.push((*remaining, *last));
            }
        }
    }
    assert_eq!(
        remaining_seen,
        alloc::vec![(2, false), (1, false), (0, true)],
        "three nudges, and the last one says it is the last"
    );

    let fx = t.tick(20);
    assert_eq!(
        fx.iter()
            .filter(|e| matches!(e, Effect::NudgeBudgetSpent { .. }))
            .count(),
        1,
        "the budget is announced once"
    );
    assert!(
        t.tick(25).is_empty() && t.tick(30).is_empty(),
        "and then the table stops asking"
    );
}

/// The counter resets whenever the holder actually acts, and a requeue hands
/// the next holder a fresh budget.
#[test]
fn a_requeue_hands_the_next_holder_a_fresh_budget() {
    let (mut t, p) = planned();
    let s = TaskId::sub(p.parent, 1);
    t.update(&TAMA, Authority::Peer, s, Act::Claim, "", 0)
        .unwrap();
    t.tick(5);
    t.tick(10);
    assert_eq!(t.get(s).unwrap().nudges_used, 2);
    t.tick(cfg().timers.lease); // lease expires, requeued
    assert_eq!(
        t.get(s).unwrap().nudges_used,
        0,
        "a fresh holder gets a fresh budget"
    );
}

/// The chain never waits: a tick with nobody listening still resolves every
/// deadline it owns.
#[test]
fn the_tick_resolves_deadlines_with_no_agent_present() {
    let (mut t, p) = planned();
    let s = TaskId::sub(p.parent, 1);
    t.update(&TAMA, Authority::Peer, s, Act::Claim, "", 0)
        .unwrap();
    // Nobody acts, ever. The table still moves.
    for now in (1..200).step_by(7) {
        t.tick(now);
    }
    assert_eq!(
        t.get(s).unwrap().status,
        TaskStatus::Pending,
        "requeued, unattended"
    );
}

/// Lease expiry is resolved before nudges, so a sub-task whose lease just ran
/// out is requeued rather than nudged at someone who no longer holds it.
#[test]
fn an_expired_lease_requeues_rather_than_nudging() {
    let mut t = TaskTable::new(
        Config {
            timers: Timers {
                claim_window: 10,
                lease: 5,
                work_nag: 5,
                directive_nag: 8,
                max_nudges: 3,
            },
            limits: Limits::default(),
        },
        Some(ROOT),
    );
    t.set_leader(LEAD, 0);
    let (p, _) = t.open(&ROOT, Authority::Root, "x", 0).unwrap();
    t.plan(
        &LEAD,
        Authority::Leader,
        p,
        &[PlanItem {
            who: TAMA,
            what: "work".into(),
            expect: String::new(),
        }],
        0,
    )
    .unwrap();
    t.update(
        &TAMA,
        Authority::Peer,
        TaskId::sub(p.parent, 1),
        Act::Claim,
        "",
        0,
    )
    .unwrap();
    // lease and nag both fall due at 5.
    let fx = t.tick(5);
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Requeued {
            why: Requeue::LeaseExpired,
            ..
        }
    )));
    assert!(
        !fx.iter().any(|e| matches!(e, Effect::Nudge { .. })),
        "nobody holds it any more, so nobody is nudged about it"
    );
}

// ---- directives -----------------------------------------------------------

/// A 0.8B model will not infer `[artifact: t1]` from a design document. Every
/// point where the workflow needs a leader decision, the table names the exact
/// verb and delivers it.
#[test]
fn directives_name_the_exact_verb_the_leader_must_type() {
    let (mut t, p) = opened();
    assert_eq!(directives(&t.tick(0)), alloc::vec![Directive::PlanNeeded]);

    t.plan(
        &LEAD,
        Authority::Leader,
        p,
        &[PlanItem {
            who: TAMA,
            what: "work".into(),
            expect: String::new(),
        }],
        0,
    )
    .unwrap();
    let s = TaskId::sub(p.parent, 1);
    assert!(
        directives(&t.tick(1)).is_empty(),
        "nothing for the leader while work is out"
    );

    t.update(&TAMA, Authority::Peer, s, Act::Done, "result", 2)
        .unwrap();
    assert_eq!(
        directives(&t.tick(2)),
        alloc::vec![Directive::ClearanceNeeded]
    );

    t.update(&LEAD, Authority::Leader, s, Act::Clear, "", 3)
        .unwrap();
    assert_eq!(
        directives(&t.tick(3)),
        alloc::vec![Directive::ArtifactNeeded]
    );

    t.update(
        &LEAD,
        Authority::Leader,
        p,
        Act::Artifact,
        "# report\n\nall good",
        4,
    )
    .unwrap();
    assert!(
        directives(&t.tick(4)).is_empty(),
        "a closed parent asks for nothing"
    );
}

/// Re-sent on a nag interval rather than once, because a dropped directive
/// would otherwise stall a parent permanently.
#[test]
fn directives_are_repeated_not_sent_once() {
    let (mut t, _p) = opened();
    assert_eq!(directives(&t.tick(0)).len(), 1);
    assert!(directives(&t.tick(1)).is_empty(), "not every tick");
    assert_eq!(
        directives(&t.tick(cfg().timers.directive_nag)).len(),
        1,
        "but again on the interval"
    );
}

/// Taking the role queues a directive, so promotion is an instruction to act
/// rather than a fact to notice.
#[test]
fn a_new_leader_is_woken_specifically() {
    let mut t = table();
    let fx = t.set_leader(LEAD, 0);
    assert_eq!(directives(&fx), alloc::vec![Directive::LeaderElected]);
    assert_eq!(fx[0].to(), Some(&LEAD));
    assert!(
        t.set_leader(LEAD, 1).is_empty(),
        "re-installing the same leader is a no-op"
    );
}

#[test]
fn a_leaderless_table_emits_no_directives_but_still_ticks() {
    let mut t = table();
    let (_p, _) = t.open(&ROOT, Authority::Root, "x", 0).unwrap();
    assert!(directives(&t.tick(0)).is_empty(), "nobody to tell");
}

// ---- clearance and reopening ---------------------------------------------

#[test]
fn reopening_returns_work_to_its_assignee_with_a_fresh_offer() {
    let (mut t, p) = planned();
    let s = TaskId::sub(p.parent, 1);
    t.update(&TAMA, Authority::Peer, s, Act::Done, "thin", 1)
        .unwrap();
    let fx = t
        .update(&LEAD, Authority::Leader, s, Act::Reopen, "not enough", 2)
        .unwrap();
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Requeued {
            why: Requeue::Reopened,
            ..
        }
    )));
    assert_eq!(assigned_to(&fx, TAMA), Some(s));
    let task = t.get(s).unwrap();
    assert_eq!(task.status, TaskStatus::Pending);
    assert_eq!(task.outcome, None, "the rejected result does not linger");
}

// ---- artifacts ------------------------------------------------------------

/// An artifact requires EVERY sub-task cleared. This is the invariant that
/// makes "root is not a worker" matter.
#[test]
fn an_artifact_requires_every_subtask_cleared() {
    let (mut t, p) = planned();
    let (s1, s2) = (TaskId::sub(p.parent, 1), TaskId::sub(p.parent, 2));
    assert_eq!(
        t.update(&LEAD, Authority::Leader, p, Act::Artifact, "# early", 1)
            .unwrap_err(),
        Error::SubtasksOutstanding
    );
    t.update(&TAMA, Authority::Peer, s1, Act::Done, "a", 1)
        .unwrap();
    t.update(&LEAD, Authority::Leader, s1, Act::Clear, "", 2)
        .unwrap();
    assert_eq!(
        t.update(
            &LEAD,
            Authority::Leader,
            p,
            Act::Artifact,
            "# still early",
            3
        )
        .unwrap_err(),
        Error::SubtasksOutstanding
    );
    t.update(&KURO, Authority::Peer, s2, Act::Done, "b", 3)
        .unwrap();
    t.update(&LEAD, Authority::Leader, s2, Act::Clear, "", 4)
        .unwrap();
    assert!(t
        .update(&LEAD, Authority::Leader, p, Act::Artifact, "# now", 5)
        .is_ok());
}

#[test]
fn an_artifact_closes_the_parent_and_stores_readable_markdown() {
    let (mut t, p) = planned();
    for (s, who) in [
        (TaskId::sub(p.parent, 1), TAMA),
        (TaskId::sub(p.parent, 2), KURO),
    ] {
        t.update(&who, Authority::Peer, s, Act::Done, "r", 1)
            .unwrap();
        t.update(&LEAD, Authority::Leader, s, Act::Clear, "", 2)
            .unwrap();
    }
    let body = "# Does this codebase work?\n\n## Answer\n\nNo, there is a race.\n";
    let fx = t
        .update(&LEAD, Authority::Leader, p, Act::Artifact, body, 9)
        .unwrap();
    assert!(fx.iter().any(|e| matches!(e, Effect::Closed { .. })));
    assert_eq!(t.get(p).unwrap().status, TaskStatus::Closed);

    let a = t.artifact(p).unwrap();
    assert_eq!(
        a.title, "Does this codebase work?",
        "title is derived, never asked of the model"
    );
    assert_eq!(
        a.body, body,
        "stored verbatim — markdown is never parsed as a rule"
    );
    assert_eq!(a.author, LEAD);
    assert_eq!(a.at, 9);
}

#[test]
fn an_artifact_without_a_heading_still_lands() {
    let (mut t, p) = opened();
    t.plan(
        &LEAD,
        Authority::Leader,
        p,
        &[PlanItem {
            who: TAMA,
            what: "w".into(),
            expect: String::new(),
        }],
        0,
    )
    .unwrap();
    let s = TaskId::sub(p.parent, 1);
    t.update(&TAMA, Authority::Peer, s, Act::Done, "r", 1)
        .unwrap();
    t.update(&LEAD, Authority::Leader, s, Act::Clear, "", 2)
        .unwrap();
    t.update(
        &LEAD,
        Authority::Leader,
        p,
        Act::Artifact,
        "no heading at all",
        3,
    )
    .unwrap();
    assert_eq!(t.artifact(p).unwrap().title, "untitled");
}

// ---- limits and refusal ---------------------------------------------------

/// The machine enforces its own bounds, so the pallet's `BoundedVec` limits
/// and the machine's limits cannot disagree.
#[test]
fn limits_are_enforced_by_the_machine_itself() {
    let (mut t, p) = planned();
    let s = TaskId::sub(p.parent, 1);
    let huge = "x".repeat(Limits::default().max_result + 1);
    assert_eq!(
        t.update(&TAMA, Authority::Peer, s, Act::Done, &huge, 1)
            .unwrap_err(),
        Error::TooLong
    );
    assert_eq!(
        t.get(s).unwrap().status,
        TaskStatus::Pending,
        "refused means unchanged"
    );
}

/// Applied-versus-refused is typed. Three places in the litter re-derived
/// acceptance by testing whether a note began with the word "refused"; the
/// "already claimed" refusals said no such thing, so a no-op would have been
/// replicated as though it had happened.
#[test]
fn a_refusal_changes_nothing_and_produces_no_effects() {
    let (mut t, p) = planned();
    let s = TaskId::sub(p.parent, 1);
    let before = t.get(s).unwrap().clone();
    let err = t
        .update(&KURO, Authority::Peer, s, Act::Claim, "", 1)
        .unwrap_err();
    assert_eq!(err, Error::NotYours);
    assert_eq!(
        t.get(s).unwrap(),
        &before,
        "a refused act leaves no trace to replicate"
    );
}

#[test]
fn a_parent_and_a_subtask_cannot_be_confused_for_one_another() {
    let (mut t, p) = planned();
    assert_eq!(
        t.update(&TAMA, Authority::Peer, p, Act::Claim, "", 1)
            .unwrap_err(),
        Error::WrongKind
    );
    assert_eq!(
        t.update(
            &LEAD,
            Authority::Leader,
            TaskId::sub(p.parent, 1),
            Act::Artifact,
            "# x",
            1
        )
        .unwrap_err(),
        Error::WrongKind
    );
}

// ---- the shape of the defaults -------------------------------------------

/// The unit that matters is an LLM turn, not a second. The litter's
/// `CLAIM_WINDOW` was 180 s at first — *shorter than a single turn* — so an
/// offer lapsed and was re-made while its assignee was still thinking about
/// the first copy.
#[test]
fn timers_are_sized_to_an_llm_turn() {
    let d = Timers::default();
    // At 6 s blocks, a 4B-model turn measured 120–200 s ≈ 20–34 blocks.
    const TURN: BlockNumber = 34;
    assert!(
        d.claim_window > TURN,
        "an offer must outlive the turn thinking about it"
    );
    assert!(
        d.lease > d.claim_window,
        "a claim must outlive its own offer window"
    );
    assert!(
        d.work_nag < d.lease,
        "a holder is asked before it is replaced"
    );
    assert!(
        d.max_nudges > 0 && d.max_nudges < 10,
        "bounded, and not a loop"
    );
}

// ---- a whole parent, end to end ------------------------------------------

#[test]
fn a_parent_task_runs_from_open_to_artifact() {
    let mut t = table();
    t.set_leader(LEAD, 0);
    let (p, fx) = t
        .open(&ROOT, Authority::Root, "debate & report", 0)
        .unwrap();
    assert!(matches!(fx[0], Effect::Opened { .. }));

    assert_eq!(directives(&t.tick(0)), alloc::vec![Directive::PlanNeeded]);
    let fx = t
        .plan(
            &LEAD,
            Authority::Leader,
            p,
            &[
                PlanItem {
                    who: TAMA,
                    what: "build+tests".into(),
                    expect: "stability".into(),
                },
                PlanItem {
                    who: KURO,
                    what: "audit locking".into(),
                    expect: "bugs".into(),
                },
            ],
            1,
        )
        .unwrap();
    let (s1, s2) = (
        assigned_to(&fx, TAMA).unwrap(),
        assigned_to(&fx, KURO).unwrap(),
    );

    t.update(&TAMA, Authority::Peer, s1, Act::Claim, "", 2)
        .unwrap();
    t.update(&KURO, Authority::Peer, s2, Act::Claim, "", 2)
        .unwrap();
    t.update(&TAMA, Authority::Peer, s1, Act::Done, "214 tests green", 6)
        .unwrap();
    t.update(
        &KURO,
        Authority::Peer,
        s2,
        Act::Done,
        "lock unheld on the error path",
        7,
    )
    .unwrap();

    assert_eq!(
        directives(&t.tick(7)),
        alloc::vec![Directive::ClearanceNeeded]
    );
    t.update(&LEAD, Authority::Leader, s1, Act::Clear, "", 8)
        .unwrap();
    t.update(&LEAD, Authority::Leader, s2, Act::Clear, "", 8)
        .unwrap();
    assert_eq!(
        directives(&t.tick(8)),
        alloc::vec![Directive::ArtifactNeeded]
    );

    let report = "# Does this codebase work?\n\n## Answer\n\nCompiles; races under load.\n";
    t.update(&LEAD, Authority::Leader, p, Act::Artifact, report, 9)
        .unwrap();

    assert_eq!(t.get(p).unwrap().status, TaskStatus::Closed);
    assert_eq!(t.artifact(p).unwrap().title, "Does this codebase work?");
    assert!(t.tick(100).is_empty(), "a closed litter goes quiet");
}
