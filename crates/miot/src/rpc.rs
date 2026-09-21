//! `--rpc` — talk to a **real** `miot-node` instead of an in-process chain.
//!
//! Everything else in this binary (`--live`, `--chat`, the default scripted
//! demo) drives its own `TestExternalities` in this process, the way this
//! crate always has. This module is the one path that signs a real
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

/// `~/.akuma/miot/id_ed25519.seed` — named after the ssh convention it stands
/// in for, since this is the same idea: an identity you keep, not one you
/// retype. Not an OpenSSH key (see the module doc) — 64 hex chars, this
/// crate's own seed spelling.
fn identity_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    std::path::Path::new(&home).join(".akuma/miot/id_ed25519.seed")
}

/// The public half of [`identity_path`] — an `authorized_keys`-shaped line,
/// safe to read aloud or paste anywhere, since it's a public key. This is
/// what makes the persisted identity useful for more than signing:
/// `MIOT_ROOT_PUBKEY` on `miot-node` reads exactly this format
/// (`account_from_ssh`), so pasting this file's content there is how the
/// swarm learns *this* identity is the one with root.
fn pub_path() -> std::path::PathBuf {
    identity_path().with_file_name("id_ed25519.pub")
}

/// The persisted identity, creating one on first use.
///
/// Without this, every `--rpc` call with no `--identity-seed` would need a
/// fresh throwaway account — fine for one command, useless for "the same
/// swarm member every time." Generated with the system RNG (`getrandom`),
/// not a small int: this one is meant to be kept, not to double as a
/// human-readable placeholder like the `1,2,3,4,5` seed convention cats use.
///
/// Also (re)writes [`pub_path`] whenever it's missing or out of sync with the
/// seed, so the operator always has a fresh `MIOT_ROOT_PUBKEY` value to hand
/// the node — computing that from a private seed is not something anyone
/// should have to do by hand.
fn load_or_create_identity() -> Identity {
    let path = identity_path();
    let identity = if let Ok(text) = std::fs::read_to_string(&path) {
        Identity::from_seed(&parse_seed(text.trim()))
    } else {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("system RNG");
        std::fs::create_dir_all(path.parent().unwrap()).expect("create ~/.akuma/miot");
        std::fs::write(&path, hex::encode(seed)).expect("write identity seed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        println!(
            "  {DIM}new identity generated and saved to {}{OFF}",
            path.display(),
            DIM = "\x1b[2m",
            OFF = "\x1b[0m"
        );
        Identity::from_seed(&seed)
    };

    let pub_path = pub_path();
    let line = identity.ssh_public_line("miot-root");
    let stale = std::fs::read_to_string(&pub_path).map(|existing| existing.trim() != line.trim()).unwrap_or(true);
    if stale {
        std::fs::write(&pub_path, &line).expect("write public key");
        println!(
            "  {DIM}public key at {} — pass its content as MIOT_ROOT_PUBKEY to give this identity root{OFF}",
            pub_path.display(),
            DIM = "\x1b[2m",
            OFF = "\x1b[0m"
        );
    }

    identity
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
/// `--rpc <url> [--identity-seed <seed>] [--roster name=seed,...] (--open
/// "<text>" | --say "<body>" [--to <name>])`. Without `--identity-seed`,
/// signs as the persisted identity at `~/.akuma/miot/id_ed25519.seed`
/// (created on first use) — the same account every time, rather than a
/// fresh throwaway one per call.
pub async fn run(node: &str, seed: Option<&str>, roster: &str, open: Option<&str>, say: Option<&str>, to: Option<&str>) {
    let identity = match seed {
        Some(s) => Identity::from_seed(&parse_seed(s)),
        None => load_or_create_identity(),
    };
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
