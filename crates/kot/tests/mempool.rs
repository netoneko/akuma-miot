//! A mock of the AWS-pair scenario over real HTTP (`HANDOFF.md`, "One-way
//! reachability"; `node.rs::no_primary`): a node that knows who leads but
//! has no configured route to them any more shouldn't refuse a write
//! outright — it should queue it and relay to a peer it *can* reach, same
//! as a push-only follower today has no way to write at all while the
//! primary is on the side it can't call out to.
//!
//! Three members, quorum 2: alpha and beta elect a primary between
//! themselves first (deterministic — 2 of 3 roster members is already a
//! majority), so which one wins is observed rather than assumed. Both keep
//! a route to gamma (so whoever wins can still push it blocks — the primary
//! can always reach the AWS pair today, just not the other way), but gamma
//! joins with a route to whichever of the two lost, and none at all to the
//! primary. A submit straight to gamma has to cross that gap by relay
//! (`node.rs::mempool_round`) to land at all, and by push to sync back down
//! and be seen as sealed from gamma's own side.

use codec::Encode;
use kot::node::{self, NodeConfig, Running};
use miot_keys::Identity;
use miot_runtime::{client, RuntimeCall};
use polkadot_sdk::*;
use sp_core::H256;
use std::time::Duration;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// mTLS pinning gates the connection itself, not just header signatures —
/// see `election.rs`'s `trusted_client`.
fn trusted_client() -> reqwest::Client {
    let root = Identity::from_seed(&[1; 32]);
    let trusted: Vec<_> = (1..=3u8).map(|n| Identity::from_seed(&[n; 32]).account()).collect();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .use_preconfigured_tls(kot::tls::client_config(&root, trusted))
        .build()
        .unwrap()
}

