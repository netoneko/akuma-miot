//! The operator's client when a node goes away mid-session — the REPL
//! refused every line while the one node it knew restarted, and the
//! operator retyped it until the node came back (2026-09-25). Now it fails
//! over to the node's peers, and a write nobody can take is queued and sent
//! once a node answers.

use kot::client::{Client, Sent};
use kot::common::Roster;
use kot::node::{self, NodeConfig, Running};
use miot_keys::Identity;
use miot_runtime::RuntimeCall;
use polkadot_sdk::*;
use std::time::Duration;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn acct(n: u8) -> miot_runtime::AccountId {
    Identity::from_seed(&[n; 32]).account()
}

fn cfg(who: u8, name: &str, port: u16, peers: Vec<String>, db: std::path::PathBuf) -> NodeConfig {
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
        patrons: vec![],
        learner: false,
    }
}

fn url(port: u16) -> String {
    format!("https://127.0.0.1:{port}")
}

fn open(text: &str) -> RuntimeCall {
    RuntimeCall::Litter(pallet_litter::Call::open { text: text.into() })
}

/// Signed as root, like the operator.
async fn client(node: &str) -> Client {
    Client::connect(vec![node.to_string()], Identity::from_seed(&[1; 32]), Roster::default()).await.unwrap()
}

async fn has_task(node: &str, text: &str, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if let Ok(mut c) = Client::connect(vec![node.to_string()], Identity::from_seed(&[1; 32]), Roster::default()).await {
            if c.tasks_text().await.contains(text) {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_on_one_node_fails_over_to_its_peers() {
    let ports = [free_port(), free_port(), free_port()];
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let names = ["alpha", "beta", "gamma"];
    let mut nodes: Vec<Option<Running>> = Vec::new();
    for i in 0..3 {
        let peers = (0..3).filter(|&j| j != i).map(|j| url(ports[j])).collect();
        nodes.push(Some(node::start(cfg(i as u8 + 1, names[i], ports[i], peers, dirs[i].path().join("db"))).await.unwrap()));
    }
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Told about alpha only, as `--node` with no MIOT_NODES.
    let mut c = client(&url(ports[0])).await;
    nodes[0].take().unwrap().abort();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Alpha is gone; the write goes through a peer it learned from alpha.
    match c.submit_or_queue(open("after alpha died")).await {
        Ok(Sent::Now) => {}
        Ok(Sent::Queued) => panic!("queued, though two nodes were up"),
        Err(e) => panic!("refused: {e}"),
    }
    assert_ne!(c.node, url(ports[0]));
    assert!(has_task(&url(ports[1]), "after alpha died", Duration::from_secs(10)).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_with_no_node_up_is_queued_and_lands_when_one_returns() {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let solo = || cfg(1, "solo", port, vec![], dir.path().join("db"));
    let running = node::start(solo()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut c = client(&url(port)).await;
    running.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Nobody to take it: queued, not refused, and the call returns.
    match c.submit_or_queue(open("typed while down")).await {
        Ok(Sent::Queued) => {}
        Ok(Sent::Now) => panic!("sent with the only node down"),
        Err(e) => panic!("refused instead of queued: {e}"),
    }

    // A node answers on that address again; the queue delivers it. A fresh
    // store, not the old one: in-process, the aborted node's open HTTP/2
    // connections (this client's pool) keep its store locked, which a real
    // restart — a process exit — never does.
    let dir2 = tempfile::tempdir().unwrap();
    let _back = node::start(cfg(1, "solo", port, vec![], dir2.path().join("db"))).await.unwrap();
    assert!(has_task(&url(port), "typed while down", Duration::from_secs(20)).await, "the queued write never landed");
}
