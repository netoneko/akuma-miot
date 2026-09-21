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

extern crate alloc;

use polkadot_sdk::*;

use frame_support::{
    derive_impl, parameter_types,
    traits::{ConstU32, ConstU64, ConstU8},
};
use sp_runtime::{generic, traits::BlakeTwo256, AccountId32, MultiSignature};

pub use miot_primitives as primitives;
pub use pallet_litter;

pub type BlockNumber = u64;

/// An account **is** an ed25519 public key (`miot_keys::Identity::account`) —
/// not a hash of one, not a small int. That is what lets a signer be
/// *recovered* from a signature rather than merely asserted alongside one,
/// which is the entire reason this project moved onto a chain in the first
/// place (`pallet_litter`'s own doc comment).
pub type AccountId = AccountId32;
pub type Signature = MultiSignature;
pub type Header = generic::Header<BlockNumber, BlakeTwo256>;

/// The standard `frame-system` checks, and nothing project-specific: replay
/// protection (`CheckNonce`), chain/spec binding (`CheckGenesis`,
/// `CheckSpecVersion`, `CheckTxVersion`), and expiry (`CheckMortality`).
/// `miot-keys`'s own doc comment named exactly this tuple before any of it
/// existed — this is that promise kept, not a new design.
pub type SignedExtra = (
    frame_system::CheckGenesis<Runtime>,
    frame_system::CheckSpecVersion<Runtime>,
    frame_system::CheckTxVersion<Runtime>,
    frame_system::CheckMortality<Runtime>,
    frame_system::CheckNonce<Runtime>,
);

pub type UncheckedExtrinsic = generic::UncheckedExtrinsic<AccountId, RuntimeCall, Signature, SignedExtra>;

/// A real block, not a `MockBlock`: the extrinsic type is signed and
/// verifiable, so this crate now needs a header/body shape `miot-node` can
/// hash and chain, not a test-only stand-in.
pub type Block = generic::Block<Header, UncheckedExtrinsic>;

frame_support::construct_runtime!(
    pub enum Runtime {
        System: frame_system,
        Litter: pallet_litter,
    }
);

/// Identifies this runtime on the wire — what `CheckSpecVersion`/
/// `CheckTxVersion` bind a signature to. Bump `spec_version` on any change
/// that makes an old signed extrinsic mean something different; there is no
/// `impl_runtime_apis!` block consuming `apis`, so it stays empty.
pub const VERSION: sp_version::RuntimeVersion = sp_version::RuntimeVersion {
    spec_name: alloc::borrow::Cow::Borrowed("akuma-miot"),
    impl_name: alloc::borrow::Cow::Borrowed("akuma-miot"),
    authoring_version: 1,
    spec_version: 1,
    impl_version: 0,
    apis: sp_version::create_apis_vec!([]),
    transaction_version: 1,
    system_version: 1,
};

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Runtime {
    type Block = Block;
    type AccountId = AccountId;
    type Lookup = sp_runtime::traits::IdentityLookup<AccountId>;
    type Version = RuntimeVersionGetter;
}

pub struct RuntimeVersionGetter;
impl frame_support::traits::Get<sp_version::RuntimeVersion> for RuntimeVersionGetter {
    fn get() -> sp_version::RuntimeVersion {
        VERSION.clone()
    }
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

/// Applies real blocks: `initialize_block` runs `System::initialize` +
/// every pallet's `on_initialize` (this is where `Litter::on_initialize`
/// actually fires now, not a hand-called `OnInitialize::on_initialize`),
/// `apply_extrinsic` runs the full signature/nonce/mortality pipeline before
/// dispatch, and `finalize_block` runs `on_finalize` and returns the real
/// header `miot-node` hashes and chains as `parent_hash` for the next block.
/// This is why `frame-executive` was already a dependency before any of this
/// existed — hand-rolling this lifecycle correctly is exactly what it is for.
pub type Executive = frame_executive::Executive<
    Runtime,
    Block,
    frame_system::ChainContext<Runtime>,
    Runtime,
    AllPalletsWithSystem,
>;

/// Building and signing an [`UncheckedExtrinsic`] — the one place this logic
/// lives, so `miot-cat` and `miot --rpc` (and anything else that ever
/// needs to submit) share it instead of each re-deriving the wire format.
pub mod client {
    use super::*;
    use codec::Encode;
    use miot_keys::Identity;
    use sp_core::H256;
    use sp_runtime::generic::{Era, SignedPayload};

    /// What a signer needs before it can build a valid extension set, read
    /// from a real node (`GET /meta`) rather than hardcoded — the whole
    /// point of `CheckGenesis`/`CheckSpecVersion`/`CheckTxVersion` is that a
    /// signature only means something against a specific chain and runtime.
    #[derive(Clone, Copy, Debug)]
    pub struct Meta {
        pub genesis_hash: H256,
        pub spec_version: u32,
        pub tx_version: u32,
    }

    /// Sign `call` as `identity`, at `nonce`.
    ///
    /// **Cannot** call [`SignedPayload::new`] — that runs each extension's
    /// `implicit()`, and `CheckGenesis`/`CheckMortality` read it from
    /// on-chain storage (`BlockHash`), which does not exist in a client
    /// process. [`SignedPayload::from_raw`] is the client-side path: supply
    /// the same bytes a real node's storage would have produced, fetched
    /// over the wire instead of read from an externality. Immortal by
    /// construction — one operator's own swarm has no mempool a stale
    /// extrinsic could be replayed against, so there is nothing a mortality
    /// window would be defending here.
    pub fn sign(identity: &Identity, call: RuntimeCall, nonce: u32, meta: &Meta) -> UncheckedExtrinsic {
        let extra: SignedExtra = (
            frame_system::CheckGenesis::new(),
            frame_system::CheckSpecVersion::new(),
            frame_system::CheckTxVersion::new(),
            frame_system::CheckMortality::from(Era::immortal()),
            frame_system::CheckNonce::from(nonce),
        );
        let implicit = (meta.genesis_hash, meta.spec_version, meta.tx_version, meta.genesis_hash, ());
        let raw = SignedPayload::<RuntimeCall, SignedExtra>::from_raw(call.clone(), extra.clone(), implicit);
        let signature = raw.using_encoded(|payload| identity.sign(payload));
        UncheckedExtrinsic::new_signed(call, identity.account(), Signature::Ed25519(signature), extra)
    }
}
