//! Mock elections: N [`Mesh`]es on a simulated network with a fake clock.
//!
//! The network is a reachability matrix, so a partition is just some links
//! set to false, and a kill is a node that stops ticking and answering. Every
//! tick checks the one safety property that matters: **at most one leader
//! per term, ever.** Liveness is checked per test ("a leader emerges within
//! so long").

use super::*;

const STEP_MS: u64 = 100;
const POLL_MS: u64 = 1_000;
const BLOCK_MS: u64 = 1_000;

struct Sim {
    nodes: Vec<Mesh>,
    heads: Vec<u64>,
    alive: Vec<bool>,
    link: Vec<Vec<bool>>,
    now: u64,
    /// term → who led it. The safety invariant.
    leaders_by_term: BTreeMap<u64, Name>,
    /// Blocks delivered by push rather than pull.
    pushes: usize,
}

fn name(i: usize) -> Name {
    format!("n{i}")
}

fn idx(route: &str) -> usize {
    route.trim_start_matches('n').parse().unwrap()
}

impl Sim {
    fn new(n: usize) -> Self {
        let nodes = (0..n)
            .map(|i| {
                let routes = (0..n).filter(|&j| j != i).map(name).collect();
                Mesh::new(name(i), routes, Timing::default(), Hard::default(), 0, 0x9e37_79b9_7f4a_7c15 ^ (i as u64 * 7919))
            })
            .collect();
        Sim {
            nodes,
            heads: vec![0; n],
            alive: vec![true; n],
            link: vec![vec![true; n]; n],
            now: 0,
            leaders_by_term: BTreeMap::new(),
            pushes: 0,
        }
    }

    fn reach(&self, a: usize, b: usize) -> bool {
        self.call(a, b) && self.call(b, a)
    }

    /// Can `a` open a connection to `b`? The answer comes back on it, so
    /// this is all a request/response needs. `link` is directional:
    /// `link[a][b]` false with `link[b][a]` true is a NAT.
    fn call(&self, a: usize, b: usize) -> bool {
        self.alive[a] && self.alive[b] && self.link[a][b]
    }

    /// Split into two sides. Links inside a side stay up.
    fn partition(&mut self, side: &[usize]) {
        let n = self.nodes.len();
        for a in 0..n {
            for b in 0..n {
                self.link[a][b] = side.contains(&a) == side.contains(&b);
            }
        }
    }

    fn heal(&mut self) {
        for row in &mut self.link {
            row.iter_mut().for_each(|l| *l = true);
        }
    }

    /// Deliver `req` from `c` to everyone it can reach, feed the replies
    /// back, and keep going if a pre-vote turned into a real one.
    fn campaign(&mut self, c: usize, mut req: VoteRequest) {
        loop {
            let mut next = None;
            for v in 0..self.nodes.len() {
                if v == c || !self.call(c, v) {
                    continue;
                }
                let reply = self.nodes[v].on_vote_request(&req, self.now, self.heads[v]);
                let (now, head) = (self.now, self.heads[c]);
                if let Some(r) = self.nodes[c].on_vote_reply(&name(v), &req, reply, now, head) {
                    next = Some(r);
                }
            }
            match next {
                Some(r) => req = r,
                None => return,
            }
        }
    }

    fn step(&mut self) {
        self.now += STEP_MS;
        let n = self.nodes.len();

        if self.now % POLL_MS == 0 {
            for a in 0..n {
                for b in 0..n {
                    if a != b && self.call(a, b) {
                        // The poll carries the poller's status; the answer
                        // carries the polled node's.
                        let mine = self.nodes[a].status(self.heads[a], "");
                        self.nodes[b].on_inbound(mine, self.now);
                        let st = self.nodes[b].status(self.heads[b], "");
                        self.nodes[a].on_status(&name(b), st, self.now);
                    }
                }
            }
        }

        for i in 0..n {
            if !self.alive[i] {
                continue;
            }
            if let Some(req) = self.nodes[i].tick(self.now, self.heads[i]) {
                self.campaign(i, req);
            }
        }

        // The block loop and the replica pull, reduced to the one number the
        // election cares about.
        if self.now % BLOCK_MS == 0 {
            for i in 0..n {
                if self.alive[i] && self.nodes[i].is_leader() {
                    self.heads[i] += 1;
                    self.nodes[i].appended();
                }
            }
            for i in 0..n {
                if let Some(l) = self.nodes[i].leader_route().map(idx) {
                    if self.call(i, l) && self.heads[l] > self.heads[i] {
                        self.heads[i] = self.heads[l];
                        self.nodes[i].appended();
                    }
                }
            }
            // The leader's push to whoever isn't pulling. Taken only from
            // the leader the receiver follows (what the node checks too).
            for l in 0..n {
                if !self.alive[l] || !self.nodes[l].is_leader() {
                    continue;
                }
                let (now, head) = (self.now, self.heads[l]);
                for t in self.nodes[l].push_targets(now, head).into_iter().map(|r| idx(&r)) {
                    let st = self.nodes[l].status(head, "");
                    if !self.call(l, t) {
                        continue;
                    }
                    self.nodes[t].on_inbound(st.clone(), now);
                    if self.nodes[t].accepts_push_from(&st) {
                        self.heads[t] = head;
                        self.nodes[t].appended();
                        self.pushes += 1;
                    }
                }
            }
        }

        for i in 0..n {
            let m = &self.nodes[i];
            if m.is_leader() {
                let prev = self.leaders_by_term.entry(m.term()).or_insert_with(|| m.name().to_string());
                assert_eq!(prev, m.name(), "two leaders in term {} at t={}ms", m.term(), self.now);
            }
        }
    }

