//! The log, and the one rule that resolves disagreement.

use super::*;

fn store() -> (Store, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    (s, d)
}

/// `b"<n>"` stands in for a block; the store never looks inside one.
fn blk(n: u64, tag: &str) -> Vec<u8> {
    format!("block{n}{tag}").into_bytes()
}
fn st(n: u64, tag: &str) -> Vec<u8> {
    format!("state{n}{tag}").into_bytes()
}

fn fill(s: &mut Store, upto: u64, tag: &str) {
    for h in (s.head() + 1)..=upto {
        s.append(h, &blk(h, tag), &st(h, tag)).unwrap();
    }
}

#[test]
fn an_empty_store_has_no_head() {
    let (s, _d) = store();
    assert!(s.is_empty());
    assert_eq!(s.head(), 0);
    assert_eq!(s.block(1).unwrap(), None);
}

#[test]
fn blocks_and_their_state_land_together() {
    let (mut s, _d) = store();
    fill(&mut s, 3, "");
    assert_eq!(s.head(), 3);
    assert_eq!(s.block(2).unwrap(), Some(blk(2, "")));
    assert_eq!(s.state_at(2).unwrap(), Some(st(2, "")));
}

/// A gap would make rewind and replay meaningless, so it is refused rather
/// than stored.
#[test]
fn the_log_must_be_contiguous() {
    let (mut s, _d) = store();
    s.append(1, &blk(1, ""), &st(1, "")).unwrap();
    let e = s.append(3, &blk(3, ""), &st(3, "")).unwrap_err();
    assert!(matches!(e, Error::NotContiguous { expected: 2, got: 3 }));
    assert_eq!(s.head(), 1, "the refusal left the log alone");
}

#[test]
fn a_reopened_store_finds_its_head() {
    let d = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(d.path()).unwrap();
        fill(&mut s, 5, "");
    }
    let s = Store::open(d.path()).unwrap();
    assert_eq!(s.head(), 5);
    assert_eq!(s.state_at(5).unwrap(), Some(st(5, "")));
}

// ---- the leader wins ------------------------------------------------------

/// The whole conflict-resolution rule, in one test: we diverged from the
/// leader at 3, so everything we built on top of our own version of 4 is
/// discarded and the state at 3 comes back.
#[test]
fn a_rewind_discards_our_own_blocks_and_returns_the_state_to_adopt() {
    let (mut s, _d) = store();
    fill(&mut s, 6, "-ours");

    let (state, dropped) = s.rewind_to(3).unwrap();
    assert_eq!(dropped, 3, "4, 5 and 6 were ours and are gone");
    assert_eq!(state, Some(st(3, "-ours")), "the fork point's state is what we adopt from");
    assert_eq!(s.head(), 3);
    assert_eq!(s.block(4).unwrap(), None);
    assert_eq!(s.state_at(4).unwrap(), None, "state above the fork goes with the blocks");

    // …and the leader's version of 4 onward goes on cleanly.
    fill(&mut s, 6, "-leader");
    assert_eq!(s.block(4).unwrap(), Some(blk(4, "-leader")));
    assert_eq!(s.head(), 6);
}

#[test]
fn rewinding_to_the_head_is_a_no_op_that_still_hands_back_the_state() {
    let (mut s, _d) = store();
    fill(&mut s, 4, "");
    let (state, dropped) = s.rewind_to(4).unwrap();
    assert_eq!(dropped, 0);
    assert_eq!(state, Some(st(4, "")));
    assert_eq!(s.head(), 4);
}

/// Adopting the leader's chain from genesis.
#[test]
fn rewinding_to_zero_empties_the_log() {
    let (mut s, _d) = store();
    fill(&mut s, 3, "-ours");
    let (state, dropped) = s.rewind_to(0).unwrap();
    assert_eq!(dropped, 3);
    assert_eq!(state, None, "there is no state at genesis to restore");
    assert!(s.is_empty());
    fill(&mut s, 2, "-leader");
    assert_eq!(s.block(1).unwrap(), Some(blk(1, "-leader")));
}

