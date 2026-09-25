//! Who produces blocks.
//!
//! Leader election for the mesh of `kot` nodes, as a pure state machine: no
//! clock, no I/O. `kot`'s node wraps it in HTTP; the tests here wrap it in a
//! simulated network with a fake clock, partitions and kills. Same split as
//! `miot-tasks` — the thing that has to be right is testable in milliseconds.
//!
//! # Two leaders, two words
//!
//! `pallet-litter`'s `leader` is the *litter* leader, an agent role (who
//! plans). This crate elects the **mesh leader**: which node's block log is
//! canonical and which one accepts `/submit`. They are unrelated axes. In the
//! node this crate's leader is still called the *primary*, and everyone
//! else a *replica* (HANDOFF item 5's words); here it is Raft's vocabulary,
//! because that is what this is.
//!
//! # What this is and isn't
//!
//! Raft's **election** only — terms, one vote per term, a majority quorum,
//! randomized timeouts — plus the two extensions that make it behave on a
//! real network (Raft thesis §9.6, §4.2.3):
//!
//! - **Pre-vote.** A node that stops hearing a leader first asks "would you
//!   vote for me?" *without* bumping its term. Only a majority yes starts a
//!   real election. A node on the wrong side of a partition therefore never
//!   inflates its term, and never deposes a healthy leader when it rejoins.
//! - **Leader stickiness + check-quorum.** A node that heard from a live
//!   leader within the minimum election timeout refuses to vote at all; a
//!   leader that can't see a majority for that long steps down on its own.
//!   That is how *"that peer is unreachable"* is told apart from *"that peer
//!   lost the election"*: an unreachable leader stops counting after one
//!   timeout, a live one is never voted against.
//!
//! **Not** Raft's log replication. Blocks move by the replica pulling the
//! leader's log over `/chain/blocks`, and disagreement is still resolved by
//! `miot-store`'s *leader wins, back to the last compaction*. Election only
//! decides who the leader is. So there's no commit index.
//!
//! # Reachability that only goes one way
//!
//! Pull assumes every follower can call its leader. A node behind someone
//! else's NAT can't (the AWS pair, reaching home through a router with no
//! forwards, 2026-09-24): it heard nothing, followed nobody, and sat as a
//! pre-candidate forever while home, which *could* call it, never said a
//! thing it could use. Two additions cover that without changing who
//! decides anything:
//!
//! - **Status goes both ways.** The poller sends its own [`Status`] with the
//!   poll, and the polled node feeds it to [`Mesh::on_inbound`]. A leader's
//!   poll is then a heartbeat in both directions. A peer heard only this way
//!   has no route from here, so it is keyed by name ([`Mesh::heard`]).
//! - **The leader pushes to a peer that's stuck.** [`Mesh::push_targets`]
//!   names every peer whose head differs from the leader's and hasn't moved
//!   for an election window: it isn't pulling. The node then sends it
//!   blocks; the receiver takes them only from the leader it follows, in
//!   the current term ([`Mesh::accepts_push_from`]). A block the
//! leader produced but no follower pulled before the leader died is lost to
//! the rewind, the same "records, not work" loss `miot-store`'s docs
//! already accept.
//!
//! The one piece of log replication this does borrow is the up-to-date
//! check: a vote goes only to a candidate whose log is at least as far along
//! as the voter's, compared as `(head_term, head)`. `head_term` is the term
//! the node was in when it last appended a block. It's what stops an
//! ex-leader that kept producing on a minority partition, and so has a
//! *higher* head, from winning with blocks the majority never saw.
//!
//! One operator's trusted swarm: nobody lies about their term or head.
//! Nothing here defends against a member that does.
//!
//! # Learners
//!
//! A node outside the genesis roster can still follow the chain: a
//! **learner** ([`Mesh::learner`], Raft's non-voting member). It polls and
//! follows like anyone else, but never campaigns and never votes, so it
//! can't lead and can't sway who does. The members on the other side keep
//! it out of the election too: it isn't in their routes, so it's not in
//! their quorum, and `kot`'s node never feeds its status to [`Mesh`] at all.
//! Why it exists: friends of the operator who want to watch the chain from
//! a different network without a new genesis (2026-09-25).
//!
//! A learner on another network usually can't reach the leader at all,
//! only whichever members face the internet, and nobody pushes to it. So it
//! doesn't insist on the leader: [`Mesh::pull_sources`] names every member
//! it can reach that is in the current term and follows a leader, the
//! leader first, then furthest along. A replica's log is the leader's, a
//! pull interval behind, and a rewind reaches it the same way it reaches
//! the replica.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A node's name — `--as`. What a status or a vote is signed off with.
pub type Name = String;

