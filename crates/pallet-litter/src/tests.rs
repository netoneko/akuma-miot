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
fn genesis_commits_the_roster_by_name_and_in_order() {
    new_test_ext().execute_with(|| {
        let r = Litter::roster();
        assert_eq!(r.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["root", "lead", "tama", "kuro"]);
        assert_eq!(r.iter().map(|(_, a)| *a).collect::<Vec<_>>(), [ROOT, LEAD, TAMA, KURO]);
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

/// A standalone artifact needs no task at all — not even a genesis leader —
/// and reads back the same way a task's own artifact does.
#[test]
fn a_standalone_artifact_needs_no_leader_or_task() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::publish_standalone_artifact(RuntimeOrigin::signed(TAMA), "# hi".into()));
        assert!(effects().iter().any(|e| matches!(e, Effect::StandaloneArtifact { .. })));
        assert_eq!(Litter::table().len(), 0, "touches no task state");

        let (id, a) = Litter::standalone_artifacts().into_iter().next().unwrap();
        assert_eq!(a.title, "hi");
        assert_eq!(a.author, TAMA);
        assert_eq!(Litter::standalone_artifact(id).unwrap(), a);
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

/// Messages are counted apart from tool calls, on the primary and — by
/// replaying the effect — on every replica.
#[test]
fn report_stats2_splits_messages_and_replays() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::report_stats2(RuntimeOrigin::signed(TAMA), 3, 2, 5, 900, 1_000));
        assert_eq!(Litter::messages_sent(&TAMA), Some(5));
        assert_eq!(Litter::all_stats().into_iter().find(|(w, _)| *w == TAMA).unwrap().1.tool_calls, 2);
        let fx = effects();
        assert!(fx.contains(&Effect::StatsReported2 { who: TAMA, turns: 3, tool_calls: 2, messages: 5, tokens: 900, ms: 1_000 }), "{fx:?}");
    });
    // A replica: storage rebuilt from the effect alone.
    new_test_ext().execute_with(|| {
        Litter::replay_effect(&Effect::StatsReported2 { who: KURO, turns: 1, tool_calls: 0, messages: 1, tokens: 10, ms: 5 }, 1);
        assert_eq!(Litter::messages_sent(&KURO), Some(1));
    });
}

/// An agent on an older build still reports through the old call; its
/// messages simply aren't known — `None`, not a made-up zero.
#[test]
fn old_report_stats_still_works_without_messages() {
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::report_stats(RuntimeOrigin::signed(TAMA), 4, 7, 100, 50));
        assert_eq!(Litter::messages_sent(&TAMA), None);
        Litter::replay_effect(&Effect::StatsReported { who: KURO, turns: 1, tool_calls: 1, tokens: 1, ms: 1 }, 1);
        assert_eq!(Litter::all_stats().len(), 2);
    });
}

/// The encoding claim behind adding a variant instead of a field: the old
/// variant's index is unchanged (so every block already on disk decodes),
/// and the new one comes right after it.
#[test]
fn stats_variants_keep_their_indices() {
    use codec::Encode;
    let old = Effect::<u64>::StatsReported { who: 1, turns: 0, tool_calls: 0, tokens: 0, ms: 0 }.encode();
    let new = Effect::<u64>::StatsReported2 { who: 1, turns: 0, tool_calls: 0, messages: 0, tokens: 0, ms: 0 }.encode();
    assert_eq!(old[0], 13, "StatsReported must stay variant 13 (as committed before StatsReported2) — it's in every block on disk");
    assert_eq!(new[0], 14);
}

// ── messaging: votes, comments, epochs ──────────────────────────────────

use miot_primitives::{ArtifactId, MessageId};

