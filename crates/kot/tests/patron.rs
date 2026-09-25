//! A patron over real HTTP: three members and one node outside the
//! roster (`--patron`), which the members list in `--patrons`. It keeps
//! up with the chain through a change of primary, reads like a member, and
//! can't vote, push or write. `miot-mesh`'s tests cover the learner's
//! election logic; this covers the node wiring — TLS, the header gates, and
//! a patron's status staying out of the members' election.

use codec::Encode;
use kot::node::{self, NodeConfig, Running};
use miot_keys::Identity;
use miot_runtime::{client, RuntimeCall};
use polkadot_sdk::*;
use sp_core::H256;
use std::time::Duration;

const FRIEND: u8 = 9;
const STRANGER: u8 = 8;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn acct(n: u8) -> miot_runtime::AccountId {
    Identity::from_seed(&[n; 32]).account()
}

/// A client that presents seed `who`'s key in the handshake and pins the
/// members (and the friend's node) as servers.
fn client_as(who: u8) -> reqwest::Client {
    let trusted: Vec<_> = (1..=5u8).chain([FRIEND]).map(acct).collect();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .use_preconfigured_tls(kot::tls::client_config(&Identity::from_seed(&[who; 32]), trusted))
        .build()
        .unwrap()
}

fn cfg(who: u8, name: &str, port: u16, peers: Vec<String>, db: std::path::PathBuf, learner: bool) -> NodeConfig {
    NodeConfig {
        name: name.into(),
        identity: Identity::from_seed(&[who; 32]),
        bind: "127.0.0.1".into(),
        port,
        db,
        peers,
        root: acct(1),
        leader: acct(2),
        roster: (1..=5u8).map(|n| (format!("cat{n}"), acct(n))).collect(),
        block_ms: 200,
        sync_ms: 100,
        poll_ms: 100,
        timing: miot_mesh::Timing { election_min_ms: 600, election_max_ms: 1200 },
        patrons: if learner { vec![] } else { vec![("friend".into(), acct(FRIEND))] },
        learner,
    }
}

/// Members 0..3, the friend's node at index 3.
struct Litter {
    ports: [u16; 4],
    dirs: Vec<tempfile::TempDir>,
    nodes: [Option<Running>; 4],
    /// The members the friend can reach — like AWS, the only ones facing
    /// the internet. The others' routes go nowhere from its network.
    friend_reaches: Vec<usize>,
}

const NAMES: [&str; 4] = ["alpha", "beta", "gamma", "friend"];

impl Litter {
    fn url(&self, i: usize) -> String {
        format!("https://127.0.0.1:{}", self.ports[i])
    }

    fn cfg(&self, i: usize) -> NodeConfig {
        // Members list each other, never the friend. The friend lists them
        // all, but only the ones it can reach have a working address.
        let peers = if i == 3 {
            (0..3).map(|j| if self.friend_reaches.contains(&j) { self.url(j) } else { format!("https://127.0.0.1:{}", free_port()) }).collect()
        } else {
            (0..3).filter(|&j| j != i).map(|j| self.url(j)).collect()
        };
        let who = if i == 3 { FRIEND } else { i as u8 + 1 };
        cfg(who, NAMES[i], self.ports[i], peers, self.dirs[i].path().join("db"), i == 3)
    }

    /// The members only; the friend starts once we know who leads.
    async fn start() -> Self {
        let mut l = Litter {
            ports: [free_port(), free_port(), free_port(), free_port()],
            dirs: (0..4).map(|_| tempfile::tempdir().unwrap()).collect(),
            nodes: [None, None, None, None],
            friend_reaches: vec![0, 1, 2],
        };
        for i in 0..3 {
            l.nodes[i] = Some(node::start(l.cfg(i)).await.unwrap());
        }
        l
    }

    async fn start_friend(&mut self, reaches: Vec<usize>) {
        self.friend_reaches = reaches;
        self.nodes[3] = Some(node::start(self.cfg(3)).await.unwrap());
    }

