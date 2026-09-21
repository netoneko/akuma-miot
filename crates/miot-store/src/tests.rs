//! The log, its compaction points, and the one rule that resolves
//! disagreement.

use super::*;

fn store() -> (Store, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    (s, d)
}

/// The store never looks inside a block, so a tag is enough to tell "ours"
/// from "the leader's".
fn blk(n: u64, tag: &str) -> Vec<u8> {
    format!("block{n}{tag}").into_bytes()
}
fn st(n: u64) -> Vec<u8> {
    format!("state{n}").into_bytes()
}

fn fill(s: &mut Store, upto: u64, tag: &str) {
    for h in (s.head() + 1)..=upto {
        s.append(h, &blk(h, tag)).unwrap();
    }
}

#[test]
fn an_empty_store_is_at_genesis() {
    let (s, _d) = store();
    assert!(s.is_empty());
    assert_eq!(s.head(), 0);
    assert_eq!(s.last_checkpoint(), 0, "no compaction yet means genesis");
    assert_eq!(s.checkpoint_state().unwrap(), None);
}

#[test]
fn the_log_must_be_contiguous() {
    let (mut s, _d) = store();
    s.append(1, &blk(1, "")).unwrap();
    assert!(matches!(
        s.append(3, &blk(3, "")).unwrap_err(),
        Error::NotContiguous { expected: 2, got: 3 }
    ));
    assert_eq!(s.head(), 1, "the refusal left the log alone");
}

#[test]
fn a_reopened_store_finds_its_head_and_its_compaction() {
    let d = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(d.path()).unwrap();
        fill(&mut s, 5, "");
        s.compact(4, &st(4)).unwrap();
    }
    let s = Store::open(d.path()).unwrap();
    assert_eq!(s.head(), 5);
    assert_eq!(s.last_checkpoint(), 4);
    assert_eq!(s.checkpoint_state().unwrap(), Some(st(4)));
}

// ---- compaction -----------------------------------------------------------

/// After a compaction there is nothing to replay from below it, so keeping
/// those blocks would be keeping history nobody can use.
#[test]
fn compaction_drops_the_blocks_beneath_it() {
    let (mut s, _d) = store();
    fill(&mut s, 6, "");
    let pruned = s.compact(4, &st(4)).unwrap();
    assert_eq!(pruned, 4, "1..=4 went");
    assert_eq!(s.block(3).unwrap(), None);
    assert_eq!(s.block(5).unwrap(), Some(blk(5, "")), "above the compaction is untouched");
    assert_eq!(s.head(), 6, "compaction never moves the head");
}

#[test]
fn only_the_latest_compaction_state_is_kept() {
    let (mut s, _d) = store();
    fill(&mut s, 10, "");
    s.compact(3, &st(3)).unwrap();
    s.compact(7, &st(7)).unwrap();
    assert_eq!(s.last_checkpoint(), 7);
    assert_eq!(s.checkpoint_state().unwrap(), Some(st(7)));
}

#[test]
fn a_compaction_above_the_head_or_below_the_last_is_refused() {
    let (mut s, _d) = store();
    fill(&mut s, 5, "");
    s.compact(4, &st(4)).unwrap();
    assert!(matches!(s.compact(9, &st(9)).unwrap_err(), Error::BadCheckpoint { .. }));
    assert!(matches!(s.compact(2, &st(2)).unwrap_err(), Error::BadCheckpoint { .. }));
    assert_eq!(s.last_checkpoint(), 4, "both refusals changed nothing");
}

// ---- the leader wins ------------------------------------------------------

/// The whole rule: we diverged above the last compaction, so we go back to the
/// compaction — not to the fork point — and replay the leader from there.
#[test]
fn a_rewind_lands_on_the_last_compaction_not_on_the_fork_point() {
    let (mut s, _d) = store();
    fill(&mut s, 3, "");
    s.compact(3, &st(3)).unwrap();
    fill(&mut s, 7, "-ours");

    let r = s.rewind_for_fork(4).unwrap();
    assert_eq!(r.height, 3, "the compaction, even though we agreed through 4");
    assert_eq!(r.state, Some(st(3)));
    assert_eq!(r.dropped, 4, "4..=7 were ours and are gone");
    assert_eq!(s.head(), 3);
    assert_eq!(s.block(4).unwrap(), None);

    // The leader's chain goes on cleanly from the landing point.
    fill(&mut s, 8, "-leader");
    assert_eq!(s.block(4).unwrap(), Some(blk(4, "-leader")));
    assert_eq!(s.head(), 8);
}

