//! Patron approval on chain, end to end over real HTTP: three members, one
//! of them serving `httpapi` (plain HTTP, `crate::httpapi`). A stranger —
//! a key the mesh has never heard of — can't get past the mesh port's
//! handshake; it files a request through `httpapi`, root approves it, and
//! from the next block it reads through the pinned mesh port like a member,
//! talks through `httpapi`, approves a second stranger, and is locked out
//! again when root revokes it. Along the way: `httpapi`'s route and call
//! allowlists and its CSRF guard.

use codec::Encode;
use kot::node::{self, NodeConfig, Running};
use miot_keys::Identity;
use miot_runtime::{client, RuntimeCall};
use polkadot_sdk::*;
use sp_core::H256;
use std::time::Duration;

const ROOT: u8 = 1;
const STRANGER: u8 = 20;
const SECOND: u8 = 21;
const ORIGIN: &str = "https://teahouse.test";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn id(n: u8) -> Identity {
    Identity::from_seed(&[n; 32])
}

fn acct(n: u8) -> miot_runtime::AccountId {
    id(n).account()
}

fn hex_of(n: u8) -> String {
    miot_keys::to_hex(&acct(n))
}

/// A mesh-port client presenting seed `who`'s key, pinning the members.
/// A fresh one each call, so every check is a fresh handshake.
fn mesh_client(who: u8) -> reqwest::Client {
    let trusted: Vec<_> = (1..=3u8).map(acct).collect();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .use_preconfigured_tls(kot::tls::client_config(&id(who), trusted))
        .build()
        .unwrap()
}

fn plain() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap()
}

struct Litter {
    ports: [u16; 3],
    api: String,
    _dirs: Vec<tempfile::TempDir>,
    nodes: Vec<Running>,
}

impl Litter {
    async fn start() -> Self {
        let ports = [free_port(), free_port(), free_port()];
        let api_port = free_port();
        let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let mut nodes = Vec::new();
        for i in 0..3 {
            let who = i as u8 + 1;
            let cfg = NodeConfig {
                name: format!("cat{who}"),
                identity: id(who),
                bind: "127.0.0.1".into(),
                port: ports[i],
                db: dirs[i].path().join("db"),
                peers: (0..3).filter(|&j| j != i).map(|j| format!("https://127.0.0.1:{}", ports[j])).collect(),
                root: acct(1),
                leader: acct(2),
                roster: (1..=3u8).map(|n| (format!("cat{n}"), acct(n))).collect(),
                block_ms: 200,
                sync_ms: 100,
                poll_ms: 100,
                timing: miot_mesh::Timing { election_min_ms: 600, election_max_ms: 1200 },
                patrons: vec![],
                learner: false,
                httpapi_listen: (i == 0).then(|| format!("127.0.0.1:{api_port}")),
                httpapi_origins: vec![ORIGIN.into()],
            };
            nodes.push(node::start(cfg).await.unwrap());
        }
        Litter { ports, api: format!("http://127.0.0.1:{api_port}"), _dirs: dirs, nodes }
    }

    fn mesh(&self, i: usize) -> String {
        format!("https://127.0.0.1:{}", self.ports[i])
    }

