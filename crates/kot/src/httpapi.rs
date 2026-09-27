//! `httpapi` — the node's plain-HTTP API for everything that isn't a node:
//! a browser, a would-be patron with no key the mesh knows yet.
//!
//! The mesh port is pinned mTLS: a handshake from a key outside the roster
//! (and the patron lists) never gets as far as HTTP. This listener is the
//! other door, and deliberately a narrow one:
//!
//! - **Plain HTTP, bound to loopback or a private bridge.** TLS is nginx's
//!   job in front of it, with a real certificate (`teahouse.akuma.sh/api/`
//!   on the AWS box) — a browser can't pin a node's self-signed key.
//! - **An allowlist of routes.** Anything not listed in [`router`] is a
//!   404 here: no `/chain/*`, no `/mesh/*`, no `/mempool/relay`, no
//!   `/activity` POST. Consensus never crosses this door.
//! - **Three routes need no auth at all**, and one of them writes: a patron
//!   request, which has to carry the requester's own signature and is
//!   rate-limited here and bounded on chain.
//! - **Reads are the mesh port's own handlers**, so they're gated the same
//!   way — the signed request headers (`x-miot-signer`/`x-miot-sig`),
//!   checked against `Node::is_reader`. A browser signs those with its own
//!   key (WebCrypto Ed25519); approval on chain is what makes it a reader.
//! - **`/submit` takes seven calls only** — `say`, `post`, `react`, `vote`,
//!   `approve_patron`, `reject_patron_request`, `revoke_patron` — whoever
//!   signs. Task verbs, clear/compact, stats and artifacts go through the
//!   mesh port or not at all.
//! - **CSRF:** there are no cookies, so no ambient credential to ride on;
//!   the guard is on the request's shape ([`csrf`]). A `POST` must carry
//!   [`CSRF_HEADER`] and a non-form content type, which a cross-site form
//!   can't send and a cross-site `fetch` can't send without a preflight this
//!   API never approves; a request that names an `Origin` must name an
//!   allowed one.
//!
//! `docs/HTTPAPI.md` is the reference; `docs/MESH_AUTH.md`, "Patron
//! approval and httpapi", says why it's shaped this way.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Request, State as AxState};
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use codec::Decode;
use miot_runtime::{RuntimeCall, UncheckedExtrinsic};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::node::{self, Shared};

/// Every `POST` must carry this, set to `1`.
pub const CSRF_HEADER: &str = "x-miot-request";
/// Patron requests accepted per [`REQUEST_WINDOW`], across all callers — on
/// top of the chain's own bound (`pallet_litter::MAX_PATRON_REQUESTS`
/// pending at once) and nginx's per-address limit in front.
pub const REQUESTS_PER_WINDOW: usize = 10;
pub const REQUEST_WINDOW: Duration = Duration::from_secs(60);
/// A patron request is a name and a 500-byte note; this is generous.
const REQUEST_BODY_MAX: usize = 4 * 1024;
/// A `post` is at most a 2 KB body plus its envelope.
const SUBMIT_BODY_MAX: usize = 16 * 1024;

/// What only this door keeps: who may call it, and how often a stranger
/// has.
struct Door {
    origins: Vec<String>,
    recent: Mutex<VecDeque<Instant>>,
}

/// The whole router — routes not listed here don't exist on this port.
/// `origins`: the `Origin` values a browser may call from (e.g.
/// `https://teahouse.akuma.sh`); a request with no `Origin` (curl, `kot`)
/// isn't a browser and isn't checked.
pub fn router(shared: Shared, origins: Vec<String>) -> Router {
    let door = Arc::new(Door { origins, recent: Mutex::new(VecDeque::new()) });
    Router::new()
        // Open.
        .route("/genesis", get(genesis))
        .route("/patron-request", post(patron_request).layer(DefaultBodyLimit::max(REQUEST_BODY_MAX)))
        .route("/patron/{who}", get(patron_status))
        // Readers — the mesh port's handlers, gated by signed headers.
        .route("/head", get(node::head))
        .route("/events", get(node::events))
        .route("/roster", get(node::roster))
        .route("/meta", get(node::meta))
        .route("/tasks", get(node::tasks))
        .route("/artifacts", get(node::all_artifacts))
        .route("/artifact/{id}", get(node::artifact))
        .route("/notes", get(node::standalone_artifacts))
        .route("/note/{id}", get(node::standalone_artifact))
        .route("/stats", get(node::all_stats))
        .route("/activity", get(node::activity_get))
        .route("/patrons", get(node::patrons))
        .route("/tx/{hash}", get(node::tx_status))
        .route("/account/{id}", get(node::account))
        // The one signed write, limited to talking and patron calls.
        .route("/submit", post(submit).layer(DefaultBodyLimit::max(SUBMIT_BODY_MAX)))
        .layer(middleware::from_fn(csrf))
        .layer(Extension(door))
        .with_state(shared)
}