    async fn kill(&mut self, i: usize) {
        if let Some(r) = self.nodes[i].take() {
            r.abort();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    /// The one member producing, once the other live members follow it.
    async fn primary(&self, within: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let mut producing = Vec::new();
            let mut following = Vec::new();
            for (i, n) in self.nodes.iter().enumerate().take(3) {
                let Some(n) = n else { continue };
                let g = n.shared.lock().await;
                if g.is_producing() {
                    producing.push(i);
                } else {
                    following.push(g.mesh().leader().map(str::to_string));
                }
            }
            if let [p] = producing[..] {
                if following.iter().all(|l| l.as_deref() == Some(NAMES[p])) {
                    return p;
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "no stable primary: producing={producing:?} following={following:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The friend's node holds the primary's log and state, and knows who
    /// leads — whoever it actually pulls from.
    async fn friend_caught_up(&self, p: usize, within: Duration) {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let (ph, ps) = {
                let mut g = self.nodes[p].as_ref().unwrap().shared.lock().await;
                (g.head(), g.state_fingerprint())
            };
            let (fh, fs, leader, producing) = {
                let mut g = self.nodes[3].as_ref().unwrap().shared.lock().await;
                (g.head(), g.state_fingerprint(), g.mesh().leader().map(str::to_string), g.is_producing())
            };
            assert!(!producing, "a patron never produces");
            if fh == ph && fs == ps && leader.as_deref() == Some(NAMES[p]) {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "friend never caught up: head {fh} vs {ph}, following {leader:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Sign `call` as seed `who` and submit it to `node`, retrying through an
/// election. Returns the final status and body.
async fn submit_as(who: u8, node: &str, call: RuntimeCall) -> (reqwest::StatusCode, String) {
    let http = client_as(who);
    let id = Identity::from_seed(&[who; 32]);
    for _ in 0..50 {
        let v: serde_json::Value =
            http.get(format!("{node}/meta")).headers(node::sign_headers(&id, b"")).send().await.unwrap().json().await.unwrap();
        let meta = client::Meta {
            genesis_hash: H256::from_slice(&hex::decode(v["genesis_hash"].as_str().unwrap()).unwrap()),
            spec_version: v["spec_version"].as_u64().unwrap() as u32,
            tx_version: v["tx_version"].as_u64().unwrap() as u32,
        };
        let a = miot_keys::to_hex(&id.account());
        let n: serde_json::Value =
            http.get(format!("{node}/account/{a}")).headers(node::sign_headers(&id, b"")).send().await.unwrap().json().await.unwrap();
        let Some(nonce) = n["nonce"].as_u64() else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let uxt = client::sign(&id, call.clone(), nonce as u32, &meta);
        let r = http.post(format!("{node}/submit")).body(uxt.encode()).send().await.unwrap();
        let status = r.status();
        let body = r.text().await.unwrap_or_default();
        if body.contains("no primary") || body.contains("unreachable") {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        return (status, body);
    }
    panic!("submit never got an answer from a primary");
}

fn open(text: &str) -> RuntimeCall {
    RuntimeCall::Litter(pallet_litter::Call::open { text: text.into() })
}

async fn task_texts(who: u8, node: &str) -> Vec<String> {
    let id = Identity::from_seed(&[who; 32]);
    let rows: Vec<serde_json::Value> =
        client_as(who).get(format!("{node}/tasks")).headers(node::sign_headers(&id, b"")).send().await.unwrap().json().await.unwrap();
    rows.iter().filter_map(|t| t["text"].as_str().map(str::to_string)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_patron_keeps_up_reads_and_never_votes_or_writes() {
    let mut l = Litter::start().await;
    let friend = l.url(3);

    // 1. The members elect among themselves. The friend can reach only the
    //    two replicas (the primary is behind a router, like home is) and
    //    still holds the primary's log, pulled from a replica.
    let first = l.primary(Duration::from_secs(15)).await;
    l.start_friend((0..3).filter(|&j| j != first).collect()).await;
    let (status, body) = submit_as(1, &l.url((first + 1) % 3), open("before the kill")).await;
    assert!(status.is_success(), "{body}");
    l.friend_caught_up(first, Duration::from_secs(10)).await;

    // 2. It reads like a member: on its own node, signed as itself, and on
    //    a member's.
    assert!(task_texts(FRIEND, &friend).await.contains(&"before the kill".to_string()));
    assert!(task_texts(FRIEND, &l.url(first)).await.contains(&"before the kill".to_string()));

    // 3. The members' election never counted it: quorum is still 2 of 3.
    {
        let g = l.nodes[first].as_ref().unwrap().shared.lock().await;
        assert_eq!(g.mesh().quorum(), 2);
        assert!(g.mesh().heard().keys().all(|n| n != "friend"), "a patron's status never reaches the mesh");
    }

    // 4. Kill the primary. The two left elect one of themselves (the friend
    //    can't: it never campaigns), and the friend follows the new one.
    l.kill(first).await;
    let second = l.primary(Duration::from_secs(15)).await;
    assert_ne!(second, first);
    let (status, body) = submit_as(1, &l.url(second), open("after the kill")).await;
    assert!(status.is_success(), "{body}");
    l.friend_caught_up(second, Duration::from_secs(10)).await;
    assert!(task_texts(FRIEND, &friend).await.contains(&"after the kill".to_string()));

    // 5. It can't write: its account has no standing on chain, so the
    //    primary refuses what it signs — whether sent to a member or
    //    forwarded by its own node.
    for node in [l.url(second), friend.clone()] {
        let (status, body) = submit_as(FRIEND, &node, open("from the friend")).await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }

    // 6. It can't vote or push: those gates are members-only.
    let id = Identity::from_seed(&[FRIEND; 32]);
    let vote = serde_json::to_vec(&miot_mesh::VoteRequest { term: 1_000, candidate: "friend".into(), head: 1_000, head_term: 1_000, pre: false }).unwrap();
    let r = client_as(FRIEND)
        .post(format!("{}/mesh/vote", l.url(second)))
        .headers(node::sign_headers(&id, &vote))
        .header("content-type", "application/json")
        .body(vote)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED);
    let r = client_as(FRIEND).post(format!("{}/chain/push", l.url(second))).headers(node::sign_headers(&id, b"{}")).body("{}").send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 7. A key nobody listed doesn't get past the handshake.
    assert!(client_as(STRANGER).get(format!("{}/mesh/status", l.url(second))).send().await.is_err());
}
