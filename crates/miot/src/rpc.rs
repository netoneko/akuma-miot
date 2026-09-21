//! `--rpc` — talk to a **real** `miot-node` instead of an in-process chain.
//!
//! Everything else in this binary (`--live`, `--chat`, the default scripted
//! demo) drives its own `TestExternalities` in this process, the way
//! `miot-sim` always has. This module is the one path that signs a real
//! [`miot_runtime::UncheckedExtrinsic`] and POSTs it to a node over HTTP —
//! the same wire `miot-cat` uses, reused rather than re-derived
//! (`miot_runtime::client::sign` is the shared piece).
//!
//! # What this is not
//!
//! There is no OpenSSH **private**-key loading here. `miot-keys` only ever
//! reads a *public* key line (`account_from_ssh`) — deriving an account, not
//! a signing capability — because parsing an OpenSSH private key is its own
//! security-sensitive piece of work that hasn't been built. `--seed` is a
//! deterministic ed25519 seed (a small int, `[n; 32]`, or 64 hex chars),
//! exactly what `miot-cat`'s `MIOT_SEED` accepts. Signing as the *real*
//! operator key still means running this against a node whose
//! `MIOT_ROOT_PUBKEY`/`MIOT_ROOT` was set to match a seed you actually hold,
//! same as any other account here — there is no special root path.

use codec::Encode;
use miot_keys::Identity;
use miot_runtime::{client, AccountId, RuntimeCall};
use polkadot_sdk::*;
use sp_core::H256;

fn parse_seed(spec: &str) -> [u8; 32] {
    if let Ok(n) = spec.parse::<u8>() {
        return [n; 32];
    }
    let bytes = hex::decode(spec.trim_start_matches("0x"))
        .unwrap_or_else(|_| panic!("seed {spec:?} is neither a small int nor 64 hex chars"));
    bytes.try_into().unwrap_or_else(|_| panic!("seed {spec:?} is not 32 bytes"))
}

/// `name=seed,name=seed,...` — the same spelling `MIOT_ROSTER` uses, resolved
/// client-side only. A tag never goes on the wire as a name; it is a lookup
/// against this, same as `docs/CLI.md` §2 describes.
fn resolve(roster: &str, name: &str) -> Option<AccountId> {
    let name = name.trim().trim_start_matches('@').to_ascii_lowercase();
    roster.split(',').find_map(|p| {
        let (n, seed) = p.split_once('=')?;
        (n.trim() == name).then(|| Identity::from_seed(&parse_seed(seed.trim())).account())
    })
}

async fn meta(http: &reqwest::Client, node: &str) -> client::Meta {
    let v: serde_json::Value = http
        .get(format!("{node}/meta"))
        .send()
        .await
        .expect("node unreachable (meta)")
        .json()
        .await
        .expect("bad /meta response");
    let genesis_hash = H256::from_slice(&hex::decode(v["genesis_hash"].as_str().unwrap()).unwrap());
    client::Meta {
        genesis_hash,
        spec_version: v["spec_version"].as_u64().unwrap() as u32,
        tx_version: v["tx_version"].as_u64().unwrap() as u32,
    }
}

async fn nonce(http: &reqwest::Client, node: &str, who: &AccountId) -> u32 {
    let v: serde_json::Value = http
        .get(format!("{node}/account/{}", miot_keys::to_hex(who)))
        .send()
        .await
        .expect("node unreachable (nonce)")
        .json()
        .await
        .expect("bad /account response");
    v["nonce"].as_u64().unwrap_or(0) as u32
}

async fn submit(http: &reqwest::Client, node: &str, identity: &Identity, call: RuntimeCall) {
    let m = meta(http, node).await;
    let n = nonce(http, node, &identity.account()).await;
    let uxt = client::sign(identity, call, n, &m);
    let r = http.post(format!("{node}/submit")).body(uxt.encode()).send().await.expect("submit failed");
    if r.status().is_success() {
        println!("  submitted, signed as {}", miot_keys::short(&identity.account()));
    } else {
        let e: serde_json::Value = r.json().await.unwrap_or_default();
        println!("  refused: {}", e.get("error").unwrap_or(&e));
    }
}

/// Print the log from `since` forward, block/who/what — the same shape the
/// scripted demo and `--chat` already print, so `--rpc` output reads like
/// every other mode here rather than inventing a fourth spelling of it.
async fn watch(http: &reqwest::Client, node: &str, since: u64, seconds: u64) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut cursor = since;
    while tokio::time::Instant::now() < deadline {
        let batch: Vec<serde_json::Value> = match http.get(format!("{node}/events?since={cursor}")).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        for e in &batch {
            cursor = cursor.max(e["seq"].as_u64().unwrap_or(cursor));
            println!("  {DIM}block {}{OFF}  {}", e["block"], e["effect"], DIM = "\x1b[2m", OFF = "\x1b[0m");
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// One-shot: sign, submit, watch the log for a few seconds, exit.
///
/// `--rpc <url> --identity-seed <seed> [--roster name=seed,...] (--open
/// "<text>" | --say "<body>" [--to <name>])`.
pub async fn run(node: &str, seed: &str, roster: &str, open: Option<&str>, say: Option<&str>, to: Option<&str>) {
    let identity = Identity::from_seed(&parse_seed(seed));
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().unwrap();

    println!("  {DIM}rpc {node} — signed as {}{OFF}", miot_keys::short(&identity.account()), DIM = "\x1b[2m", OFF = "\x1b[0m");

    let since = match http.get(format!("{node}/head")).send().await {
        Ok(r) => r.json::<serde_json::Value>().await.ok().and_then(|h| h["seq"].as_u64()).unwrap_or(0),
        Err(_) => 0,
    };

    if let Some(text) = open {
        submit(&http, node, &identity, RuntimeCall::Litter(pallet_litter::Call::open { text: text.to_string() })).await;
    } else if let Some(body) = say {
        let to = to.and_then(|n| resolve(roster, n));
        if let Some(name) = to.as_ref() {
            println!("  {DIM}→ {}{OFF}", miot_keys::short(name), DIM = "\x1b[2m", OFF = "\x1b[0m");
        }
        submit(&http, node, &identity, RuntimeCall::Litter(pallet_litter::Call::say { to, body: body.to_string() })).await;
    } else {
        println!("  nothing to do — pass --open \"<text>\" or --say \"<body>\"");
        return;
    }

    watch(&http, node, since, 8).await;
}