    fn run(&mut self, ms: u64) {
        for _ in 0..ms / STEP_MS {
            self.step();
        }
    }

    /// Run until exactly one live node leads and every live node that can
    /// reach it follows it. Panics after `within` ms.
    fn settle(&mut self, within: u64) -> usize {
        let deadline = self.now + within;
        while self.now < deadline {
            self.step();
            if let Some(l) = self.stable_leader() {
                return l;
            }
        }
        panic!("no stable leader within {within}ms: {:?}", self.roles());
    }

    fn stable_leader(&self) -> Option<usize> {
        let leaders: Vec<usize> = (0..self.nodes.len()).filter(|&i| self.alive[i] && self.nodes[i].is_leader()).collect();
        let [l] = leaders[..] else { return None };
        let all_follow = (0..self.nodes.len())
            .filter(|&i| i != l && self.reach(i, l))
            .all(|i| self.nodes[i].leader() == Some(self.nodes[l].name()));
        all_follow.then_some(l)
    }

    fn leaders(&self) -> Vec<usize> {
        (0..self.nodes.len()).filter(|&i| self.alive[i] && self.nodes[i].is_leader()).collect()
    }

    fn roles(&self) -> Vec<(Name, Role, u64, bool)> {
        self.nodes.iter().zip(&self.alive).map(|(m, a)| (m.name().to_string(), m.role(), m.term(), *a)).collect()
    }
}

#[test]
fn five_nodes_elect_exactly_one_leader() {
    let mut s = Sim::new(5);
    let l = s.settle(20_000);
    s.run(30_000);
    assert_eq!(s.stable_leader(), Some(l), "a healthy mesh keeps its leader: {:?}", s.roles());
    assert_eq!(s.leaders_by_term.len(), 1, "one election, not a churn of them");
}

#[test]
fn a_single_node_mesh_leads_itself() {
    let mut s = Sim::new(1);
    s.settle(10_000);
}

#[test]
fn killing_the_leader_elects_another_and_the_log_keeps_growing() {
    let mut s = Sim::new(5);
    let first = s.settle(20_000);
    s.run(5_000);
    let head_at_kill = s.heads[first];
    s.alive[first] = false;

    let second = s.settle(20_000);
    assert_ne!(second, first);
    assert!(s.nodes[second].term() > s.nodes[first].term());
    s.run(5_000);
    assert!(s.heads[second] > head_at_kill, "the new leader produces past where the old one stopped");
}

#[test]
fn two_of_five_down_still_elects_three_down_does_not() {
    let mut s = Sim::new(5);
    let first = s.settle(20_000);
    let others: Vec<usize> = (0..5).filter(|&i| i != first).collect();
    s.alive[first] = false;
    s.alive[others[0]] = false;
    s.settle(20_000);

    let l = s.leaders()[0];
    s.alive[l] = false;
    s.run(60_000);
    assert!(s.leaders().is_empty(), "two of five is not a quorum: {:?}", s.roles());
}