#[test]
fn rewinding_forward_is_refused() {
    let (mut s, _d) = store();
    fill(&mut s, 2, "");
    assert!(matches!(s.rewind_to(9).unwrap_err(), Error::RewindAhead { head: 2, to: 9 }));
    assert_eq!(s.head(), 2);
}

// ---- finding where we disagree -------------------------------------------

#[test]
fn the_fork_point_is_the_last_height_we_agree_on() {
    let (mut s, _d) = store();
    fill(&mut s, 5, "-ours");
    // The leader agrees with us through 3, then differs.
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
fn total_agreement_forks_at_the_head() {
    let (mut s, _d) = store();
    fill(&mut s, 3, "");
    let theirs = vec![blk(1, ""), blk(2, ""), blk(3, "")];
    assert_eq!(s.fork_point(1, &theirs).unwrap(), 3);
}

#[test]
fn disagreement_from_the_very_first_block_forks_below_it() {
    let (mut s, _d) = store();
    fill(&mut s, 3, "-ours");
    let theirs = vec![blk(1, "-leader")];
    assert_eq!(s.fork_point(1, &theirs).unwrap(), 0, "nothing in common; rewind to genesis");
}

/// The leader being further along than us is not a fork.
#[test]
fn a_leader_ahead_of_us_agrees_as_far_as_we_go() {
    let (mut s, _d) = store();
    fill(&mut s, 2, "");
    let theirs = vec![blk(1, ""), blk(2, ""), blk(3, ""), blk(4, "")];
    assert_eq!(s.fork_point(1, &theirs).unwrap(), 2);
}

/// End to end: diverge, find the fork, rewind, adopt.
#[test]
fn a_cat_that_diverged_converges_on_the_leader() {
    let (mut s, _d) = store();
    fill(&mut s, 3, "");
    fill(&mut s, 6, "-ours");

    let leader: Vec<Vec<u8>> = (1..=8)
        .map(|h| if h <= 3 { blk(h, "") } else { blk(h, "-leader") })
        .collect();

    let fork = s.fork_point(1, &leader).unwrap();
    assert_eq!(fork, 3);
    let (state, dropped) = s.rewind_to(fork).unwrap();
    assert_eq!(dropped, 3);
    assert_eq!(state, Some(st(3, "")));

    for h in (fork + 1)..=8 {
        s.append(h, &leader[(h - 1) as usize], &st(h, "-leader")).unwrap();
    }
    assert_eq!(s.head(), 8);
    assert_eq!(s.block(4).unwrap(), Some(blk(4, "-leader")), "ours is gone, theirs is canonical");
}

// ---- pruning --------------------------------------------------------------

#[test]
fn pruning_drops_old_blocks_and_moves_the_floor() {
    let (mut s, _d) = store();
    fill(&mut s, 10, "");
    let dropped = s.prune(8).unwrap();
    assert_eq!(dropped, 7, "1..=7 went");
    assert_eq!(s.oldest(), 7);
    assert_eq!(s.block(5).unwrap(), None);
    assert_eq!(s.block(8).unwrap(), Some(blk(8, "")));
    assert_eq!(s.head(), 10, "pruning the tail never touches the head");
}

#[test]
fn a_rewind_past_the_pruned_floor_is_refused_rather_than_silently_wrong() {
    let (mut s, _d) = store();
    fill(&mut s, 10, "");
    s.prune(8).unwrap();
    assert!(matches!(s.rewind_to(4).unwrap_err(), Error::Pruned { oldest: 7, wanted: 4 }));
    assert_eq!(s.head(), 10, "and it changed nothing");
}

#[test]
fn a_reopened_pruned_store_remembers_its_floor() {
    let d = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(d.path()).unwrap();
        fill(&mut s, 10, "");
        s.prune(8).unwrap();
    }
    let s = Store::open(d.path()).unwrap();
    assert_eq!(s.head(), 10);
    assert_eq!(s.oldest(), 7);
}