/// One voter, one side: voting the other way moves them, repeating their
/// own side withdraws.
#[test]
fn votes_toggle_and_move() {
    let art = ArtifactId::Note(7);
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::vote(RuntimeOrigin::signed(TAMA), art, true));
        assert_ok!(Litter::vote(RuntimeOrigin::signed(KURO), art, true));
        assert_eq!(Litter::tally(art).up, vec![TAMA, KURO]);
        // TAMA changes their mind — moved, not doubled.
        assert_ok!(Litter::vote(RuntimeOrigin::signed(TAMA), art, false));
        assert_eq!(Litter::tally(art).up, vec![KURO]);
        assert_eq!(Litter::tally(art).down, vec![TAMA]);
        // KURO repeats their own side — a withdrawal.
        assert_ok!(Litter::vote(RuntimeOrigin::signed(KURO), art, true));
        assert!(Litter::tally(art).up.is_empty());
        // The effect rode along, so a replica folds to the same place.
        let fx = effects();
        assert_eq!(fx.iter().filter(|e| matches!(e, Effect::Voted { .. })).count(), 4, "{fx:?}");
    });
}

/// A comment on an artifact lands in that artifact's thread — chain
/// storage, keyed by the current epoch — and `replay_effect` folds it the
/// same way, so a replica's thread matches the producer's.
#[test]
fn artifact_comments_pack_under_the_artifact_and_replay() {
    let art = ArtifactId::Task(TaskId::parent(1));
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::post(
            RuntimeOrigin::signed(TAMA),
            MessageId(0xdead_beef),
            None,
            "the conclusion section overstates this".into(),
            None,
            Some(art),
            vec![],
            false,
            false,
        ));
        let thread = Litter::comments(art);
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].who, TAMA);
        assert_eq!(thread[0].body, "the conclusion section overstates this");
        // Replicas fold the effect, not the call.
        let fx = effects();
        Litter::replay_effect(&fx.last().unwrap().clone(), 1);
        assert_eq!(Litter::comments(art).len(), 2, "replay must land in the same thread");
    });
}

/// Each epoch gains its own comment *section* — stamped on the entry, not
/// used as a key, so the whole thread stays one storage read and nothing
/// is lost at a boundary (`docs/MESSAGING.md`).
#[test]
fn compaction_opens_a_fresh_comment_section() {
    let art = ArtifactId::Note(3);
    new_test_ext().execute_with(|| {
        assert_ok!(Litter::post(
            RuntimeOrigin::signed(TAMA),
            MessageId(1),
            None,
            "epoch 0 note".into(),
            None,
            Some(art),
            vec![],
            false,
            false,
        ));
        assert_eq!(Litter::epoch(), 0);
        assert_eq!(Litter::comments(art).len(), 1);
        // `request_compaction` is the operator's epoch boundary.
        assert_ok!(Litter::request_compaction(RuntimeOrigin::signed(ROOT)));
        assert_eq!(Litter::epoch(), 1);
        assert_ok!(Litter::post(
            RuntimeOrigin::signed(KURO),
            MessageId(2),
            None,
            "epoch 1 note".into(),
            None,
            Some(art),
            vec![],
            false,
            false,
        ));
        // Both survive, each stamped with its session.
        let thread = Litter::comments(art);
        assert_eq!(thread.iter().map(|c| c.epoch).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(thread.last().unwrap().body, "epoch 1 note");
        // `clear_all` is the other boundary.
        assert_ok!(Litter::clear_all(RuntimeOrigin::signed(ROOT)));
        assert_eq!(Litter::epoch(), 2);
        // Non-root cannot turn the epoch.
        assert_noop!(
            Litter::request_compaction(RuntimeOrigin::signed(TAMA)),
            Error::<Test>::NotAuthorized
        );
    });
}

/// Same claim as `stats_variants_keep_their_indices`, for the messaging
/// variants: appended, never spliced.
#[test]
fn messaging_variants_are_appended_in_order() {
    use codec::Encode;
    let said = Effect::<u64>::Said { from: 1, to: None, body: String::new(), from_root: false, no_ack: false, off_record: false }.encode();
    assert_eq!(said[0], 3, "Said must stay variant 3 — it's in every block on disk");
    let message = Effect::<u64>::Message { id: MessageId(0), from: 1, to: None, body: String::new(), parent: None, artifact_id: None, tags: vec![], from_root: false, no_ack: false, off_record: false }.encode();
    let reacted = Effect::<u64>::Reacted { who: 1, target: MessageId(0), emoji: String::new() }.encode();
    let voted = Effect::<u64>::Voted { who: 1, artifact: ArtifactId::Note(0), up: true }.encode();
    assert_eq!(message[0], 15);
    assert_eq!(reacted[0], 16);
    assert_eq!(voted[0], 17);
}