/// A 3-member roster (quorum 2), so two live nodes already form a majority
/// — unlike `election.rs`'s shared 5-member `cfg`, which needs all three of
/// its own test's nodes just to reach quorum, leaving no room to bring a
/// third node up *after* the other two have already settled on a primary.
fn cfg(who: u8, name: &str, port: u16, peers: Vec<String>, db: std::path::PathBuf) -> NodeConfig {
    let seed = |n: u8| Identity::from_seed(&[n; 32]).account();
    NodeConfig {
        name: name.into(),
        identity: Identity::from_seed(&[who; 32]),
        bind: "127.0.0.1".into(),
        port,
        db,
        peers,
        root: seed(1),
        leader: seed(1),
        roster: (1..=3u8).map(|n| (format!("cat{n}"), seed(n))).collect(),
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

/// Poll until exactly one of the given nodes is producing and the other
/// follows it — same shape as `election.rs`'s `Mesh3::primary`, sized for
/// however many nodes are live right now rather than a fixed three.
async fn primary(nodes: &[&Running], within: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let mut producing = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            if n.shared.lock().await.is_producing() {
                producing.push(i);
            }
        }
        if let [p] = producing[..] {
            return p;
        }
        assert!(tokio::time::Instant::now() < deadline, "no stable primary among {} live node(s): producing={producing:?}", nodes.len());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn meta(http: &reqwest::Client, node: &str, as_who: &Identity) -> client::Meta {
    let v: serde_json::Value =
        http.get(format!("{node}/meta")).headers(node::sign_headers(as_who, b"")).send().await.unwrap().json().await.unwrap();
    client::Meta {
        genesis_hash: H256::from_slice(&hex::decode(v["genesis_hash"].as_str().unwrap()).unwrap()),
        spec_version: v["spec_version"].as_u64().unwrap() as u32,
        tx_version: v["tx_version"].as_u64().unwrap() as u32,
    }
}

async fn nonce(http: &reqwest::Client, node: &str, as_who: &Identity) -> u32 {
    let acct = miot_keys::to_hex(&as_who.account());
    let v: serde_json::Value =
        http.get(format!("{node}/account/{acct}")).headers(node::sign_headers(as_who, b"")).send().await.unwrap().json().await.unwrap();
    v["nonce"].as_u64().unwrap_or(0) as u32
}

async fn task_texts(http: &reqwest::Client, node: &str, as_who: &Identity) -> Vec<String> {
    let rows: Vec<serde_json::Value> =
        http.get(format!("{node}/tasks")).headers(node::sign_headers(as_who, b"")).send().await.unwrap().json().await.unwrap();
    rows.iter().filter_map(|t| t["text"].as_str().map(str::to_string)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_with_no_route_to_the_primary_relays_through_a_peer_it_can_reach() {
    let http = trusted_client();
    let root = Identity::from_seed(&[1; 32]);

    // alpha and beta, fully peered to each other and (already, even before
    // it's up) to gamma: 2 of 3 roster members is already quorum, so they
    // settle a primary on their own — gamma isn't live yet and can't
    // influence which one it is. Both get gamma's URL up front because
    // whichever wins needs a route *to* gamma to push it blocks later
    // (`Mesh::push_targets`) — the same shape home has to yuki/shiro today:
    // home can reach AWS, AWS can't reach home back.
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (port_a, port_b, port_g) = (free_port(), free_port(), free_port());
    let alpha = node::start(cfg(1, "alpha", port_a, vec![url(port_b), url(port_g)], dir_a.path().join("db"))).await.unwrap();
    let beta = node::start(cfg(2, "beta", port_b, vec![url(port_a), url(port_g)], dir_b.path().join("db"))).await.unwrap();

    let p = primary(&[&alpha, &beta], Duration::from_secs(10)).await;
    let f_port = if p == 0 { port_b } else { port_a };

    // gamma joins with a route to the loser only — none at all to whoever
    // just won. `route()` on gamma will always see `Route::Nobody(Some(_))`
    // for as long as that stays true: the same shape yuki/shiro are in
    // against a home primary today.
    let dir_g = tempfile::tempdir().unwrap();
    let gamma = node::start(cfg(3, "gamma", port_g, vec![url(f_port)], dir_g.path().join("db"))).await.unwrap();
    let gamma_url = url(port_g);

    // gamma has to learn who leads (via beta/alpha's status gossip) before
    // its own `/submit` can even name the primary in "pending"'s note.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!gamma.shared.lock().await.is_producing(), "gamma has no route to the primary; it can't have won leadership");

    let m = meta(&http, &gamma_url, &root).await;
    let n = nonce(&http, &gamma_url, &root).await;
    let call = RuntimeCall::Litter(pallet_litter::Call::open { text: "via gamma relay".into() });
    let uxt = client::sign(&root, call, n, &m);
    let r = http.post(format!("{gamma_url}/submit")).body(uxt.encode()).send().await.unwrap();
    assert!(r.status().is_success(), "gamma should queue it, not refuse it: {}", r.status());
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["status"], "pending", "{body:?}");
    let hash = body["tx_hash"].as_str().expect("tx_hash in the pending ack").to_string();

    // mempool_round (every poll_ms) should relay it to beta/alpha, get it
    // applied on the real primary, and sync the resulting block back down —
    // all three converge on the same task text.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let (a, b, g) = (
            task_texts(&http, &url(port_a), &root).await,
            task_texts(&http, &url(port_b), &root).await,
            task_texts(&http, &gamma_url, &root).await,
        );
        if a.contains(&"via gamma relay".to_string()) && b.contains(&"via gamma relay".to_string()) && g.contains(&"via gamma relay".to_string()) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never converged on the relayed task: alpha={a:?} beta={b:?} gamma={g:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // And gamma — the node that could never have applied it itself — can
    // say so when asked directly, by relaying the read the same way it
    // relayed the write.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let v: serde_json::Value = http
            .get(format!("{gamma_url}/tx/{hash}"))
            .headers(node::sign_headers(&root, b""))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if v["status"] == "sealed" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "tx never reported sealed via gamma: {v:?}");
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    alpha.abort();
    beta.abort();
    gamma.abort();
}
