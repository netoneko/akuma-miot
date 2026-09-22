//! Tests for the **wrapping**, not the lifecycle.
//!
//! The lifecycle has 31 host-native tests in `miot-tasks` that need neither a
//! runtime nor a block. What can only go wrong here is the wrapper: authority
//! recovered from the origin, state round-tripping through storage, effects
//! reaching the event log, refusals writing nothing, and the tick running on
//! block cadence without anyone asking it to.

use polkadot_sdk::*;

use crate::mock::*;
use crate::Error;
use frame_support::{assert_noop, assert_ok};
use miot_primitives::{Act, Directive, Effect, PlanItem, TaskId, TaskStatus};

fn plan_two(parent: TaskId) {
    assert_ok!(Litter::plan(
        RuntimeOrigin::signed(LEAD),
        parent,
        vec![
            PlanItem { who: TAMA, what: "run the build".into(), expect: "pass/fail".into() },
            PlanItem { who: KURO, what: "audit locking".into(), expect: String::new() },
        ],
    ));
}

/// The whole reason for the move to a chain: the sender is not a field.
#[test]
fn authority_is_recovered_from_the_origin_not_claimed() {
    new_test_ext().execute_with(|| {
        // A peer cannot open a parent, however much it would like to.
        assert_noop!(
            Litter::open(RuntimeOrigin::signed(TAMA), "sneak one in".into()),
            Error::<Test>::NotAuthorized
        );
        // The operator can, because the origin says so.
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "debate & report".into()));
        // So can the leader.
        assert_ok!(Litter::open(RuntimeOrigin::signed(LEAD), "another".into()));
        // Unsigned is nobody.
        assert!(Litter::open(RuntimeOrigin::none(), "x".into()).is_err());
    });
}

#[test]
fn genesis_installs_the_operator_and_the_leader() {
    new_test_ext().execute_with(|| {
        let t = Litter::table();
        assert_eq!(t.leader(), Some(&LEAD));
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "x".into()));
    });
}

#[test]
fn only_governance_may_hand_out_the_key_to_the_cat_house() {
    new_test_ext().execute_with(|| {
        assert_noop!(
            Litter::set_root(RuntimeOrigin::signed(ROOT), TAMA),
            sp_runtime::DispatchError::BadOrigin
        );
        assert_ok!(Litter::set_root(RuntimeOrigin::root(), TAMA));
        // TAMA is the operator now, and may open.
        assert_ok!(Litter::open(RuntimeOrigin::signed(TAMA), "mine now".into()));
    });
}

#[test]
fn only_the_operator_may_install_a_leader() {
    new_test_ext().execute_with(|| {
        assert_noop!(
            Litter::set_leader(RuntimeOrigin::signed(TAMA), TAMA),
            Error::<Test>::NotAuthorized
        );
        assert_ok!(Litter::set_leader(RuntimeOrigin::signed(ROOT), TAMA));
        assert_eq!(Litter::table().leader(), Some(&TAMA));
        assert!(effects()
            .iter()
            .any(|e| matches!(e, Effect::Directed { directive: Directive::LeaderElected, .. })));
    });
}

#[test]
fn state_round_trips_through_storage_between_extrinsics() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "debate & report".into()));
        let p = TaskId::parent(1);
        plan_two(p);
        // A separate extrinsic, a separate load of State — the sub-tasks that
        // `plan` created must still be there.
        assert_ok!(Litter::update(
            RuntimeOrigin::signed(TAMA),
            TaskId::sub(1, 1),
            Act::Claim,
            String::new()
        ));
        assert_eq!(Litter::task(TaskId::sub(1, 1)).unwrap().status, TaskStatus::InProgress);
        assert_eq!(Litter::table().subtasks(p).count(), 2);
    });
}

/// A refused act writes nothing and emits nothing. The litter's own bug: three
/// places re-derived acceptance by testing whether a note began with the word
/// "refused", and the "already claimed" refusals said no such thing — so a
/// no-op would have been replicated as though it had happened.
#[test]
fn a_refused_extrinsic_leaves_no_state_and_no_event() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "x".into()));
        plan_two(TaskId::parent(1));
        let before = crate::Litter::<Test>::get();
        let events_before = effects().len();

        // kuro claiming tama's sub-task.
        assert_noop!(
            Litter::update(
                RuntimeOrigin::signed(KURO),
                TaskId::sub(1, 1),
                Act::Claim,
                String::new()
            ),
            Error::<Test>::NotYours
        );

        assert_eq!(crate::Litter::<Test>::get(), before, "storage untouched");
        assert_eq!(effects().len(), events_before, "nothing to replicate");
    });
}

