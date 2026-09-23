//! A minimal runtime: `frame_system` + this pallet, nothing else.

use polkadot_sdk::*;

use frame_support::{derive_impl, parameter_types};
use sp_runtime::BuildStorage;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
    pub enum Test {
        System: frame_system,
        Litter: crate,
    }
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
    type Block = Block;
}

parameter_types! {
    // Small and readable. The *defaults* in `miot-primitives` are sized to an
    // LLM turn; these are sized to a test.
    pub const ClaimWindow: u32 = 10;
    pub const Lease: u32 = 20;
    pub const WorkNag: u32 = 5;
    pub const DirectiveNag: u32 = 8;
    pub const MaxNudges: u8 = 3;
    pub const MaxReoffers: u8 = 3;
    pub const MaxDirectiveNudges: u8 = 3;
    pub const GcKeepFor: u32 = 50;
    pub const MaxText: u32 = 4096;
    pub const MaxResult: u32 = 16 * 1024;
    pub const MaxArtifact: u32 = 64 * 1024;
    pub const MaxTitle: u32 = 128;
    pub const MaxSubtasks: u32 = 8;
    pub const MaxTasks: u32 = 512;
    pub const MaxMessage: u32 = 2048;
}

impl crate::Config for Test {
    type ClaimWindow = ClaimWindow;
    type Lease = Lease;
    type WorkNag = WorkNag;
    type DirectiveNag = DirectiveNag;
    type MaxNudges = MaxNudges;
    type MaxReoffers = MaxReoffers;
    type MaxDirectiveNudges = MaxDirectiveNudges;
    type GcKeepFor = GcKeepFor;
    type MaxText = MaxText;
    type MaxResult = MaxResult;
    type MaxArtifact = MaxArtifact;
    type MaxTitle = MaxTitle;
    type MaxSubtasks = MaxSubtasks;
    type MaxTasks = MaxTasks;
    type MaxMessage = MaxMessage;
}

pub const ROOT: u64 = 1;
pub const LEAD: u64 = 2;
pub const TAMA: u64 = 3;
pub const KURO: u64 = 4;

pub fn new_test_ext() -> sp_io::TestExternalities {
    let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
    crate::GenesisConfig::<Test> {
        root: Some(ROOT),
        leader: Some(LEAD),
        roster: [("root", ROOT), ("lead", LEAD), ("tama", TAMA), ("kuro", KURO)].map(|(n, a)| (n.into(), a)).to_vec(),
    }
        .assimilate_storage(&mut t)
        .unwrap();
    let mut ext: sp_io::TestExternalities = t.into();
    // Events are not deposited at block 0.
    ext.execute_with(|| System::set_block_number(1));
    ext
}

/// Run `on_initialize` for every block up to and including `to`.
pub fn roll_to(to: u64) {
    use frame_support::traits::OnInitialize;
    while System::block_number() < to {
        System::set_block_number(System::block_number() + 1);
        <Litter as OnInitialize<u64>>::on_initialize(System::block_number());
    }
}

/// Every effect emitted so far, in order.
pub fn effects() -> Vec<miot_primitives::Effect<u64>> {
    System::events()
        .into_iter()
        .filter_map(|r| match r.event {
            RuntimeEvent::Litter(crate::Event::Happened(e)) => Some(e),
            _ => None,
        })
        .collect()
}
