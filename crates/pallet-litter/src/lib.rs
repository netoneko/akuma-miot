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
        Act, Artifact, ArtifactId, BlockNumber, Config as MiotConfig, Effect, Error as TaskError,
        Limits, MessageId, PlanItem, TaskId, Timers,
    };
    use miot_tasks::{State, Task, TaskTable};

    /// One voter, one side: voting the other way moves them, repeating
    /// their own side withdraws. Free and shared by [`Pallet::vote`] and
    /// [`Pallet::replay_effect`], so a replica folds a vote exactly the
    /// way the primary recorded it.
    fn cast_vote<A: Clone + PartialEq>(t: &mut Tally<A>, who: &A, up: bool) {
        let (own, other) = if up { (&mut t.up, &mut t.down) } else { (&mut t.down, &mut t.up) };
        if let Some(i) = own.iter().position(|v| v == who) {
            own.remove(i);
            return;
        }
        if let Some(i) = other.iter().position(|v| v == who) {
            other.remove(i);
        }
        own.push(who.clone());
    }

    /// A cat's own cumulative work stats, self-reported via
    /// [`Pallet::report_stats`] — see [`Stats`]. Plain scalars, not a
    /// per-tool breakdown: the tool vocabulary this project actually offers
    /// is small and fixed (`miot_llm`'s `*_tools` functions), so a caller
    /// wanting a breakdown reads it off the reporter's own turn-by-turn
    /// log instead of this chain-wide summary paying to store one.
    #[derive(Debug, Clone, Default, PartialEq, Eq, codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo)]
    pub struct CatStats {
        pub turns: u32,
        pub tool_calls: u32,
        pub tokens: u64,
        pub ms: u64,
    }

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
    /// Set by a node that is **folding someone else's blocks** — a follower
    /// syncing from the mesh leader, or any node replaying its own store on
    /// start. While set, [`Pallet::on_initialize`] does *not* run
    /// `TaskTable::tick`: the block being folded already carries the tick's
    /// effects (the producer drained them into that block's body), and
    /// [`Pallet::replay_effect`] applies them. Running the tick locally as
    /// well applied every tick effect twice — harmless for the set-style
    /// ones (`Nudge` recomputes from `remaining`), but `Directed` *increments*
    /// `directive_nudges_used`, so every replica, and every primary after a
    /// restart, burned a leader's directive budget at double speed. Found
    /// wiring election (2026-09-22): a promoted follower would have failed
    /// parents early. `/tasks` never shows that counter, which is why the
    /// "byte-identical" replica checks never caught it.
    ///
    /// `gc` still runs either way: it drops rows without emitting an
    /// effect, so a folding node has no other way to learn about it.
    ///
    /// Node-local by intent, but it lives in storage (and so in a
    /// compaction snapshot) because the hook has no other channel to the
    /// host — a node must re-assert it after replacing its externalities.
    #[pallet::storage]
    pub type Replaying<T: Config> = StorageValue<_, bool, ValueQuery>;

    /// One cat's self-reported cumulative work stats — turns taken, tool
    /// calls made, tokens spent, milliseconds spent thinking. Self-reported
    /// and cumulative (each report replaces the whole record, not a delta)
    /// so a dropped report never desyncs a running total the way an
    /// increment-only counter would. No `Effect` — nothing here is a wake
    /// reason, so emitting one would only grow `/events` for every turn of
    /// every cat, for readers who can already just ask this directly.
    #[pallet::storage]
    pub type Stats<T: Config> = StorageMap<_, Blake2_128Concat, T::AccountId, CatStats, ValueQuery>;

    /// Messages a cat has sent (`SendMessage`), counted apart from its tool
    /// calls — [`Pallet::report_stats2`]. Its own map rather than a new
    /// `CatStats` field: `CatStats`' encoding is inside every checkpoint
    /// snapshot already written, and a changed struct would stop decoding.
    #[pallet::storage]
    pub type MessagesSent<T: Config> = StorageMap<_, Blake2_128Concat, T::AccountId, u32, ValueQuery>;

    /// An artifact's vote tally — [`Pallet::vote`]. One voter appears in at
    /// most one side (voting again on the other side moves them); a voter
    /// repeats its own side to withdraw. Kept as plain `Vec`s, not counts:
    /// "who thought this was trustworthy" is the signal a reader wants, and
    /// every artifact ever voted on is a small litter, so the map stays
    /// small by construction.
    #[derive(Debug, Clone, PartialEq, Eq, codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo)]
    pub struct Tally<AccountId> {
        pub up: Vec<AccountId>,
        pub down: Vec<AccountId>,
    }

    /// Hand-written rather than derived: a derived `Default` would demand
    /// `T::AccountId: Default`, which an `AccountId32` universe doesn't
    /// give — and `ValueQuery` needs a default.
    impl<AccountId> Default for Tally<AccountId> {
        fn default() -> Self {
            Tally { up: Vec::new(), down: Vec::new() }
        }
    }

    #[pallet::storage]
    pub type Votes<T: Config> = StorageMap<_, Blake2_128Concat, ArtifactId, Tally<T::AccountId>, ValueQuery>;

    /// One comment on one artifact — [`Pallet::post`] with an
    /// `artifact_id`. On-chain **storage**, not just an effect, on purpose:
    /// an artifact's comment thread is part of the artifact ("packaged
    /// under it"), and the block log is what compaction shrinks — a
    /// comment that lived only as a log effect would vanish at the next
    /// `/clear` while the artifact it belongs to survived. Votes made the
    /// same trade ([`Votes`]); reactions didn't (they're ephemeral social
    /// gloss by design).
    #[derive(Debug, Clone, PartialEq, Eq, codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo)]
    pub struct Comment<AccountId> {
        pub who: AccountId,
        pub at: BlockNumber,
        pub body: String,
    }

    /// How far back an artifact's thread goes. A sliding window, oldest
    /// dropped first — same philosophy as the nudge budget: unbounded
    /// growth is a loop that pays forever, and the log itself still holds
    /// the early thread until the next compaction.
    pub const COMMENT_KEEP: usize = 32;

    /// The current epoch — the session bounded by a compaction. Bumped by
    /// [`Pallet::clear_all`] and [`Pallet::request_compaction`], the two
    /// calls that make the node snapshot and shrink the block log, so the
    /// counter itself rides every compaction snapshot: a replica folding
    /// effects after a rewind lands on the same epoch the primary did.
    /// Nothing here is valid *across* an epoch boundary — messages' live
    /// window is exactly one epoch — which is why artifact comments are
    /// keyed by it (see [`Comments`]): each epoch naturally gains its own
    /// comment section, and no thread grows eternal under an artifact.
    #[pallet::storage]
    pub type Epoch<T: Config> = StorageValue<_, u32, ValueQuery>;

    #[pallet::storage]
    pub type Comments<T: Config> = StorageDoubleMap<_, Blake2_128Concat, ArtifactId, Blake2_128Concat, u32, Vec<Comment<T::AccountId>>, ValueQuery>;

    /// Who is in this litter, by name: `(name, account)`, in genesis order.
    /// Written once at genesis and never changed by any call — membership is
    /// static (a new member is a new genesis), so there is no dispatchable
    /// that touches it.
    ///
    /// On chain rather than only in each node's `MIOT_ROSTER` so the names
    /// are agreed on like everything else in genesis: every node's roster
    /// ends up in state, a compaction snapshot carries it, and a client reads
    /// it from `/roster` instead of trusting its own copy for names. (The
    /// node still takes it from config: it needs the accounts for mTLS
    /// pinning before any state exists.)
    #[pallet::storage]
    pub type Roster<T: Config> = StorageValue<_, Vec<(String, T::AccountId)>, ValueQuery>;

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
        /// Every member, by name. See [`Roster`].
        pub roster: Vec<(String, T::AccountId)>,
    }

    #[pallet::genesis_build]
    impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
        fn build(&self) {
            Litter::<T>::mutate(|s| {
                s.root = self.root.clone();
                s.leader = self.leader.clone();
            });
            Roster::<T>::put(self.roster.clone());
        }
    }

    #[pallet::hooks]
    impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
        /// Law I, as a line of code. Runs every block, consults nobody.
        fn on_initialize(n: BlockNumberFor<T>) -> Weight {
            let now: BlockNumber = n.unique_saturated_into();
            let mut table = Self::table();
            let effects = if Replaying::<T>::get() { Vec::new() } else { table.tick(now) };
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
        ///
        /// `off_record`: the sender's own signal that this one must never be
        /// written into the block log — see `Effect::Said`'s doc comment for
        /// exactly what that does and doesn't buy.
        #[pallet::call_index(6)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn say(
            origin: OriginFor<T>,
            to: Option<T::AccountId>,
            body: String,
            no_ack: bool,
            off_record: bool,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::apply(|t, auth, _now| t.say(&who, auth, to.clone(), &body, no_ack, off_record), &who)
        }

        /// Fail every open parent at once. Operator only — a session
        /// boundary ("start fresh on this chain"), not a task-lifecycle act.
        #[pallet::call_index(7)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn clear_all(origin: OriginFor<T>) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::apply(|t, auth, now| t.clear_all(auth, now), &who)?;
            // A session boundary is an epoch boundary (`docs/MESSAGING.md`):
            // this is the compaction trigger, so the counter moves with it
            // — inside this block's state, and so inside the snapshot.
            Epoch::<T>::mutate(|e| *e = e.saturating_add(1));
            Ok(())
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

        /// Publish a standalone artifact — markdown with no task behind it.
        /// Anyone may call this; there is nothing to authorize against,
        /// because there is no task whose lifecycle this could be mistaken
        /// for closing.
        #[pallet::call_index(8)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn publish_standalone_artifact(origin: OriginFor<T>, text: String) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::apply(|t, _auth, now| t.publish_standalone_artifact(&who, &text, now).map(|(_, fx)| fx), &who)
        }

        /// Ask the node to snapshot state and shrink the block log at the
        /// next block close — the same mechanism `clear_all` triggers as a
        /// side effect, but standalone: no task is failed, and
        /// `miot_tasks::TaskTable` never sees this call at all, since there
        /// is nothing in it for the lifecycle machine to apply. `kot`'s
        /// node reacts to it the same way it reacts to `clear_all` — by
        /// matching the extrinsic's own call, in `Node::submit`, not by an
        /// `Effect` (there is nothing to emit: no state changed here for a
        /// replica to replay). Operator only, same reasoning as
        /// `clear_all` — deciding when to shrink the log is the operator's
        /// call, not any cat's, even though `RequestCompaction` is offered
        /// as a tool like any other.
        #[pallet::call_index(9)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn request_compaction(origin: OriginFor<T>) -> DispatchResult {
            let who = ensure_signed(origin)?;
            let state = Litter::<T>::get();
            ensure!(Self::authority_of(&who, &state) == miot_primitives::Authority::Root, Error::<T>::NotAuthorized);
            // Same epoch bump as `clear_all`: compaction is the boundary,
            // whether or not a task was failed on the way past it.
            Epoch::<T>::mutate(|e| *e = e.saturating_add(1));
            Ok(())
        }

        /// Report this cat's own cumulative work stats — turns, tool
        /// calls, tokens, milliseconds — for anyone (any other cat, the
        /// operator) to read back via [`Self::all_stats`]. `who` is
        /// `ensure_signed`, same as everywhere else here: a cat can only
        /// ever overwrite its *own* record, never claim to be reporting
        /// for another account.
        ///
        /// Deposits an `Effect` for the same reason every other write here
        /// does — the primary applies this write directly (below), but a
        /// replica only ever learns about a state change by replaying the
        /// effect log (`Node::apply_block` → `Self::replay_effect`), never
        /// by re-running the original extrinsic. Found live 2026-09-23: an
        /// earlier version wrote straight to storage with no effect, and
        /// `GET /stats` came back correct on the primary, permanently
        /// empty on every replica.
        #[pallet::call_index(10)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn report_stats(origin: OriginFor<T>, turns: u32, tool_calls: u32, tokens: u64, ms: u64) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Stats::<T>::insert(who.clone(), CatStats { turns, tool_calls, tokens, ms });
            Self::deposit_event(Event::Happened(Effect::StatsReported { who, turns, tool_calls, tokens, ms }));
            Ok(())
        }

        /// [`Self::report_stats`] with `messages` (SendMessage calls) split
        /// out of `tool_calls`. A new call rather than a changed one: an
        /// agent on an older build still reports through index 10, and a
        /// node on an older build refuses this one outright instead of
        /// misreading it.
        #[pallet::call_index(11)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn report_stats2(origin: OriginFor<T>, turns: u32, tool_calls: u32, messages: u32, tokens: u64, ms: u64) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Stats::<T>::insert(who.clone(), CatStats { turns, tool_calls, tokens, ms });
            MessagesSent::<T>::insert(who.clone(), messages);
            Self::deposit_event(Event::Happened(Effect::StatsReported2 { who, turns, tool_calls, messages, tokens, ms }));
            Ok(())
        }

        /// Say something as a **reply** to an earlier message, and/or under
        /// topic tags — [`Effect::Message`], artifact 5 §1.2/§2.3. Same
        /// semantics as [`Self::say`] otherwise: `no_ack` is the sender's
        /// own "needs no reply", `off_record` keeps it out of the block
        /// log. `parent` is the block number of the message being answered;
        /// `None` is a top-level message (tags alone still route here, so
        /// tagged conversation threads stay one variant).
        ///
        /// Not routed through `TaskTable` — chat touches no lifecycle state,
        /// exactly like `say`'s own no-op — so this is the same thin shape
        /// `report_stats` uses: validate, emit, and let replicas fold the
        /// effect.
        #[pallet::call_index(12)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn post(
            origin: OriginFor<T>,
            id: MessageId,
            to: Option<T::AccountId>,
            body: String,
            parent: Option<MessageId>,
            artifact_id: Option<ArtifactId>,
            tags: Vec<String>,
            no_ack: bool,
            off_record: bool,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            ensure!(body.len() <= T::MaxMessage::get() as usize, Error::<T>::TooLong);
            ensure!(tags.len() <= 8, Error::<T>::TooLong);
            ensure!(tags.iter().all(|t| !t.is_empty() && t.len() <= 32), Error::<T>::TooLong);
            let state = Litter::<T>::get();
            let from_root = Self::authority_of(&who, &state) == miot_primitives::Authority::Root;
            // A comment on an artifact joins that artifact's thread — chain
            // storage, keyed by the current epoch (`docs/MESSAGING.md`).
            // The effect still rides below, so replays land in the same
            // thread and live readers see it in `/events`.
            if let Some(a) = &artifact_id {
                let now: BlockNumber =
                    frame_system::Pallet::<T>::block_number().unique_saturated_into();
                let epoch = Epoch::<T>::get();
                Comments::<T>::mutate(a, epoch, |v| {
                    v.push(Comment { who: who.clone(), at: now, body: body.clone() });
                    if v.len() > COMMENT_KEEP {
                        v.remove(0);
                    }
                });
            }
            Self::deposit_event(Event::Happened(Effect::Message { id, from: who, to, body, parent, artifact_id, tags, from_root, no_ack, off_record }));
            Ok(())
        }

        /// React to the message with id `target` with an emoji —
        /// [`Effect::Reacted`], artifact 5 §2.1. The agreement sora and tama
        /// typed out at length would have been one of these. Non-waking by
        /// construction: a reaction *is* the acknowledgment. Effect-only —
        /// no storage — because a reaction is ephemeral gloss by design
        /// (`docs/MESSAGING.md`).
        #[pallet::call_index(13)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn react(origin: OriginFor<T>, target: MessageId, emoji: String) -> DispatchResult {
            let who = ensure_signed(origin)?;
            ensure!(!emoji.is_empty() && emoji.len() <= 16, Error::<T>::TooLong);
            Self::deposit_event(Event::Happened(Effect::Reacted { who, target, emoji }));
            Ok(())
        }

        /// Vote an artifact up or down — [`Effect::Voted`], artifact 5
        /// §1.1. Voting the other way moves the voter; repeating the same
        /// way withdraws. Read back via [`Self::tally`]. Anyone may: a
        /// quality signal is only as good as its sample.
        #[pallet::call_index(14)]
        #[pallet::weight(Weight::from_parts(10_000, 0))]
        pub fn vote(origin: OriginFor<T>, artifact: ArtifactId, up: bool) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Votes::<T>::mutate(artifact, |t| cast_vote(t, &who, up));
            Self::deposit_event(Event::Happened(Effect::Voted { who, artifact, up }));
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

        pub fn standalone_artifact(id: u32) -> Option<Artifact<T::AccountId>> {
            Self::table().standalone_artifact(id).cloned()
        }

        pub fn standalone_artifacts() -> Vec<(u32, Artifact<T::AccountId>)> {
            Self::table().standalone_artifacts().to_vec()
        }

        /// Every closed parent's artifact, for the merged `/artifacts` listing.
        pub fn artifacts() -> Vec<(TaskId, Artifact<T::AccountId>)> {
            Self::table().artifacts().to_vec()
        }

        /// Every account that has ever called `report_stats`, and its
        /// latest report — for `GET /stats`. `StorageMap` has no built-in
        /// "list everything," hence the iteration here rather than a
        /// single storage read.
        pub fn all_stats() -> Vec<(T::AccountId, CatStats)> {
            Stats::<T>::iter().collect()
        }

        /// `who`'s messages sent, from its latest `report_stats2` — `None`
        /// if it has only ever reported through the old call, which didn't
        /// count them apart.
        pub fn messages_sent(who: &T::AccountId) -> Option<u32> {
            MessagesSent::<T>::contains_key(who).then(|| MessagesSent::<T>::get(who))
        }

        /// Fold a previously-emitted effect into storage — the replay path,
        /// not a new occurrence. Deliberately bypasses `deposit_event`: this
        /// effect already happened and was already told to the litter once,
        /// live; a node catching its own storage up after a restart is not
        /// a fresh thing for `System::events()` to report. `miot-node` owns
        /// telling clients about replayed history via its own `/events` log
        /// instead — see `docs/PROTOCOL.md` and `HANDOFF.md` item 2.
        pub fn replay_effect(effect: &Effect<T::AccountId>, now: BlockNumber) {
            // `Stats` lives outside `Litter`/`TaskTable` entirely — fold it
            // directly rather than through `table.apply`, which only knows
            // about `miot_tasks::State`.
            if let Effect::StatsReported { who, turns, tool_calls, tokens, ms } = effect {
                Stats::<T>::insert(who.clone(), CatStats { turns: *turns, tool_calls: *tool_calls, tokens: *tokens, ms: *ms });
                return;
            }
            if let Effect::StatsReported2 { who, turns, tool_calls, messages, tokens, ms } = effect {
                Stats::<T>::insert(who.clone(), CatStats { turns: *turns, tool_calls: *tool_calls, tokens: *tokens, ms: *ms });
                MessagesSent::<T>::insert(who.clone(), *messages);
                return;
            }
            if let Effect::Voted { who, artifact, up } = effect {
                Votes::<T>::mutate(artifact.clone(), |t| cast_vote(t, who, *up));
                return;
            }
            if let Effect::Message { from, artifact_id: Some(a), body, .. } = effect {
                // The comment half of `post` — the same insert the primary
                // executed, against the same (snapshot-carried) epoch.
                let epoch = Epoch::<T>::get();
                Comments::<T>::mutate(a, epoch, |v| {
                    v.push(Comment { who: from.clone(), at: now, body: body.clone() });
                    if v.len() > COMMENT_KEEP {
                        v.remove(0);
                    }
                });
                return;
            }
            let mut table = Self::table();
            table.apply(effect, now);
            Litter::<T>::put(table.into_state());
        }

        /// See [`Roster`].
        pub fn roster() -> Vec<(String, T::AccountId)> {
            Roster::<T>::get()
        }

        /// An artifact's current tally — `GET /artifact/{id}`'s and
        /// `/note/{id}`'s `votes` field.
        pub fn tally(artifact: ArtifactId) -> Tally<T::AccountId> {
            Votes::<T>::get(artifact)
        }

        /// The current epoch's comment thread under an artifact — the only
        /// one that exists as far as this session is concerned.
        pub fn comments(artifact: ArtifactId) -> Vec<Comment<T::AccountId>> {
            Comments::<T>::get(artifact, Epoch::<T>::get())
        }

        /// The current epoch — session bounded by the last compaction.
        pub fn epoch() -> u32 {
            Epoch::<T>::get()
        }

        /// See [`Replaying`]. Plain function, not a call: whether this node
        /// is producing or folding blocks is the host's business.
        pub fn set_replaying(on: bool) {
            Replaying::<T>::put(on);
        }

        /// Run the tick for the block that is currently open, as
        /// `on_initialize` would have if [`Replaying`] had been off when it
        /// opened. A node promoted to block producer mid-block calls this
        /// once so the first block it closes isn't missing its tick.
        pub fn tick_now() {
            let now: BlockNumber = frame_system::Pallet::<T>::block_number().unique_saturated_into();
            let mut table = Self::table();
            let effects = table.tick(now);
            if !effects.is_empty() {
                Self::commit(table, effects);
            }
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