/// How *this* node reaches a peer: its URL from here. Peers are keyed by
/// route, not name, because two nodes can reach the same peer by different
/// addresses (the Firecracker guests are behind NAT), and because a peer's
/// name is only learned once it answers.
pub type Route = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Follower,
    /// Asking for pre-votes. Term not bumped yet.
    PreCandidate,
    Candidate,
    Leader,
}

/// What must survive a restart, or a node could vote twice in one term.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hard {
    pub term: u64,
    pub voted_for: Option<Name>,
    /// The term this node was in when it last appended a block. See the
    /// module doc's up-to-date check.
    pub head_term: u64,
}

/// What a node says about itself on `/mesh/status`. Everyone polls everyone;
/// a leader's status *is* its heartbeat. Pull rather than push so each node
/// only needs its own outbound routes, which is what NAT allows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub name: Name,
    /// Opaque to this crate — whatever the caller's `status()` was given.
    /// `kot` puts its hex `AccountId` here so a client can resolve `name`
    /// (a mesh/routing label, not necessarily human-facing) to a roster
    /// name for display, without `miot-mesh` knowing what an account is.
    pub account: String,
    pub term: u64,
    pub role: Role,
    pub leader: Option<Name>,
    pub head: u64,
    pub head_term: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoteRequest {
    pub term: u64,
    pub candidate: Name,
    pub head: u64,
    pub head_term: u64,
    /// A pre-vote: "would you?", changes no state on either side.
    pub pre: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoteReply {
    pub term: u64,
    pub granted: bool,
}

/// Election timeouts, in milliseconds. A follower that hears nothing from a
/// leader for a random time in `[min, max]` starts a pre-vote. The spread is
/// what keeps two nodes from splitting the vote every time.
///
/// These are network timeouts, not LLM-turn timeouts. The block loop and the
/// agent loop never wait on an election, so there's no reason to size them
/// to a turn the way `miot-runtime`'s task timers are.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub election_min_ms: u64,
    pub election_max_ms: u64,
}

impl Default for Timing {
    fn default() -> Self {
        // Status polls run every second (kot's default), so a leader has to
        // miss about four in a row before anyone moves. `kot run` defaults
        // to 10-20 s for the real network; the tests keep these.
        Timing { election_min_ms: 4_000, election_max_ms: 8_000 }
    }
}

pub struct Mesh {
    name: Name,
    routes: Vec<Route>,
    timing: Timing,
    hard: Hard,
    hard_dirty: bool,
    role: Role,
    leader: Option<Name>,
    leader_route: Option<Route>,
    last_leader_contact: Option<u64>,
    leader_since: u64,
    deadline: u64,
    rng: u64,
    /// Routes that granted in the current campaign, pre or real.
    votes: BTreeSet<Route>,
    /// The term the current pre-vote asks for (`hard.term + 1` when it began).
    campaign_term: u64,
    /// Last successful status per route, and when.
    seen: BTreeMap<Route, (u64, Status)>,
    /// Last status each peer *sent* us (a poll of ours, a push), by name,
    /// and when. The only record of a peer we have no working route to.
    heard: BTreeMap<Name, (u64, Status)>,
    /// Leader only: per route, a head that differs from ours and when that
    /// head was first seen. Cleared when the peer matches us again.
    lagging: BTreeMap<Route, (u64, u64)>,
    /// Leader only: routes that have needed a push this term. They get one
    /// whenever they differ, without waiting out another window.
    pushed: BTreeSet<Route>,
    /// Never campaigns, never votes. See the module doc's "Learners".
    learner: bool,
}

impl Mesh {
    /// `routes` are the *other* members, as this node reaches them. `seed`
    /// only spreads election timeouts; any per-node value will do.
    pub fn new(name: Name, routes: Vec<Route>, timing: Timing, hard: Hard, now: u64, seed: u64) -> Self {
        let mut m = Mesh {
            name,
            routes,
            timing,
            hard,
            hard_dirty: false,
            role: Role::Follower,
            leader: None,
            leader_route: None,
            last_leader_contact: None,
            leader_since: 0,
            deadline: 0,
            // xorshift must not start at zero.
            rng: seed | 1,
            votes: BTreeSet::new(),
            campaign_term: 0,
            seen: BTreeMap::new(),
            heard: BTreeMap::new(),
            lagging: BTreeMap::new(),
            pushed: BTreeSet::new(),
            learner: false,
        };
        m.reset_deadline(now);
        m
    }

