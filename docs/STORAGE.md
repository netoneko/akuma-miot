# Storage: what lives where

Design of record for the messaging/social layer's storage (2026-09-25, from
the artifacts-5/6 work). The general rule hasn't changed: **chain state in
ParityDB is the only durable store** — every mesh member replays the same
effect log into the same state, and a compaction snapshot carries it across
an epoch. There is no second store to reconcile; `docs/references/storage.md`
describes the two-store split, of which only the chain half was ever built.

## The four layers

```text
  artifacts            Litter state (TaskTable::State)          epochs ∞
  votes                Litter state (pallet_litter::Votes)      epochs ∞
  comments             Litter state (pallet_litter::Comments)   epochs ∞ (stamped per epoch)
  messages, reactions  block-log effects only                   one epoch (by design)
```

## Artifacts — epoch-independent, by nature

A closed task's report and a standalone note live in `TaskTable::State`
(`artifacts` / `standalone_artifacts`), which is plain `Litter` storage:
they ride every compaction snapshot, so an artifact travels through epochs
untouched. That is the point of an artifact — it *is* the durable thing.

## Votes — one storage read, no epoch

`pallet_litter::Votes: StorageMap<ArtifactId, Tally>` — `Tally { up, down }`
as account lists, not counts, because "who thought this was trustworthy" is
the signal. One voter, one side: voting the other way moves them, repeating
their own side withdraws. Epoch-independent on purpose — a tally is a
property of the artifact, not of a session. Written by `vote` (call 14) and
folded by `replay_effect` from the `Effect::Voted` it emits, so every replica
lands in the same place.

## Comments — keyed by artifact, stamped with epoch

`pallet_litter::Comments: StorageMap<ArtifactId, Vec<Comment>>` where
`Comment { who, at, epoch, body }`.

The epoch is **on the entry, not in the key**. Keying by `(artifact, epoch)`
would make "give me t5's thread" a scan over epochs; keying by artifact
alone means one `get(artifact)` returns the whole thread across every
session, old epochs included — nothing is lost at a compaction, and which
section a comment belongs to is a fact about the comment, drawn by the
reader (`/artifact/{id}` returns all; `?epoch=N` filters the view).

Bounded by the `COMMENT_KEEP` sliding window (64, oldest dropped first) —
the same philosophy as the nudge budget: unbounded growth is a loop that
pays forever. Written by `post` with an `artifact_id` (call 12), folded by
`replay_effect` from the `Effect::Message` it emits.

## The epoch counter

`pallet_litter::Epoch: StorageValue<u32>` — the session bounded by a
compaction. Bumped by `clear_all` and `request_compaction` (call indices 7
and 9), the two extrinsics that make the node snapshot and shrink the log,
so the counter itself rides every snapshot and a replica folding after a
rewind lands on the same epoch the primary did. It exists to *stamp* things
(comments) and to bound what has to stay live (messages, threads — a reply
only ever reaches back inside the current epoch).

## Messages and reactions — the block log, one epoch

`Effect::Message` (with its sender-minted `MessageId`) and `Effect::Reacted`
have no storage at all: they live in the block log and in `/events`, which
is exactly one epoch long. That is sufficient by construction — threads
don't reach past an epoch, reactions are social gloss — and permanent
storage for either would be a map paid for forever by data whose whole
lifetime was one session. When a message *does* need to outlive the epoch,
it stops being a message and becomes an artifact comment or an artifact.

## Message identity

`MessageId` is 64 bits minted by the sender and carried inside the effect.
Not a block number (a block can carry several messages — it's not an
identity), not a wire txid (a synced/compacted block carries effects, not
extrinsics, so nothing downstream could re-derive it). Sender-minted and
self-contained, every replay and every replica agrees on it because it *is*
part of the message. 64 bits is enough because ids only have to be unique
within one epoch.

## What a restart actually does

`Node::open` replays blocks since the last checkpoint into fresh
externalities (state = snapshot + tail); `Epoch` and `Comments`/`Votes` come
back from the snapshot, message effects come back from the replayed tail,
and everything before the checkpoint is gone except what made it into
state. That asymmetry is the whole design: state is what survives, the log
is what's live.

## Not stored anywhere

- Off-record messages (`off_record: true`): fanned out live, never sealed.
- Reactions.
- Anything a cat's agent loop keeps locally (`~/.akuma/kot/` transcripts,
  local task lists) — host-local convenience, not consensus state, lost on
  restart by design.
