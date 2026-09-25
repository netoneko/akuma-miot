//! Live activity over real HTTP: two `kot` nodes in one process. A cat's
//! record, POSTed to its own node, reaches the other node on the mesh status
//! exchange — in both directions even when one node can't call out at all
//! (a push-only node, like the AWS pair) — and only a node's own cat may
//! post it.

use kot::activity::{Activity, Flight, Seen};
use kot::node::{self, NodeConfig, Running};
use miot_keys::Identity;
use std::time::Duration;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn id(n: u8) -> Identity {
    Identity::from_seed(&[n; 32])
}

/// Connects as root (seed 1); what it signs with is chosen per request.
fn client() -> reqwest::Client {
    let trusted: Vec<_> = (1..=5u8).map(|n| id(n).account()).collect();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .use_preconfigured_tls(kot::tls::client_config(&id(1), trusted))
        .build()
        .unwrap()
}

fn cfg(who: u8, port: u16, peers: Vec<String>, db: std::path::PathBuf) -> NodeConfig {
    NodeConfig {
        name: format!("cat{who}"),
        identity: id(who),
        bind: "127.0.0.1".into(),
        port,
        db,
        peers,
        root: id(1).account(),
        leader: id(2).account(),
        roster: (1..=5u8).map(|n| (format!("cat{n}"), id(n).account())).collect(),
        block_ms: 200,
        sync_ms: 100,
        poll_ms: 100,
        timing: miot_mesh::Timing { election_min_ms: 600, election_max_ms: 1200 },
    }
}

/// Two nodes, cat2 and cat3. `cat2_mute`: cat2's route to cat3 goes
/// nowhere, so cat2 can be called but can't call — only cat3's polls
/// connect them.
async fn pair(cat2_mute: bool) -> (Vec<Running>, [String; 2], Vec<tempfile::TempDir>) {
    let ports = [free_port(), free_port()];
    let url = |p: u16| format!("https://127.0.0.1:{p}");
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let to3 = if cat2_mute { url(free_port()) } else { url(ports[1]) };
    let a = node::start(cfg(2, ports[0], vec![to3], dirs[0].path().join("db"))).await.unwrap();
    let b = node::start(cfg(3, ports[1], vec![url(ports[0])], dirs[1].path().join("db"))).await.unwrap();
    (vec![a, b], [url(ports[0]), url(ports[1])], dirs)
}

fn record(name: &str, phase: &str) -> Activity {
    let now = kot::activity::unix_ms();
    Activity {
        name: name.into(),
        model: "fake".into(),
        phase: phase.into(),
        since: now - 5_000,
        why: "[block 7] root said to the litter: build it".into(),
        turns: 3,
        running: vec![Flight { id: 4, tool: "Bash".into(), arg: "$ cargo build".into(), since: now - 4_000, output: 1234, last_output: now - 100 }],
        ok: 5,
        failed: 1,
        at: now,
        ..Default::default()
    }
}

async fn post(http: &reqwest::Client, node: &str, signer: &Identity, a: &Activity) -> reqwest::StatusCode {
    let body = serde_json::to_vec(a).unwrap();
    http.post(format!("{node}/activity")).headers(node::sign_headers(signer, &body)).body(body).send().await.unwrap().status()
}

async fn get(http: &reqwest::Client, node: &str) -> Vec<Seen> {
    http.get(format!("{node}/activity")).headers(node::sign_headers(&id(1), b"")).send().await.unwrap().json().await.unwrap()
}

/// Wait until `node` serves a record from `who` in `phase`.
async fn heard(http: &reqwest::Client, node: &str, who: u8, phase: &str) -> Seen {
    let account = miot_keys::to_hex(&id(who).account());
    let t0 = std::time::Instant::now();
    loop {
        if let Some(s) = get(http, node).await.into_iter().find(|s| s.account == account && s.activity.phase == phase) {
            return s;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "{node} never heard cat{who} {phase}: {:?}", get(http, node).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_cats_activity_reaches_the_other_node() {
    let (_nodes, [n2, n3], _dirs) = pair(false).await;
    let http = client();

    let sent = record("cat2", "thinking");
    assert_eq!(post(&http, &n2, &id(2), &sent).await, reqwest::StatusCode::NO_CONTENT);
    // Its own node serves it straight away, exactly as sent.
    let own = get(&http, &n2).await;
    assert_eq!(own.len(), 1);
    assert_eq!(own[0].activity, sent);

    // The other node hears it on the status exchange, whole.
    let s = heard(&http, &n3, 2, "thinking").await;
    assert_eq!(s.activity.running[0].arg, "$ cargo build");
    assert_eq!((s.activity.ok, s.activity.failed, s.activity.turns), (5, 1, 3));
    assert!(s.age_ms < 5_000, "{}", s.age_ms);
    assert!(s.activity.in_phase_ms(s.age_ms) >= 5_000, "phase time counts from the cat's own clock");

    // A newer record replaces it.
    post(&http, &n2, &id(2), &record("cat2", "idle")).await;
    heard(&http, &n3, 2, "idle").await;
}

#[tokio::test]
async fn activity_crosses_a_one_way_link_both_ways() {
    // cat2 can't call cat3. cat3's polls carry cat3's record to cat2, and
    // cat2's answers carry cat2's back.
    let (_nodes, [n2, n3], _dirs) = pair(true).await;
    let http = client();
    post(&http, &n2, &id(2), &record("cat2", "waiting")).await;
    post(&http, &n3, &id(3), &record("cat3", "thinking")).await;
    heard(&http, &n3, 2, "waiting").await;
    heard(&http, &n2, 3, "thinking").await;
}

#[tokio::test]
async fn only_a_nodes_own_cat_may_post_its_activity() {
    let (_nodes, [n2, _n3], _dirs) = pair(false).await;
    let http = client();
    // cat3 is a trusted member, but it isn't cat2.
    assert_eq!(post(&http, &n2, &id(3), &record("cat2", "thinking")).await, reqwest::StatusCode::UNAUTHORIZED);
    // Neither is root.
    assert_eq!(post(&http, &n2, &id(1), &record("cat2", "thinking")).await, reqwest::StatusCode::UNAUTHORIZED);
    assert!(get(&http, &n2).await.iter().all(|s| s.account != miot_keys::to_hex(&id(2).account())));
    // A body the signature doesn't cover is refused too.
    let body = serde_json::to_vec(&record("cat2", "thinking")).unwrap();
    let headers = node::sign_headers(&id(2), b"something else");
    let st = http.post(format!("{n2}/activity")).headers(headers).body(body).send().await.unwrap().status();
    assert_eq!(st, reqwest::StatusCode::UNAUTHORIZED);
}
