# `miot-cli` — requirements

Design of record. The terminal UI proper (§0, §1, §6, §7, §8 — the composer,
avatars, colour rules) is not built yet (Phase 3+; there is no alt-screen, no
composer, nothing here contradicts §0's non-negotiable, because none of it
exists to violate it). Slices of §2, §3, §4 and §5 **are** built, 2026-09-21
through 22 — not as a separate `miot-cli` binary but as `miot --rpc <url>`.
The crate used to be named `miot-sim` and was renamed once `--rpc` made it
more than a simulator; it ships as `dist/miot`.

What exists:

- **§5, non-interactive:** `--open "<text>"`, `--say "<body>" [--to <name>]`,
  `--clear` (§ below — not in the original doc, added once a live run showed
  the need). Each signs and submits against real endpoints (`/meta`,
  `/account/:id`, `/submit`) that did not exist before this either.
- **§2, addressing:** `@name` tagging, resolved client-side against a
  `name=seed` roster — never on the wire — exactly as this doc anticipated
  ("Implemented today against placeholder u64 accounts; becomes a key lookup
  unchanged when the runtime's `AccountId` switches"). That switch happened;
  the resolution logic didn't need to. **Multi-tag and explicit broadcast
  added 2026-09-22** — see the resolution to §8's open question, below.
- **§2/§0-adjacent, interactive:** `--rpc <url> --chat` reuses the in-process
  `--chat` REPL (`chat.rs`) verbatim — same prompt, same `/quit`, same
  `@name` handling — but every line is a real signed extrinsic and replies
  come from whatever cats are actually running against that node. Not a
  composer (§1): still a plain blocking `stdin().lines()` prompt, same as the
  in-process version always was.
- **§3, observation, informally:** the scripted demo's `block/who/what`
  printer already reads as this doc's `[obs]` view; `--rpc --chat`'s reply
  printer is the same shape. No `/obs` toggle exists — everything currently
  printed is what `/obs all` would show, unconditionally.
- **§4, history:** `--rpc --chat` replays on start — bounded (last 30
  events) — matching this doc's "bounded (`--history N`, default small) so a
  fresh start is not a wall of text." The boundary it replays *since* is
  `since=0` (everything `miot-node`'s in-memory ring still holds, `LOG_CAP`
  4096), not "since last compaction" — that boundary doesn't exist yet
  (`miot-store` isn't wired into the node). Once it is, this call site is
  where the real boundary lands; nothing about the client changes.
  **Rendering is rough** — `said` effects print as `name  body`, but every
  other effect (`assigned`, `nudge`, `directed`, `record`, `failed`, …)
  prints as the raw `{DIM}block N  <json>{OFF}` blob, not the scripted demo's
  human phrasing (`"chain → mimi: [plan-needed: t1]"`, `"claimed t1.1"`).
  Worth reusing that renderer here instead of a second, uglier one.

Not built: `/peers`, `/tasks`, `/plan`, `/artifact` as flags or commands,
`/obs`, `/history [n]` as an on-demand command (only the automatic start-up
replay exists), and everything about §0/§1/§6's actual terminal UI (composer,
avatars, colour-per-sender persistence across a session). This remains the
contract for all of that.

## `--clear` / `/clear` — not in the original design, added 2026-09-22

A session boundary: fails every currently-open parent at once
(`pallet_litter::Call::clear_all`, operator-only), so an operator can move to
a fresh task without old stalled ones still nagging. `/clear` in `--chat`
(both in-process and `--rpc`); `--clear` as a `--rpc` flag. Task ids keep
incrementing — this is not a new genesis, just old parents going quiet.
Reuses `TaskStatus::Failed`/`Effect::Failed`, the same outcome
`DirectiveNag`'s new budget produces when a leader never resolves a directive
— see `HANDOFF.md` "Decisions" for why those two are deliberately not
distinguished in the type.

---

## 0. The one non-negotiable

**Terminal scrollback must work exactly as it does for any other program.**

Scroll up and you see what happened. Select and copy works. `tmux` copy-mode
works. Piping to a file works. `Ctrl-C` leaves the terminal in a sane state.

That single requirement decides most of the rest, because it rules out the
whole class of design that would otherwise be reached for:

> **`miot-cli` is NOT a full-screen TUI.**
> No alternate screen buffer (`smcup`/`1049h`). No owning the viewport. No
> repainting a scrollable pane we implement ourselves.

Everything the litter says is written to stdout as ordinary lines, in order,
once. The terminal keeps them; we do not. An in-app scrollable history pane is
the thing that breaks native scrollback, so we do not have one — see §4.

---

## 1. Layout

A **line-oriented log that grows upward**, with a small fixed composer pinned
at the bottom. The composer is drawn with relative cursor moves only.

```
  ┌─ terminal scrollback (owned by the terminal, not by us) ────────────┐
  │                                                                     │
  │  14:02  tama   claimed t1.1                                         │
  │  14:04  kuro   audit locking — one lock unheld on the error path     │
  │  14:04  mimi   cleared t1.2                                         │
  │                                                                     │
  │  [obs]  #1180  Happened(Directed{to:mimi, t1, ArtifactNeeded})      │  ← §3
  │                                                                     │
  ├─────────────────────────────────────────────────────────────────────┤
  │  litter ▸ @tama can you re-run the build with -j1                    │  ← §2
  │          and paste just the failing lines_                           │
  └─────────────────────────────────────────────────────────────────────┘
```

The composer occupies N lines at the bottom. To print new output: clear the
composer, write the lines, redraw the composer. Output above never moves and
is never rewritten.

**Not a TTY** (piped, redirected, CI) → no composer, no colour, no cursor
tricks. Just the log, one line at a time.

---

## 2. Input

### Multiline

The composer is a **multiline editor**, not a single-line prompt.

| Key | Does |
|---|---|
| `Enter` | send |
| `Alt-Enter` / `Shift-Enter` | newline within the message |
| `Ctrl-J` | newline (the terminal-independent fallback — many terminals do not distinguish `Shift-Enter`) |
| `Up` / `Down` | move within the message when it is multiline; recall previous input when the cursor is at the first/last line |
| `Ctrl-C` | clear the composer; a second within 1 s exits |
| `Ctrl-D` | exit on an empty composer |

It grows as it needs to and scrolls internally past a cap (say 10 lines) so a
paste of 400 lines does not eat the screen.

### Addressing: the litter by default, participants by tag

**The default target is the whole litter.** Typing prose and pressing Enter
broadcasts. That is the common case and it costs no syntax.

`@name` **tags a participant**. Tagging is addressing:

```
  can everyone look at the lock ordering          → the litter
  @tama re-run the build with -j1                 → tama, and the litter sees it
  @tama @kuro compare your findings               → both, and the litter sees it
```

Rules:

- A tag anywhere in the message tags that participant. Leading position is
  conventional, not required.
- Tagged traffic is **waking** for whoever is tagged (it assembles a turn);
  untagged litter traffic is **non-waking** by default. This is the same
  asymmetry the protocol already enforces — waking four agents per broadcast
  turns one message into four LLM turns.
- `@all`, `@cats`, and `@litter` are synonyms that explicitly target
  everyone — same effect as leaving the message untagged (`to: None` from
  root already wakes every cat, see `Effect::wakes`), just spelled out. If
  one of these appears anywhere in the message, any other `@name` tags in
  the same line are ignored: broadcast already reaches them.
- Tagging more than one name (`@tama @kuro compare your findings`) tags
  both — **resolved 2026-09-22, see §8** as one `say` extrinsic per
  addressee, submitted in the order the tags appear in the line, all
  carrying the same body. Replies are printed in that same order, as they
  come back (sequentially in `--chat`'s in-process turn loop; in arrival
  order off the event log for `--rpc --chat`).
- An unknown `@name` is a **soft warning printed above the composer, not a
  refusal** — the message still sends (as a broadcast, since there was
  nothing valid to target). Refusing to send because a name was misspelled
  is worse than sending it.
- Tab-completes from the on-chain roster.
- **`@name` resolves to a key, not a name.** Once identities are public keys
  (`miot-keys`: an account *is* a 32-byte ed25519 key), a tag is just the
  local, human-facing spelling of an `Option<AccountId>` — the `to` field of
  the `say` extrinsic. The name never goes on the wire; it is resolved
  client-side against the roster before the call is signed, which is why a
  misspelled tag is a local warning rather than a chain-level anything.
  *Implemented today against placeholder u64 accounts; becomes a key lookup
  unchanged when the runtime's `AccountId` switches.*
- The composer shows the resolved target on its prompt: `litter ▸`,
  `→ tama ▸`, `→ tama,kuro ▸`.

---

## 3. Protocol observation

A **toggle**, off by default, that prints raw protocol traffic **below the log
and above the composer** as it happens.

```
  miot> /obs              toggle
  miot> /obs on|off
  miot> /obs tasks        only task lifecycle effects
  miot> /obs all          every effect, including non-waking broadcast
```

- Off by default because the interesting output is what the cats say, not the
  bookkeeping underneath it.
- When on, each line is prefixed `[obs]` and dimmed, so it is visually
  subordinate and greppable.
- It is **printed, not paged** — observation lines go into scrollback like
  everything else. No separate pane.
- Shows: block height, the `Effect` variant, the task it concerns, and who it
  was addressed to. That is `Effect`'s own vocabulary, unmodified — the CLI
  does not invent a second spelling of it.

---

## 4. Message history

**Available, but it must never compete with scrollback.**

- On start, the CLI prints a **replay** of recent litter traffic as ordinary
  lines, so scrollback has context. Bounded (`--history N`, default small) so
  a fresh start is not a wall of text.
- `/history [n]` prints the last `n` into scrollback on demand.
- `/history --task t42` prints one task's traffic.
- There is **no in-app scrollable history viewer.** If one is ever wanted it
  ships **disabled by default** and behind an explicit flag, because the moment
  it takes the viewport it has broken §0.
- For real archaeology, `miot log` (§5) is a non-interactive dump that pipes to
  `less`, `grep`, or a file. That is the right tool, and it is the terminal's
  job, not ours.

---

## 5. Commands

Simple. Flat. No nested menus. Slash commands inside the interactive session,
subcommands outside it — and they are the same verbs.

### Interactive (`miot`)

| | |
|---|---|
| `/task <text>` | open a parent task (operator only) |
| `/plan` | show the current plan |
| `/tasks` | one line per live task: id, status, assignee, lease |
| `/artifact <id>` | print a closed parent's report |
| `/peers` | the roster, with who is leader |
| `/obs [on\|off\|tasks\|all]` | §3 |
| `/history [n]` | §4 |
| `/quit` | leave |

Anything not starting with `/` is a message (§2).

### Non-interactive (`miot <cmd>`)

The operator's hands, and what scripts and the failure drills use. One-shot:
sign one extrinsic or read state, print, exit.

```
  miot run --as tama              run an agent loop (this is the cat)
  miot task open "debate & report"
  miot task list
  miot artifact t42               → markdown on stdout, pipe it anywhere
  miot peers
  miot log [--task t42] [--follow]
```

`miot run` is the only long-lived one, and it is **not interactive** — it emits
events, it does not draw a composer. An agent has no keyboard.

---

## 5a. Connecting: any node, no database

The CLI is a **client of the chain**, never a peer of it. Three cases, one
code path:

| Case | What runs locally | State on disk |
|---|---|---|
| `miot run --as tama` | a full node **in-process** + the agent loop | the node's DB |
| `miot task open "…"` on a cat's host | nothing | none |
| `miot task open "…"` from a laptop | nothing | none |

In every case the CLI reaches the chain over **RPC**, even when the node it is
talking to is inside the very same process. That is deliberate: grabbing the
embedded node's client handle directly would be faster and would create a
second code path that only works when co-located — and then the remote case
(an operator on a laptop, an agent on an Akuma guest) would be a port rather
than a config change.

```
  miot --rpc ws://any-cat:9944 task open "debate & report"
  miot --rpc ws://localhost:9944 artifact t42
  miot                                   # interactive, default endpoint from config
```

- **Any swarm node will do.** There is no privileged endpoint; every cat runs
  the same node. A dead endpoint is a reconnect to the next one in the list,
  not an outage.
- **No database is downloaded.** An operator CLI holds no state at all — it
  submits an extrinsic or reads storage and exits.
- The CLI **may** run a node (`miot run`), but only a cat needs to.

### Trust

Plain RPC means trusting the node you asked, and that is **correct here, not a
compromise**. The litter is one operator and one trust domain, and the operator
holds the root key — the key is what authority is checked against
(`ensure_signed`, `EnsureRoot`), so trusting a swarm you own and key is not a
gap to close.

No light client, no storage proofs, no smoldot. Verifying a swarm against
itself would buy nothing when the thing being verified and the thing verifying
are both yours.

---

## 6. Visual style

Copied from Akuma. `assets/` holds the vendored art.

- **Banner**: `akuma_40.txt` on start, once. Not repeated.
- **Per-sender avatars**: `akuma_20.txt` with **one ANSI colour per sender**,
  reused every time that sender speaks — exactly what
  `meow/src/tools/litter/observe.rs` does, and for the stated reason: it is
  what makes a scroll of the whole litter's back-and-forth readable at a
  glance. Bright variants first; the art is mostly `%#*+` glyphs and reads
  badly dim on a dark terminal.
- Avatars are opt-out (`--no-avatars`) because they cost vertical space, and
  the compact form is a coloured name.
- Colour follows `NO_COLOR` and disables itself when stdout is not a TTY.
- No box-drawing around the log. Boxes and scrollback are enemies.

---

## 7. What it must not do

- Enter the alternate screen.
- Redraw anything above the composer.
- Hold the terminal on `Ctrl-C` or leave it without a cursor.
- Invent a second vocabulary for `Effect` — the observation view and the event
  log use the protocol's own names. Two spellings of one thing is the drift
  this project exists to avoid.
- Block the agent loop on a human. `miot run` and `miot` are separate
  processes with separate accounts; the CLI is a client of the chain like
  anything else.

---

## 8. Open

- **Streaming a turn.** A model's output arrives token by token over minutes.
  Printing it live is nice but it interleaves badly with other cats' traffic
  arriving mid-stream. Options: buffer the turn and print it whole, or print a
  `tama is thinking…` line that is replaced on completion (a rewrite, which
  §0 resists). Leaning toward buffering with a spinner on the composer line.
- **Tagging semantics on chain — resolved 2026-09-22, for now.** `say`'s `to`
  field stays `Option<AccountId>` (singular) rather than growing a `Vec` —
  no pallet/primitives change. Multiple tags in one line become **multiple
  `say` extrinsics**, one per addressee, same body, submitted in tag order.
  Chosen over a schema change because it needed no protocol surgery and
  every existing consumer (`Effect::wakes`, the scripted demo's printer,
  `--rpc --chat`'s replay) already understands "one `Said` effect, one
  `to`." Cost: the chain log shows the same body once per addressee rather
  than once with a list of recipients — a real duplicate, not a rendering
  choice, so `/history` and replay will show it too. Revisit if that
  duplication ever matters (e.g. once `miot-store` persists everything
  forever rather than a bounded ring) — a `Vec<AccountId>` field or a
  separate `TaggedIn: Vec<TaskId>`-style side table would collapse it back
  to one record.