/// The partition case, which is the one election is really for. The old
/// leader on the minority side has to notice it has lost the majority and
/// stop claiming to lead (check-quorum). The majority has to elect a new
/// one. After the heal, the old leader has to follow the new one: it must
/// neither depose it nor win with the blocks it produced alone.
#[test]
fn a_leader_cut_off_in_the_minority_steps_down_and_rejoins_as_a_follower() {
    let mut s = Sim::new(5);
    let old = s.settle(20_000);
    let buddy = (old + 1) % 5;
    s.partition(&[old, buddy]);

    s.run(15_000);
    assert!(!s.nodes[old].is_leader(), "check-quorum: a leader without a majority steps down: {:?}", s.roles());
    let new = s.stable_leader().expect("the majority side elects its own");
    assert!(new != old && new != buddy);
    let minority_term = s.nodes[old].term().max(s.nodes[buddy].term());
    assert!(minority_term < s.nodes[new].term(), "pre-vote: the minority never inflates its term");

    s.heal();
    s.run(10_000);
    assert_eq!(s.stable_leader(), Some(new), "the heal must not depose the majority's leader: {:?}", s.roles());
}

/// A node whose own link flaps keeps timing out, but without pre-vote it
/// would bump its term every time and depose a healthy leader on return.
#[test]
fn an_isolated_node_does_not_disrupt_a_healthy_leader_on_return() {
    let mut s = Sim::new(5);
    let leader = s.settle(20_000);
    let loner = (leader + 2) % 5;
    let term = s.nodes[leader].term();
    s.partition(&[loner]);
    s.run(60_000);
    assert_eq!(s.nodes[loner].term(), term, "no term inflation while alone");
    s.heal();
    s.run(5_000);
    assert_eq!(s.stable_leader(), Some(leader));
    assert_eq!(s.nodes[leader].term(), term);
}

/// The up-to-date check, directly. A candidate behind the voter's log is
/// refused even in a term the voter hasn't voted in.
#[test]
fn a_vote_goes_only_to_a_log_at_least_as_far_along() {
    let mut voter = Mesh::new("v".into(), vec!["c".into(), "x".into()], Timing::default(), Hard { term: 3, voted_for: None, head_term: 3 }, 0, 1);
    let behind = VoteRequest { term: 4, candidate: "c".into(), head: 9, head_term: 3, pre: false };
    assert!(!voter.on_vote_request(&behind, 0, 10).granted);
    let older_term = VoteRequest { term: 4, candidate: "c".into(), head: 50, head_term: 2, pre: false };
    assert!(!voter.on_vote_request(&older_term, 0, 10).granted, "a higher head from an older term loses");
    let level = VoteRequest { term: 4, candidate: "c".into(), head: 10, head_term: 3, pre: false };
    assert!(voter.on_vote_request(&level, 0, 10).granted);
}

/// The persisted vote is what a restart must honour.
#[test]
fn a_restarted_node_does_not_vote_twice_in_one_term() {
    let mut a = Mesh::new("v".into(), vec!["c1".into(), "c2".into()], Timing::default(), Hard::default(), 0, 1);
    let r1 = VoteRequest { term: 1, candidate: "c1".into(), head: 0, head_term: 0, pre: false };
    assert!(a.on_vote_request(&r1, 0, 0).granted);
    let hard = a.take_dirty().expect("a vote marks hard state dirty");
    assert_eq!(a.take_dirty(), None);

    let mut b = Mesh::new("v".into(), vec!["c1".into(), "c2".into()], Timing::default(), hard, 0, 2);
    let r2 = VoteRequest { term: 1, candidate: "c2".into(), head: 0, head_term: 0, pre: false };
    assert!(!b.on_vote_request(&r2, 0, 0).granted);
    assert!(b.on_vote_request(&r1, 0, 0).granted, "re-granting the same candidate is fine");
}

/// Many seeds, random kills and heals, checking only safety. Liveness
/// under arbitrary chaos isn't promised; one leader per term is.
#[test]
fn chaos_never_produces_two_leaders_in_one_term() {
    for seed in 0..20u64 {
        let mut s = Sim::new(5);
        let mut r = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) | 1;
        let mut next = move || {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            r
        };
        for _ in 0..40 {
            match next() % 4 {
                0 => {
                    let i = (next() % 5) as usize;
                    s.alive[i] = !s.alive[i];
                }
                1 => {
                    let side: Vec<usize> = (0..5).filter(|_| next() % 2 == 0).collect();
                    s.partition(&side);
                }
                2 => s.heal(),
                _ => {}
            }
            s.run(3_000);
        }
        s.alive.iter_mut().for_each(|a| *a = true);
        s.heal();
        s.settle(30_000);
    }
}

