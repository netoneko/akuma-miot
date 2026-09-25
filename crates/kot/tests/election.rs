//! A mock election over real HTTP: three `kot` nodes in one process, on
//! localhost, with fast timers. `miot-mesh`'s own tests cover the election
//! logic on a simulated network. This test covers the wiring: role changes
//! actually start and stop the block loop, a replica forwards `/submit`, a
//! new primary is reconciled against, and a killed primary comes back as a
//! follower whose log converges.

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

/// Every call in this file signs and connects as root (seed 1) — mTLS
/// pinning now gates the connection itself, not just the header envelope,
/// so the test's `reqwest::Client` needs root's identity too, not just its
/// header signatures.
fn trusted_client() -> reqwest::Client {
    let root = Identity::from_seed(&[1; 32]);
    let trusted: Vec<_> = (1..=5u8).map(|n| Identity::from_seed(&[n; 32]).account()).collect();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .use_preconfigured_tls(kot::tls::client_config(&root, trusted))
        .build()
        .unwrap()
}

fn cfg(who: u8, name: &str, port: u16, peers: Vec<String>, db: std::path::PathBuf) -> NodeConfig {
    let seed = |n: u8| Identity::from_seed(&[n; 32]).account();
    NodeConfig {
        name: name.into(),
        // `who` matches this node's own position in the `1..=5` seeds
        // `roster` draws from below, so its signed mesh traffic verifies
        // against its own genesis.
        identity: Identity::from_seed(&[who; 32]),
        bind: "127.0.0.1".into(),
        port,
        db,
        peers,
        root: seed(1),
        leader: seed(2),
        roster: (1..=5u8).map(|n| (format!("cat{n}"), seed(n))).collect(),
        block_ms: 200,
        sync_ms: 100,
        poll_ms: 100,
        timing: miot_mesh::Timing { election_min_ms: 600, election_max_ms: 1200 },
        patrons: vec![],
        learner: false,
    }
}

struct Mesh3 {
    names: [&'static str; 3],
    ports: [u16; 3],
    dirs: Vec<tempfile::TempDir>,
    nodes: [Option<Running>; 3],
    /// A node whose routes to the others go nowhere: it can be called, but
    /// can't call anyone — a peer behind a NAT with no forwards.
    mute: Option<usize>,
}

impl Mesh3 {
    fn url(&self, i: usize) -> String {
        format!("https://127.0.0.1:{}", self.ports[i])
    }

    fn cfg(&self, i: usize) -> NodeConfig {
        let peers = (0..3)
            .filter(|&j| j != i)
            .map(|j| if self.mute == Some(i) { format!("https://127.0.0.1:{}", free_port()) } else { self.url(j) })
            .collect();
        cfg(i as u8 + 1, self.names[i], self.ports[i], peers, self.dirs[i].path().join("db"))
    }

    fn new() -> Self {
        Mesh3 {
            names: ["alpha", "beta", "gamma"],
            ports: [free_port(), free_port(), free_port()],
            dirs: (0..3).map(|_| tempfile::tempdir().unwrap()).collect(),
            nodes: [None, None, None],
            mute: None,
        }
    }

    async fn start_all(&mut self) {
        for i in 0..3 {
            self.nodes[i] = Some(node::start(self.cfg(i)).await.unwrap());
        }
    }

    async fn start() -> Self {
        let mut m = Self::new();
        m.start_all().await;
        m
    }