    /// Wait until some member produces, so writes have somewhere to land.
    async fn settle(&self) {
        for _ in 0..200 {
            for n in &self.nodes {
                if n.shared.lock().await.is_producing() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no primary");
    }
}

/// A signed GET on the mesh port as `who`: `Ok(status)`, or `Err` if the
/// handshake itself was refused.
async fn mesh_get(who: u8, url: &str) -> Result<reqwest::StatusCode, String> {
    let r = mesh_client(who).get(url).headers(node::sign_headers(&id(who), b"")).send().await.map_err(|e| e.to_string())?;
    Ok(r.status())
}

/// Sign `call` as `who` and submit it — to a mesh port (`https://…`, over
/// mTLS) or to `httpapi` (`http://…`, with its CSRF header). Retries through
/// an election or a stale nonce.
async fn submit(who: u8, base: &str, call: RuntimeCall) -> (reqwest::StatusCode, String) {
    let over_api = base.starts_with("http://");
    let http = if over_api { plain() } else { mesh_client(who) };
    let signer = id(who);
    for _ in 0..60 {
        let meta: serde_json::Value = match http.get(format!("{base}/meta")).headers(node::sign_headers(&signer, b"")).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => serde_json::Value::Null,
        };
        let nonce: serde_json::Value = match http.get(format!("{base}/account/{}", hex_of(who))).headers(node::sign_headers(&signer, b"")).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => serde_json::Value::Null,
        };
        let (Some(g), Some(nonce)) = (meta["genesis_hash"].as_str(), nonce["nonce"].as_u64()) else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let meta = client::Meta {
            genesis_hash: H256::from_slice(&hex::decode(g).unwrap()),
            spec_version: meta["spec_version"].as_u64().unwrap() as u32,
            tx_version: meta["tx_version"].as_u64().unwrap() as u32,
        };
        let uxt = client::sign(&signer, call.clone(), nonce as u32, &meta);
        let mut req = http.post(format!("{base}/submit")).body(uxt.encode());
        if over_api {
            req = req.header("x-miot-request", "1").header("content-type", "application/octet-stream");
        }
        let r = req.send().await.unwrap();
        let status = r.status();
        let body = r.text().await.unwrap_or_default();
        // Accepted — applied, or queued ("pending", whose note can itself
        // say "no primary") — is an answer; only a refusal is retried.
        if accepted(&body) {
            return (status, body);
        }
        if body.contains("no primary") || body.contains("unreachable") || body.contains("Stale") || body.contains("Future") {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        return (status, body);
    }
    panic!("submit never got an answer");
}

/// `who` asks to be a patron through `httpapi`.
async fn request(api: &str, who: u8, name: &str, note: &str) -> (reqwest::StatusCode, String) {
    let g: serde_json::Value = plain().get(format!("{api}/genesis")).send().await.unwrap().json().await.unwrap();
    let domain: [u8; 32] = hex::decode(g["request_domain"].as_str().unwrap()).unwrap().try_into().unwrap();
    let message = (g["request_context"].as_str().unwrap().as_bytes(), domain, name, note).encode();
    let sig = id(who).sign(&message);
    let body = serde_json::json!({"who": hex_of(who), "name": name, "note": note, "sig": hex::encode(sig.0)});
    for _ in 0..60 {
        let r = plain().post(format!("{api}/patron-request")).header("x-miot-request", "1").json(&body).send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        if accepted(&text) {
            return (status, text);
        }
        if text.contains("no primary") || text.contains("Stale") {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        return (status, text);
    }
    panic!("request never got an answer");
}

fn accepted(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body).is_ok_and(|v| v["ok"] == true)
}

async fn status_of(api: &str, who: u8) -> String {
    let v: serde_json::Value = plain().get(format!("{api}/patron/{}", hex_of(who))).send().await.unwrap().json().await.unwrap();
    v["status"].as_str().unwrap_or("").to_string()
}

async fn until(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..150 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn say(to: u8, body: &str) -> RuntimeCall {
    RuntimeCall::Litter(pallet_litter::Call::say { to: Some(acct(to)), body: body.into(), no_ack: false, off_record: false })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stranger_requests_is_approved_talks_and_is_revoked() {
    let l = Litter::start().await;
    l.settle().await;
    let api = l.api.clone();
    let mesh = l.mesh(0);

    // A key nobody knows can't even finish the handshake on the mesh port.
    assert!(mesh_get(STRANGER, &format!("{mesh}/head")).await.is_err(), "a stranger got past the mesh port's handshake");
    assert_eq!(status_of(&api, STRANGER).await, "none");

    // The request door: CSRF shape first.
    let r = plain().post(format!("{api}/patron-request")).json(&serde_json::json!({})).send().await.unwrap();
    assert_eq!(r.status(), 403, "a POST without the CSRF header is refused");
    let r = plain().get(format!("{api}/genesis")).header("origin", "https://evil.test").send().await.unwrap();
    assert_eq!(r.status(), 403, "a foreign Origin is refused");
    let r = plain().get(format!("{api}/genesis")).header("origin", ORIGIN).send().await.unwrap();
    assert_eq!(r.status(), 200, "the allowed Origin gets through");

    // The request itself, with its note.
    let (code, body) = request(&api, STRANGER, "neobeav", "Kirill's friend, wants to watch").await;
    assert!(code.is_success(), "request refused: {code} {body}");
    until("the request is pending", async || status_of(&api, STRANGER).await == "pending").await;

    // Root approves it over the pinned mesh port.
    let approve = RuntimeCall::Litter(pallet_litter::Call::approve_patron { who: acct(STRANGER) });
    let (code, body) = submit(ROOT, &mesh, approve).await;
    assert!(code.is_success(), "approve refused: {code} {body}");
    until("approved", async || status_of(&api, STRANGER).await == "approved").await;

    // Now it's a reader on the mesh port — every member, fresh handshakes.
    for i in 0..3 {
        let url = format!("{}/head", l.mesh(i));
        until("the patron reads through the mesh port", async || mesh_get(STRANGER, &url).await == Ok(reqwest::StatusCode::OK)).await;
    }

    // It talks through httpapi…
    let (code, body) = submit(STRANGER, &api, say(3, "hello from outside")).await;
    assert!(code.is_success(), "a patron's say via httpapi was refused: {code} {body}");
    // …but a member verb is refused at the door — even from root.
    let open = RuntimeCall::Litter(pallet_litter::Call::open { text: "a task".into() });
    let (code, _) = submit(ROOT, &api, open.clone()).await;
    assert_eq!(code, 403, "httpapi must not take `open`, whoever signs");
    let clear = RuntimeCall::Litter(pallet_litter::Call::clear_all {});
    assert_eq!(submit(ROOT, &api, clear).await.0, 403);
    // And on chain too: through the mesh port, a patron's `open` is refused.
    let (code, body) = submit(STRANGER, &mesh, open).await;
    assert!(!code.is_success() && body.contains("NotAuthorized"), "a patron opened a task: {code} {body}");

    // No consensus routes on httpapi.
    for path in ["/chain/blocks", "/chain/head", "/mesh/status", "/mesh/peers", "/mempool/relay"] {
        let r = plain().get(format!("{api}{path}")).send().await.unwrap();
        assert_eq!(r.status(), 404, "{path} must not exist on httpapi");
    }

    // A patron approves the next one.
    let (code, body) = request(&api, SECOND, "otter", "friend of neobeav").await;
    assert!(code.is_success(), "{code} {body}");
    until("second pending", async || status_of(&api, SECOND).await == "pending").await;
    let approve2 = RuntimeCall::Litter(pallet_litter::Call::approve_patron { who: acct(SECOND) });
    let (code, body) = submit(STRANGER, &api, approve2).await;
    assert!(code.is_success(), "a patron couldn't approve another: {code} {body}");
    until("second approved", async || status_of(&api, SECOND).await == "approved").await;

    // Root revokes the first: locked out of the mesh port again.
    let revoke = RuntimeCall::Litter(pallet_litter::Call::revoke_patron { who: acct(STRANGER) });
    let (code, body) = submit(ROOT, &mesh, revoke).await;
    assert!(code.is_success(), "revoke refused: {code} {body}");
    until("the revoked patron is refused", async || mesh_get(STRANGER, &format!("{mesh}/head")).await.is_err()).await;
    // The one it approved stays.
    assert_eq!(status_of(&api, SECOND).await, "approved");
    // And the revoked key's reads through httpapi are refused too.
    let r = plain().get(format!("{api}/head")).headers(node::sign_headers(&id(STRANGER), b"")).send().await.unwrap();
    assert_eq!(r.status(), 401, "a revoked patron still reads through httpapi");

    for n in l.nodes {
        n.abort();
    }
}