#[test]
fn listing_yourself_as_a_peer_does_not_inflate_the_quorum() {
    let mut m = Mesh::new("a".into(), vec!["self".into(), "b".into(), "c".into()], Timing::default(), Hard::default(), 0, 1);
    assert_eq!(m.quorum(), 3);
    let me = m.status(0, "");
    m.on_status(&"self".to_string(), me, 0);
    assert_eq!(m.routes(), &["b".to_string(), "c".to_string()]);
    assert_eq!(m.quorum(), 2);
}

// ── one-way reachability (the AWS pair behind home's router, 2026-09-24) ──

/// Node 4 can be called by everyone and can call nobody — a peer behind a
/// NAT it doesn't control. Before status went both ways and the leader
/// pushed, it heard nothing, followed nobody and campaigned forever. Now it
/// follows the leader, keeps up with its log, and never disturbs it.
#[test]
fn a_node_that_cannot_call_out_follows_and_keeps_up() {
    let mut s = Sim::new(5);
    for b in 0..5 {
        s.link[4][b] = false;
    }
    s.run(20_000);
    let leaders = s.leaders();
    let [l] = leaders[..] else { panic!("one leader: {:?}", s.roles()) };
    assert_ne!(l, 4, "it can't collect votes, so it can't lead");
    s.run(30_000);
    assert_eq!(s.leaders(), vec![l], "the leader was never disturbed: {:?}", s.roles());
    assert_eq!(s.nodes[4].leader(), Some(s.nodes[l].name()), "{:?}", s.roles());
    assert_eq!(s.nodes[4].role(), Role::Follower);
    assert_eq!(s.nodes[4].leader_route(), None, "no route: it follows by push alone");
    assert!(s.heads[l] - s.heads[4] <= 6, "log kept up by push: leader {} vs {}", s.heads[l], s.heads[4]);
    assert!(s.pushes > 0);
    assert_eq!(s.leaders_by_term.len(), 1, "no churn: {:?}", s.leaders_by_term);
}

/// Everyone who can pull is never pushed to: push is for the stuck only.
#[test]
fn a_healthy_mesh_never_pushes() {
    let mut s = Sim::new(5);
    s.settle(20_000);
    s.run(30_000);
    assert_eq!(s.pushes, 0);
}

/// A peer both polled and polling is one peer for check-quorum, not two.
/// A leader that only hears one of four peers — by both routes — has two
/// of five, short of a quorum of three, and steps down.
#[test]
fn check_quorum_counts_a_peer_heard_both_ways_once() {
    let t = Timing::default();
    let routes = vec!["n1".into(), "n2".into(), "n3".into(), "n4".into()];
    let mut m = Mesh::new("n0".into(), routes, t, Hard::default(), 0, 7);
    // Win an election outright: pre-vote, then the real one, from n1..n3.
    let pre = m.tick(t.election_max_ms, 0).expect("campaigns");
    let mut real = None;
    for r in ["n1", "n2", "n3"] {
        if let Some(v) = m.on_vote_reply(&r.to_string(), &pre, VoteReply { term: 0, granted: true }, t.election_max_ms, 0) {
            real = Some(v);
        }
    }
    let real = real.expect("pre-vote won");
    for r in ["n1", "n2", "n3"] {
        m.on_vote_reply(&r.to_string(), &real, VoteReply { term: real.term, granted: true }, t.election_max_ms, 0);
    }
    assert!(m.is_leader());
    let peer = |term| Status { name: "n1".into(), account: String::new(), term, role: Role::Follower, leader: Some("n0".into()), head: 0, head_term: 0 };
    let mut now = t.election_max_ms;
    for _ in 0..20 {
        now += 500;
        m.on_status(&"n1".to_string(), peer(real.term), now);
        m.on_inbound(peer(real.term), now);
        m.tick(now, 0);
    }
    assert!(!m.is_leader(), "one peer heard twice is still one peer");
}

/// Push targets: a peer whose head keeps moving is pulling; one that
/// sits still behind us is not.
#[test]
fn push_targets_names_only_the_stuck_peer() {
    let mut s = Sim::new(3);
    let l = s.settle(20_000);
    let stuck = (l + 1) % 3;
    for b in 0..3 {
        s.link[stuck][b] = false;
    }
    let before = s.pushes;
    s.run(20_000);
    assert!(s.pushes > before, "the stuck peer was pushed to");
    let other = (l + 2) % 3;
    let (now, head) = (s.now, s.heads[l]);
    let targets = s.nodes[l].push_targets(now, head);
    assert!(!targets.contains(&name(other)), "a pulling peer is never a target: {targets:?}");
}