/// Serve [`router`] on `listen` (plain HTTP) until the process ends.
pub async fn serve(shared: Shared, listen: &str, origins: Vec<String>) -> Result<tokio::task::JoinHandle<()>, String> {
    let tcp = tokio::net::TcpListener::bind(listen).await.map_err(|e| format!("httpapi: bind {listen}: {e}"))?;
    println!("[httpapi] plain HTTP on {listen}; browser origins: {}", if origins.is_empty() { "none".to_string() } else { origins.join(", ") });
    let app = router(shared, origins);
    Ok(tokio::spawn(async move {
        if let Err(e) = axum::serve(tcp, app).await {
            eprintln!("[httpapi] server ended: {e}");
        }
    }))
}

/// The CSRF guard — see the module docs.
async fn csrf(Extension(door): Extension<Arc<Door>>, req: Request, next: Next) -> Response {
    if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        if !door.origins.iter().any(|o| o == origin) {
            return refuse(StatusCode::FORBIDDEN, &format!("origin {origin} is not allowed"));
        }
    }
    if req.method() == Method::POST {
        if req.headers().get(CSRF_HEADER).and_then(|v| v.to_str().ok()) != Some("1") {
            return refuse(StatusCode::FORBIDDEN, &format!("a POST here needs the header {CSRF_HEADER}: 1"));
        }
        let ctype = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
        let essence = ctype.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        if essence != "application/json" && essence != "application/octet-stream" {
            return refuse(StatusCode::UNSUPPORTED_MEDIA_TYPE, "a POST here must be application/json or application/octet-stream");
        }
    }
    next.run(req).await
}

fn refuse(code: StatusCode, error: &str) -> Response {
    (code, Json(serde_json::json!({"ok": false, "error": error}))).into_response()
}

/// What a requester signs against: this chain's request domain, the exact
/// context string, and who's already in it by name.
async fn genesis(AxState(n): AxState<Shared>) -> Response {
    let mut n = n.lock().await;
    let (domain, roster) = n.with_state(|| {
        (pallet_litter::Pallet::<miot_runtime::Runtime>::request_domain(), pallet_litter::Pallet::<miot_runtime::Runtime>::roster())
    });
    Json(serde_json::json!({
        "request_domain": hex::encode(domain),
        "request_context": String::from_utf8_lossy(pallet_litter::PATRON_REQUEST_CONTEXT),
        "roster": roster.into_iter().map(|(name, _)| name).collect::<Vec<_>>(),
        "max_name": pallet_litter::MAX_PATRON_NAME,
        "max_note": pallet_litter::MAX_PATRON_NOTE,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct PatronRequestBody {
    /// The requester's account, 64 hex.
    who: String,
    name: String,
    note: String,
    /// ed25519 over `request_message(name, note)`, 128 hex.
    sig: String,
}

async fn patron_request(AxState(n): AxState<Shared>, Extension(door): Extension<Arc<Door>>, Json(body): Json<PatronRequestBody>) -> Response {
    let Ok(who) = miot_keys::from_hex(&body.who) else {
        return refuse(StatusCode::BAD_REQUEST, "who: not a 64-hex account");
    };
    let Some(sig) = hex::decode(body.sig.trim_start_matches("0x")).ok().and_then(|b| <[u8; 64]>::try_from(b).ok()) else {
        return refuse(StatusCode::BAD_REQUEST, "sig: not 128 hex (an ed25519 signature)");
    };
    {
        let mut recent = door.recent.lock().await;
        let now = Instant::now();
        while recent.front().is_some_and(|t| now.duration_since(*t) > REQUEST_WINDOW) {
            recent.pop_front();
        }
        if recent.len() >= REQUESTS_PER_WINDOW {
            return refuse(StatusCode::TOO_MANY_REQUESTS, "too many patron requests right now; try again in a minute");
        }
        recent.push_back(now);
    }
    node::carry_patron_request(&n, who, body.name, body.note, sig).await.into_response()
}

/// `none`, `pending` or `approved` — what a requester polls after asking.
async fn patron_status(AxState(n): AxState<Shared>, Path(who): Path<String>) -> Response {
    let Ok(who) = miot_keys::from_hex(&who) else {
        return refuse(StatusCode::BAD_REQUEST, "not a 64-hex account");
    };
    let mut n = n.lock().await;
    let status = n.with_state(|| {
        use pallet_litter::Pallet;
        if Pallet::<miot_runtime::Runtime>::is_patron(&who) {
            "approved"
        } else if pallet_litter::PatronRequests::<miot_runtime::Runtime>::contains_key(&who) {
            "pending"
        } else {
            "none"
        }
    });
    Json(serde_json::json!({"status": status})).into_response()
}

/// Whether `call` may come in through this door at all.
pub fn allowed_here(call: &RuntimeCall) -> bool {
    use pallet_litter::Call as C;
    matches!(
        call,
        RuntimeCall::Litter(
            C::say { .. } | C::post { .. } | C::react { .. } | C::vote { .. } | C::approve_patron { .. } | C::reject_patron_request { .. } | C::revoke_patron { .. }
        )
    )
}

async fn submit(AxState(n): AxState<Shared>, body: Bytes) -> Response {
    let uxt = match UncheckedExtrinsic::decode(&mut &body[..]) {
        Ok(u) => u,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, &format!("bad extrinsic: {e}")),
    };
    if !allowed_here(&uxt.function) {
        return refuse(StatusCode::FORBIDDEN, "only say, post, react, vote and the patron calls may be submitted here");
    }
    node::accept_extrinsic(&n, body).await.into_response()
}
