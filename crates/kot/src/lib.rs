//! `kot` — the litter's one binary, as a library so the election tests can
//! run several nodes in one process.
//!
//! - [`node`] — a mesh node: the chain, durable, with an elected primary.
//! - [`agent`] — one cat's agent loop, a client of a node like any other.
//! - [`client`] — the operator's one-shot verbs and REPL.
//! - [`agent_state_machine`] — the one agent loop, hosted by both [`agent`] and [`chat`].
//!
//! `crates/miot` used to be the node and the client, and `kot` the agent
//! loop. They merged here (`docs/CLEANUP.md` item 2), and `miot` is gone.

pub mod activity;
pub mod agent;
pub mod chat;
pub mod client;
pub mod common;
pub mod local_tasks;
pub mod agent_state_machine;
pub mod langfuse_log;
pub mod node;
pub mod tls;
pub mod ui;
pub mod version;
