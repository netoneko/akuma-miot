//! `kot` — the litter's one binary, as a library so the election tests can
//! run several nodes in one process.
//!
//! - [`node`] — a mesh node: the chain, durable, with an elected primary.
//! - [`agent`] — one cat's agent loop, a client of a node like any other.
//! - [`client`] — the operator's one-shot verbs and REPL.
//!
//! `crates/miot` used to be the node and the client, and `kot` the agent
//! loop. They merged here (`docs/CLEANUP.md` item 2), and `miot` is gone.

pub mod agent;
pub mod client;
pub mod common;
pub mod node;
pub mod version;