#[test]
fn every_task_error_maps_to_a_pallet_error() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "x".into()));
        let p = TaskId::parent(1);
        assert_noop!(
            Litter::plan(RuntimeOrigin::signed(LEAD), p, vec![]),
            Error::<Test>::EmptyPlan
        );
        assert_noop!(
            Litter::plan(
                RuntimeOrigin::signed(LEAD),
                p,
                vec![PlanItem { who: ROOT, what: "canvass yourself".into(), expect: String::new() }]
            ),
            Error::<Test>::RootNotAssignable
        );
        plan_two(p);
        assert_noop!(
            Litter::update(RuntimeOrigin::signed(LEAD), p, Act::Artifact, "# early".into()),
            Error::<Test>::SubtasksOutstanding
        );
        assert_noop!(
            Litter::update(
                RuntimeOrigin::signed(TAMA),
                TaskId::sub(1, 1),
                Act::Done,
                "x".repeat(MaxResult::get() as usize + 1)
            ),
            Error::<Test>::TooLong
        );
    });
}

/// Law I, proven: nobody submits an extrinsic, and the table still moves.
#[test]
fn the_chain_ticks_without_anyone_asking_it_to() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "x".into()));
        plan_two(TaskId::parent(1));
        assert_ok!(Litter::update(
            RuntimeOrigin::signed(TAMA),
            TaskId::sub(1, 1),
            Act::Claim,
            String::new()
        ));
        assert_eq!(Litter::task(TaskId::sub(1, 1)).unwrap().status, TaskStatus::InProgress);

        // No extrinsics at all from here. Only blocks.
        roll_to(60);

        assert_eq!(
            Litter::task(TaskId::sub(1, 1)).unwrap().status,
            TaskStatus::Pending,
            "the lease expired and the work requeued, unattended"
        );
        assert!(effects().iter().any(|e| matches!(e, Effect::Requeued { .. })));
        assert!(effects().iter().any(|e| matches!(e, Effect::Nudge { .. })));
    });
}

#[test]
fn the_leader_is_told_which_verb_to_type_by_the_tick() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "debate & report".into()));
        roll_to(2);
        assert!(
            effects().iter().any(|e| matches!(
                e,
                Effect::Directed { to, directive: Directive::PlanNeeded, .. } if *to == LEAD
            )),
            "a parent nobody split must produce a plan-needed directive"
        );
    });
}

/// End to end, through real extrinsics and real blocks.
#[test]
fn a_parent_runs_from_open_to_artifact_on_chain() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "debate & report".into()));
        let p = TaskId::parent(1);
        roll_to(2);
        plan_two(p);

        for (who, s) in [(TAMA, TaskId::sub(1, 1)), (KURO, TaskId::sub(1, 2))] {
            assert_ok!(Litter::update(RuntimeOrigin::signed(who), s, Act::Claim, String::new()));
            assert_ok!(Litter::update(
                RuntimeOrigin::signed(who),
                s,
                Act::Done,
                "a result".into()
            ));
            assert_ok!(Litter::update(
                RuntimeOrigin::signed(LEAD),
                s,
                Act::Clear,
                String::new()
            ));
        }

        roll_to(5);
        assert!(effects()
            .iter()
            .any(|e| matches!(e, Effect::Directed { directive: Directive::ArtifactNeeded, .. })));

        let report = "# Does this codebase work?\n\n## Answer\n\nCompiles; races under load.\n";
        assert_ok!(Litter::update(
            RuntimeOrigin::signed(LEAD),
            p,
            Act::Artifact,
            report.into()
        ));

        assert_eq!(Litter::task(p).unwrap().status, TaskStatus::Closed);
        let a = Litter::artifact(p).unwrap();
        assert_eq!(a.title, "Does this codebase work?", "derived, never asked of the model");
        assert_eq!(a.body, report, "stored verbatim — markdown is never a consensus rule");
        assert_eq!(a.author, LEAD);
    });
}

/// GC drops a closed parent's bookkeeping and keeps its artifact, because the
/// artifact is the durable output the whole lifecycle exists to produce.
#[test]
fn gc_reclaims_closed_parents_but_never_their_artifacts() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "x".into()));
        let p = TaskId::parent(1);
        plan_two(p);
        for (who, s) in [(TAMA, TaskId::sub(1, 1)), (KURO, TaskId::sub(1, 2))] {
            assert_ok!(Litter::update(RuntimeOrigin::signed(who), s, Act::Done, "r".into()));
            assert_ok!(Litter::update(RuntimeOrigin::signed(LEAD), s, Act::Clear, String::new()));
        }
        assert_ok!(Litter::update(RuntimeOrigin::signed(LEAD), p, Act::Artifact, "# done".into()));
        assert_eq!(Litter::table().len(), 3, "parent + two sub-tasks");

        roll_to(2 + GcKeepFor::get() as u64);

        assert_eq!(Litter::table().len(), 0, "the bookkeeping is disposable");
        assert!(Litter::artifact(p).is_some(), "the artifact is not");
        assert_eq!(Litter::artifact(p).unwrap().title, "done");
    });
}

#[test]
fn the_waking_rule_survives_the_trip_through_events() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "x".into()));
        plan_two(TaskId::parent(1));
        let fx = effects();
        let assigned = fx.iter().find(|e| matches!(e, Effect::Assigned { .. })).unwrap();
        let planned = fx.iter().find(|e| matches!(e, Effect::Planned { .. })).unwrap();
        assert!(assigned.wakes(), "an assignment wakes its assignee");
        assert!(!planned.wakes(), "a broadcast record wakes nobody");
    });
}

