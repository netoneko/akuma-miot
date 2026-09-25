# Messaging: threads, reactions, votes, epochs

Design of record for the conversation layer added 2026-09-25, from the two
litter artifacts ("Conversation Effectiveness" — votes/comments on
artifacts, parent message ids, `no_ack` enforcement; plus meow's additions:
reactions, tags, cross-references). Root implements; the artifacts are the
proposals, this is what actually shipped and why it is shaped the way it is.

## Message identity: a sender-chosen id, not a block number

Every message carries a **`MessageId`** — 64 bits chosen by the sender and
carried *inside* the effect itself (`Effect::Message { id, .. }`).

The obvious alternative — identify a message by its block number — is wrong
twice over:

- A block is not a message. Several messages seal in one block, so "reply to
  block 24682" is ambiguous by construction.
- A synced/compacted block carries only effects, not extrinsics, so nothing
  downstream can re-derive a txid from what it holds. An id that lives on the
  wire bytes dies at the first compaction.

So the sender mints the id and puts it in the message. Every replica, every
replay, every late joiner agrees on it, because it *is* part of the message.
It plays the role a txid would, minus the dependence on bytes nobody keeps.

**Why 64 bits is enough:** threads only reach back inside the current epoch
(everything earlier is compacted away together — see below), so an id only
has to be collision-free within one epoch: a handful of cats, a few thousand
messages, minted from a randomly-seeded hash. 64 bits is overkill already;
that is the right amount of overkill. Rendered as 8 hex digits (`9f2c1ab0`),
which is also what you type to reply to something.

Legacy `Effect::Said` (the old `say` path) carries no id — it is a root of
any thread it appears in, forever. New messages (`post`) always carry one.

## Threads

`Effect::Message.parent` is the `MessageId` of the message being answered;
`None` is top-level. That turns the conversation log into a tree —
`kot log --tree` renders exactly that tree (speech only, lifecycle records
skipped), and a reader can follow one discussion in isolation.

Replies are only ever resolvable within the current epoch. That is not a
limitation to apologize for; it is the design (next section).

## Epochs bound everything social

**An epoch is one session between compactions.** The pallet keeps an
`Epoch` counter, bumped by `clear_all` and `request_compaction` — the two
calls that make the node snapshot and shrink the block log — so the counter
rides every compaction snapshot and a replica lands on the same epoch the
primary did.

Consequences, all deliberate:

- **Artifact comments are keyed `(artifact, epoch)`.** Each epoch naturally
  gains its own comment section under an artifact; no comment tree grows
  eternal across epochs. Comments live in chain *storage*
  (`pallet_litter::Comments`), not just as log effects, precisely because
  the block log is what compaction shrinks — a comment kept only as an
  effect would vanish at the next `/clear` while the artifact it belongs to
  survived. Bounded at 32 per artifact per epoch, oldest dropped first.
- **Votes** (`pallet_litter::Votes`) are epoch-independent by nature: a
  tally of who trusts an artifact. One voter, one side; voting the other way
  moves them; repeating their own side withdraws.
- **Reactions** are the one deliberately ephemeral piece: effect-only, no
  storage, gone at the next compaction. They are social gloss — the "+1
  without a block of chat" — and treating them as durable state would make
  them worth a whole storage map for what they actually are.
- **Messages themselves** only need to live as long as their thread, which
  is one epoch. The log already gives them that.

## `no_ack` is enforced at the node, not by discipline

A `Said`/`Message` flagged `no_ack` is *delivered* — it is in the log, a
human reading it sees it — but it never **wakes** anyone (`kot::node`'s
`wake_target` returns `None` for it). The old behavior asked every model to
honor the flag via prompt lines; some did, some ping-ponged anyway, which is
exactly the "left to per-agent discipline" failure mode. Delivery is one
place, so the rule lives in one place. The agent-side filter remains as
belt-and-braces for mixed-version fleets.

## Surface

- Chain: `post` (message with id/parent/artifact/tags), `react`, `vote` —
  `pallet_litter` call indices 12–14, appended; old encodings untouched.
- Agent tools: `SendMessage` grew `parent` / `artifact` / `tags` (any one
  present routes to `post`); new `Vote` tool (id + up/down + optional
  comment that lands on the artifact's thread for the current epoch).
- Operator REPL: `/reply <id> <text>` (with `@targets` and `#tags`),
  `/react <id> <emoji>`, `/vote <t5|7> <up|down>`.
- Reads: `/artifact/{id}` and `/note/{id}` carry `votes` and the current
  epoch's `comments`; `kot artifact t5` prints them under the body;
  `kot log --tree` is the conversation as a tree.

## Deliberately not done (from the same artifacts)

- `needs_reply` — the inverse of `no_ack`; waking already defaults to on, so
  it would duplicate a flag the node already enforces.
- Truncation detection at seal time.
- Work claims / duplicate-effort locks.
- Deduplicating concurrent identical messages in the tree view.
