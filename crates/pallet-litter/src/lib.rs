//! `pallet-litter` — the Akuma Miot task lifecycle, on chain.
//!
//! **This pallet is a wrapper and nothing more.** Every decision lives in
//! [`miot_tasks::TaskTable`], which is a pure state machine with no clock, no
//! I/O and no interior mutability. What happens here is only ever:
//!
//! ```text
//!   ensure_signed  →  load State  →  TaskTable::apply  →  store State  →  emit
//! ```
//!
//! Keeping it that thin is the point. The same machine runs unchanged inside
//! `miot-coord` with no chain at all, so the lifecycle is exercised by 30-odd
//! host-native tests that need neither a runtime nor a block, and this crate's
//! own tests only have to prove the *wrapping* is right.
//!
//! # What the chain buys, in one line
//!
//! The litter's own open problem was that `from` is a string the sender picks:
//! *"root is the key to the cat house"*, and anyone who could reach the hub
//! socket could claim to be the operator. Here the sender is never a field —
//! it is recovered by [`frame_system::ensure_signed`] — so a forged identity is
//! not a policy failure, it is a signature that does not verify.
//!
//! # Law I: the chain never waits
//!
//! [`Pallet::on_initialize`] ticks the table every block. Leases expire, offers
//! are re-made, holders are nudged and the leader is told which verb to type,
//! all without consulting anybody. An agent that goes silent — mid-turn, or
//! forever — costs the chain nothing.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use polkadot_sdk::*;

pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
    // The umbrella re-exports every sub-crate as a module, and the glob at
    // crate root does not reach inside this one.
    use polkadot_sdk::*;

    use alloc::string::String;
    use alloc::vec::Vec;
    use frame_support::pallet_prelude::*;
    use frame_system::pallet_prelude::*;
    use sp_runtime::traits::UniqueSaturatedInto;

    use miot_primitives::{
        Act, Artifact, BlockNumber, Config as MiotConfig, Effect, Error as TaskError, Limits,
        PlanItem, TaskId, Timers,
    };
    use miot_tasks::{State, Task, TaskTable};

    /// `State` holds `String`s and `Vec`s whose bounds are enforced by
    /// [`miot_tasks`] itself ([`Limits`]) rather than by the type system, so
    /// FRAME cannot compute a static maximum for it.
    ///
    /// That is a deliberate, bounded trade and not an oversight: the machine
    /// refuses anything past `max_text`/`max_result`/`max_artifact`/`max_tasks`
    /// **before** it mutates, and `Error::TooLong`/`TooManyTasks` are covered by
    /// tests. Keeping the limits in the state machine is what lets the identical
    /// code run in `miot-coord` off-chain, where `BoundedVec` means nothing.
    ///
    /// The upgrade path, if the table ever gets big: split `State` into a
    /// per-task `StorageMap` of bounded rows. `TaskTable`'s interface does not
    /// move when that happens.
    #[pallet::pallet]
    #[pallet::without_storage_info]
    pub struct Pallet<T>(_);

    #[pallet::config]
    pub trait Config: polkadot_sdk::frame_system::Config {
        // `type RuntimeEvent` is deliberately absent. Since polkadot-sdk's
        // 2412-era FRAME it is a reserved associated type inherited from
        // `frame_system::Config`, and re-declaring it here is an error rather
        // than a redundancy.

        // ---- timers, in BLOCKS ------------------------------------------
        //
        // The unit that matters is an LLM turn, not a second: a turn on a 4B
        // model measured 120-200 s, which at 6 s blocks is 20-34 blocks. The
        // litter's claim window was once *shorter than a single turn*, so an
        // offer lapsed and was re-made while its assignee was still thinking
        // about the first copy. Every default here is sized past that.
        /// An offer nobody claimed is re-offered.
        #[pallet::constant]
        type ClaimWindow: Get<BlockNumber>;
        /// A claimed sub-task whose holder went silent is requeued.
        #[pallet::constant]
        type Lease: Get<BlockNumber>;
        /// How often a holder is told to get on with it.
        #[pallet::constant]
        type WorkNag: Get<BlockNumber>;
        /// How often an outstanding leader directive is repeated.
        #[pallet::constant]
        type DirectiveNag: Get<BlockNumber>;
        /// How many consecutive unanswered reminders a holder gets. Bounded,
        /// because every nudge costs an LLM turn.
        #[pallet::constant]
        type MaxNudges: Get<u8>;
        /// How many times an unclaimed offer is re-made to the same assignee
        /// before the table stops and asks the leader to re-home it. Bounded
        /// for the same reason nudges are: re-offering forever to a cat that
        /// will never answer stalls the parent permanently.
        #[pallet::constant]
        type MaxReoffers: Get<u8>;
        /// How many consecutive unanswered directive nags a leader gets
        /// before the table fails the parent outright. There is no
        /// reassignment act for a leader, so unlike a worker's nudge budget
        /// running out, exhausting this one is terminal for the parent.
        #[pallet::constant]
        type MaxDirectiveNudges: Get<u8>;
        /// How long a closed parent's rows linger before GC. Its artifact is
        /// kept forever regardless.
        #[pallet::constant]
        type GcKeepFor: Get<BlockNumber>;

        // ---- sizes -------------------------------------------------------
        #[pallet::constant]
        type MaxText: Get<u32>;
        /// Bounded results are a forcing function: an agent that cannot
        /// publish a 40 KB build log has to say what happened instead of
        /// pasting what scrolled by.
        #[pallet::constant]
        type MaxResult: Get<u32>;
        #[pallet::constant]
        type MaxArtifact: Get<u32>;
        #[pallet::constant]
        type MaxTitle: Get<u32>;
        #[pallet::constant]
        type MaxSubtasks: Get<u32>;
        #[pallet::constant]
        type MaxTasks: Get<u32>;
        #[pallet::constant]
        type MaxMessage: Get<u32>;
    }

    /// The whole table, as one value.
    #[pallet::storage]
    pub type Litter<T: Config> = StorageValue<_, State<T::AccountId>, ValueQuery>;

    /// Every effect the machine produced, verbatim.
    ///
    /// One variant wrapping [`Effect`] rather than a hand-mirrored copy of it.
    /// A second spelling of the same vocabulary is exactly the drift this
    /// project exists to avoid — it is why `litter-wire` is one crate shared by
    /// both ends of the wire — and [`Effect::wakes`] means an agent's
    /// aggregator consults the type rather than re-deriving the waking rule
    /// from an event name.
    #[pallet::event]
    #[pallet::generate_deposit(pub(super) fn deposit_event)]
    pub enum Event<T: Config> {
        Happened(Effect<T::AccountId>),
    }

    #[pallet::error]
    pub enum Error<T> {
        NotAuthorized,
        NoSuchTask,
        WrongStatus,
        WrongKind,
        NotYours,
        /// `root` is in the roster so it can open tasks, but there is no agent
        /// loop behind it, so it can never be assigned work. A sub-task nobody
        /// can claim is a parent that can never close.
        RootNotAssignable,
        EmptyPlan,
        TooManySubtasks,
        AlreadyPlanned,
        SubtasksOutstanding,
        TooLong,
        AlreadySubmitted,
        AlreadyClaimed,
        TooManyTasks,
    }

    impl<T: Config> From<TaskError> for Error<T> {
        fn from(e: TaskError) -> Self {
            match e {
                TaskError::NotAuthorized => Error::NotAuthorized,
                TaskError::NoSuchTask => Error::NoSuchTask,
                TaskError::WrongStatus => Error::WrongStatus,
                TaskError::WrongKind => Error::WrongKind,
                TaskError::NotYours => Error::NotYours,
                TaskError::RootNotAssignable => Error::RootNotAssignable,
                TaskError::EmptyPlan => Error::EmptyPlan,
                TaskError::TooManySubtasks => Error::TooManySubtasks,
                TaskError::AlreadyPlanned => Error::AlreadyPlanned,
                TaskError::SubtasksOutstanding => Error::SubtasksOutstanding,
                TaskError::TooLong => Error::TooLong,
                TaskError::AlreadySubmitted => Error::AlreadySubmitted,
                TaskError::AlreadyClaimed => Error::AlreadyClaimed,
                TaskError::TooManyTasks => Error::TooManyTasks,
            }
        }
    }

    #[pallet::genesis_config]
    #[derive(frame_support::DefaultNoBound)]
    pub struct GenesisConfig<T: Config> {
        /// The operator account. Opens parent tasks; never assigned any.
        pub root: Option<T::AccountId>,
        /// Who starts as leader.
        pub leader: Option<T::AccountId>,
    }

    #[pallet::genesis_build]
    impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
        fn build(&self) {
            Litter::<T>::mutate(|s| {
                s.root = self.root.clone();
                s.leader = self.leader.clone();
            });
        }
    }

    #[pallet::hooks]
    impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
        /// Law I, as a line of code. Runs every block, consults nobody.
        fn on_initialize(n: BlockNumberFor<T>) -> Weight {
            let now: BlockNumber = n.unique_saturated_into();
            let mut table = Self::table();
            let effects = table.tick(now);
            let dropped = table.gc(now, T::GcKeepFor::get());
            if !effects.is_empty() || dropped > 0 {
                Self::commit(table, effects);
            }
            // Placeholder. Real weights need `frame-benchmarking`, which is an
            // open question for a chain nobody pays for — see
            // `docs/MAPPING_REPORT.md` §7.4.
            Weight::from_parts(10_000, 0)
        }
    }

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        /// Open a parent task. Operator or leader only — the one privileged
        /// act in the protocol.
        #[pallet::call_index(0)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn open(origin: OriginFor<T>, text: String) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::apply(|t, auth, now| t.open(&who, auth, &text, now).map(|(_, fx)| fx), &who)
        }

        /// Split a parent into directed sub-tasks. Leader only, **one call** —
        /// without that the table could never know planning had finished, so
        /// "all sub-tasks cleared" would never be decidable and the artifact
        /// would never fire.
        #[pallet::call_index(1)]
        #[pallet::weight(Weight::from_parts(20_000, 0))]
        pub fn plan(
            origin: OriginFor<T>,
            parent: TaskId,
            assignments: Vec<PlanItem<T::AccountId>>,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::apply(|t, auth, now| t.plan(&who, auth, parent, &assignments, now), &who)
        }

        /// Every per-task act: claim, done, failed, clear, reopen, artifact.
        ///
        /// One dispatchable with a status enum rather than six near-identical
        /// ones. A small model picks a *value* more reliably than it picks
        /// among similar tool names, and a new act then costs a value instead
        /// of new surface.
        #[pallet::call_index(2)]
        #[pallet::weight(Weight::from_parts(15_000, 0))]
        pub fn update(
            origin: OriginFor<T>,
            task: TaskId,
            act: Act,
            text: String,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::apply(|t, auth, now| t.update(&who, auth, task, act, &text, now), &who)
        }

        /// Install a leader. Operator only.
        ///
        /// Taking the role queues a `LeaderElected` directive for the new
        /// holder, because promotion is an instruction to act and not merely a
        /// fact to notice.
        #[pallet::call_index(3)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn set_leader(origin: OriginFor<T>, who: T::AccountId) -> DispatchResult {
            let caller = ensure_signed(origin)?;
            let state = Litter::<T>::get();
            ensure!(state.root.as_ref() == Some(&caller), Error::<T>::NotAuthorized);
            let now: BlockNumber =
                frame_system::Pallet::<T>::block_number().unique_saturated_into();
            let mut table = TaskTable::from_state(state, Self::miot_config());
            let effects = table.set_leader(who, now);
            Self::commit(table, effects);
            Ok(())
        }

        /// Say something to one cat, or to the whole litter.
        ///
        /// The operator's way in. A litter that can only exchange task
        /// transitions cannot be asked anything.
        #[pallet::call_index(6)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn say(
            origin: OriginFor<T>,
            to: Option<T::AccountId>,
            body: String,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::apply(|t, auth, _now| t.say(&who, auth, to.clone(), &body), &who)
        }

        /// Move a sub-task to a different cat. Leader only.
        ///
        /// How a litter recovers from a member that cannot do the job — it
        /// died, it wedged, or it reported `failed`. Without this an assignee
        /// holds its sub-task for life, because a requeue deliberately keeps
        /// `assignee` (the work is still *theirs*, merely unclaimed), and a
        /// parent can never close while one sub-task is stuck with a cat that
        /// will never answer.
        ///
        /// Not folded into [`Self::update`]'s act enum: that enum is what a
        /// *worker* does to its own task, and every one of those acts takes
        /// only text. This is a leader act that takes another account, like
        /// [`Self::plan`].
        #[pallet::call_index(5)]
        #[pallet::weight(Weight::from_parts(15_000, 0))]
        pub fn reassign(
            origin: OriginFor<T>,
            task: TaskId,
            to: T::AccountId,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::apply(|t, auth, now| t.reassign(&who, auth, task, to.clone(), now), &who)
        }

        /// Install the operator account. Governance/sudo only — this is the
        /// key to the cat house, so it is not something a peer can hand
        /// itself.
        #[pallet::call_index(4)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn set_root(origin: OriginFor<T>, who: T::AccountId) -> DispatchResult {
            ensure_root(origin)?;
            Litter::<T>::mutate(|s| s.root = Some(who));
            Ok(())
        }
    }

    impl<T: Config> Pallet<T> {
        /// Timers and limits, assembled from the runtime's constants. Never
        /// stored — a chain retunes a timer with an upgrade, not a migration.
        pub fn miot_config() -> MiotConfig {
            MiotConfig {
                timers: Timers {
                    claim_window: T::ClaimWindow::get(),
                    lease: T::Lease::get(),
                    work_nag: T::WorkNag::get(),
                    directive_nag: T::DirectiveNag::get(),
                    max_nudges: T::MaxNudges::get(),
                    max_reoffers: T::MaxReoffers::get(),
                    max_directive_nudges: T::MaxDirectiveNudges::get(),
                },
                limits: Limits {
                    max_text: T::MaxText::get() as usize,
                    max_result: T::MaxResult::get() as usize,
                    max_artifact: T::MaxArtifact::get() as usize,
                    max_title: T::MaxTitle::get() as usize,
                    max_subtasks: T::MaxSubtasks::get() as usize,
                    max_tasks: T::MaxTasks::get() as usize,
                    max_message: T::MaxMessage::get() as usize,
                },
            }
        }

        pub fn table() -> TaskTable<T::AccountId> {
            TaskTable::from_state(Litter::<T>::get(), Self::miot_config())
        }

        pub fn task(id: TaskId) -> Option<Task<T::AccountId>> {
            Self::table().get(id).cloned()
        }

        pub fn artifact(parent: TaskId) -> Option<Artifact<T::AccountId>> {
            Self::table().artifact(parent).cloned()
        }

        /// Authority is decided from the **recovered** caller, never from a
        /// field the caller filled in. This is the whole reason the litter
        /// moved onto a chain.
        fn authority_of(who: &T::AccountId, state: &State<T::AccountId>) -> miot_primitives::Authority {
            use miot_primitives::Authority;
            if state.root.as_ref() == Some(who) {
                Authority::Root
            } else if state.leader.as_ref() == Some(who) {
                Authority::Leader
            } else {
                Authority::Peer
            }
        }

        /// load → apply → store → emit. A refused act writes nothing and emits
        /// nothing: applied-versus-refused is typed, so there is no note to
        /// read and no half-applied state to replicate.
        fn apply<F>(f: F, who: &T::AccountId) -> DispatchResult
        where
            F: FnOnce(
                &mut TaskTable<T::AccountId>,
                miot_primitives::Authority,
                BlockNumber,
            ) -> Result<Vec<Effect<T::AccountId>>, TaskError>,
        {
            let state = Litter::<T>::get();
            let auth = Self::authority_of(who, &state);
            let now: BlockNumber =
                frame_system::Pallet::<T>::block_number().unique_saturated_into();
            let mut table = TaskTable::from_state(state, Self::miot_config());
            let effects = f(&mut table, auth, now).map_err(Error::<T>::from)?;
            Self::commit(table, effects);
            Ok(())
        }

        fn commit(table: TaskTable<T::AccountId>, effects: Vec<Effect<T::AccountId>>) {
            Litter::<T>::put(table.into_state());
            for e in effects {
                Self::deposit_event(Event::Happened(e));
            }
        }
    }
}

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