    async fn kill(&mut self, i: usize) {
        if let Some(r) = self.nodes[i].take() {
            r.abort();
        }
        // Let the aborted tasks drop, so the store's lock and the port are free.
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    async fn revive(&mut self, i: usize) {
        self.nodes[i] = Some(node::start(self.cfg(i)).await.unwrap());
    }

    /// The one live node producing blocks, once every other live node
    /// follows it.
    async fn primary(&self, within: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let mut producing = Vec::new();
            let mut following = Vec::new();
            for (i, n) in self.nodes.iter().enumerate() {
                let Some(n) = n else { continue };
                let g = n.shared.lock().await;
                if g.is_producing() {
                    producing.push(i);
                } else {
                    following.push((i, g.mesh().leader().map(str::to_string)));
                }
            }
            if let [p] = producing[..] {
                if following.iter().all(|(_, l)| l.as_deref() == Some(self.names[p])) {
                    return p;
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "no stable primary: producing={producing:?} following={following:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Every live node holds the same log and the same state. Heads may lag
    /// by a block in flight, so compare each follower's whole range against
    /// the primary's prefix, then the state once heads match.
    async fn converged(&self, within: Duration) {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let mut views = Vec::new();
            for n in self.nodes.iter().flatten() {
                let mut g = n.shared.lock().await;
                views.push((g.head(), g.state_fingerprint()));
            }
            let same_head = views.windows(2).all(|w| w[0].0 == w[1].0);
            let same_state = views.windows(2).all(|w| w[0].1 == w[1].1);
            if same_head && same_state {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "never converged: heads {:?}", views.iter().map(|v| v.0).collect::<Vec<_>>());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn blocks(&self, i: usize, http: &reqwest::Client) -> Vec<String> {
        // `/chain/blocks` is mesh-internal now (docs/MESH_AUTH.md) — sign as
        // root (a trusted genesis account) to read it the way a real peer
        // would, not the way `kot`'s own client ever does.
        let root = Identity::from_seed(&[1; 32]);
        let mut out = Vec::new();
        let mut from = 1;
        loop {
            let query = format!("from={from}&limit=256");
            let rows: Vec<serde_json::Value> = http
                .get(format!("{}/chain/blocks?{query}", self.url(i)))
                .headers(node::sign_headers(&root, query.as_bytes()))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if rows.is_empty() {
                return out;
            }
            from += rows.len() as u64;
            out.extend(rows.into_iter().map(|r| r["body_hex"].as_str().unwrap().to_string()));
        }
    }
}

/// Sign `call` as dev seed 1 (root) and submit it to `node`, retrying
/// while the mesh has no primary (a 503 during an election is expected).
async fn submit(http: &reqwest::Client, node: &str, call: RuntimeCall) {
    let root = Identity::from_seed(&[1; 32]);
    for _ in 0..50 {
        // `/meta` and `/account` are client-facing but still gated
        // (`require_client_auth`) — sign as root, a trusted genesis account,
        // same as any real `kot` client would.
        let v: serde_json::Value =
            http.get(format!("{node}/meta")).headers(node::sign_headers(&root, b"")).send().await.unwrap().json().await.unwrap();
        let meta = client::Meta {
            genesis_hash: H256::from_slice(&hex::decode(v["genesis_hash"].as_str().unwrap()).unwrap()),
            spec_version: v["spec_version"].as_u64().unwrap() as u32,
            tx_version: v["tx_version"].as_u64().unwrap() as u32,
        };
        let acct = miot_keys::to_hex(&root.account());
        let n: serde_json::Value = http
            .get(format!("{node}/account/{acct}"))
            .headers(node::sign_headers(&root, b""))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let Some(nonce) = n["nonce"].as_u64() else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let uxt = client::sign(&root, call.clone(), nonce as u32, &meta);
        let r = http.post(format!("{node}/submit")).body(uxt.encode()).send().await.unwrap();
        if r.status().is_success() {
            return;
        }
        let body = r.text().await.unwrap_or_default();
        assert!(body.contains("no primary") || body.contains("unreachable"), "submit refused for real: {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("submit never landed");
}

fn open(text: &str) -> RuntimeCall {
    RuntimeCall::Litter(pallet_litter::Call::open { text: text.into() })
}

async fn task_texts(http: &reqwest::Client, node: &str) -> Vec<String> {
    let root = Identity::from_seed(&[1; 32]);
    let rows: Vec<serde_json::Value> =
        http.get(format!("{node}/tasks")).headers(node::sign_headers(&root, b"")).send().await.unwrap().json().await.unwrap();
    rows.iter().filter_map(|t| t["text"].as_str().map(str::to_string)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mesh_of_three_survives_losing_its_primary() {
    let http = trusted_client();
    let mut m = Mesh3::start().await;

    // 1. One primary, elected, not configured.
    let first = m.primary(Duration::from_secs(15)).await;
    let follower = (first + 1) % 3;

    // 2. A write sent to a *replica* lands, via forwarding, and replicates.
    submit(&http, &m.url(follower), open("before the kill")).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    m.converged(Duration::from_secs(10)).await;
    for i in 0..3 {
        assert!(task_texts(&http, &m.url(i)).await.contains(&"before the kill".to_string()));
    }

    // 3. Kill the primary. The other two (a quorum of 2 of 3) elect one of
    // themselves, and it keeps producing past where the old one stopped.
    let head_at_kill = m.nodes[first].as_ref().unwrap().shared.lock().await.head();
    m.kill(first).await;
    let second = m.primary(Duration::from_secs(15)).await;
    assert_ne!(second, first);
    let survivor = (0..3).find(|&i| i != first && i != second).unwrap();
    submit(&http, &m.url(survivor), open("after the kill")).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    m.converged(Duration::from_secs(10)).await;
    assert!(m.nodes[second].as_ref().unwrap().shared.lock().await.head() > head_at_kill);

    // 4. The old primary comes back on its old store. It must rejoin as a
    // follower of the new one, not produce again, and converge on the new
    // one's log, rewinding whatever it made that nobody pulled.
    m.revive(first).await;
    let still = m.primary(Duration::from_secs(15)).await;
    assert_eq!(still, second, "a returning node must not depose a healthy primary");
    m.converged(Duration::from_secs(15)).await;
    let texts = task_texts(&http, &m.url(first)).await;
    assert!(texts.contains(&"before the kill".to_string()) && texts.contains(&"after the kill".to_string()), "{texts:?}");

    // Byte-for-byte: the returned node's log is the primary's prefix.
    let theirs = m.blocks(second, &http).await;
    let ours = m.blocks(first, &http).await;
    assert!(!ours.is_empty() && theirs.starts_with(&ours[..ours.len().min(theirs.len())]), "logs diverge");

    for i in 0..3 {
        m.kill(i).await;
    }
}

/// A mesh of one (no peers) elects itself at once and behaves like the old
/// single primary did — the local-dev case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mesh_of_one_produces_on_its_own() {
    let http = trusted_client();
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let r = node::start(cfg(1, "solo", port, vec![], dir.path().join("db"))).await.unwrap();
    let url = format!("https://127.0.0.1:{port}");
    submit(&http, &url, open("alone")).await;
    // `submit` now acks as soon as it's queued (mempool_round applies it
    // once this node wins its own election, `node.rs::mempool_round`'s
    // `Route::Here` arm) rather than only after `Route::Here` is already
    // true — a faster ack, but it no longer doubles as "wait for this node
    // to become primary" the way blocking-until-success incidentally did.
    // Poll for both explicitly instead of trusting a fixed sleep to outlast
    // one election timeout (600-1200ms).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if r.shared.lock().await.is_producing() && r.shared.lock().await.head() > 0 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "solo node never became primary and applied its own submit");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(task_texts(&http, &url).await, vec!["alone".to_string()]);
    r.abort();
}

/// The roster a node was started with is genesis state, served by name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_genesis_roster_is_on_chain() {
    let http = trusted_client();
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let r = node::start(cfg(1, "solo", port, vec![], dir.path().join("db"))).await.unwrap();
    let rows: Vec<serde_json::Value> = http
        .get(format!("https://127.0.0.1:{port}/roster"))
        .headers(node::sign_headers(&Identity::from_seed(&[1; 32]), b""))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["cat1", "cat2", "cat3", "cat4", "cat5"]);
    assert_eq!(rows[2]["account"].as_str().unwrap(), miot_keys::to_hex(&Identity::from_seed(&[3; 32]).account()));
    r.abort();
}

/// A block log remembers its genesis. Reopened under a different roster —
/// a new chain — the node refuses instead of replaying the old one, which
/// the chain's own (constant) genesis hash could never catch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_refuses_a_different_genesis() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let r = node::start(cfg(1, "solo", free_port(), vec![], db.clone())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    r.abort();
    drop(r); // `Running` holds the node, and so the store's lock
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut renamed = cfg(1, "solo", free_port(), vec![], db.clone());
    renamed.roster[4].0 = "yuki".into();
    let err = node::start(renamed).await.err().expect("a relabeled roster is a different genesis");
    assert!(err.contains("different genesis"), "{err}");

    // The same genesis still opens.
    let r = node::start(cfg(1, "solo", free_port(), vec![], db)).await.unwrap();
    r.abort();
}

/// The AWS pair's case, 2026-09-24: a member that everyone can call and
/// that can call no one. Pull can't reach it, so before status went both
/// ways and the primary pushed, it followed nobody. Here it also starts with
/// a log of its own — blocks and a task nobody else has, made while it ran
/// alone — so the primary has to find the fork from its side and tell it to
/// rewind before the blocks it pushes can land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_cannot_call_out_follows_by_push() {
    let http = trusted_client();
    let mut m = Mesh3::new();

    // gamma alone first, on its own store: a log nobody else will have.
    {
        let solo = node::start(cfg(3, "gamma", m.ports[2], vec![], m.dirs[2].path().join("db"))).await.unwrap();
        submit(&http, &m.url(2), open("gamma alone")).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(solo.shared.lock().await.head() > 0);
        solo.abort();
        drop(solo);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    m.mute = Some(2);
    m.start_all().await;

    // `primary` waits until every live node follows one producer —
    // gamma included, which only a push can make happen.
    let p = m.primary(Duration::from_secs(20)).await;
    assert_ne!(p, 2, "gamma can't reach anyone, so it can't collect votes");
    assert_eq!(m.nodes[2].as_ref().unwrap().shared.lock().await.mesh().leader_route(), None, "gamma has no route: push only");

    submit(&http, &m.url(p), open("pushed to gamma")).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    m.converged(Duration::from_secs(20)).await;
    let texts = task_texts(&http, &m.url(2)).await;
    assert!(texts.contains(&"pushed to gamma".to_string()), "{texts:?}");
    assert!(!texts.contains(&"gamma alone".to_string()), "its own fork was rewound: {texts:?}");
    let theirs = m.blocks(p, &http).await;
    let ours = m.blocks(2, &http).await;
    assert!(!ours.is_empty() && theirs.starts_with(&ours[..ours.len().min(theirs.len())]), "gamma's log is the primary's prefix");

    // And it keeps up afterwards, not just once.
    let head_then = m.nodes[p].as_ref().unwrap().shared.lock().await.head();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    m.converged(Duration::from_secs(10)).await;
    assert!(m.nodes[2].as_ref().unwrap().shared.lock().await.head() > head_then);

    for i in 0..3 {
        m.kill(i).await;
    }
}
