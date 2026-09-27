//! Reference bytes for anything that builds a signed extrinsic outside
//! Rust — the web UI's `public/app.js` does (`docs/WEBUI.md`, "Sending").
//! Prints, for one `say`, the signed payload, its blake2-256, and the
//! extrinsic exactly as `client::sign` makes it, so a JS encoder can be
//! checked byte for byte instead of by whether a node happens to accept it.
//!
//!   cargo run -q -p miot-runtime --example tx_vectors -- <seed 64 hex> <nonce> <body> [to 64 hex]

use codec::Encode;
use miot_keys::Identity;
use miot_runtime::client::{sign, Meta};
use miot_runtime::{pallet_litter, RuntimeCall, SignedExtra};
use polkadot_sdk::*;
use sp_core::H256;
use sp_runtime::generic::{Era, SignedPayload};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let seed: [u8; 32] = unhex(&a[1]).try_into().expect("32-byte seed");
    let nonce: u32 = a[2].parse().expect("nonce");
    let body = a[3].clone();
    let to = a.get(4).map(|h| miot_keys::from_hex(h).expect("account"));
    let identity = Identity::from_seed(&seed);
    // The local dev chain's meta: genesis hash zero, spec 1, tx 1 (`GET /meta`).
    let meta = Meta { genesis_hash: H256::zero(), spec_version: 1, tx_version: 1 };

    let call = RuntimeCall::Litter(pallet_litter::Call::say { to, body, no_ack: false, off_record: false });
    let extra: SignedExtra = (
        frame_system::CheckGenesis::new(),
        frame_system::CheckSpecVersion::new(),
        frame_system::CheckTxVersion::new(),
        frame_system::CheckMortality::from(Era::immortal()),
        frame_system::CheckNonce::from(nonce),
    );
    let implicit = (meta.genesis_hash, meta.spec_version, meta.tx_version, meta.genesis_hash, ());
    let raw = SignedPayload::<RuntimeCall, SignedExtra>::from_raw(call.clone(), extra, implicit);
    let (call_bytes, extra_bytes, implicit_bytes) = (raw.deconstruct().0.encode(), (Era::immortal(), codec::Compact(nonce)).encode(), implicit.encode());
    let mut payload = call_bytes.clone();
    payload.extend(&extra_bytes);
    payload.extend(&implicit_bytes);
    let signed_over = if payload.len() > 256 { sp_io::hashing::blake2_256(&payload).to_vec() } else { payload.clone() };

    println!("account      {}", miot_keys::to_hex(&identity.account()));
    println!("call         {}", hex(&call_bytes));
    println!("extra        {}", hex(&extra_bytes));
    println!("implicit     {}", hex(&implicit_bytes));
    println!("payload_len  {}", payload.len());
    println!("signed_over  {}", hex(&signed_over));
    println!("blake2_256   {}", hex(&sp_io::hashing::blake2_256(&payload)));
    let xt = sign(&identity, call, nonce, &meta);
    println!("extrinsic    {}", hex(&xt.encode()));
}