    /// This node only follows: it never campaigns and never votes, so it
    /// never leads — not even alone, with no routes.
    pub fn learner(mut self) -> Self {
        self.learner = true;
        self
    }
    pub fn is_learner(&self) -> bool {
        self.learner
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn role(&self) -> Role {
        self.role
    }
    pub fn term(&self) -> u64 {
        self.hard.term
    }
    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }
    /// Who leads, by name — `None` during an election.
    pub fn leader(&self) -> Option<&str> {
        self.leader.as_deref()
    }
    /// How to reach the leader from here. `None` if this node leads, or
    /// nobody does.
    pub fn leader_route(&self) -> Option<&str> {
        self.leader_route.as_deref()
    }
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }
    /// Last status seen per route, and when. For `kot peers`.
    pub fn seen(&self) -> &BTreeMap<Route, (u64, Status)> {
        &self.seen
    }
    /// Last status each peer sent us, by name, and when. For `kot peers`.
    pub fn heard(&self) -> &BTreeMap<Name, (u64, Status)> {
        &self.heard
    }
    /// Members needed to win, this node included.
    pub fn quorum(&self) -> usize {
        (self.routes.len() + 1) / 2 + 1
    }
    pub fn hard(&self) -> &Hard {
        &self.hard
    }
    /// `Some` once after every change to [`Hard`]. The caller persists it
    /// *before* answering anything else. A vote that's forgotten on restart
    /// can be cast twice.
    pub fn take_dirty(&mut self) -> Option<Hard> {
        std::mem::take(&mut self.hard_dirty).then(|| self.hard.clone())
    }

    pub fn status(&self, head: u64, account: &str) -> Status {
        Status {
            name: self.name.clone(),
            account: account.to_string(),
            term: self.hard.term,
            role: self.role,
            leader: self.leader.clone(),
            head,
            head_term: self.hard.head_term,
        }
    }

    /// This node just appended a block, as leader or from the leader it
    /// follows. Either way, in the current term.
    pub fn appended(&mut self) {
        if self.hard.head_term != self.hard.term {
            self.hard.head_term = self.hard.term;
            self.hard_dirty = true;
        }
    }

    /// Advance the clock. Returns a vote request to broadcast if this node
    /// just started campaigning.
    pub fn tick(&mut self, now: u64, head: u64) -> Option<VoteRequest> {
        if self.role == Role::Leader {
            // Check-quorum. A leader that can't reach a majority can't tell
            // that they haven't already moved on, so it stops claiming to lead.
            if self.quorum() > 1 && now >= self.leader_since + self.timing.election_min_ms {
                // By name: a peer we both poll and are polled by is one
                // peer, not two.
                let fresh: BTreeSet<&str> = self
                    .seen
                    .values()
                    .chain(self.heard.values())
                    .filter(|(at, st)| now.saturating_sub(*at) <= self.timing.election_min_ms && st.term <= self.hard.term)
                    .map(|(_, st)| st.name.as_str())
                    .collect();
                if fresh.len() + 1 < self.quorum() {
                    self.step_down(now);
                }
            }
            return None;
        }
        if self.learner || now < self.deadline {
            return None;
        }
        self.campaign(now, head)
    }

    fn campaign(&mut self, now: u64, head: u64) -> Option<VoteRequest> {
        self.reset_deadline(now);
        self.leader = None;
        self.leader_route = None;
        self.votes.clear();
        if self.quorum() == 1 {
            // Alone: nobody to ask.
            self.hard.term += 1;
            self.hard.voted_for = Some(self.name.clone());
            self.hard_dirty = true;
            self.become_leader(now);
            return None;
        }
        self.role = Role::PreCandidate;
        self.campaign_term = self.hard.term + 1;
        Some(VoteRequest {
            term: self.campaign_term,
            candidate: self.name.clone(),
            head,
            head_term: self.hard.head_term,
            pre: true,
        })
    }

    fn become_leader(&mut self, now: u64) {
        self.lagging.clear();
        self.pushed.clear();
        self.role = Role::Leader;
        self.leader = Some(self.name.clone());
        self.leader_route = None;
        self.leader_since = now;
        self.last_leader_contact = None;
        self.votes.clear();
    }

    fn step_down(&mut self, now: u64) {
        self.role = Role::Follower;
        self.leader = None;
        self.leader_route = None;
        self.votes.clear();
        self.reset_deadline(now);
    }

    fn adopt_term(&mut self, term: u64, now: u64) {
        if term > self.hard.term {
            self.hard.term = term;
            self.hard.voted_for = None;
            self.hard_dirty = true;
        }
        self.step_down(now);
    }

    fn up_to_date(&self, head_term: u64, head: u64, my_head: u64) -> bool {
        (head_term, head) >= (self.hard.head_term, my_head)
    }

    /// A peer's `/mesh/status` answered. The leader's answer is its heartbeat.
    pub fn on_status(&mut self, from: &Route, st: Status, now: u64) {
        if st.name == self.name {
            // Our own route in the peer list. Drop it rather than let it
            // count toward the quorum, so one peers list can be copied to
            // every host.
            self.routes.retain(|r| r != from);
            return;
        }
        self.seen.insert(from.clone(), (now, st.clone()));
        self.observe(Some(from), st, now);
    }

    /// A peer told us its status by calling *us*: sent with its own poll,
    /// or with a push. Same effect as polling it, except that we may have
    /// no route to it — then it's known by name only, and if it leads, we
    /// follow it without a route to pull from (its pushes are the log).
    pub fn on_inbound(&mut self, st: Status, now: u64) {
        if st.name == self.name {
            return;
        }
        self.heard.insert(st.name.clone(), (now, st.clone()));
        // A route we've had an answer on, if any, is still the better way
        // to reach it.
        let route = self.seen.iter().find(|(_, (_, s))| s.name == st.name).map(|(r, _)| r.clone());
        if let Some(r) = route {
            self.seen.insert(r.clone(), (now, st.clone()));
            self.observe(Some(&r), st, now);
        } else {
            self.observe(None, st, now);
        }
    }

    /// What a peer's status means for us, however it arrived. `route` is how
    /// to reach it, if we can.
    fn observe(&mut self, route: Option<&Route>, st: Status, now: u64) {
        if st.term > self.hard.term {
            self.adopt_term(st.term, now);
        }
        if st.role == Role::Leader && st.term == self.hard.term && self.role != Role::Leader {
            self.role = Role::Follower;
            self.votes.clear();
            // Don't trade a route for none: a leader we can pull from that
            // also calls us is still one we can pull from.
            if self.leader.as_deref() != Some(st.name.as_str()) || route.is_some() {
                self.leader_route = route.cloned();
            }
            self.leader = Some(st.name);
            self.last_leader_contact = Some(now);
            self.reset_deadline(now);
        } else if self.leader.as_deref() == Some(st.name.as_str()) && st.role != Role::Leader {
            // Our leader stepped down. Keep the deadline running; don't reset it.
            self.leader = None;
            self.leader_route = None;
        } else if self.learner && self.leader.is_none() && st.term == self.hard.term && st.role == Role::Follower {
            // A learner that can't reach the leader still learns its name
            // from a member that follows it — for display; it pulls from
            // that member (`pull_sources`).
            self.leader = st.leader;
        }
    }

    /// Whether to take blocks pushed by the node whose status is `st`: only
    /// from the leader we follow, in our current term.
    pub fn accepts_push_from(&self, st: &Status) -> bool {
        self.role != Role::Leader
            && st.role == Role::Leader
            && st.term == self.hard.term
            && self.leader.as_deref() == Some(st.name.as_str())
    }

    /// Leader only: the routes to push blocks to, given our `head`. A peer
    /// qualifies when we can reach it (a fresh status), its head differs
    /// from ours, and that head hasn't moved for an election window. A peer
    /// that pulls catches up within a sync interval and never gets here.
    /// Once a peer has needed a push, it gets one whenever it differs, for
    /// the rest of this leadership: otherwise a peer that can only be
    /// pushed to would trail by a whole window every time.
    pub fn push_targets(&mut self, now: u64, head: u64) -> Vec<Route> {
        if self.role != Role::Leader {
            self.lagging.clear();
            self.pushed.clear();
            return Vec::new();
        }
        let window = self.timing.election_min_ms;
        let mut out = Vec::new();
        for (route, (at, st)) in &self.seen {
            if now.saturating_sub(*at) > window || st.head == head {
                self.lagging.remove(route);
                continue;
            }
            let e = self.lagging.entry(route.clone()).or_insert((st.head, now));
            if e.0 != st.head {
                *e = (st.head, now);
            }
            if self.pushed.contains(route) || now.saturating_sub(e.1) >= window {
                out.push(route.clone());
            }
        }
        self.pushed.extend(out.iter().cloned());
        out
    }

    /// Where to pull blocks from, best first. A member pulls only from the
    /// leader. A learner takes the leader if it can reach it, and otherwise
    /// any member it has heard from in the last election window that's in
    /// the current term and follows someone, furthest along first.
    pub fn pull_sources(&self, now: u64) -> Vec<Route> {
        if !self.learner {
            return self.leader_route.iter().cloned().collect();
        }
        let fresh = |at: u64| now.saturating_sub(at) <= self.timing.election_min_ms;
        let mut out: Vec<(bool, u64, Route)> = self
            .seen
            .iter()
            .filter(|(_, (at, st))| fresh(*at) && st.term == self.hard.term && st.leader.is_some())
            .map(|(r, (_, st))| (st.role == Role::Leader, st.head, r.clone()))
            .collect();
        out.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));
        out.into_iter().map(|(_, _, r)| r).collect()
    }

    /// Answer a vote request. `head` is this node's own.
    pub fn on_vote_request(&mut self, req: &VoteRequest, now: u64, head: u64) -> VoteReply {
        let deny = |m: &Self| VoteReply { term: m.hard.term, granted: false };
        if self.learner {
            return deny(self);
        }
        // Stickiness: never help depose a leader we can still hear, and a
        // leader never votes against itself. If it really is cut off,
        // check-quorum retires it on its own.
        let leader_alive = self.role == Role::Leader
            || self.last_leader_contact.is_some_and(|t| now < t + self.timing.election_min_ms) && self.leader.is_some();
        if leader_alive || req.candidate == self.name {
            return deny(self);
        }
        if req.pre {
            let granted = req.term > self.hard.term && self.up_to_date(req.head_term, req.head, head);
            return VoteReply { term: self.hard.term, granted };
        }
        if req.term < self.hard.term {
            return deny(self);
        }
        if req.term > self.hard.term {
            self.adopt_term(req.term, now);
        }
        let free = match &self.hard.voted_for {
            None => true,
            Some(c) => *c == req.candidate,
        };
        let granted = free && self.up_to_date(req.head_term, req.head, head);
        if granted {
            if self.hard.voted_for.as_deref() != Some(req.candidate.as_str()) {
                self.hard.voted_for = Some(req.candidate.clone());
                self.hard_dirty = true;
            }
            self.reset_deadline(now);
        }
        VoteReply { term: self.hard.term, granted }
    }

    /// A reply to a request this node sent. Returns the real vote request to
    /// broadcast if a pre-vote just succeeded.
    pub fn on_vote_reply(&mut self, from: &Route, req: &VoteRequest, reply: VoteReply, now: u64, head: u64) -> Option<VoteRequest> {
        if reply.term > self.hard.term {
            self.adopt_term(reply.term, now);
            return None;
        }
        if !reply.granted {
            return None;
        }
        if req.pre {
            if self.role != Role::PreCandidate || req.term != self.campaign_term {
                return None; // a stale campaign's answer
            }
            self.votes.insert(from.clone());
            if self.votes.len() + 1 >= self.quorum() {
                self.hard.term = self.campaign_term;
                self.hard.voted_for = Some(self.name.clone());
                self.hard_dirty = true;
                self.role = Role::Candidate;
                self.votes.clear();
                self.reset_deadline(now);
                return Some(VoteRequest {
                    term: self.hard.term,
                    candidate: self.name.clone(),
                    head,
                    head_term: self.hard.head_term,
                    pre: false,
                });
            }
        } else if self.role == Role::Candidate && req.term == self.hard.term {
            self.votes.insert(from.clone());
            if self.votes.len() + 1 >= self.quorum() {
                self.become_leader(now);
            }
        }
        None
    }

    fn reset_deadline(&mut self, now: u64) {
        // xorshift64. Only spreads timeouts; nothing depends on its quality.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let span = self.timing.election_max_ms.saturating_sub(self.timing.election_min_ms) + 1;
        self.deadline = now + self.timing.election_min_ms + self.rng % span;
    }
}

#[cfg(test)]
mod tests;