/// With no compaction yet, the only place to land is genesis.
#[test]
fn a_rewind_with_no_compaction_goes_to_genesis() {
    let (mut s, _d) = store();
    fill(&mut s, 4, "-ours");
    let r = s.rewind_for_fork(2).unwrap();
    assert_eq!(r.height, 0);
    assert_eq!(r.state, None, "nothing to restore; rebuild from nothing");
    assert_eq!(r.dropped, 4);
    assert!(s.is_empty());
}

/// We cannot restore a state we no longer hold, so the honest answer is
/// genesis rather than pretending.
#[test]
fn a_fork_below_the_last_compaction_goes_to_genesis() {
    let (mut s, _d) = store();
    fill(&mut s, 6, "");
    s.compact(5, &st(5)).unwrap();
    fill(&mut s, 8, "-ours");

    let r = s.rewind_for_fork(2).unwrap();
    assert_eq!(r.height, 0, "the fork is below the compaction we hold");
    assert_eq!(r.state, None);
    assert!(s.is_empty());
    assert_eq!(s.last_checkpoint(), 0, "and the stale compaction went with it");
}

#[test]
fn agreeing_all_the_way_still_rewinds_to_the_compaction() {
    let (mut s, _d) = store();
    fill(&mut s, 5, "");
    s.compact(2, &st(2)).unwrap();
    let r = s.rewind_for_fork(5).unwrap();
    assert_eq!(r.height, 2);
    assert_eq!(r.dropped, 3, "3..=5 are replayed from the leader rather than trusted");
}

// ---- finding the fork -----------------------------------------------------

#[test]
fn the_fork_point_is_the_last_height_we_agree_on() {
    let (mut s, _d) = store();
    fill(&mut s, 5, "-ours");
    let theirs = vec![
        blk(1, "-ours"),
        blk(2, "-ours"),
        blk(3, "-ours"),
        blk(4, "-leader"),
        blk(5, "-leader"),
    ];
    assert_eq!(s.fork_point(1, &theirs).unwrap(), 3);
}

#[test]
fn a_leader_ahead_of_us_is_not_a_fork() {
    let (mut s, _d) = store();
    fill(&mut s, 2, "");
    let theirs = vec![blk(1, ""), blk(2, ""), blk(3, ""), blk(4, "")];
    assert_eq!(s.fork_point(1, &theirs).unwrap(), 2);
}

#[test]
fn disagreement_from_the_first_block_forks_below_it() {
    let (mut s, _d) = store();
    fill(&mut s, 3, "-ours");
    assert_eq!(s.fork_point(1, &[blk(1, "-leader")]).unwrap(), 0);
}

// ---- end to end -----------------------------------------------------------

/// A cat diverges, notices, and converges on the leader — keeping everything
/// up to the compaction the litter had already agreed on.
#[test]
fn a_cat_that_diverged_converges_on_the_leader() {
    let (mut s, _d) = store();
    fill(&mut s, 4, "");
    s.compact(4, &st(4)).unwrap();
    fill(&mut s, 7, "-ours");

    // The leader shares our history through the compaction, then differs.
    let leader: Vec<Vec<u8>> = (5..=9).map(|h| blk(h, "-leader")).collect();
    let fork = s.fork_point(5, &leader).unwrap();
    assert_eq!(fork, 4, "we agree on nothing above the compaction");

    let r = s.rewind_for_fork(fork).unwrap();
    assert_eq!(r.height, 4);
    assert_eq!(r.state, Some(st(4)), "adopt this and replay");
    assert_eq!(r.dropped, 3);

    for (i, b) in leader.iter().enumerate() {
        s.append(5 + i as u64, b).unwrap();
    }
    assert_eq!(s.head(), 9);
    assert_eq!(s.block(5).unwrap(), Some(blk(5, "-leader")), "theirs is canonical");
    assert_eq!(s.last_checkpoint(), 4, "the agreed compaction survived the whole thing");
}
