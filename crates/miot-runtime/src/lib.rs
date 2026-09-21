//! The Akuma Miot runtime: `frame_system` + `pallet-litter`, and nothing else.
//!
//! **Executed natively.** There is no wasm blob, no `impl_runtime_apis!` and no
//! `sc-executor`. FRAME's runtime side is an ordinary Rust library — the
//! pallet's own tests have always run it this way — so a node that does not
//! need forkless upgrades can call [`frame_executive::Executive`] directly and
//! skip the entire wasm toolchain.
//!
//! What that costs: no runtime upgrades, and none of the `sc-*` tooling that
//! expects a blob. What it buys: no wasm build, no wasm executor, and no state
//! trie database — which is the difference between "runs on Linux" and "runs
//! anywhere, including a hobby kernel with ext2 and no mmap'd DB".
//!
//! The upgrade path is deliberately still open. Nothing here is unusual; add
//! `substrate-wasm-builder` and an `impl_runtime_apis!` block later and this
//! same runtime compiles to a blob.
//!
//! No `pallet-balances`, no `pallet-transaction-payment`, no `pallet-aura`, no
//! `pallet-grandpa`: a litter is one operator's trusted swarm, so there are no
//! fees to charge and consensus is the node's business, not the runtime's.

#![cfg_attr(not(feature = "std"), no_std)]

use polkadot_sdk::*;

use frame_support::{
    derive_impl, parameter_types,
    traits::{ConstU32, ConstU64, ConstU8},
};
use sp_runtime::{generic, traits::BlakeTwo256};

pub use miot_primitives as primitives;
pub use pallet_litter;

pub type BlockNumber = u64;
pub type AccountId = u64;
pub type Header = generic::Header<BlockNumber, BlakeTwo256>;
pub type Block = frame_system::mocking::MockBlock<Runtime>;

frame_support::construct_runtime!(
    pub enum Runtime {
        System: frame_system,
        Litter: pallet_litter,
    }
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Runtime {
    type Block = Block;
    type AccountId = AccountId;
}

parameter_types! {
    // ONE BLOCK IS SIX SECONDS (miot-node's BLOCK_MS), so a wake cadence of
    // N minutes is N * 10 blocks.
    //
    // The unit that actually matters is one LLM turn. Measured on qwen3:4b
    // across four cats sharing one GPU: a worker turn is 30-120 s and a leader
    // turn — planning, or clearing several sub-tasks — reached **230 s**.
    //
    // A wake cadence shorter than a turn is the failure the litter already
    // documented and this project then reproduced: at a 20 s directive
    // interval the leader was re-woken eleven times per turn and spent its
    // throughput answering directives that were stale before it read them.
    //
    // So: **a 3-minute wake cadence**, anchored per task. Each sub-task's nag
    // and each parent's directive run on their own clock from when that thing
    // became due, so the litter is not a metronome that wakes everyone at
    // once — it is N independent timers that mostly stay quiet.

    /// 6 min. An offer nobody claimed is re-made. Two full turns, so an
    /// assignee that is merely slow is never re-offered out from under itself.
    pub const ClaimWindow: u32 = 60;
    /// 15 min. A claimed sub-task whose holder went silent is requeued. Long
    /// enough that a genuinely working cat is never interrupted.
    pub const Lease: u32 = 150;
    /// 3 min. How often a holder is told to get on with it.
    pub const WorkNag: u32 = 30;
    /// 3 min. How often an outstanding leader directive repeats.
    pub const DirectiveNag: u32 = 30;
    /// A day. How long a closed parent's rows linger before GC; its artifact
    /// is kept forever regardless.
    pub const GcKeepFor: u32 = 14_400;
}

impl pallet_litter::Config for Runtime {
    type ClaimWindow = ClaimWindow;
    type Lease = Lease;
    type WorkNag = WorkNag;
    type DirectiveNag = DirectiveNag;
    type MaxNudges = ConstU8<3>;
    type MaxReoffers = ConstU8<3>;
    type GcKeepFor = GcKeepFor;
    type MaxText = ConstU32<4096>;
    // A forcing function, not a safety rail: a cat that cannot publish a 40 KB
    // build log has to say what happened instead of pasting what scrolled by.
    type MaxResult = ConstU32<{ 16 * 1024 }>;
    type MaxArtifact = ConstU32<{ 64 * 1024 }>;
    type MaxTitle = ConstU32<128>;
    type MaxSubtasks = ConstU32<8>;
    type MaxTasks = ConstU32<512>;
    type MaxMessage = ConstU32<2048>;
}

const _: Option<ConstU64<0>> = None;
