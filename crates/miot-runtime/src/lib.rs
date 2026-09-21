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
    // At 6 s blocks. Every one of these is longer than it looks, because the
    // unit that matters is an LLM turn: 120-200 s on a 4B model is 20-34
    // blocks, and the litter's first claim window was *shorter than one turn*.
    pub const ClaimWindow: u32 = 100;   // 10 min
    pub const Lease: u32 = 150;         // 15 min
    pub const WorkNag: u32 = 25;        // 2.5 min
    pub const DirectiveNag: u32 = 20;   // 2 min
    pub const GcKeepFor: u32 = 14_400;  // a day
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