/// The recovery path, on chain: a cat reports it cannot do the job and the
/// leader hands the work to one that can. Without this a parent stalls
/// forever on a member that will never answer.
#[test]
fn a_leader_can_re_home_work_away_from_a_cat_that_cannot() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "debate & report".into()));
        let p = TaskId::parent(1);
        plan_two(p);
        let s = TaskId::sub(1, 1); // tama's

        assert_ok!(Litter::update(
            RuntimeOrigin::signed(TAMA),
            s,
            Act::Failed,
            "no toolchain on this box".into()
        ));

        // A peer cannot re-home; only the leader decides where work goes.
        assert_noop!(
            Litter::reassign(RuntimeOrigin::signed(KURO), s, KURO),
            Error::<Test>::NotAuthorized
        );
        // And never onto the operator, which has no agent behind it.
        assert_noop!(
            Litter::reassign(RuntimeOrigin::signed(LEAD), s, ROOT),
            Error::<Test>::RootNotAssignable
        );

        assert_ok!(Litter::reassign(RuntimeOrigin::signed(LEAD), s, KURO));
        let t = Litter::task(s).unwrap();
        assert_eq!(t.assignee, Some(KURO));
        assert_eq!(t.status, TaskStatus::Pending);
        assert_eq!(t.outcome, None, "the new holder starts clean");

        // kuro can now actually take it — tama's grip is released.
        assert_ok!(Litter::update(RuntimeOrigin::signed(KURO), s, Act::Claim, String::new()));
    });
}

/// The tick asks for it by name rather than leaving a sub-task silently stuck.
#[test]
fn a_stuck_subtask_reaches_the_leader_as_reassign_needed() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "x".into()));
        plan_two(TaskId::parent(1));
        // Nobody ever claims. Only blocks pass.
        roll_to(200);
        assert!(
            effects().iter().any(|e| matches!(
                e,
                Effect::Directed { directive: Directive::ReassignNeeded, .. }
            )),
            "an exhausted offer must become a named verb for the leader"
        );
    });
}

/// Produce `to` blocks live, recording each block's effects the way a node
/// persists them (tick effects first, then whatever was submitted in it).
fn produce_log(to: u64) -> (Vec<(u64, Vec<Effect<u64>>)>, miot_tasks::State<u64>) {
    use frame_support::traits::OnInitialize;
    let mut blocks = Vec::new();
    let state = new_test_ext().execute_with(|| {
        let mut seen = 0;
        for b in 1..=to {
            if b > 1 {
                System::set_block_number(b);
                <Litter as OnInitialize<u64>>::on_initialize(b);
            }
            match b {
                // Never planned: nagged with PlanNeeded until the budget
                // runs out and the parent fails — `Directed` every time.
                1 => {
                    assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "never planned".into()));
                }
                3 => {
                    assert_ok!(Litter::open(RuntimeOrigin::signed(ROOT), "planned".into()));
                    plan_two(TaskId::parent(2));
                }
                5 => {
                    assert_ok!(Litter::update(RuntimeOrigin::signed(TAMA), TaskId::sub(2, 1), Act::Claim, String::new()));
                }
                _ => {}
            }
            let all = effects();
            blocks.push((b, all[seen..].to_vec()));
            seen = all.len();
        }
        crate::pallet::Litter::<Test>::get()
    });
    (blocks, state)
}

/// Fold a recorded log into a fresh chain, as a follower or a restarting
/// node does: open each block (running `on_initialize`), then
/// `replay_effect` its recorded body.
fn fold_log(blocks: &[(u64, Vec<Effect<u64>>)], replaying: bool) -> miot_tasks::State<u64> {
    use frame_support::traits::OnInitialize;
    new_test_ext().execute_with(|| {
        crate::Pallet::<Test>::set_replaying(replaying);
        for (b, fx) in blocks {
            if *b > 1 {
                System::set_block_number(*b);
                <Litter as OnInitialize<u64>>::on_initialize(*b);
            }
            for e in fx {
                Litter::replay_effect(e, *b as u32);
            }
        }
        crate::pallet::Litter::<Test>::get()
    })
}

/// A follower's state must equal the producer's field for field — it is
/// what a promoted follower keeps producing from. With the tick running
/// locally as well (the pre-`Replaying` behaviour), every `Directed`
/// counted twice and the states drifted.
#[test]
fn a_folded_block_log_reproduces_the_producers_state_exactly() {
    let (blocks, live) = produce_log(60);
    assert!(
        blocks.iter().flat_map(|(_, fx)| fx).any(|e| matches!(e, Effect::Directed { .. })),
        "the scenario must exercise directive nags"
    );
    assert_eq!(fold_log(&blocks, true), live);
    assert_ne!(fold_log(&blocks, false), live, "control: double-ticking must visibly diverge");
}
